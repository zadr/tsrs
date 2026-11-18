use std::time::SystemTime;

use std::sync::atomic::{AtomicUsize, Ordering};

use tsrs_core::tspath;

use tsrs_vfs::osvfs::os::*;

// A fresh directory under the system temp dir (t.TempDir()).
pub(super) fn temp_dir() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tsrs_vfs_test_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

pub(super) fn mklink(target: &str, link: &str, is_dir: bool) {
    #[cfg(unix)]
    {
        let _ = is_dir;
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    #[cfg(windows)]
    {
        if is_dir {
            std::os::windows::fs::symlink_dir(target, link).unwrap();
        } else {
            std::os::windows::fs::symlink_file(target, link).unwrap();
        }
    }
}

#[test]
fn test_get_global_typings_cache_location() {
    let subdir = if cfg!(windows) { "Microsoft/TypeScript" } else { "typescript" };
    let version = tsrs_core::version_major_minor();

    assert_eq!(global_typings_cache_location(None), "");
    assert_eq!(global_typings_cache_location(Some("relative-cache")), "");
    assert_eq!(global_typings_cache_location(Some("relative-home/.cache")), "");
    assert_eq!(global_typings_cache_location(Some("file:///cache?query")), "");
    assert_eq!(
        global_typings_cache_location(Some("/home/user/../cache")),
        format!("/home/cache/{subdir}/{version}")
    );
    if cfg!(windows) {
        assert_eq!(
            global_typings_cache_location(Some(r"C:\Users\me\AppData\Local")),
            format!("C:/Users/me/AppData/Local/{subdir}/{version}")
        );
    }
}

#[test]
fn test_os_read_file() {
    let fs = fs();

    let cargo_toml = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let cargo_toml_path = tspath::normalize_path(cargo_toml);

    let expected = std::fs::read_to_string(cargo_toml).unwrap();

    let contents = fs.read_file(&cargo_toml_path);
    assert_eq!(contents.as_deref(), Some(expected.as_str()));
}

#[test]
fn test_os_realpath() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = tspath::normalize_path(&home.to_string_lossy());

    let mut expected = home.clone();
    if cfg!(windows) {
        // Windows drive letters can be lowercase, but realpath will always return uppercase.
        expected = format!("{}{}", expected[..1].to_uppercase(), &expected[1..]);
    }
    let realpath = fs().realpath(&home);
    assert_eq!(realpath, expected);
}

#[test]
fn test_os_rejects_network_paths() {
    if cfg!(windows) {
        // On POSIX, a leading // still names the same local directory.
        return;
    }
    let dir = temp_dir();
    std::fs::write(format!("{dir}/a.txt"), "a").unwrap();
    let network_dir = format!("/{dir}");
    let network_file = format!("{network_dir}/a.txt");
    let new_network_file = format!("{network_dir}/b.txt");
    assert!(tspath::is_network_path(&network_file));

    let fs = fs();
    assert!(!fs.directory_exists(&network_dir));
    assert!(!fs.file_exists(&network_file));
    assert_eq!(fs.read_file(&network_file), None);
    assert!(fs.stat(&network_file).is_none());
    let entries = fs.get_accessible_entries(&network_dir);
    assert!(entries.files.is_empty() && entries.directories.is_empty());
    assert_eq!(fs.realpath(&network_file), network_file);

    assert!(fs.write_file(&new_network_file, "b").is_err());
    assert!(fs.append_file(&new_network_file, "b").is_err());
    assert!(fs.chtimes(&network_file, SystemTime::now(), SystemTime::now()).is_err());
    assert!(fs.remove(&network_file).is_err());

    assert!(std::fs::metadata(format!("{dir}/a.txt")).is_ok());
    assert!(!std::path::Path::new(&format!("{dir}/b.txt")).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_os_use_case_sensitive_file_names() {
    let fs = fs();

    // Just check that it works.
    fs.use_case_sensitive_file_names();

    if cfg!(windows) {
        assert!(!fs.use_case_sensitive_file_names());
    } else if cfg!(target_os = "linux") {
        assert!(fs.use_case_sensitive_file_names());
    }
}

#[test]
fn test_os_bom_and_directories() {
    let tmp = tspath::normalize_slashes(&temp_dir());
    let fs = fs();

    let file = tspath::combine_paths(&tmp, &["a/b/c.ts"]);
    fs.write_file(&file, "\u{FEFF}hello").unwrap();
    assert_eq!(fs.read_file(&file).as_deref(), Some("hello"));
    assert!(fs.file_exists(&file));
    assert!(!fs.directory_exists(&file));
    assert!(fs.directory_exists(&tspath::combine_paths(&tmp, &["a/b"])));
    assert_eq!(fs.read_file(&tspath::combine_paths(&tmp, &["a"])), None);

    fs.append_file(&file, " world").unwrap();
    assert_eq!(fs.read_file(&file).as_deref(), Some("hello world"));

    let stat = fs.stat(&file).unwrap();
    assert_eq!(stat.name(), "c.ts");
    assert!(stat.mode().is_regular());

    fs.remove(&tspath::combine_paths(&tmp, &["a"])).unwrap();
    assert!(!fs.file_exists(&file));
    let _ = std::fs::remove_dir_all(&tmp);
}

// Letters that differ between nearby offsets, so a misplaced chunk shows.
fn letters(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| b'a' + ((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 59) as u8 % 26)
        .collect()
}

// Around std::fs::read's 32-byte end-of-file probe, the page size and its 8 KiB read granule.
const READ_SIZES: [usize; 10] = [0, 1, 31, 32, 33, 4095, 4096, 4097, 8192, (1 << 20) + 1];

#[test]
fn test_os_read_file_sizes() {
    let dir = temp_dir();
    for size in READ_SIZES {
        let path = format!("{dir}/{size}.ts");
        std::fs::write(&path, letters(size)).unwrap();
        let want = String::from_utf8(letters(size)).unwrap();
        assert_eq!(fs().read_file(&path), Some(want), "size {size}");
    }
    assert_eq!(fs().read_file(&format!("{dir}/missing.ts")), None);
    assert_eq!(fs().read_file(&dir), None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_os_read_file_encodings() {
    let dir = temp_dir();
    for (name, content, want) in [
        ("invalid.ts", &b"a\xC3(b\x80"[..], "a\u{FFFD}(b\u{FFFD}"),
        ("utf8_bom.ts", b"\xEF\xBB\xBFhello", "hello"),
        ("bom_only.ts", b"\xEF\xBB\xBF", ""),
        ("utf16le.ts", b"\xFF\xFEh\0i\0", "hi"),
        ("utf16be.ts", b"\xFE\xFF\0h\0i", "hi"),
    ] {
        let path = format!("{dir}/{name}");
        std::fs::write(&path, content).unwrap();
        assert_eq!(fs().read_file(&path).as_deref(), Some(want), "{name}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

// Writes `content` into the FIFO at `path` from another thread while `read` reads it.
#[cfg(unix)]
fn read_fifo<T>(path: &str, content: &[u8], read: impl FnOnce(&str) -> T) -> T {
    let writer = std::thread::spawn({
        let (path, content) = (path.to_string(), content.to_vec());
        move || std::fs::write(path, content).unwrap()
    });
    let got = read(path);
    writer.join().unwrap();
    got
}

#[cfg(unix)]
#[test]
fn test_os_read_file_fifo() {
    let dir = temp_dir();
    let path = format!("{dir}/pipe.ts");
    let c_path = std::ffi::CString::new(path.as_str()).unwrap();
    // SAFETY: c_path is NUL-terminated and outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    // Past the 64 KiB pipe buffer, so the reader sees several short reads.
    let content = letters(200_000);
    let got = read_fifo(&path, &content, |path| fs().read_file(path));
    assert_eq!(got, Some(String::from_utf8(content.clone()).unwrap()));
    assert_eq!(read_fifo(&path, &content, |path| os_read_file(path).unwrap()), content);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_os_read_file_matches_std() {
    let dir = temp_dir();
    for size in READ_SIZES {
        let path = format!("{dir}/{size}.ts");
        std::fs::write(&path, letters(size)).unwrap();
        assert_eq!(os_read_file(&path).unwrap(), std::fs::read(&path).unwrap(), "size {size}");
    }
    for path in [dir.clone(), format!("{dir}/missing.ts")] {
        let (got, want) = (os_read_file(&path).unwrap_err(), std::fs::read(&path).unwrap_err());
        assert_eq!((got.kind(), got.to_string()), (want.kind(), want.to_string()), "{path}");
    }
    if cfg!(unix) {
        assert_eq!(os_read_file("/dev/null").unwrap(), b"");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

// The file grows or shrinks between fstat and the read: `size` is stale.
#[cfg(unix)]
#[test]
fn test_read_regular_file_stale_size() {
    let dir = temp_dir();
    let path = format!("{dir}/a.ts");
    let mib = 1 << 20;
    for (size, actual) in [
        (0, 0),
        (33, 33),
        (0, 1),
        (0, 32),
        (0, 33),
        (0, 5000),
        (1, 2),
        (1, 33),
        (1, 34),
        (32, 64),
        (32, 65),
        (4096, 4097),
        (4096, 4096 + 32),
        (4096, 4096 + 33),
        (4096, 8192 + 33),
        (4096, mib + 1),
        (mib + 1, 3 * mib),
        (1, 0),
        (100, 0),
        (100, 1),
        (100, 99),
        (4097, 4096),
        (mib + 1, 33),
    ] {
        std::fs::write(&path, letters(actual)).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(read_regular_file(&file, size).unwrap(), letters(actual), "{size} -> {actual}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_clean() {
    for (input, want) in [
        ("", "."),
        ("abc", "abc"),
        ("abc/def", "abc/def"),
        ("a/b/c", "a/b/c"),
        (".", "."),
        ("..", ".."),
        ("../..", "../.."),
        ("../../abc", "../../abc"),
        ("/abc", "/abc"),
        ("/", "/"),
        ("abc/", "abc"),
        ("abc/def/", "abc/def"),
        ("/abc/", "/abc"),
        ("abc//def//ghi", "abc/def/ghi"),
        ("//abc", "/abc"),
        ("abc/./def", "abc/def"),
        ("/./abc/def", "/abc/def"),
        ("abc/def/ghi/../jkl", "abc/def/jkl"),
        ("abc/def/../ghi/../jkl", "abc/jkl"),
        ("abc/def/..", "abc"),
        ("abc/def/../..", "."),
        ("/abc/def/../..", "/"),
        ("abc/def/../../..", ".."),
        ("/abc/def/../../..", "/"),
        ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
        ("abc/./../def", "def"),
        ("abc//./../def", "def"),
        ("abc/../../././../def", "../../def"),
    ] {
        assert_eq!(clean(input), want, "clean({input:?})");
    }
}

// Not in Go: swapCase uses unicode.ToUpper / unicode.ToLower, the simple mappings (U+1FB3 -> U+1FBC, U+0130 -> 'i').
#[test]
fn test_swap_case_simple_mapping() {
    assert_eq!(swap_case("\u{1FB3}\u{130}aB\u{DF}"), "\u{1FBC}iAb\u{DF}");
}
