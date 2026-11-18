// tsrs-only: tests for the per-root streams (`fseventsMaxRootStreams`); Go has one set of streams for every root.

use std::sync::Arc;

use tsrs_fswatch::debounce::Debounce;
use tsrs_fswatch::fsevents_darwin::{
    FLAG_KERNEL_DROPPED, FLAG_MUST_SCAN_SUB_DIRS, FLAG_USER_DROPPED, FSEVENTS_MAX_ROOT_STREAMS,
    FsEventsBackend, new_fs_events_backend,
};
use tsrs_fswatch::fsevents_darwin_ffi::{dispatch_test_event, teardown_stream};
use tsrs_fswatch::pathcompare::BytePathComparer;
use tsrs_fswatch::watcher::{DirWatch, ERR_OVERFLOW, WatcherImpl};

fn root_dirs(n: usize) -> (std::path::PathBuf, Vec<String>) {
    let base = std::env::temp_dir().join(format!("tsrs-fswatch-roots-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let base = {
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(&base).unwrap()
    };
    let dirs = (0..n)
        .map(|i| {
            let dir = base.join(format!("r{i:04}"));
            std::fs::create_dir(&dir).unwrap();
            dir.to_str().unwrap().to_string()
        })
        .collect();
    (base, dirs)
}

fn stream_counts(backend: &FsEventsBackend) -> (usize, usize, usize) {
    let st = backend.mu.lock().unwrap();
    (st.root_streams.len(), st.shared_roots.len(), st.shared_streams.len())
}

#[test]
fn each_root_gets_its_own_stream_up_to_the_limit_and_the_rest_share() {
    let (base, dirs) = root_dirs(FSEVENTS_MAX_ROOT_STREAMS + 3);
    let db: Arc<Debounce> = Debounce::new();
    let watches: Vec<Arc<DirWatch>> = dirs
        .iter()
        .map(|dir| {
            DirWatch::new(dir.clone(), dir.clone(), &db, BytePathComparer::default(), None, true)
        })
        .collect();
    let backend = new_fs_events_backend();

    backend.subscribe_many(&watches[..2]).unwrap().unwrap();
    assert_eq!(stream_counts(&backend), (2, 0, 0));

    backend.subscribe_many(&watches[2..]).unwrap().unwrap();
    assert_eq!(stream_counts(&backend), (FSEVENTS_MAX_ROOT_STREAMS, 3, 1));

    // A second watch on a root that has its own stream (here non-recursive) joins that stream.
    let same_root = DirWatch::new(
        dirs[0].clone(),
        dirs[0].clone(),
        &db,
        BytePathComparer::default(),
        None,
        false,
    );
    backend.subscribe(&same_root).unwrap();
    assert_eq!(stream_counts(&backend), (FSEVENTS_MAX_ROOT_STREAMS, 3, 1));
    backend.close_watch(&watches[0]).unwrap();
    assert_eq!(stream_counts(&backend), (FSEVENTS_MAX_ROOT_STREAMS, 3, 1));
    backend.close_watch(&same_root).unwrap();
    assert_eq!(stream_counts(&backend), (FSEVENTS_MAX_ROOT_STREAMS - 1, 3, 1));

    let last = watches.len() - 1;
    backend.close_watch(&watches[last]).unwrap();
    assert_eq!(stream_counts(&backend), (FSEVENTS_MAX_ROOT_STREAMS - 1, 2, 1));

    for watch in &watches[1..last] {
        backend.close_watch(watch).unwrap();
    }
    assert_eq!(stream_counts(&backend), (0, 0, 0));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_drop_under_one_root_does_not_overflow_another() {
    let (base, dirs) = root_dirs(2);
    let db = Debounce::new();
    let quiet = DirWatch::new(
        dirs[0].clone(),
        dirs[0].clone(),
        &db,
        BytePathComparer::default(),
        None,
        true,
    );
    let busy = DirWatch::new(
        dirs[1].clone(),
        dirs[1].clone(),
        &db,
        BytePathComparer::default(),
        None,
        true,
    );
    let busy_non_recursive = DirWatch::new(
        dirs[1].clone(),
        dirs[1].clone(),
        &db,
        BytePathComparer::default(),
        None,
        false,
    );
    let watches = [quiet, busy, busy_non_recursive];
    let backend = new_fs_events_backend();
    backend.subscribe_many(&watches).unwrap().unwrap();

    // Keep the callbacks the real subscription created, but stop native delivery before injecting events:
    // whether fseventsd drops a burst of writes depends on the OS and machine load, not watcher correctness.
    let streams = {
        let st = backend.mu.lock().unwrap();
        dirs.iter().map(|dir| Arc::clone(&st.root_streams[dir])).collect::<Vec<_>>()
    };
    let callbacks: Vec<_> = streams
        .iter()
        .map(|stream| {
            let mut cb = stream.cb.lock().unwrap().take().unwrap();
            let native = stream.stream.lock().unwrap().take().unwrap();
            teardown_stream(native, Some(&mut cb));
            cb
        })
        .collect();
    for watch in &watches {
        let _ = watch.events.drain();
    }

    // An overflow can name the root or an ancestor. The ancestor case would match the quiet root too if
    // its watch leaked into the busy stream's callback. Both logical watches on the busy root must receive it.
    for path in [dirs[1].as_str(), base.to_str().unwrap()] {
        for dropped in [FLAG_USER_DROPPED, FLAG_KERNEL_DROPPED, 0] {
            dispatch_test_event(&callbacks[1], path, FLAG_MUST_SCAN_SUB_DIRS | dropped);
            let (events, err) = watches[0].events.drain();
            assert_eq!(events, []);
            assert!(err.is_none(), "the busy root's drop overflowed the quiet root");
            for watch in &watches[1..] {
                let (events, err) = watch.events.drain();
                assert_eq!(events, []);
                assert!(
                    err.is_some_and(|err| err.is(ERR_OVERFLOW)),
                    "the busy root missed its overflow"
                );
            }
        }
    }

    for watch in &watches {
        backend.close_watch(watch).unwrap();
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn write_settling_holds_multi_file_native_batches() {
    const CHILD: &str = "write-settling-batch-child";
    if !std::env::args().any(|arg| arg == CHILD) {
        // Write settling is process-wide. Run only this case in the child so other watcher tests keep their setting.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fsevents_darwin_test::write_settling_holds_multi_file_native_batches",
                "--nocapture",
                CHILD,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
            "the child must run the isolated case"
        );
        return;
    }

    tsrs_fswatch::set_write_settling(true);
    let (base, dirs) = root_dirs(1);
    let paths: Vec<_> = (0..20).map(|i| format!("{}/f{i}.ts", dirs[0])).collect();
    for path in &paths {
        std::fs::write(path, "export {};\n").unwrap();
    }
    let db = Debounce::new_without_loop_for_test();
    let watch = DirWatch::new(
        dirs[0].clone(),
        dirs[0].clone(),
        &db,
        BytePathComparer::default(),
        None,
        true,
    );
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&batches);
    watch.add_callback(
        &dirs[0],
        &dirs[0],
        true,
        Arc::new(move |events, err| {
            assert!(err.is_none());
            let paths: std::collections::BTreeSet<_> =
                events.iter().map(|e| e.path.clone()).collect();
            seen.lock().unwrap().push(paths);
        }),
        None,
        "",
    );
    let cb = tsrs_fswatch::fsevents_darwin_ffi::new_stream_callback(&[
        tsrs_fswatch::fsevents_darwin::FseventsWatchSnapshot {
            w: Arc::clone(&watch),
            state: Arc::new(tsrs_fswatch::fsevents_darwin::FseventsState::default()),
        },
    ])
    .unwrap();
    let flags = tsrs_fswatch::fsevents_darwin::FLAG_ITEM_CREATED
        | tsrs_fswatch::fsevents_darwin::FLAG_ITEM_IS_FILE;

    dispatch_test_event(&cb, &paths[0], flags);
    assert!(
        !db.leading_edge_is_held_for_test(),
        "one complete save must not hold the leading edge"
    );
    watch.trigger_callbacks_for_test();
    assert_eq!(
        *batches.lock().unwrap(),
        vec![std::collections::BTreeSet::from([paths[0].clone()])]
    );
    batches.lock().unwrap().clear();

    let events: Vec<_> = paths.iter().map(|path| (path.as_str(), flags)).collect();
    tsrs_fswatch::fsevents_darwin_ffi::dispatch_test_events(&cb, &events);
    assert!(
        db.leading_edge_is_held_for_test(),
        "a native multi-file batch must hold the leading edge"
    );
    assert!(batches.lock().unwrap().is_empty());
    watch.trigger_callbacks_for_test();
    assert_eq!(*batches.lock().unwrap(), vec![paths.into_iter().collect()]);
    watch.destroy_debounce_for_test();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn write_settling_counts_the_files_a_rename_batch_left() {
    if !crate::isolated::in_child(
        "fsevents_darwin_test::write_settling_counts_the_files_a_rename_batch_left",
    ) {
        return;
    }
    tsrs_fswatch::set_write_settling(true);
    let (base, dirs) = root_dirs(1);
    let path = |name: &str| format!("{}/{name}", dirs[0]);
    std::fs::write(path("a.ts"), "export {};\n").unwrap();
    std::fs::write(path("b.ts"), "export {};\n").unwrap();
    std::os::unix::fs::symlink(path("a.ts"), path("link.ts")).unwrap();
    let db = Debounce::new_without_loop_for_test();
    let watch = DirWatch::new(
        dirs[0].clone(),
        dirs[0].clone(),
        &db,
        BytePathComparer::default(),
        None,
        true,
    );
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&delivered);
    watch.add_callback(
        &dirs[0],
        &dirs[0],
        true,
        Arc::new(move |events, _| {
            let mut events: Vec<String> =
                events.iter().map(|e| format!("{} {}", e.kind, e.path)).collect();
            events.sort();
            seen.lock().unwrap().push(events);
        }),
        None,
        "",
    );
    let cb = tsrs_fswatch::fsevents_darwin_ffi::new_stream_callback(&[
        tsrs_fswatch::fsevents_darwin::FseventsWatchSnapshot {
            w: Arc::clone(&watch),
            state: Arc::new(tsrs_fswatch::fsevents_darwin::FseventsState::default()),
        },
    ])
    .unwrap();
    let renamed_file = tsrs_fswatch::fsevents_darwin::FLAG_ITEM_RENAMED
        | tsrs_fswatch::fsevents_darwin::FLAG_ITEM_IS_FILE;

    // An atomic save: the temporary file is gone, the saved one is there. One file: no burst.
    tsrs_fswatch::fsevents_darwin_ffi::dispatch_test_events(
        &cb,
        &[(&path("a.ts.tmp"), renamed_file), (&path("a.ts"), renamed_file)],
    );
    assert!(!db.leading_edge_is_held_for_test());
    watch.trigger_callbacks_for_test();
    assert_eq!(
        *delivered.lock().unwrap(),
        [[format!("delete {}", path("a.ts.tmp")), format!("update {}", path("a.ts"))]]
    );
    delivered.lock().unwrap().clear();

    // A symlink reported as a file counts as the file it points to: two files, a burst.
    tsrs_fswatch::fsevents_darwin_ffi::dispatch_test_events(
        &cb,
        &[(&path("link.ts"), renamed_file), (&path("b.ts"), renamed_file)],
    );
    assert!(db.leading_edge_is_held_for_test());
    watch.trigger_callbacks_for_test();
    assert_eq!(
        *delivered.lock().unwrap(),
        [[format!("update {}", path("b.ts")), format!("update {}", path("link.ts"))]]
    );
    watch.destroy_debounce_for_test();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn event_paths_are_delivered_in_nfc() {
    let base = std::env::temp_dir().join(format!("tsrs-fswatch-nfc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let root = &std::fs::canonicalize(&base).unwrap().to_str().unwrap().to_string();
    let db = Debounce::new_without_loop_for_test();
    let watch =
        DirWatch::new(root.clone(), root.clone(), &db, BytePathComparer::default(), None, true);
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&delivered);
    watch.add_callback(
        root,
        root,
        true,
        Arc::new(move |events, _| {
            seen.lock().unwrap().extend(events.into_iter().map(|e| e.path));
        }),
        None,
        "",
    );
    let cb = tsrs_fswatch::fsevents_darwin_ffi::new_stream_callback(&[
        tsrs_fswatch::fsevents_darwin::FseventsWatchSnapshot {
            w: Arc::clone(&watch),
            state: Arc::new(tsrs_fswatch::fsevents_darwin::FseventsState::default()),
        },
    ])
    .unwrap();
    let long = "a/".repeat(700) + "z.ts";
    let ascii: String = (0x20u8..0x7f).filter(|&b| b != b'/').map(char::from).collect();
    // (reported, delivered)
    let names = [
        ("plain.ts", "plain.ts"),
        (ascii.as_str(), ascii.as_str()),
        ("\u{7f}\u{1}.ts", "\u{7f}\u{1}.ts"),
        (long.as_str(), long.as_str()),
        ("e\u{301}.ts", "\u{e9}.ts"),
        ("\u{e9}.ts", "\u{e9}.ts"),
        ("\u{212b}.ts", "\u{c5}.ts"),
        ("\u{1100}\u{1161}.ts", "\u{ac00}.ts"),
        ("\u{1f600}.ts", "\u{1f600}.ts"),
        ("a\u{323}\u{307}.ts", "\u{1ea1}\u{307}.ts"),
    ];
    let flags = tsrs_fswatch::fsevents_darwin::FLAG_ITEM_CREATED
        | tsrs_fswatch::fsevents_darwin::FLAG_ITEM_IS_DIR;
    let paths: Vec<String> = names.iter().map(|(name, _)| format!("{root}/{name}")).collect();
    let events: Vec<(&str, u32)> = paths.iter().map(|path| (path.as_str(), flags)).collect();
    tsrs_fswatch::fsevents_darwin_ffi::dispatch_test_events(&cb, &events);
    watch.trigger_callbacks_for_test();
    let mut got = delivered.lock().unwrap().clone();
    got.sort();
    let mut want: Vec<String> = names.iter().map(|(_, nfc)| format!("{root}/{nfc}")).collect();
    want.sort();
    // An event list keeps one entry per path.
    want.dedup();
    assert_eq!(got, want);
    watch.destroy_debounce_for_test();
    let _ = std::fs::remove_dir_all(&base);
}
