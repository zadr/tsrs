// What a non-recursive FSEvents watch delivers from its root's stream, which reports every change below the root.

use std::sync::{Arc, Mutex};

use tsrs_fswatch::debounce::Debounce;
use tsrs_fswatch::fsevents_darwin::{
    FLAG_ITEM_CREATED, FLAG_ITEM_IS_DIR, FLAG_ITEM_IS_FILE, FLAG_ITEM_REMOVED, FLAG_ITEM_RENAMED,
    FseventsState, FseventsWatchSnapshot,
};
use tsrs_fswatch::fsevents_darwin_ffi::{
    StreamCallback, dispatch_test_events, new_stream_callback,
};
use tsrs_fswatch::pathcompare::BytePathComparer;
use tsrs_fswatch::watcher::{DirWatch, ERR_WATCH_TERMINATED};

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
