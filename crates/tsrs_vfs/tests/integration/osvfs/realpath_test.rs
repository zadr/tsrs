use tsrs_core::tspath;

use crate::osvfs::os_test::{mklink, temp_dir};
use tsrs_vfs::osvfs::os::*;

fn setup_symlinks() -> (String, String) {
    let tmp = temp_dir();

    let target = format!("{tmp}/target");
    let target_file = format!("{target}/file");

    let link = format!("{tmp}/link");
    let link_file = format!("{link}/file");

    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(&target_file, "hello").unwrap();

    mklink(&target, &link, true);

    (target_file, link_file)
}

#[test]
fn test_symlink_realpath() {
    let (target_file, link_file) = setup_symlinks();

    let got_contents = std::fs::read_to_string(&link_file).unwrap();
    assert_eq!(got_contents, "hello");

    let fs = fs();

    let target_realpath = fs.realpath(&tspath::normalize_path(&target_file));
    let link_realpath = fs.realpath(&tspath::normalize_path(&link_file));

    assert_eq!(target_realpath, link_realpath, "expected realpath of target and link to be equal");
}

#[test]
fn test_realpath_relative_symlink_and_missing() {
    let tmp = tspath::normalize_slashes(&temp_dir());
    std::fs::create_dir_all(format!("{tmp}/real/sub")).unwrap();
    std::fs::write(format!("{tmp}/real/sub/f.ts"), "").unwrap();
    mklink("real/sub", &format!("{tmp}/rel"), true);

    let fs = fs();
    let real = fs.realpath(&format!("{tmp}/real/sub/f.ts"));
    assert_eq!(fs.realpath(&format!("{tmp}/rel/f.ts")), real);
    // ".." after a symlink is physical (EvalSymlinks semantics): rel/.. is real/, so this does not exist.
    let dotdot = format!("{tmp}/rel/../real/sub/f.ts");
    assert_eq!(fs.realpath(&dotdot), dotdot);
    assert_eq!(fs.realpath(&format!("{tmp}/rel/../sub/f.ts")), real);

    // A path that does not exist is returned unchanged.
    let missing = format!("{tmp}/does/not/exist.ts");
    assert_eq!(fs.realpath(&missing), missing);
}

#[test]
fn test_get_accessible_entries() {
    let tmp = temp_dir();
    let target = format!("{tmp}/target");
    let link = format!("{tmp}/link");

    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&link).unwrap();

    let target_file1 = format!("{target}/file1");
    let target_file2 = format!("{target}/file2");

    std::fs::write(&target_file1, "hello").unwrap();
    std::fs::write(&target_file2, "world").unwrap();

    let target_dir1 = format!("{target}/dir1");
    let target_dir2 = format!("{target}/dir2");

    std::fs::create_dir_all(&target_dir1).unwrap();
    std::fs::create_dir_all(&target_dir2).unwrap();

    mklink(&target_file1, &format!("{link}/file1"), false);
    mklink(&target_file2, &format!("{link}/file2"), false);
    mklink(&target_dir1, &format!("{link}/dir1"), true);
    mklink(&target_dir2, &format!("{link}/dir2"), true);

    let fs = fs();

    let entries = fs.get_accessible_entries(&tspath::normalize_path(&link));

    assert_eq!(entries.directories, vec!["dir1", "dir2"]);
    assert_eq!(entries.files, vec!["file1", "file2"]);
    let symlinks =
        entries.symlinks.expect("expected Symlinks to be set for directory with symlinks");
    assert_eq!(symlinks.len(), 4);
    for name in ["file1", "file2", "dir1", "dir2"] {
        assert!(symlinks.contains(name), "expected {name:?} to be in Symlinks");
    }

    // Non-symlink directory should have empty Symlinks.
    let entries = fs.get_accessible_entries(&tspath::normalize_path(&target));
    assert_eq!(entries.directories, vec!["dir1", "dir2"]);
    assert_eq!(entries.files, vec!["file1", "file2"]);
    let symlinks =
        entries.symlinks.expect("expected Symlinks to be non-nil for directory without symlinks");
    assert_eq!(symlinks.len(), 0);
}

// A tree with each kind of component the realpath walk meets. Links are relative unless named abs_.
#[cfg(unix)]
struct Tree {
    // As created, under the temp dir (on macOS /var/folders/..., and /var is a link to private/var).
    root: String,
    real: String,
}

#[cfg(unix)]
fn tree() -> Tree {
    let root = tspath::normalize_slashes(&temp_dir());
    let real = tspath::normalize_slashes(&std::fs::canonicalize(&root).unwrap().to_string_lossy());
    for dir in [
        "real/sub",
        "real/deep/a/b/c",
        "nested",
        "node_modules/.pnpm/pkg@1/node_modules/pkg",
        "node_modules/.pnpm/dep@2/node_modules/dep",
        "node_modules/@scope",
        "\u{fc}",
    ] {
        std::fs::create_dir_all(format!("{root}/{dir}")).unwrap();
    }
    for file in [
        "real/f.ts",
        "real/sub/g.ts",
        "real/deep/a/b/c/d.ts",
        "node_modules/.pnpm/pkg@1/node_modules/pkg/index.d.ts",
        "node_modules/.pnpm/dep@2/node_modules/dep/index.d.ts",
        "\u{fc}/\u{f1}.ts",
    ] {
        std::fs::write(format!("{root}/{file}"), "").unwrap();
    }
    for (target, link) in [
        ("real/f.ts", "leaf.ts"),
        (&format!("{real}/real") as &str, "abs_dir"),
        ("real", "rel_dir"),
        ("rel_dir", "chain"),
        ("../real", "nested/up"),
        ("nowhere", "dangling"),
        ("loop_b", "loop_a"),
        ("loop_a", "loop_b"),
        ("self", "self"),
        (".pnpm/pkg@1/node_modules/pkg", "node_modules/pkg"),
        ("../.pnpm/pkg@1/node_modules/pkg", "node_modules/@scope/name"),
        ("../../dep@2/node_modules/dep", "node_modules/.pnpm/pkg@1/node_modules/dep"),
    ] {
        mklink(target, &format!("{root}/{link}"), true);
    }
    Tree { root, real }
}

// (input, realpath) for each input through the temp dir's link and through its real path.
#[cfg(unix)]
fn cases(t: &Tree) -> Vec<(String, String)> {
    let real = &t.real;
    let mut cases = Vec::new();
    for base in [&t.root, &t.real] {
        let to = |input: &str, want: &str| (format!("{base}/{input}"), format!("{real}/{want}"));
        let same = |input: &str| (format!("{base}/{input}"), format!("{base}/{input}"));
        cases.extend([
            to("real/f.ts", "real/f.ts"),
            to("real/deep/a/b/c/d.ts", "real/deep/a/b/c/d.ts"),
            to("real/deep/a/b", "real/deep/a/b"),
            to("leaf.ts", "real/f.ts"),
            to("abs_dir/sub/g.ts", "real/sub/g.ts"),
            to("rel_dir", "real"),
            to("rel_dir/deep/a/b/c/d.ts", "real/deep/a/b/c/d.ts"),
            to("chain/sub/g.ts", "real/sub/g.ts"),
            to("nested/up", "real"),
            to("nested/up/f.ts", "real/f.ts"),
            to(
                "node_modules/pkg/index.d.ts",
                "node_modules/.pnpm/pkg@1/node_modules/pkg/index.d.ts",
            ),
            to(
                "node_modules/@scope/name/index.d.ts",
                "node_modules/.pnpm/pkg@1/node_modules/pkg/index.d.ts",
            ),
            to(
                "node_modules/pkg/../dep/index.d.ts",
                "node_modules/.pnpm/dep@2/node_modules/dep/index.d.ts",
            ),
            to(
                "node_modules/.pnpm/pkg@1/node_modules/dep/index.d.ts",
                "node_modules/.pnpm/dep@2/node_modules/dep/index.d.ts",
            ),
            to("\u{fc}/\u{f1}.ts", "\u{fc}/\u{f1}.ts"),
            to("/real/./sub//g.ts", "real/sub/g.ts"),
            // ".." is lexical over real directories and physical after a link (EvalSymlinks).
            to("real/sub/../f.ts", "real/f.ts"),
            to("rel_dir/../real/f.ts", "real/f.ts"),
            to("rel_dir/sub/../../leaf.ts", "real/f.ts"),
            to("nested/up/../leaf.ts", "real/f.ts"),
            same("rel_dir/../f.ts"),
            // Dangling links, loops, a regular file as a directory, missing components: unchanged.
            same("dangling"),
            same("dangling/x.ts"),
            same("loop_a"),
            same("loop_a/x.ts"),
            same("self"),
            same("self/x.ts"),
            same("real/f.ts/x.ts"),
            same("real/f.ts/"),
            same("leaf.ts/x.ts"),
            same("does/not/exist.ts"),
            same("real/missing/x.ts"),
            same("real/deep/a/missing/b/c.ts"),
            same("node_modules/missing/index.d.ts"),
        ]);
        // ".." at the root stays at the root.
        cases.push((format!("/..{base}/leaf.ts"), format!("{real}/real/f.ts")));
        // Case-only and Unicode-normalization-only spellings: where the volume finds them, the walk keeps the
        // input's spelling (Linux's realpath(3) would return the stored one, so only macOS checks that case).
        for (rest, dir) in [("REAL/f.ts", "REAL"), ("u\u{308}/\u{f1}.ts", "u\u{308}")] {
            let input = format!("{base}/{rest}");
            if std::fs::symlink_metadata(format!("{real}/{dir}")).is_err() {
                cases.push((input.clone(), input));
            } else if cfg!(target_os = "macos") {
                cases.push((input, format!("{real}/{rest}")));
            }
        }
    }
    cases
}

#[cfg(unix)]
#[test]
fn test_realpath_walk_cases() {
    let t = tree();
    let fs = fs();
    for (input, want) in cases(&t) {
        assert_eq!(fs.realpath(&input), want, "{input}");
    }
}

// The cached file system remembers what each realpath call found on the way; any order of inputs, repeated, gives
// what the uncached file system gives for each input alone.
#[cfg(unix)]
#[test]
fn test_cached_realpath_equals_uncached_in_any_order() {
    use tsrs_vfs::FS as _;

    let t = tree();
    let cases = cases(&t);
    let n = cases.len();
    let mut orders: Vec<Vec<usize>> = vec![(0..n).collect(), (0..n).rev().collect()];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..6 {
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            order.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        orders.push(order);
    }
    for order in &orders {
        let cached = tsrs_vfs::cachedvfs::from(fs());
        for _ in 0..2 {
            for &i in order {
                let (input, want) = &cases[i];
                assert_eq!(cached.realpath(input), *want, "{input} in order {order:?}");
            }
        }
    }
    for (i, (input, want)) in cases.iter().enumerate() {
        let cached = tsrs_vfs::cachedvfs::from(fs());
        for (other, _) in cases.iter().take(i).chain(cases.iter().skip(i + 1)) {
            cached.realpath(other);
        }
        assert_eq!(cached.realpath(input), *want, "{input} after every other input");
    }
}

// A link replaced between cache generations (a watch cycle, a language-server snapshot): after clear_cache, and on
// every call while the cache is disabled, realpath sees the new target.
#[cfg(unix)]
#[test]
fn test_cached_realpath_after_relink() {
    use tsrs_vfs::FS as _;

    let tmp = tspath::normalize_slashes(&temp_dir());
    let real = tspath::normalize_slashes(&std::fs::canonicalize(&tmp).unwrap().to_string_lossy());
    for v in ["v1", "v2"] {
        std::fs::create_dir_all(format!("{tmp}/{v}/lib")).unwrap();
        std::fs::write(format!("{tmp}/{v}/lib/f.ts"), "").unwrap();
        std::fs::write(format!("{tmp}/{v}/lib/g.ts"), "").unwrap();
    }
    let relink = |target: &str| {
        let _ = std::fs::remove_file(format!("{tmp}/cur"));
        mklink(target, &format!("{tmp}/cur"), true);
    };
    let f = format!("{tmp}/cur/lib/f.ts");
    let g = format!("{tmp}/cur/lib/g.ts");

    relink("v1");
    let cached = tsrs_vfs::cachedvfs::from(fs());
    assert_eq!(cached.realpath(&f), format!("{real}/v1/lib/f.ts"));
    relink("v2");
    cached.clear_cache();
    assert_eq!(cached.realpath(&f), format!("{real}/v2/lib/f.ts"));
    assert_eq!(cached.realpath(&g), format!("{real}/v2/lib/g.ts"));

    cached.disable_and_clear_cache();
    relink("v1");
    assert_eq!(cached.realpath(&f), format!("{real}/v1/lib/f.ts"));
    assert_eq!(cached.realpath(&g), format!("{real}/v1/lib/g.ts"));
    relink("v2");
    assert_eq!(cached.realpath(&g), format!("{real}/v2/lib/g.ts"));
    cached.enable();
    assert_eq!(cached.realpath(&f), format!("{real}/v2/lib/f.ts"));
    std::fs::remove_dir_all(&tmp).unwrap();
}
