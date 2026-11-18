// What a non-recursive FSEvents watch delivers from its root's stream, which reports every change below the root.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use tsrs_fswatch::debounce::Debounce;
use tsrs_fswatch::fsevents_darwin::{
    FLAG_ITEM_CREATED, FLAG_ITEM_IS_DIR, FLAG_ITEM_IS_FILE, FLAG_ITEM_REMOVED, FLAG_ITEM_RENAMED,
    FLAG_KERNEL_DROPPED, FLAG_MUST_SCAN_SUB_DIRS, FLAG_USER_DROPPED, FseventsState,
    FseventsWatchSnapshot,
};
use tsrs_fswatch::fsevents_darwin_ffi::{
    StreamCallback, dispatch_test_event, dispatch_test_events, new_stream_callback,
};
use tsrs_fswatch::pathcompare::BytePathComparer;
use tsrs_fswatch::watcher::{DirWatch, ERR_OVERFLOW, ERR_WATCH_TERMINATED};

// FSEvents.h
const ITEM_INODE_META_MOD: u32 = 0x0000_0400;
const ITEM_MODIFIED: u32 = 0x0000_1000;

const FILE: u32 = FLAG_ITEM_IS_FILE;
const DIR: u32 = FLAG_ITEM_IS_DIR;

struct Fixture {
    base: String,
    root: String,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let base = std::env::temp_dir()
            .join(format!("tsrs-fswatch-non-recursive-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap().to_str().unwrap().to_string();
        let root = format!("{base}/real/nested/root");
        std::fs::create_dir_all(format!("{root}/sub")).unwrap();
        Fixture { base, root }
    }

    fn path(&self, rel: &str) -> String {
        format!("{}/{rel}", self.root)
    }

    fn write(&self, rel: &str) {
        std::fs::write(self.path(rel), "export {};\n").unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

type Delivered = Arc<Mutex<Vec<(Vec<String>, Option<tsrs_fswatch::Error>)>>>;

struct Watched {
    watch: Arc<DirWatch>,
    delivered: Delivered,
}

impl Watched {
    fn new(dir: &str, physical_dir: &str, recursive: bool, db: &Arc<Debounce>) -> Watched {
        let watch = DirWatch::new(
            dir.to_string(),
            physical_dir.to_string(),
            db,
            BytePathComparer { ignore_case: true },
            None,
            recursive,
        );
        let delivered: Delivered = Arc::default();
        let sink = Arc::clone(&delivered);
        watch.add_callback(
            dir,
            physical_dir,
            recursive,
            Arc::new(move |events, err| {
                let mut events: Vec<String> =
                    events.iter().map(|e| format!("{} {}", e.kind, e.path)).collect();
                events.sort();
                sink.lock().unwrap().push((events, err));
            }),
            None,
            "",
        );
        Watched { watch, delivered }
    }

    fn snapshot(&self) -> FseventsWatchSnapshot {
        FseventsWatchSnapshot {
            w: Arc::clone(&self.watch),
            state: Arc::new(FseventsState::default()),
        }
    }

    // The events one debounce fire delivers, and the error if any.
    fn fire(&self) -> (Vec<String>, Option<tsrs_fswatch::Error>) {
        self.watch.trigger_callbacks_for_test();
        let mut delivered = self.delivered.lock().unwrap();
        assert!(delivered.len() <= 1, "one fire delivers one batch: {delivered:?}");
        delivered.pop().unwrap_or_default()
    }
}

fn stream(watches: &[&Watched]) -> StreamCallback {
    let snapshots: Vec<_> = watches.iter().map(|w| w.snapshot()).collect();
    *new_stream_callback(&snapshots).unwrap()
}

fn nfd(s: &str) -> String {
    s.replace('\u{e9}', "e\u{301}")
}

// The events of a batch below a non-recursive watch's root: the root's own changes, its children's, and deeper ones.
fn mixed_batch(f: &Fixture) -> (Vec<(String, u32)>, Vec<String>) {
    f.write("child.ts");
    f.write("in.ts");
    f.write("Case.ts");
    f.write("sub/deep.ts");
    f.write("\u{e9}.ts");
    let case_old_exists = std::fs::symlink_metadata(f.path("case.ts")).is_ok();
    let events = vec![
        (f.root.clone(), ITEM_INODE_META_MOD | DIR),
        (f.path("child.ts"), ITEM_MODIFIED | FILE),
        (f.path("sub"), FLAG_ITEM_CREATED | DIR),
        (f.path("sub/deep.ts"), FLAG_ITEM_CREATED | ITEM_MODIFIED | FILE),
        (f.path("sub/x/y.ts"), FLAG_ITEM_REMOVED | FILE),
        (f.path("gone.ts"), FLAG_ITEM_REMOVED | FILE),
        (f.path("out.ts"), FLAG_ITEM_RENAMED | FILE),
        (f.path("in.ts"), FLAG_ITEM_RENAMED | FILE),
        (f.path("case.ts"), FLAG_ITEM_RENAMED | FILE),
        (f.path("Case.ts"), FLAG_ITEM_RENAMED | FILE),
        (nfd(&f.path("\u{e9}.ts")), FLAG_ITEM_CREATED | FILE),
        (nfd(&f.path("sub/\u{e9}/deep.ts")), FLAG_ITEM_CREATED | FILE),
        (f.path("sub/\u{e9}"), FLAG_ITEM_CREATED | DIR),
    ];
    let mut delivered = vec![
        "delete {root}/gone.ts",
        "delete {root}/out.ts",
        "update {root}/Case.ts",
        "update {root}/child.ts",
        "update {root}/in.ts",
        "update {root}/sub",
        "update {root}/\u{e9}.ts",
    ];
    // A case-only rename on a case-insensitive volume: the old spelling still finds the file.
    delivered.push(if case_old_exists { "update {root}/case.ts" } else { "delete {root}/case.ts" });
    let mut delivered: Vec<String> = delivered.iter().map(|d| d.replace("{root}", "")).collect();
    delivered.sort();
    (events, delivered)
}

fn with_root(events: &[String], root: &str) -> Vec<String> {
    let mut events: Vec<String> =
        events.iter().map(|e| e.replacen(' ', &format!(" {root}"), 1)).collect();
    events.sort();
    events
}

#[test]
fn a_non_recursive_watch_delivers_its_root_and_direct_children_only() {
    let f = Fixture::new("children");
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, false, &db);
    let cb = stream(&[&watched]);
    let (events, delivered) = mixed_batch(&f);
    let events: Vec<_> = events.iter().map(|(p, flags)| (p.as_str(), *flags)).collect();
    dispatch_test_events(&cb, &events);
    let (got, err) = watched.fire();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(got, with_root(&delivered, &f.root));

    dispatch_test_events(&cb, &[(&f.root, FLAG_ITEM_REMOVED | DIR)]);
    let (got, err) = watched.fire();
    assert_eq!(got, Vec::<String>::new());
    assert!(
        err.is_some_and(|e| e.is(ERR_WATCH_TERMINATED)),
        "the root's removal terminates the watch"
    );
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn deeper_events_leave_no_trace_on_a_non_recursive_watch() {
    let f = Fixture::new("deeper");
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, false, &db);
    let cb = stream(&[&watched]);
    f.write("sub/deep.ts");
    let non_ascii = nfd(&f.path("sub/\u{e9}/deep.ts"));
    dispatch_test_events(
        &cb,
        &[
            (&f.path("sub/deep.ts"), FLAG_ITEM_CREATED | FILE),
            (&non_ascii, FLAG_ITEM_CREATED | FILE),
            (&f.path("sub/a/b/c/d.ts"), FLAG_ITEM_REMOVED | FILE),
        ],
    );
    assert_eq!(watched.watch.events.get_events(), []);
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn a_non_recursive_watch_through_a_symlink_delivers_direct_children_under_its_name() {
    let f = Fixture::new("symlink");
    let link = format!("{}/link", f.base);
    std::os::unix::fs::symlink(&f.root, &link).unwrap();
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&link, &f.root, false, &db);
    let cb = stream(&[&watched]);
    let (events, delivered) = mixed_batch(&f);
    let events: Vec<_> = events.iter().map(|(p, flags)| (p.as_str(), *flags)).collect();
    dispatch_test_events(&cb, &events);
    let (got, err) = watched.fire();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(got, with_root(&delivered, &link));
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn a_recursive_watch_on_the_same_root_still_gets_every_event() {
    let f = Fixture::new("mixed");
    let db = Debounce::new_without_loop_for_test();
    let shallow = Watched::new(&f.root, &f.root, false, &db);
    let deep = Watched::new(&f.root, &f.root, true, &db);
    let cb = stream(&[&shallow, &deep]);
    let (events, delivered) = mixed_batch(&f);
    let events: Vec<_> = events.iter().map(|(p, flags)| (p.as_str(), *flags)).collect();
    dispatch_test_events(&cb, &events);
    let (got, err) = shallow.fire();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(got, with_root(&delivered, &f.root));
    let (got, err) = deep.fire();
    assert!(err.is_none(), "{err:?}");
    for path in ["sub/deep.ts", "sub/x/y.ts", "sub/\u{e9}/deep.ts", "sub/\u{e9}"] {
        assert!(
            got.iter().any(|e| e.ends_with(&format!("{}/{path}", f.root))),
            "the recursive watch missed {path}: {got:?}"
        );
    }
    shallow.watch.destroy_debounce_for_test();
    deep.watch.destroy_debounce_for_test();
}

const DROPS: [u32; 3] = [
    FLAG_MUST_SCAN_SUB_DIRS | FLAG_USER_DROPPED,
    FLAG_MUST_SCAN_SUB_DIRS | FLAG_KERNEL_DROPPED,
    FLAG_MUST_SCAN_SUB_DIRS,
];

// Longer than the relisting's allowance for coarse timestamps: what changed before is not reported again.
fn wait_past_timestamp_resolution() {
    std::thread::sleep(std::time::Duration::from_millis(2100));
}

fn chmod(path: &str, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn assert_overflow(watched: &Watched) {
    let (got, err) = watched.fire();
    assert_eq!(got, Vec::<String>::new());
    assert!(err.is_some_and(|e| e.is(ERR_OVERFLOW)), "expected an overflow");
}

#[test]
fn a_drop_on_a_non_recursive_stream_reports_what_changed_in_its_root() {
    let f = Fixture::new("relist");
    for name in ["same.ts", "mod.ts", "perm.ts", "atomic.ts", "gone.ts", "case.ts", "target.ts"] {
        f.write(name);
    }
    for dir in ["gonedir", "sub2"] {
        std::fs::create_dir(f.path(dir)).unwrap();
    }
    std::os::unix::fs::symlink(f.path("target.ts"), f.path("link.ts")).unwrap();
    wait_past_timestamp_resolution();

    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, false, &db);
    let cb = stream(&[&watched]);

    f.write("new.ts");
    std::fs::create_dir(f.path("newdir")).unwrap();
    std::fs::remove_file(f.path("gone.ts")).unwrap();
    std::fs::remove_dir(f.path("gonedir")).unwrap();
    // The same size: only the timestamps tell.
    std::fs::write(f.path("mod.ts"), "export {} \n").unwrap();
    chmod(&f.path("perm.ts"), 0o600);
    std::fs::write(f.path("atomic.tmp"), "export const a = 1;\n").unwrap();
    std::fs::rename(f.path("atomic.tmp"), f.path("atomic.ts")).unwrap();
    std::fs::rename(f.path("case.ts"), f.path("Case.ts")).unwrap();
    std::fs::write(nfd(&f.path("\u{e9}.ts")), "export {};\n").unwrap();
    f.write("sub/deeper.ts");
    chmod(&f.path("sub2"), 0o700);
    std::fs::remove_file(f.path("link.ts")).unwrap();
    std::os::unix::fs::symlink(f.path("mod.ts"), f.path("link.ts")).unwrap();
    let fifo = std::ffi::CString::new(f.path("fifo")).unwrap();
    // SAFETY: `fifo` is a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    wait_past_timestamp_resolution();

    let case_old_exists = std::fs::symlink_metadata(f.path("case.ts")).is_ok();
    let mut want: Vec<String> = [
        ("delete", "gone.ts"),
        ("delete", "gonedir"),
        ("update", "Case.ts"),
        ("update", "atomic.ts"),
        ("update", "fifo"),
        ("update", "link.ts"),
        ("update", "mod.ts"),
        ("update", "new.ts"),
        ("update", "newdir"),
        ("update", "perm.ts"),
        ("update", "sub2"),
        ("update", "\u{e9}.ts"),
        (if case_old_exists { "update" } else { "delete" }, "case.ts"),
    ]
    .iter()
    .map(|(kind, name)| format!("{kind} {}", f.path(name)))
    .collect();
    want.sort();
    dispatch_test_event(&cb, &f.root, DROPS[0]);
    let (got, err) = watched.fire();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(got, want);

    // The next drop reports only what changed since this listing, for each kind of drop.
    for (i, drop) in DROPS.into_iter().enumerate() {
        f.write(&format!("later{i}.ts"));
        wait_past_timestamp_resolution();
        dispatch_test_event(&cb, &f.root, drop);
        let (got, err) = watched.fire();
        assert!(err.is_none(), "{err:?}");
        assert_eq!(got, [format!("update {}/later{i}.ts", f.root)]);
    }
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn a_drop_reported_above_or_below_the_root_relists_it_too() {
    let f = Fixture::new("relist-paths");
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, false, &db);
    let cb = stream(&[&watched]);
    for (i, path) in [f.base.clone(), f.path("sub"), f.path("sub/x/y")].iter().enumerate() {
        f.write(&format!("n{i}.ts"));
        dispatch_test_event(&cb, path, DROPS[0]);
        let (got, err) = watched.fire();
        assert!(err.is_none(), "{err:?}");
        assert!(got.contains(&format!("update {}/n{i}.ts", f.root)), "{path}: {got:?}");
    }
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn an_entry_changed_just_before_a_listing_is_reported_again() {
    let f = Fixture::new("relist-recent");
    f.write("recent.ts");
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, false, &db);
    let cb = stream(&[&watched]);
    dispatch_test_event(&cb, &f.root, DROPS[0]);
    let (got, err) = watched.fire();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(got, [format!("update {}/recent.ts", f.root)]);
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn a_drop_after_the_root_was_replaced_or_removed_overflows() {
    let f = Fixture::new("relist-replaced");
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, false, &db);
    let cb = stream(&[&watched]);
    std::fs::rename(&f.root, format!("{}/moved", f.base)).unwrap();
    std::fs::create_dir(&f.root).unwrap();
    dispatch_test_event(&cb, &f.root, DROPS[0]);
    assert_overflow(&watched);

    std::fs::remove_dir(&f.root).unwrap();
    dispatch_test_event(&cb, &f.root, DROPS[0]);
    assert_overflow(&watched);
    watched.watch.destroy_debounce_for_test();
}

#[test]
fn a_drop_on_a_stream_with_a_recursive_watch_overflows() {
    let f = Fixture::new("relist-recursive");
    let db = Debounce::new_without_loop_for_test();
    let shallow = Watched::new(&f.root, &f.root, false, &db);
    let deep = Watched::new(&f.root, &f.root, true, &db);
    let cb = stream(&[&shallow, &deep]);
    dispatch_test_event(&cb, &f.root, DROPS[0]);
    assert_overflow(&shallow);
    assert_overflow(&deep);
    shallow.watch.destroy_debounce_for_test();
    deep.watch.destroy_debounce_for_test();
}

#[test]
fn deleting_a_callbacks_directory_terminates_only_that_callback() {
    let f = Fixture::new("terminate");
    for dir in ["sub2", "other", "keep"] {
        std::fs::create_dir(f.path(dir)).unwrap();
    }
    std::os::unix::fs::symlink(f.path("sub2"), f.path("link")).unwrap();
    let db = Debounce::new_without_loop_for_test();
    let watched = Watched::new(&f.root, &f.root, true, &db);
    // (callback directory, its physical directory)
    let dirs = [
        (f.path("sub"), f.path("sub")),
        (f.path("link"), f.path("sub2")),
        (f.path("other"), f.path("other")),
        (f.path("keep"), f.path("keep")),
    ];
    let results: Vec<Delivered> = dirs
        .iter()
        .map(|(dir, physical)| {
            let delivered: Delivered = Arc::default();
            let sink = Arc::clone(&delivered);
            watched.watch.add_callback(
                dir,
                physical,
                true,
                Arc::new(move |_, err| sink.lock().unwrap().push((Vec::new(), err))),
                None,
                "",
            );
            delivered
        })
        .collect();
    let terminated = |i: usize| {
        results[i]
            .lock()
            .unwrap()
            .iter()
            .any(|(_, err)| err.as_ref().is_some_and(|e| e.is(ERR_WATCH_TERMINATED)))
    };
    let cb = stream(&[&watched]);
    let symlink = 0x0004_0000;

    // A file below a callback's directory, and one beside it.
    dispatch_test_events(
        &cb,
        &[
            (&f.path("sub/file.ts"), FLAG_ITEM_REMOVED | FILE),
            (&f.path("KEEP.ts"), FLAG_ITEM_REMOVED | FILE),
        ],
    );
    watched.fire();
    assert!((0..4).all(|i| !terminated(i)));

    // The directory itself, the symlink a callback watches through, the symlink's target in a different case.
    dispatch_test_events(&cb, &[(&f.path("sub"), FLAG_ITEM_REMOVED | DIR)]);
    watched.fire();
    assert!(terminated(0) && !terminated(1) && !terminated(2) && !terminated(3));
    dispatch_test_events(&cb, &[(&f.path("link"), FLAG_ITEM_REMOVED | symlink)]);
    watched.fire();
    assert!(terminated(1) && !terminated(2) && !terminated(3));
    dispatch_test_events(&cb, &[(&f.path("OTHER"), FLAG_ITEM_REMOVED | DIR)]);
    watched.fire();
    assert!(terminated(2) && !terminated(3));
    // A deletion without a type, which may have been a directory.
    dispatch_test_events(&cb, &[(&f.root, FLAG_ITEM_REMOVED)]);
    watched.fire();
    assert!(terminated(3));
    watched.watch.destroy_debounce_for_test();
}
