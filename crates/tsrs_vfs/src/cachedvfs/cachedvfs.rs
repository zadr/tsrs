use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use tsrs_core::tspath;

use crate::FS as VFS;
use crate::{Entries, FileInfo};

// SyncMap is collections.SyncMap: a concurrent map with Load/Store/Clear, keyed by path strings and probed with
// `&str` (the sharded `tsrs_core::collections::SyncMap`: program construction probes these caches from every
// worker, vscode: 250k acquisitions in the 64-thread parse phase, notes/perf-frontend-64.md).
struct SyncMap<V> {
    m: tsrs_core::collections::SyncMap<String, V>,
}

impl<V: Clone> SyncMap<V> {
    fn new() -> Self {
        SyncMap { m: tsrs_core::collections::SyncMap::default() }
    }

    fn load(&self, key: &str) -> Option<V> {
        self.m.load(key)
    }

    fn store(&self, key: &str, value: V) {
        self.m.store(key.to_string(), value);
    }

    fn clear(&self) {
        self.m.clear();
    }
}

pub struct FS<T: VFS> {
    fs: T,
    enabled: AtomicBool,
    // tsrs-only: a write or remove went through since the cache was cleared, so its listings may be older than what
    // a probe of the file system would see (`listed`).
    wrote: AtomicBool,

    directory_exists_cache: SyncMap<bool>,
    file_exists_cache: SyncMap<bool>,
    get_accessible_entries_cache: SyncMap<Entries>,
    realpath_cache: SyncMap<String>,
    stat_cache: SyncMap<Option<FileInfo>>,
}

pub fn from<T: VFS>(fs: T) -> FS<T> {
    FS {
        fs,
        enabled: AtomicBool::new(true),
        wrote: AtomicBool::new(false),
        directory_exists_cache: SyncMap::new(),
        file_exists_cache: SyncMap::new(),
        get_accessible_entries_cache: SyncMap::new(),
        realpath_cache: SyncMap::new(),
        stat_cache: SyncMap::new(),
    }
}

impl<T: VFS> FS<T> {
    pub fn disable_and_clear_cache(&self) {
        if self.enabled.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            self.clear_cache();
        }
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    pub fn clear_cache(&self) {
        self.directory_exists_cache.clear();
        self.file_exists_cache.clear();
        self.get_accessible_entries_cache.clear();
        self.realpath_cache.clear();
        self.stat_cache.clear();
        self.wrote.store(false, Ordering::SeqCst);
    }

    // tsrs-only: forgets the cached listings and keeps the other caches (`tsc -p` drops the include glob's listings
    // once the program is built; they answer the probes of program construction).
    pub fn clear_listings(&self) {
        self.get_accessible_entries_cache.clear();
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    // tsrs-only: (file_exists, directory_exists) of path from the cached listing of its directory, where a stat would
    // answer the same (`listing_answer`).
    fn listed(&self, path: &str) -> Option<(bool, bool)> {
        if !self.enabled() || self.wrote.load(Ordering::SeqCst) || !self.fs.exact_listings() {
            return None;
        }
        let (directory, name) = directory_and_name(path)?;
        self.get_accessible_entries_cache.m.load_with(directory, |entries| listing_answer(entries, name))?
    }
}

// The directory and name of a normalized absolute path ("/a/b.ts" is ("/a", "b.ts")), the form the include glob lists
// and module resolution probes.
fn directory_and_name(path: &str) -> Option<(&str, &str)> {
    if !path.starts_with('/') || path.contains('\\') || tspath::has_relative_path_segment(path) {
        return None;
    }
    let (directory, name) = path.rsplit_once('/')?;
    (!name.is_empty()).then_some((if directory.is_empty() { "/" } else { directory }, name))
}

// The most entries an absent name is compared with (`listing_answer`): the scan is linear in the listing, a stat is
// not. The largest listing a probe reaches on the bench projects has 728 entries.
const MAX_SCANNED_ENTRIES: usize = 1024;

// What an exact listing says of a name in its directory, where a stat says the same: a listed file or directory that
// is not a symlink, or an absent name that no entry can alias. With every name ASCII (`Entries::exact`), an entry
// aliases the name only by ASCII case (case-insensitive volumes); the name itself may be an alias where it has a `~`
// (FAT and NTFS short names), a `:` (NTFS streams over SMB) or a trailing dot or space (stripped by FAT and Windows
// servers).
fn listing_answer(entries: &Entries, name: &str) -> Option<(bool, bool)> {
    if !entries.exact {
        return None;
    }
    for (names, is_directory) in [(&entries.files, false), (&entries.directories, true)] {
        if names.binary_search_by(|n| n.as_str().cmp(name)).is_ok() {
            return (!entries.symlinks.as_ref()?.contains(name))
                .then_some((!is_directory, is_directory));
        }
    }
    let unaliased = name.is_ascii()
        && !name.contains(['~', ':'])
        && !name.ends_with(['.', ' '])
        && entries.files.len() + entries.directories.len() <= MAX_SCANNED_ENTRIES
        && !entries.files.iter().chain(&entries.directories).any(|n| n.eq_ignore_ascii_case(name));
    unaliased.then_some((false, false))
}

impl<T: VFS> VFS for FS<T> {
    fn directory_exists(&self, path: &str) -> bool {
        if self.enabled()
            && let Some(ret) = self.directory_exists_cache.load(path)
        {
            return ret;
        }

        let ret = self.listed(path).map_or_else(|| self.fs.directory_exists(path), |(_, dir)| dir);

        if self.enabled() {
            self.directory_exists_cache.store(path, ret);
        }

        ret
    }

    fn file_exists(&self, path: &str) -> bool {
        if self.enabled()
            && let Some(ret) = self.file_exists_cache.load(path)
        {
            return ret;
        }

        let ret = self.listed(path).map_or_else(|| self.fs.file_exists(path), |(file, _)| file);

        if self.enabled() {
            self.file_exists_cache.store(path, ret);
        }

        ret
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        if self.enabled()
            && let Some(ret) = self.get_accessible_entries_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.get_accessible_entries(path);

        if self.enabled() {
            self.get_accessible_entries_cache.store(path, ret.clone());
        }

        ret
    }

    // tsrs-only: kept while the cache is disabled too, for the program that enables it after config parsing.
    fn keep_accessible_entries(&self, path: &str, entries: Entries) {
        if self.get_accessible_entries_cache.m.load_with(path, |_| ()).is_none() {
            self.get_accessible_entries_cache.store(path, entries);
        }
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.fs.read_file(path)
    }

    fn realpath(&self, path: &str) -> String {
        if self.enabled()
            && let Some(ret) = self.realpath_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.realpath(path);

        if self.enabled() {
            self.realpath_cache.store(path, ret.clone());
        }

        ret
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.wrote.store(true, Ordering::SeqCst);
        self.fs.remove(path)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        self.fs.chtimes(path, a_time, m_time)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        if self.enabled()
            && let Some(ret) = self.stat_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.stat(path);

        if self.enabled() {
            self.stat_cache.store(path, ret.clone());
        }

        ret
    }

    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.use_case_sensitive_file_names()
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.wrote.store(true, Ordering::SeqCst);
        self.fs.write_file(path, data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.wrote.store(true, Ordering::SeqCst);
        self.fs.append_file(path, data)
    }
}
