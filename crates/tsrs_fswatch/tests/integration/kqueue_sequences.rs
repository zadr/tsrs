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
