// The kqueue backend driven one kevent batch at a time, without its event-loop thread: each sequence runs on the
// real filesystem, the pending batch is handled in a chosen order, and the test checks the events and that every
// path on disk is watched through a descriptor on the file now at that path.
#![cfg(target_os = "macos")]

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use tsrs_fswatch::debounce::Debounce;
use tsrs_fswatch::event::EventKind;
use tsrs_fswatch::kqueue::{KqueueBackend, new_kqueue_backend};
use tsrs_fswatch::pathcompare::BytePathComparer;
use tsrs_fswatch::watcher::{DirWatch, WatcherImpl};

struct Kq {
    backend: KqueueBackend,
    db: Arc<Debounce>,
    root: String,
    _dir: TempDir,
}

struct TempDir(String);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Kq {
    fn new(name: &str) -> Kq {
        let dir = std::env::temp_dir().join(format!("tsrs-kq-seq-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap().to_string_lossy().into_owned();
        let backend = new_kqueue_backend();
        backend.open_kqueue().unwrap();
        Kq {
            backend,
            db: Debounce::new_without_loop_for_test(),
            root: root.clone(),
            _dir: TempDir(root),
        }
    }

    fn path(&self, rel: &str) -> String {
        if rel.is_empty() { self.root.clone() } else { format!("{}/{rel}", self.root) }
    }

    fn watch(&self, rel: &str, recursive: bool) -> Arc<DirWatch> {
        let dir = self.path(rel);
        let w =
            DirWatch::new(dir.clone(), dir, &self.db, BytePathComparer::default(), None, recursive);
        self.backend.subscribe(&w).unwrap();
        w
    }

    // Reads every pending event, puts directories' events first or last, and handles them as one batch.
    fn settle(&self, dirs_first: bool) {
        let mut batch = Vec::new();
        // SAFETY: zeroed kevent structs are valid.
        let mut events: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; 256];
        let timeout = libc::timespec { tv_sec: 0, tv_nsec: 50_000_000 };
        loop {
            let n = self.backend.read_events(&mut events, Some(&timeout)).unwrap();
            if n == 0 {
                break;
            }
            batch.extend_from_slice(&events[..n]);
        }
        let dir_fds = self.dir_fds();
        batch.sort_by_key(|ev| dir_fds.contains(&(ev.ident as i32)) != dirs_first);
        self.backend.handle_events(&batch, &mut Vec::new());
    }

    fn dir_fds(&self) -> Vec<i32> {
        let st = self.backend.mu.lock().unwrap();
        let mut fds = Vec::new();
        #[expect(clippy::iter_over_hash_type, reason = "collected into a set")]
        for m in st.entry_maps.values() {
            #[expect(clippy::iter_over_hash_type, reason = "collected into a set")]
            for e in m.values() {
                if e.is_dir
                    && let Some(fd) = e.state
                {
                    fds.push(fd);
                }
            }
        }
        fds
    }

    // Sorted "kind path" lines of the watch's pending events, relative to the root; the list is cleared.
    fn take(&self, w: &DirWatch) -> Vec<String> {
        let (events, _) = w.events.drain();
        let mut out: Vec<String> = events
            .into_iter()
            .map(|e| {
                let rel =
                    e.path.strip_prefix(&self.root).unwrap_or(&e.path).trim_start_matches('/');
                let kind = if e.kind == EventKind::DELETE { "delete" } else { "update" };
                format!("{kind} {rel}")
            })
            .collect();
        out.sort();
        out
    }

    // Every path under `w`'s root (direct children only when not recursive) is an entry whose descriptor is open on
    // the file now at that path, and no entry is left for a path that is gone. A link that resolves to nothing
    // cannot be opened: it may be missing or held without a descriptor.
    fn assert_watching_disk(&self, w: &Arc<DirWatch>, recursive: bool) {
        let mut disk = vec![w.dir.clone()];
        list(&w.dir, recursive, &mut disk);
        let st = self.backend.mu.lock().unwrap();
        let map_ids: Vec<_> = st.subs_by_path[&w.dir]
            .iter()
            .filter(|s| Arc::ptr_eq(&s.dir_watch, w))
            .map(|s| s.entries)
            .collect();
        assert_eq!(map_ids.len(), 1);
        let entries: Vec<(String, Option<i32>)> =
            st.entry_maps[&map_ids[0]].values().map(|e| (e.path.clone(), e.state)).collect();
        drop(st);
        for (path, fd) in &entries {
            assert!(disk.contains(path), "{path} is gone but still watched");
            match (std::fs::metadata(path), fd) {
                (Ok(m), Some(fd)) => assert_eq!(
                    fstat(*fd),
                    (m.dev(), m.ino()),
                    "{path} is watched through a stale descriptor"
                ),
                (Ok(_), None) => panic!("{path} has no descriptor"),
                (Err(_), _) => {}
            }
        }
        for path in &disk {
            if std::fs::metadata(path).is_ok() {
                assert!(entries.iter().any(|(p, _)| p == path), "{path} is not watched");
            }
        }
    }
}

fn list(dir: &str, recursive: bool, out: &mut Vec<String>) {
    for ent in std::fs::read_dir(dir).unwrap() {
        let ent = ent.unwrap();
        let p = format!("{dir}/{}", ent.file_name().to_string_lossy());
        let is_dir = ent.file_type().unwrap().is_dir();
        out.push(p.clone());
        if is_dir && recursive {
            list(&p, true, out);
        }
    }
}

fn fstat(fd: i32) -> (u64, u64) {
    // SAFETY: zeroed `stat` is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is live and writable; `fd` is open.
    assert_eq!(unsafe { libc::fstat(fd, &raw mut st) }, 0, "fstat({fd})");
    (st.st_dev as u64, st.st_ino)
}

fn write(path: &str, data: &str) {
    std::fs::write(path, data).unwrap();
}

fn lines(v: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = v.iter().map(ToString::to_string).collect();
    v.sort();
    v
}

// vim's save with 'backupcopy' off: swap file, the original renamed to the backup, the new file written, the backup
// and the swap file removed. When the directory's event is handled first, the replaced file's descriptor is closed
// and its number reused for the new file before the old file's rename event in the same batch is reached.
#[test]
fn vim_save_keeps_the_file_watched() {
    if !crate::isolated::in_child("kqueue_sequences::vim_save_keeps_the_file_watched") {
        return;
    }
    let kq = Kq::new("vim");
    write(&kq.path("a.ts"), "export const a = 0;\n");
    let w = kq.watch("", true);
    for (round, dirs_first) in [(1, true), (2, false), (3, true)] {
        write(&kq.path(".a.ts.swp"), "swap");
        std::fs::rename(kq.path("a.ts"), kq.path("a.ts~")).unwrap();
        write(&kq.path("a.ts"), &format!("export const a = {round};\n"));
        std::fs::remove_file(kq.path("a.ts~")).unwrap();
        std::fs::remove_file(kq.path(".a.ts.swp")).unwrap();
        kq.settle(dirs_first);
        let got = kq.take(&w);
        assert!(got.contains(&"update a.ts".to_string()), "round {round}: {got:?}");
        kq.assert_watching_disk(&w, true);

        write(&kq.path("a.ts"), &format!("export const a = {round}0;\n"));
        kq.settle(dirs_first);
        assert_eq!(
            kq.take(&w),
            lines(&["update a.ts"]),
            "round {round}: in-place edit after the save"
        );
    }
}

type Step = fn(&Kq);

// a.ts b.ts sub/c.ts sub/deep/d.ts other/ link -> a.ts dirlink -> sub
fn tree(kq: &Kq) {
    std::fs::create_dir_all(kq.path("sub/deep")).unwrap();
    std::fs::create_dir_all(kq.path("other")).unwrap();
    for f in ["a.ts", "b.ts", "sub/c.ts", "sub/deep/d.ts"] {
        write(&kq.path(f), "export {};\n");
    }
    std::os::unix::fs::symlink("a.ts", kq.path("link")).unwrap();
    std::os::unix::fs::symlink("sub", kq.path("dirlink")).unwrap();
}

const SEQUENCES: &[(&str, Step)] = &[
    ("atomic-save", |kq| {
        write(&kq.path("a.ts.tmp"), "export const a = 1;\n");
        std::fs::rename(kq.path("a.ts.tmp"), kq.path("a.ts")).unwrap();
    }),
    ("in-place", |kq| write(&kq.path("sub/c.ts"), "export const c = 1;\n")),
    ("create-delete-same-batch", |kq| {
        write(&kq.path("tmp.ts"), "x");
        std::fs::remove_file(kq.path("tmp.ts")).unwrap();
    }),
    ("create-dir-tree", |kq| {
        std::fs::create_dir_all(kq.path("new/x/y")).unwrap();
        write(&kq.path("new/x/y/e.ts"), "x");
        write(&kq.path("new/f.ts"), "x");
    }),
    ("rm-rf", |kq| std::fs::remove_dir_all(kq.path("sub")).unwrap()),
    ("rename-file-across", |kq| {
        std::fs::rename(kq.path("sub/c.ts"), kq.path("other/c.ts")).unwrap();
    }),
    ("rename-dir-across", |kq| {
        std::fs::rename(kq.path("sub/deep"), kq.path("other/deep")).unwrap();
    }),
    ("dir-replaced-by-file", |kq| {
        std::fs::remove_dir_all(kq.path("sub")).unwrap();
        write(&kq.path("sub"), "x");
    }),
    ("file-replaced-by-dir", |kq| {
        std::fs::remove_file(kq.path("b.ts")).unwrap();
        std::fs::create_dir(kq.path("b.ts")).unwrap();
        write(&kq.path("b.ts/x.ts"), "x");
    }),
    ("dir-renamed-over-empty-dir", |kq| {
        std::fs::create_dir(kq.path("other/deep")).unwrap();
        write(&kq.path("other/deep/z.ts"), "x");
        std::fs::rename(kq.path("other/deep"), kq.path("sub/deep2")).unwrap();
        std::fs::remove_dir(kq.path("sub/deep2")).unwrap_or_default();
    }),
    ("symlink-retarget", |kq| {
        std::os::unix::fs::symlink("b.ts", kq.path("link2")).unwrap();
        std::fs::rename(kq.path("link2"), kq.path("link")).unwrap();
    }),
    ("unrelated-create-beside-links", |kq| write(&kq.path("other.ts"), "x")),
    ("case-only-rename", |kq| std::fs::rename(kq.path("a.ts"), kq.path("A.ts")).unwrap()),
    ("non-ascii-nfc-to-nfd", |kq| {
        write(&kq.path("caf\u{e9}.ts"), "x");
        std::fs::rename(kq.path("caf\u{e9}.ts"), kq.path("cafe\u{301}.ts")).unwrap();
    }),
    ("hard-link-then-atomic-save", |kq| {
        std::fs::hard_link(kq.path("a.ts"), kq.path("hl.ts")).unwrap();
        write(&kq.path("a.ts.tmp"), "y");
        std::fs::rename(kq.path("a.ts.tmp"), kq.path("a.ts")).unwrap();
    }),
    ("swap-two-files", |kq| {
        std::fs::rename(kq.path("a.ts"), kq.path("t.ts")).unwrap();
        std::fs::rename(kq.path("b.ts"), kq.path("a.ts")).unwrap();
        std::fs::rename(kq.path("t.ts"), kq.path("b.ts")).unwrap();
    }),
    ("vim-save", |kq| {
        write(&kq.path(".a.ts.swp"), "swap");
        std::fs::rename(kq.path("a.ts"), kq.path("a.ts~")).unwrap();
        write(&kq.path("a.ts"), "export const a = 1;\n");
        std::fs::remove_file(kq.path("a.ts~")).unwrap();
        std::fs::remove_file(kq.path(".a.ts.swp")).unwrap();
    }),
];

// After each sequence, in either batch order: every watch holds exactly the paths on disk, each through a descriptor
// on the current file. The events of the sequence and of a following in-place edit of every regular file are pinned
// in kqueue_sequences.txt (recorded before the backend's maps were restructured).
#[test]
fn sequences_keep_every_path_watched() {
    let mut report = Vec::new();
    for (name, step) in SEQUENCES {
        for dirs_first in [true, false] {
            let kq = Kq::new(&format!("{name}-{dirs_first}"));
            tree(&kq);
            let watches = [("root", "", true), ("sub", "sub", true), ("flat", "", false)];
            let ws: Vec<(&str, Arc<DirWatch>, bool)> = watches
                .iter()
                .map(|(label, rel, rec)| (*label, kq.watch(rel, *rec), *rec))
                .collect();
            step(&kq);
            kq.settle(dirs_first);
            for (label, w, recursive) in &ws {
                report.push(format!("{name} dirs_first={dirs_first} {label}: {:?}", kq.take(w)));
                if std::path::Path::new(&w.dir).is_dir() {
                    kq.assert_watching_disk(w, *recursive);
                }
            }
            let mut files = Vec::new();
            list(&kq.root, true, &mut files);
            files.sort();
            for p in files {
                if std::fs::symlink_metadata(&p).unwrap().is_file() {
                    let mut data = std::fs::read(&p).unwrap();
                    data.push(b'\n');
                    std::fs::write(&p, data).unwrap();
                }
            }
            kq.settle(dirs_first);
            for (label, w, _) in &ws {
                report.push(format!(
                    "{name} dirs_first={dirs_first} {label} edits: {:?}",
                    kq.take(w)
                ));
            }
        }
    }
    let want: Vec<&str> = include_str!("kqueue_sequences.txt").lines().collect();
    for (got, want) in report.iter().zip(&want) {
        assert_eq!(got, want);
    }
    assert_eq!(report.len(), want.len());
}

// Closing one of two overlapping watches leaves the other watching through its own descriptors; closing both
// leaves no descriptor open.
#[test]
fn closing_an_overlapping_watch_keeps_the_other() {
    let kq = Kq::new("close-overlap");
    tree(&kq);
    let outer = kq.watch("", true);
    let inner = kq.watch("sub", true);
    kq.backend.close_watch(&outer).unwrap();
    write(&kq.path("sub/c.ts"), "export const c = 2;\n");
    write(&kq.path("sub/new.ts"), "x");
    std::fs::remove_file(kq.path("sub/deep/d.ts")).unwrap();
    kq.settle(true);
    assert_eq!(kq.take(&outer), Vec::<String>::new());
    assert_eq!(
        kq.take(&inner),
        lines(&["delete sub/deep/d.ts", "update sub/c.ts", "update sub/new.ts"])
    );
    kq.assert_watching_disk(&inner, true);
    kq.backend.close_watch(&inner).unwrap();
    assert!(kq.backend.mu.lock().unwrap().fd_to_entry.is_empty());
}
