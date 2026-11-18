// cachedvfs over the OS file system, probing names in directories it has listed. Each answer must be the uncached
// file system's at the same moment, whatever the volume (case-insensitive, normalization-insensitive) and the entry.

use tsrs_vfs::FS;
use tsrs_vfs::cachedvfs;
use tsrs_vfs::osvfs::os;

use crate::osvfs::os_test::{mklink, temp_dir};

fn listed(dirs: &[&str]) -> cachedvfs::FS<&'static dyn FS> {
    let cached = cachedvfs::from(os::fs());
    for dir in dirs {
        cached.get_accessible_entries(dir);
    }
    cached
}

#[track_caller]
fn assert_probes_match(cached: &impl FS, path: &str) {
    assert_eq!(cached.file_exists(path), os::fs().file_exists(path), "file_exists({path:?})");
    assert_eq!(
        cached.directory_exists(path),
        os::fs().directory_exists(path),
        "directory_exists({path:?})"
    );
}

#[test]
fn probes_of_ascii_names_match_the_file_system() {
    let dir = temp_dir();
    std::fs::write(format!("{dir}/a.ts"), "").unwrap();
    std::fs::create_dir(format!("{dir}/sub")).unwrap();
    let cached = listed(&[&dir]);
    for name in ["a.ts", "sub", "missing.ts", "A.ts", "a.TS", "SUB", "Sub", "a~1.ts", "a.ts:x", "a.ts.", "a.ts "] {
        assert_probes_match(&cached, &format!("{dir}/{name}"));
    }
}

// APFS and HFS+ match a name in another Unicode normalization; the Kelvin sign is "K" in NFD.
#[test]
fn probes_of_non_ascii_names_match_the_file_system() {
    let dir = temp_dir();
    std::fs::write(format!("{dir}/caf\u{e9}.ts"), "").unwrap();
    std::fs::write(format!("{dir}/\u{212a}.ts"), "").unwrap();
    std::fs::write(format!("{dir}/b.ts"), "").unwrap();
    let cached = listed(&[&dir]);
    for name in
        ["caf\u{e9}.ts", "cafe\u{301}.ts", "CAF\u{c9}.ts", "\u{212a}.ts", "K.ts", "k.ts", "b.ts", "c.ts"]
    {
        assert_probes_match(&cached, &format!("{dir}/{name}"));
    }
}

// Each directory has an entry a listing cannot speak for, changed after the listing so that only a new probe of the
// file system gives the right answer.
#[cfg(unix)]
#[test]
fn probes_past_symlinks_irregular_entries_and_failed_listings_match_the_file_system() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let [elsewhere, symlink, dangling, fifo, unreadable, missing] =
        ["elsewhere", "symlink", "dangling", "fifo", "unreadable", "missing"].map(|d| format!("{dir}/{d}"));
    for d in [&elsewhere, &symlink, &dangling, &fifo, &unreadable] {
        std::fs::create_dir(d).unwrap();
    }
    std::fs::write(format!("{elsewhere}/target.ts"), "").unwrap();
    mklink(&format!("{elsewhere}/target.ts"), &format!("{symlink}/link.ts"), false);
    std::fs::create_dir(format!("{elsewhere}/target")).unwrap();
    mklink(&format!("{elsewhere}/target"), &format!("{symlink}/link"), true);
    mklink(&format!("{elsewhere}/later.ts"), &format!("{dangling}/dangling.ts"), false);
    let status = std::process::Command::new("mkfifo").arg(format!("{fifo}/pipe")).status().unwrap();
    assert!(status.success());
    std::fs::write(format!("{unreadable}/a.ts"), "").unwrap();
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o300)).unwrap();

    let cached = listed(&[&symlink, &dangling, &fifo, &unreadable, &missing]);
    std::fs::remove_file(format!("{elsewhere}/target.ts")).unwrap();
    std::fs::remove_dir(format!("{elsewhere}/target")).unwrap();
    std::fs::write(format!("{elsewhere}/later.ts"), "").unwrap();
    std::fs::write(format!("{fifo}/late.ts"), "").unwrap();
    std::fs::create_dir(&missing).unwrap();
    std::fs::write(format!("{missing}/a.ts"), "").unwrap();

    for path in [
        format!("{symlink}/link.ts"),
        format!("{symlink}/link"),
        format!("{dangling}/dangling.ts"),
        format!("{fifo}/pipe"),
        format!("{fifo}/late.ts"),
        format!("{unreadable}/a.ts"),
        format!("{missing}/a.ts"),
    ] {
        assert_probes_match(&cached, &path);
    }
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o700)).unwrap();
}

// Files that appear or disappear after the listing within one run are answered as of the listing, as Go's cachedvfs
// answers a name it has probed once. (Unix only: `os::fs().exact_listings()`.)
#[test]
fn probes_after_the_directory_changes() {
    let dir = temp_dir();
    std::fs::write(format!("{dir}/early.ts"), "").unwrap();
    let cached = listed(&[&dir]);
    std::fs::remove_file(format!("{dir}/early.ts")).unwrap();
    std::fs::write(format!("{dir}/late.ts"), "").unwrap();
    assert_eq!(cached.file_exists(&format!("{dir}/early.ts")), cfg!(unix));
    assert_eq!(cached.file_exists(&format!("{dir}/late.ts")), !cfg!(unix));
}

// A listing handed back while the cache is disabled (config parsing) answers once it is enabled (the program).
#[test]
fn kept_listings_answer_once_the_cache_is_enabled() {
    let dir = temp_dir();
    std::fs::write(format!("{dir}/a.ts"), "").unwrap();
    let cached = cachedvfs::from(os::fs());
    cached.disable_and_clear_cache();
    cached.keep_accessible_entries(&dir, os::fs().get_accessible_entries(&dir));
    std::fs::remove_file(format!("{dir}/a.ts")).unwrap();
    assert!(!cached.file_exists(&format!("{dir}/a.ts")));
    cached.enable();
    assert_eq!(cached.file_exists(&format!("{dir}/a.ts")), cfg!(unix));
}

// What the cache writes, removes or forgets is seen by the next probe.
#[test]
fn probes_after_writes_and_clears_match_the_file_system() {
    let dir = temp_dir();
    std::fs::write(format!("{dir}/a.ts"), "").unwrap();
    let cached = listed(&[&dir]);
    cached.write_file(&format!("{dir}/out/a.js"), "").unwrap();
    cached.write_file(&format!("{dir}/a.js"), "").unwrap();
    cached.remove(&format!("{dir}/a.ts")).unwrap();
    for name in ["out", "out/a.js", "a.js", "a.ts"] {
        assert_probes_match(&cached, &format!("{dir}/{name}"));
    }

    let cached = listed(&[&dir]);
    std::fs::write(format!("{dir}/b.ts"), "").unwrap();
    cached.clear_cache();
    assert_probes_match(&cached, &format!("{dir}/b.ts"));
    cached.get_accessible_entries(&dir);
    std::fs::write(format!("{dir}/d.ts"), "").unwrap();
    cached.clear_listings();
    assert_probes_match(&cached, &format!("{dir}/d.ts"));
    cached.get_accessible_entries(&dir);
    std::fs::write(format!("{dir}/c.ts"), "").unwrap();
    cached.disable_and_clear_cache();
    assert_probes_match(&cached, &format!("{dir}/c.ts"));
}
