// kqueue.go: kqueue backend (macOS, FreeBSD, OpenBSD, NetBSD, DragonFlyBSD)
//
// Uses the kernel's kqueue/kevent mechanism to watch individual files and
// directories via EVFILT_VNODE. Unlike inotify, kqueue requires an open file
// descriptor per watched path, not just per directory. On macOS, O_EVTONLY
// opens files for event monitoring only; on other BSDs, O_RDONLY is used.
//
// Event dispatch (on the start thread):
//   - NOTE_WRITE on a directory → compareDir: re-read the directory from
//     disk, diff against the in-memory tree, emit update events for new
//     entries (opening + watching them) and delete events for removed ones
//     (closing their fds).
//   - NOTE_DELETE / NOTE_RENAME / NOTE_REVOKE → close the stale fd. For a
//     pure NOTE_DELETE on a file, tryRewatchLocked checks whether the path
//     was immediately recreated (atomic-save pattern) and emits update
//     instead of delete if so. Otherwise emit delete and remove from the
//     tree.
//   - NOTE_WRITE / NOTE_ATTRIB / NOTE_EXTEND on a file → emit update.
//
// Shutdown:
//   Write a byte to pipe[1] → kevent sees the pipe fd → loop exits →
//   close all tracked fds, the kqueue fd, and the pipe.
//
// Representation: Go's `*dirEntry` values are shared between a subscription's entries map and `fdToEntry`. Here
// every subscription's entries map lives in `kqState.entry_maps` (keyed by a map id; all subscriptions created by
// one `subscribe` share the map, as in Go), and `fd_to_entry` maps an fd to (map id, node). An entries map is a tree
// (`EntryMap`): each entry holds the set of entries directly under it, so listing or removing a directory's entries
// costs the size of the directory, where Go ranges the whole map. A node stores its path once; Go's subscription
// for (map, path) is a flag on the node, and `KqState::subs` indexes the subscribed nodes of every map by path,
// for Go's `subsByPath`. `compareDir` holds
// `mu` for its whole run (Go releases it around directory reads and re-acquires it in `watchPath`); only the
// event-loop thread mutates entry maps after `subscribe`, so this changes blocking, not results.

use hashbrown::HashTable;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::walkdir_unix::walk_dir;
use crate::watcher::{
    DirWatch, Error, PlatformWatcher, WatcherBase, WatcherImpl, err_watched_directory_removed,
};

// kqueue.go:85
// openForEvents opens a path for kqueue event monitoring. On darwin, O_EVTONLY
// opens the file for event notification without granting read access. On other
// BSDs, falls back to O_RDONLY.
fn open_for_events(path: &str) -> Result<i32, Error> {
    #[cfg(target_os = "macos")]
    let flags = libc::O_EVTONLY;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::O_RDONLY;
    let cpath = std::ffi::CString::new(path).map_err(|_| Error::from_errno(libc::EINVAL))?;
    // SAFETY: `cpath` is NUL-terminated and outlives the call.
    let fd = unsafe { libc::open(cpath.as_ptr(), flags) };
    if fd < 0 {
        return Err(Error::from_io(std::io::Error::last_os_error()));
    }
    Ok(fd)
}

// tsrs-only: kqueue needs a descriptor per watched file and directory, and macOS starts a process with a soft limit of
// 256. Go's runtime raises the soft RLIMIT_NOFILE to the hard limit at startup (capped at kern.maxfilesperproc on
// macOS), so tsgo has room; a Rust process does not, and the watch ran out after 256 files. Raised the same way when
// the backend starts.
fn raise_open_file_limit() {
    static RAISED: std::sync::Once = std::sync::Once::new();
    RAISED.call_once(|| {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `lim` is a live, writable rlimit out-parameter.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } != 0 {
            return;
        }
        let mut target = lim.rlim_max;
        #[cfg(target_os = "macos")]
        {
            let mut max_files: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>();
            let name = c"kern.maxfilesperproc";
            // SAFETY: `name` is NUL-terminated; `max_files` and `len` are live out-parameters of the sizes given.
            if unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    (&raw mut max_files).cast(),
                    &raw mut len,
                    std::ptr::null_mut(),
                    0,
                )
            } == 0
                && max_files > 0
            {
                target = target.min(max_files as libc::rlim_t);
            }
        }
        if target > lim.rlim_cur {
            lim.rlim_cur = target;
            // SAFETY: `lim` is a valid rlimit; failure leaves the old limit.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const lim) };
        }
    });
}

// tsrs-only: out of descriptors. Go skips a file it cannot open, which here would stop watching it without a word.
fn is_out_of_descriptors(err: &Error) -> bool {
    err.is_errno(libc::EMFILE) || err.is_errno(libc::ENFILE)
}

fn err_out_of_descriptors(path: &str, err: &Error) -> Error {
    Error::wrap(
        format!(
            "fswatch: kqueue: cannot watch {path}: {err} (each watched file needs a descriptor; raise the limit with `ulimit -n`)"
        ),
        err,
    )
}

pub fn close_fd(fd: i32) {
    // SAFETY: closing an fd this backend opened and still tracks.
    unsafe { libc::close(fd) };
}

const VNODE_FFLAGS: u32 = libc::NOTE_DELETE
    | libc::NOTE_WRITE
    | libc::NOTE_EXTEND
    | libc::NOTE_ATTRIB
    | libc::NOTE_RENAME
    | libc::NOTE_REVOKE;

fn new_kevent(fd: i32, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    // SAFETY: `kevent` is a plain C struct; all-zero is a valid value.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = fd as _;
    ev.filter = filter as _;
    ev.flags = flags as _;
    ev.fflags = fflags as _;
    ev
}

// Go `unix.Kevent(kq, changes, nil, nil)`.
fn kevent_register(kq: i32, ev: &libc::kevent) -> Result<(), Error> {
    // SAFETY: one valid change, no event list.
    let r = unsafe { libc::kevent(kq, ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if r < 0 {
        return Err(Error::from_io(std::io::Error::last_os_error()));
    }
    Ok(())
}

// tsrs-only: each registration carries its own token in `udata`. Closing an fd drops its pending events in the
// kernel, but not those already returned in the batch being handled; once the fd number is reused, such an event
// would otherwise apply to the new registration.
fn watch_fd(kq: i32, fd: i32, token: usize) -> Result<(), Error> {
    let mut ev = new_kevent(
        fd,
        libc::EVFILT_VNODE,
        libc::EV_ADD | libc::EV_CLEAR | libc::EV_ENABLE,
        VNODE_FFLAGS,
    );
    ev.udata = token as _;
    kevent_register(kq, &ev)
}

// dirEntry tracks a watched path for kqueue's fd↔path mapping.
pub struct DirEntry {
    pub path: Box<str>,
    // The path passed to the OS, when it differs from `path` (a watch through a symlinked directory).
    pub watch_path: Option<Box<str>>,
    pub is_dir: bool,
    pub state: Option<i32>, // stores the open fd
    // tsrs-only: the inode number the parent's listing gave for the path (lstat's after a re-watch).
    pub ino: u64,
}

impl DirEntry {
    pub fn new(path: String, watch_path: String, is_dir: bool, ino: u64) -> DirEntry {
        let watch_path = (watch_path != path).then(|| watch_path.into_boxed_str());
        DirEntry { path: path.into_boxed_str(), watch_path, is_dir, state: None, ino }
    }

    pub fn watch_path(&self) -> &str {
        self.watch_path.as_deref().unwrap_or(&self.path)
    }
}

pub type EntriesID = u64;

pub type NodeId = u32;

fn hash_str(s: &str) -> u64 {
    rustc_hash::FxBuildHasher.hash_one(s)
}

fn name_of(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}

struct Node {
    entry: DirEntry,
    // false: the path is not in Go's map. The node remains while paths under it stay in the map (Go removed this
    // one alone), so that they are still reached when an ancestor is removed, and while it is subscribed.
    present: bool,
    // Go's subscriptions for (map, path): one at subscribe and one per re-watch, all dropped together with every
    // subscription at the path, and kept when only the entry is removed. Several only repeat the same events.
    subscribed: bool,
    parent: Option<NodeId>,
    // The nodes directly under this one, hashed by name.
    children: HashTable<NodeId>,
}

// Go's `map[string]*dirEntry` of one subscribe, as a tree of nodes in a slab with an index by path.
pub struct EntryMap {
    // The watch whose subscribe built the map; every subscription in it is that watch's.
    pub watch: Option<Arc<DirWatch>>,
    nodes: Vec<Option<Node>>,
    free: Vec<NodeId>,
    by_path: HashTable<NodeId>,
    subscribed: usize,
}

impl EntryMap {
    pub fn with_capacity(watch: &Arc<DirWatch>, n: usize) -> EntryMap {
        // Room for an eighth more entries, as a hash map of the entries would have: the first entries created
        // after subscribe do not move the slab.
        EntryMap {
            watch: Some(Arc::clone(watch)),
            nodes: Vec::with_capacity(n + n / 8),
            free: Vec::new(),
            by_path: HashTable::with_capacity(n),
            subscribed: 0,
        }
    }

    fn watch(&self) -> &Arc<DirWatch> {
        self.watch.as_ref().unwrap()
    }

    fn node(&self, id: NodeId) -> &Node {
        self.nodes[id as usize].as_ref().unwrap()
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes[id as usize].as_mut().unwrap()
    }

    // The node at `path`, present or not.
    pub fn id(&self, path: &str) -> Option<NodeId> {
        self.by_path.find(hash_str(path), |&id| &*self.node(id).entry.path == path).copied()
    }

    fn present_entry(&self, id: NodeId) -> Option<&DirEntry> {
        self.nodes.get(id as usize)?.as_ref().filter(|n| n.present).map(|n| &n.entry)
    }

    fn present_id(&self, path: &str) -> Option<NodeId> {
        self.id(path).filter(|&id| self.node(id).present)
    }

    fn child(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        self.node(dir)
            .children
            .find(hash_str(name), |&id| name_of(&self.node(id).entry.path) == name)
            .copied()
    }

    pub fn get_mut(&mut self, path: &str) -> Option<&mut DirEntry> {
        self.present_id(path).map(|id| &mut self.node_mut(id).entry)
    }

    pub fn values(&self) -> impl Iterator<Item = &DirEntry> {
        self.nodes.iter().flatten().filter(|n| n.present).map(|n| &n.entry)
    }

    // Go `entries[e.path] = e`. Entries under the path stay. A new path's parent is already in the map, except
    // for the first entry (the watched directory); `children` sizes its set of entries (a directory's listing).
    pub fn insert(&mut self, entry: DirEntry, children: usize) -> NodeId {
        if let Some(id) = self.id(&entry.path) {
            let node = self.node_mut(id);
            node.entry = entry;
            node.present = true;
            return id;
        }
        let parent = self.id(parent_of(&entry.path));
        debug_assert!(
            parent.is_some() || self.by_path.is_empty(),
            "{} has no parent entry",
            entry.path
        );
        let hash = hash_str(&entry.path);
        let node = Node {
            entry,
            present: true,
            subscribed: false,
            parent,
            children: if children == 0 {
                HashTable::new()
            } else {
                HashTable::with_capacity(children)
            },
        };
        let id = if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = Some(node);
            id
        } else {
            self.nodes.push(Some(node));
            (self.nodes.len() - 1) as NodeId
        };
        let EntryMap { nodes, by_path, .. } = self;
        let path_hash = |&id: &NodeId| hash_str(&nodes[id as usize].as_ref().unwrap().entry.path);
        by_path.insert_unique(hash, id, path_hash);
        if let Some(parent) = parent {
            let name_hash = hash_str(name_of(&self.node(id).entry.path));
            let mut siblings = std::mem::take(&mut self.node_mut(parent).children);
            siblings.insert_unique(name_hash, id, |&c| hash_str(name_of(&self.node(c).entry.path)));
            self.node_mut(parent).children = siblings;
        }
        id
    }

    // Go `delete(entries, path)`.
    pub fn remove(&mut self, path: &str) {
        if let Some(id) = self.present_id(path) {
            self.remove_id(id);
        }
    }

    fn remove_id(&mut self, id: NodeId) {
        self.node_mut(id).present = false;
        self.prune(id);
    }

    // Frees a node that is not present and has nothing under it, then its parent if that is left the same way.
    fn prune(&mut self, mut id: NodeId) {
        loop {
            let node = self.node(id);
            if node.present || node.subscribed || !node.children.is_empty() {
                return;
            }
            match self.take_node(id).parent {
                Some(parent) => id = parent,
                None => return,
            }
        }
    }

    // Unlinks a node from the index and from its parent's children, and frees its slot.
    fn take_node(&mut self, id: NodeId) -> Node {
        let node = self.nodes[id as usize].take().unwrap();
        if let Ok(slot) = self.by_path.find_entry(hash_str(&node.entry.path), |&i| i == id) {
            slot.remove();
        }
        if let Some(parent) = node.parent
            && let Some(parent) = self.nodes[parent as usize].as_mut()
            && let Ok(slot) =
                parent.children.find_entry(hash_str(name_of(&node.entry.path)), |&i| i == id)
        {
            slot.remove();
        }
        self.free.push(id);
        node
    }

    fn reserve_children(&mut self, id: NodeId, additional: usize) {
        let mut children = std::mem::take(&mut self.node_mut(id).children);
        children.reserve(additional, |&c| hash_str(name_of(&self.node(c).entry.path)));
        self.node_mut(id).children = children;
    }

    // The nodes under `id` (not `id` itself), parents before children.
    fn subtree(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.node(id).children.iter().copied().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            stack.extend(self.node(id).children.iter().copied());
        }
        out
    }

    // The present entries under `path` (not `path` itself).
    fn descendants(&self, path: &str) -> Vec<NodeId> {
        let Some(id) = self.id(path) else {
            return Vec::new();
        };
        let mut out = self.subtree(id);
        out.retain(|&id| self.node(id).present);
        out
    }

    // Go: delete path (when `include_root`) and every path under it. Returns the descriptors the removed entries
    // held; nodes stay while they are subscribed.
    fn remove_tree(&mut self, path: &str, include_root: bool) -> Vec<i32> {
        match self.id(path) {
            Some(root) => self.remove_tree_at(root, include_root),
            None => Vec::new(),
        }
    }

    fn remove_tree_at(&mut self, root: NodeId, include_root: bool) -> Vec<i32> {
        let mut fds = Vec::new();
        let mut ids = self.subtree(root);
        for &id in &ids {
            let node = self.node_mut(id);
            if std::mem::take(&mut node.present) {
                fds.extend(node.entry.state.take());
            }
        }
        if include_root {
            let node = self.node_mut(root);
            if std::mem::take(&mut node.present) {
                fds.extend(node.entry.state.take());
            }
        }
        ids.reverse();
        for id in ids {
            let node = self.node(id);
            if !node.subscribed && node.children.is_empty() {
                self.take_node(id);
            }
        }
        self.prune(root);
        fds
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut DirEntry> {
        self.nodes.iter_mut().flatten().filter(|n| n.present).map(|n| &mut n.entry)
    }
}

// Entry counts under each directory of a pre-order walk, to size the directories' sets of entries.
fn child_counts(walked: &[(String, bool, u64)]) -> Vec<usize> {
    let mut counts = vec![0; walked.len()];
    let mut dirs: Vec<usize> = Vec::new();
    for (i, (path, is_dir, _)) in walked.iter().enumerate() {
        let parent = parent_of(path);
        while let Some(&top) = dirs.last() {
            if walked[top].0 == parent {
                counts[top] += 1;
                break;
            }
            dirs.pop();
        }
        if *is_dir {
            dirs.push(i);
        }
    }
    counts
}

// A subscription at a path, as `KqState::find_subscriptions_locked` returns it: the map's watch and its node.
pub struct KqueueSubscription {
    pub dir_watch: Arc<DirWatch>,
    pub entries: EntriesID,
    pub node: NodeId,
}

// A subscribed node.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SubRef {
    pub map: EntriesID,
    pub node: NodeId,
}

#[derive(Default)]
pub struct KqState {
    // Go's `subsByPath`: the subscribed nodes of every map, hashed by path.
    pub subs: HashTable<SubRef>,
    // fd -> (entries map, node, registration token)
    pub fd_to_entry: FxHashMap<i32, (EntriesID, NodeId, usize)>,
    pub entry_maps: FxHashMap<EntriesID, EntryMap>,
}

pub struct KqueueBackend {
    base: WatcherBase,

    pub mu: Mutex<KqState>, // local lock for kqueue-specific maps
    kq: AtomicI32,
    // pipeFDs[0] is read in the start thread only. pipeFDs[1] is written
    // by shutdown (any thread) to wake the loop, so it lives in
    // pipeWriteFD with a sentinel of -1 once closed.
    pipe_read_fd: AtomicI32,
    pipe_write_fd: AtomicI32,
    ended: Mutex<bool>,
    ended_cv: Condvar,
    next_id: AtomicU64,
}

// kqueue.go:129 (init)
pub(crate) fn init(mut w: PlatformWatcher) -> PlatformWatcher {
    w.factory = Some(|| Arc::new(new_kqueue_backend()) as Arc<dyn WatcherImpl>);
    w
}

// kqueue.go:133
pub fn new_kqueue_backend() -> KqueueBackend {
    KqueueBackend {
        base: WatcherBase::default(),
        mu: Mutex::new(KqState::default()),
        kq: AtomicI32::new(-1),
        pipe_read_fd: AtomicI32::new(-1),
        pipe_write_fd: AtomicI32::new(-1),
        ended: Mutex::new(false),
        ended_cv: Condvar::new(),
        next_id: AtomicU64::new(1),
    }
}

impl KqueueBackend {
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // The setup half of Go's `start`: the kqueue and the shutdown pipe.
    pub fn open_kqueue(&self) -> Result<(), Error> {
        // SAFETY: plain syscall.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(Error::new(format!(
                "unable to open kqueue: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.kq.store(kq, Ordering::SeqCst);

        let mut pipe_fds = [-1i32; 2];
        // SAFETY: `pipe_fds` has room for two fds.
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
            return Err(Error::new(format!(
                "unable to open pipe: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.pipe_read_fd.store(pipe_fds[0], Ordering::SeqCst);
        self.pipe_write_fd.store(pipe_fds[1], Ordering::SeqCst);

        // Watch the read side of the pipe so we can break the loop on shutdown.
        let pipe_ev = new_kevent(pipe_fds[0], libc::EVFILT_READ, libc::EV_ADD | libc::EV_CLEAR, 0);
        if let Err(err) = kevent_register(kq, &pipe_ev) {
            return Err(Error::new(format!("unable to watch pipe: {err}")));
        }
        Ok(())
    }

    fn run_loop(&self) -> Result<(), Error> {
        self.open_kqueue()?;
        self.base.notify_started();

        // SAFETY: zeroed kevent structs are valid.
        let mut events: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; 128];
        let mut watchers_touched: Vec<Arc<DirWatch>> = Vec::new();
        loop {
            let n = self.read_events(&mut events, None)?;
            if self.handle_events(&events[..n], &mut watchers_touched) {
                break;
            }
        }
        Ok(())
    }

    // One `kevent` wait (retried on EINTR); `timeout` None blocks.
    pub fn read_events(
        &self,
        events: &mut [libc::kevent],
        timeout: Option<&libc::timespec>,
    ) -> Result<usize, Error> {
        let kq = self.kq.load(Ordering::SeqCst);
        loop {
            // SAFETY: `events` has room for `events.len()` results; `timeout` is null or a live timespec.
            let n = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    timeout.map_or(std::ptr::null(), std::ptr::from_ref),
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(Error::new(format!("kevent error: {err}")));
            }
        }
    }

    // Handles one batch of events and notifies the watches it touched. Returns true when the shutdown pipe fired.
    pub fn handle_events(
        &self,
        events: &[libc::kevent],
        watchers_touched: &mut Vec<Arc<DirWatch>>,
    ) -> bool {
        let pipe_read_fd = self.pipe_read_fd.load(Ordering::SeqCst);
        let mut stop = false;
        for ev in events {
            let mut fflags = ev.fflags;
            let flags = ev.flags;
            let fd = ev.ident as i32;
            if fd == pipe_read_fd {
                stop = true;
                break;
            }

            // EV_ERROR indicates kevent couldn't apply a changelist
            // entry or that the kernel rejected the registration.
            if flags & libc::EV_ERROR != 0 {
                continue;
            }

            let st = self.mu.lock().unwrap();
            let entry = st.fd_to_entry.get(&fd).and_then(|&(map, node, token)| {
                if token != ev.udata as usize {
                    return None;
                }
                let e = st.entry_maps.get(&map)?.present_entry(node)?;
                (e.state == Some(fd)).then(|| (map, node, e.path.clone(), e.is_dir))
            });
            drop(st);
            let Some((map_id, node, path, is_dir)) = entry else {
                continue;
            };

            if fflags & libc::NOTE_WRITE != 0 && is_dir {
                self.compare_dir(fd, &path, watchers_touched);
                // NOTE_WRITE on a dir already ran compareDir above.
                // On DragonFlyBSD, rename-over coalesces NOTE_DELETE
                // with NOTE_WRITE on the parent directory (rather than
                // firing NOTE_DELETE on the replaced file's fd).
                // Skip handleFileEvent so we don't misinterpret the
                // coalesced NOTE_DELETE as the directory itself being
                // removed.
                fflags &= !libc::NOTE_DELETE;
            }
            if fflags & !libc::NOTE_WRITE != 0 || !is_dir {
                self.handle_file_event(fflags, map_id, node, &path, watchers_touched);
            }
        }

        for w in watchers_touched.drain(..) {
            w.notify();
        }
        stop
    }

    // kqueue.go:219
    fn close_fds(&self) {
        let r = self.pipe_read_fd.swap(-1, Ordering::SeqCst);
        if r >= 0 {
            close_fd(r);
        }
        let w = self.pipe_write_fd.swap(-1, Ordering::SeqCst);
        if w >= 0 {
            close_fd(w);
        }
        let kq = self.kq.swap(-1, Ordering::SeqCst);
        if kq >= 0 {
            close_fd(kq);
        }
    }

    // kqueue.go:234
    pub fn close_subscriptions(&self) {
        let mut st = self.mu.lock().unwrap();
        // tsrs-only: subscriptions outlive descriptor replacement. Close the current owned descriptors, not
        // Go's cached subscription fd, which may already have been reused by an unrelated Rust File/ReadDir.
        #[expect(
            clippy::iter_over_hash_type,
            reason = "closes each owned fd once, then clears the maps; the order is irrelevant"
        )]
        for &fd in st.fd_to_entry.keys() {
            close_fd(fd);
        }
        st.subs = HashTable::new();
        st.fd_to_entry = FxHashMap::default();
        st.entry_maps = FxHashMap::default();
    }

    // kqueue.go:266
    fn handle_file_event(
        &self,
        fflags: u32,
        map_id: EntriesID,
        node: NodeId,
        entry_path: &str,
        touched: &mut Vec<Arc<DirWatch>>,
    ) {
        let mut st = self.mu.lock().unwrap();
        let subs = st.find_subscriptions_locked(entry_path);
        let Some((state, is_dir)) = st
            .entry_maps
            .get(&map_id)
            .and_then(|m| m.present_entry(node))
            .filter(|e| &*e.path == entry_path)
            .map(|e| (e.state, e.is_dir))
        else {
            return;
        };

        if fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) != 0 {
            // Close the stale fd; the watched inode is gone.
            if let Some(old_fd) = state {
                close_fd(old_fd);
                st.fd_to_entry.remove(&old_fd);
                st.entry_maps.get_mut(&map_id).unwrap().node_mut(node).entry.state = None;
            }

            let mut recreated = false;
            if fflags & libc::NOTE_DELETE != 0
                && fflags & (libc::NOTE_RENAME | libc::NOTE_REVOKE) == 0
                && !is_dir
            {
                recreated = self.try_rewatch_locked(&mut st, map_id, node);
            }

            for sub in &subs {
                touch(touched, &sub.dir_watch);
                if recreated {
                    sub.dir_watch.events.update(entry_path);
                } else {
                    sub.dir_watch.events.remove(entry_path);
                    // If we lost a directory, walk the entries map and
                    // close every fd we had open for descendants. Some
                    // kernels (OpenBSD in particular) deliver only the
                    // parent's NOTE_DELETE/NOTE_RENAME and never fire
                    // NOTE_DELETE on the children; without this cleanup,
                    // modifying a file inside the moved tree later
                    // surfaces an event against the descendant's stale
                    // (pre-rename) path. We also emit a delete for each
                    // descendant we close, so callers don't miss those
                    // removals if the kernel didn't fire per-child events.
                    if is_dir {
                        st.close_descendant_fds_locked(&sub.dir_watch, sub.entries, entry_path);
                    }
                    // tsrs-only: Go leaves the descriptor of another map's entry for the path open; its events
                    // found no entry.
                    let fds = st
                        .entry_maps
                        .get_mut(&sub.entries)
                        .map_or_default(|m| m.remove_tree(entry_path, true));
                    for fd in fds {
                        close_fd(fd);
                        st.fd_to_entry.remove(&fd);
                    }
                    // Root-of-watch deletion: no more events can fire
                    // for this dirWatch. Tell the caller.
                    if sub.dir_watch.dir == entry_path {
                        sub.dir_watch.events.set_error(err_watched_directory_removed());
                    }
                }
            }
            if !recreated {
                let refs = st.subs_at(entry_path);
                st.unsubscribe(&refs);
            }
            return;
        }

        // tsrs-only (`set_write_settling`): kqueue reports a save's truncation at once, before its data; an empty file
        // holds the callbacks until an event shows it written (see fsevents_darwin.rs).
        #[cfg(target_os = "macos")]
        let being_written = (crate::watcher::write_settling()
            && !is_dir
            && fflags & (libc::NOTE_WRITE | libc::NOTE_ATTRIB | libc::NOTE_EXTEND) != 0)
            .then(|| {
                let watch_path = st.entry_maps[&map_id].node(node).entry.watch_path();
                std::fs::metadata(watch_path).is_ok_and(|m| m.is_file() && m.len() == 0)
            });
        for sub in &subs {
            touch(touched, &sub.dir_watch);
            if fflags & (libc::NOTE_WRITE | libc::NOTE_ATTRIB | libc::NOTE_EXTEND) != 0 {
                #[cfg(target_os = "macos")]
                match being_written {
                    Some(true) => sub.dir_watch.write_started(entry_path),
                    Some(false) => sub.dir_watch.write_finished(entry_path),
                    None => {}
                }
                sub.dir_watch.events.update(entry_path);
            }
        }
    }

    // kqueue.go:357
    // tryRewatchLocked checks whether a deleted path was immediately recreated
    // with the same type. If so, it opens a new fd, registers a kqueue watch,
    // and returns true. The caller should emit update instead of delete.
    fn try_rewatch_locked(&self, st: &mut KqState, map_id: EntriesID, node: NodeId) -> bool {
        let Some(entry) = st.entry_maps.get(&map_id).and_then(|m| m.present_entry(node)) else {
            return false;
        };
        let Ok(md) = std::fs::symlink_metadata(entry.watch_path()) else {
            return false;
        };

        // Only fast-path when the recreated path has the same type;
        // a file→dir change needs a full tree rebuild via compareDir.
        let new_is_dir = md.is_dir();
        if new_is_dir != entry.is_dir {
            return false;
        }

        let Ok(fd) = open_for_events(entry.watch_path()) else {
            return false;
        };

        let token = self.next_id() as usize;
        if watch_fd(self.kq.load(Ordering::SeqCst), fd, token).is_err() {
            close_fd(fd);
            return false;
        }

        let e = &mut st.entry_maps.get_mut(&map_id).unwrap().node_mut(node).entry;
        e.state = Some(fd);
        e.ino = std::os::unix::fs::MetadataExt::ino(&md);
        st.fd_to_entry.insert(fd, (map_id, node, token));
        true
    }

    // kqueue.go:479
    // watchPath corresponds to `kqueueBackend::watchDir`.
    fn watch_path_locked(
        &self,
        st: &mut KqState,
        w: &Arc<DirWatch>,
        map_id: EntriesID,
        node: NodeId,
    ) -> bool {
        let Some(entry) = st.entry_maps.get(&map_id).and_then(|m| m.present_entry(node)) else {
            return false;
        };

        if entry.state.is_none() {
            let fd = match open_for_events(entry.watch_path()) {
                Ok(fd) => fd,
                Err(err) => {
                    if is_out_of_descriptors(&err) {
                        w.events.set_error(err_out_of_descriptors(entry.watch_path(), &err));
                    }
                    return false;
                }
            };
            let token = self.next_id() as usize;
            if watch_fd(self.kq.load(Ordering::SeqCst), fd, token).is_err() {
                close_fd(fd);
                return false;
            }
            st.entry_maps.get_mut(&map_id).unwrap().node_mut(node).entry.state = Some(fd);
            st.fd_to_entry.insert(fd, (map_id, node, token));
        }
        st.subscribe_node(map_id, node);
        true
    }

    // kqueue.go:509
    // compareDir mirrors `kqueueBackend::compareDir`. Triggered when a watched
    // directory has NOTE_WRITE: list the dir, diff against the tree, emit
    // create/remove events.
    fn compare_dir(&self, _fd: i32, path: &str, touched: &mut Vec<Arc<DirWatch>>) -> bool {
        let mut st = self.mu.lock().unwrap();
        let subs = st.find_subscriptions_locked(path);

        // For non-recursive subscriptions, only compareDir on the root dir.
        // NOTE_WRITE on a child dir means something changed inside it, but
        // non-recursive mode shouldn't report those changes. Emit an update
        // for the child dir itself (its metadata changed) and return.
        let mut filtered_subs = Vec::new();
        for s in subs {
            if !s.dir_watch.recursive && path != s.dir_watch.dir {
                s.dir_watch.events.update(path);
                touch(touched, &s.dir_watch);
            } else {
                filtered_subs.push(s);
            }
        }
        if filtered_subs.is_empty() {
            return true;
        }
        let subs = filtered_subs;

        let dir_start = format!("{path}/");
        // Listings sorted by name, by watch path.
        let mut snapshots: FxHashMap<String, Arc<Vec<DiskEntry>>> = FxHashMap::default();

        // Each subscription has its own entries map (built in subscribe).
        // Multiple subs at the same path arise from multiple dirWatches
        // covering overlapping subtrees; their maps are always distinct, so
        // we iterate subs directly rather than trying to dedup by map identity.
        for sub in &subs {
            let map_id = sub.entries;
            let dir = sub.node;
            let Some(watch_path) = st
                .entry_maps
                .get(&map_id)
                .and_then(|m| m.present_entry(dir))
                .map(|e| e.watch_path().to_owned())
            else {
                continue;
            };
            let watch_dir_start = format!("{watch_path}/");

            let snapshot = if let Some(s) = snapshots.get(&watch_path) {
                Arc::clone(s)
            } else {
                let Ok(disk_entries) = read_entries(&watch_path) else {
                    continue;
                };
                let s = Arc::new(disk_entries);
                snapshots.insert(watch_path, Arc::clone(&s));
                s
            };

            if let Some(m) = st.entry_maps.get_mut(&map_id) {
                m.reserve_children(dir, snapshot.len().saturating_sub(m.node(dir).children.len()));
            }
            for ent in snapshot.iter() {
                let existing = st.entry_maps.get(&map_id).and_then(|m| {
                    let id = m.child(dir, &ent.name)?;
                    let e = m.present_entry(id)?;
                    Some((id, e.is_dir, e.state, e.ino))
                });
                let full_path = || [dir_start.as_str(), &ent.name].concat();
                let full_watch_path = || [watch_dir_start.as_str(), &ent.name].concat();
                if let Some((id, existing_is_dir, mut existing_state, existing_ino)) = existing {
                    if let Some(fd) = existing_state
                        && replaced(fd, existing_ino, ent, &watch_dir_start)
                    {
                        // Inode changed: path was replaced.
                        st.close_node_fd(map_id, id);
                        if existing_is_dir {
                            st.remove_entry_and_descendants_locked(map_id, &full_path(), false);
                        }
                        let e = &mut st.entry_maps.get_mut(&map_id).unwrap().node_mut(id).entry;
                        e.is_dir = ent.is_dir;
                        e.ino = ent.ino;
                        existing_state = None;
                    }
                    if existing_state.is_some() {
                        continue;
                    }
                    // Entry exists but fd is stale: the file was replaced.
                    // Re-watch it and emit an update.
                    if !self.watch_path_locked(&mut st, &sub.dir_watch, map_id, id) {
                        continue;
                    }
                    sub.dir_watch.events.update(&full_path());
                    touch(touched, &sub.dir_watch);
                    if ent.is_dir && sub.dir_watch.recursive {
                        self.walk_new_subtree_locked(&mut st, sub, map_id, &full_watch_path());
                    }
                    continue;
                }
                let e = DirEntry::new(full_path(), full_watch_path(), ent.is_dir, ent.ino);
                let Some(m) = st.entry_maps.get_mut(&map_id) else {
                    continue;
                };
                let id = m.insert(e, 0);
                if !self.watch_path_locked(&mut st, &sub.dir_watch, map_id, id) {
                    st.entry_maps.get_mut(&map_id).unwrap().remove_id(id);
                    continue;
                }
                sub.dir_watch.events.create(&st.entry_maps[&map_id].node(id).entry.path);
                touch(touched, &sub.dir_watch);

                // For recursive subscriptions, walk into the new directory
                // to catch pre-populated subdirectories (e.g. a directory
                // tree moved into the watched area).
                if ent.is_dir && sub.dir_watch.recursive {
                    self.walk_new_subtree_locked(&mut st, sub, map_id, &full_watch_path());
                }
            }

            // Detect removals: entries directly under dirStart that no longer
            // exist on disk.
            let to_remove: Vec<NodeId> = st.entry_maps.get(&map_id).map_or_default(|m| {
                m.node(dir)
                    .children
                    .iter()
                    .copied()
                    .filter(|&id| {
                        m.present_entry(id).is_some_and(|e| {
                            snapshot.binary_search_by(|d| (*d.name).cmp(name_of(&e.path))).is_err()
                        })
                    })
                    .collect()
            });
            for p in to_remove {
                let m = &st.entry_maps[&map_id];
                sub.dir_watch.events.remove(&m.node(p).entry.path);
                touch(touched, &sub.dir_watch);
                let mut ids = m.subtree(p);
                ids.push(p);
                for id in ids {
                    let Some(e) = st.entry_maps[&map_id].present_entry(id) else {
                        continue;
                    };
                    let refs = st.subs_at(&e.path);
                    st.close_node_fd(map_id, id);
                    st.unsubscribe(&refs);
                }
                st.entry_maps.get_mut(&map_id).unwrap().remove_tree_at(p, true);
            }
        }
        true
    }

    // The `walkDir(fullWatchPath, true, ...)` closures of compareDir.
    fn walk_new_subtree_locked(
        &self,
        st: &mut KqState,
        sub: &KqueueSubscription,
        map_id: EntriesID,
        full_watch_path: &str,
    ) {
        let mut found = Vec::new();
        let _ = walk_dir(full_watch_path, true, &mut |p, p_is_dir, ino| {
            found.push((p.to_string(), p_is_dir, ino));
            Ok(())
        });
        let counts = child_counts(&found);
        let mut found = found.into_iter().zip(counts);
        let Some(m) = st.entry_maps.get_mut(&map_id) else {
            return;
        };
        // The walk's root is already handled above.
        if let Some(((root, _, _), count)) = found.next()
            && let Some(id) = m.present_id(&sub.dir_watch.display_path(&root))
        {
            m.reserve_children(id, count);
        }
        for ((p, p_is_dir, ino), count) in found {
            let display_path = sub.dir_watch.display_path(&p);
            let id = st
                .entry_maps
                .get_mut(&map_id)
                .unwrap()
                .insert(DirEntry::new(display_path, p, p_is_dir, ino), count);
            sub.dir_watch.events.create(&st.entry_maps[&map_id].node(id).entry.path);
            self.watch_path_locked(st, &sub.dir_watch, map_id, id);
        }
    }
}

fn touch(touched: &mut Vec<Arc<DirWatch>>, w: &Arc<DirWatch>) {
    if !touched.iter().any(|t| Arc::ptr_eq(t, w)) {
        touched.push(Arc::clone(w));
    }
}

// Whether the listed `ent` is no longer the file whose entry holds `fd` and stored `ino`.
#[cfg(not(target_os = "dragonfly"))]
fn replaced(_fd: i32, ino: u64, ent: &DiskEntry, _watch_dir_start: &str) -> bool {
    // tsrs-only: the listing's inode number against the one stored with the entry, without a syscall. Go fstats
    // the fd and lstats the path for every listed entry on every change of the directory (DragonFly's rename-over
    // posts no NOTE_DELETE to the replaced file). Elsewhere the kernel posts NOTE_DELETE or NOTE_RENAME to the
    // descriptor whenever the path stops naming its file, so this only notices a replacement earlier in the batch.
    // A symlink always differed there (its fd is open on the target), so it is re-watched on every change.
    ent.ino != ino || ent.is_symlink
}

// Check if the fd still refers to the same inode as the path on disk. On DragonFlyBSD, rename-over doesn't fire
// NOTE_DELETE on the replaced file's fd, leaving a stale entry whose fd points to the old (now unlinked) inode.
#[cfg(target_os = "dragonfly")]
fn replaced(fd: i32, _ino: u64, ent: &DiskEntry, watch_dir_start: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fstat(fd), std::fs::symlink_metadata(format!("{watch_dir_start}{}", ent.name))) {
        (Some(fd_st), Ok(path_st)) => fd_st.0 != path_st.dev() || fd_st.1 != path_st.ino(),
        _ => false,
    }
}

// (dev, ino) of an open fd (Go `unix.Fstat`).
#[cfg(target_os = "dragonfly")]
fn fstat(fd: i32) -> Option<(u64, u64)> {
    // SAFETY: zeroed `stat` is a valid out-parameter; `fd` is an fd this backend owns.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a live, writable `stat` out-parameter for the call; `fd` is an fd this backend owns.
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return None;
    }
    Some((st.st_dev as u64, st.st_ino as u64))
}

impl KqState {
    pub fn set_entry_state(&mut self, map_id: EntriesID, path: &str, state: Option<i32>) {
        if let Some(e) = self.entry_maps.get_mut(&map_id).and_then(|m| m.get_mut(path)) {
            e.state = state;
        }
    }

    // Adds a map built by subscribe, with one subscription per entry.
    pub fn insert_map(&mut self, map_id: EntriesID, mut entries: EntryMap) {
        let mut ids = Vec::with_capacity(entries.by_path.len());
        for (id, node) in entries.nodes.iter_mut().enumerate() {
            if let Some(node) = node.as_mut().filter(|n| n.present) {
                node.subscribed = true;
                ids.push(id as NodeId);
            }
        }
        entries.subscribed = ids.len();
        self.entry_maps.insert(map_id, entries);
        let KqState { subs, entry_maps, .. } = self;
        let m = &entry_maps[&map_id];
        subs.reserve(ids.len(), |r| hash_str(path_of(entry_maps, *r)));
        for node in ids {
            subs.insert_unique(
                hash_str(&m.node(node).entry.path),
                SubRef { map: map_id, node },
                |r| hash_str(path_of(entry_maps, *r)),
            );
        }
    }

    // Go `subsByPath[path] = append(..., sub)`, without the repeats.
    fn subscribe_node(&mut self, map: EntriesID, node: NodeId) {
        let KqState { subs, entry_maps, .. } = self;
        let m = entry_maps.get_mut(&map).unwrap();
        let n = m.node_mut(node);
        if std::mem::replace(&mut n.subscribed, true) {
            return;
        }
        m.subscribed += 1;
        let hash = hash_str(&m.node(node).entry.path);
        subs.insert_unique(hash, SubRef { map, node }, |r| hash_str(path_of(entry_maps, *r)));
    }

    // The subscriptions at `path` in every map (Go `subsByPath[path]`).
    fn subs_at(&self, path: &str) -> SmallVec<[SubRef; 2]> {
        self.subs
            .iter_hash(hash_str(path))
            .filter(|r| path_of(&self.entry_maps, **r) == path)
            .copied()
            .collect()
    }

    // Go `delete(subsByPath, path)` for the `subs_at(path)` it is given.
    fn unsubscribe(&mut self, refs: &[SubRef]) {
        for &r in refs {
            let m = self.entry_maps.get_mut(&r.map).unwrap();
            let hash = hash_str(&m.node(r.node).entry.path);
            if let Ok(slot) = self.subs.find_entry(hash, |x| *x == r) {
                slot.remove();
            }
            m.node_mut(r.node).subscribed = false;
            m.subscribed -= 1;
            m.prune(r.node);
        }
    }

    // kqueue.go:337
    // closeDescendantFDsLocked closes every fd attached to an entry whose
    // path lives strictly under root, removing the kevent registration and
    // the corresponding b.subsByPath / b.fdToEntry bookkeeping, and emits a
    // delete event for each.
    fn close_descendant_fds_locked(&mut self, w: &Arc<DirWatch>, map_id: EntriesID, root: &str) {
        let Some(m) = self.entry_maps.get(&map_id) else {
            return;
        };
        for id in m.descendants(root) {
            let e = &self.entry_maps[&map_id].node(id).entry;
            w.events.remove(&e.path);
            let refs = self.subs_at(&e.path);
            self.close_node_fd(map_id, id);
            self.unsubscribe(&refs);
        }
    }

    // kqueue.go:408
    pub fn close_entry_locked(&mut self, map_id: EntriesID, path: &str) {
        if let Some(id) = self.entry_maps.get(&map_id).and_then(|m| m.present_id(path)) {
            self.close_node_fd(map_id, id);
        }
    }

    fn close_node_fd(&mut self, map_id: EntriesID, id: NodeId) {
        let e = &mut self.entry_maps.get_mut(&map_id).unwrap().node_mut(id).entry;
        if let Some(fd) = e.state.take() {
            close_fd(fd);
            self.fd_to_entry.remove(&fd);
        }
    }

    // kqueue.go:432
    // Go's removeSubsForEntriesLocked here matches no subscription (it compares the address of the caller's own
    // subscription), so the subscriptions of the removed entries stay.
    fn remove_entry_and_descendants_locked(
        &mut self,
        map_id: EntriesID,
        path: &str,
        include_root: bool,
    ) {
        let fds =
            self.entry_maps.get_mut(&map_id).map_or_default(|m| m.remove_tree(path, include_root));
        for fd in fds {
            close_fd(fd);
            self.fd_to_entry.remove(&fd);
        }
    }

    // kqueue.go:449
    fn find_subscriptions_locked(&self, path: &str) -> SmallVec<[KqueueSubscription; 2]> {
        self.subs_at(path)
            .into_iter()
            .map(|r| KqueueSubscription {
                dir_watch: Arc::clone(self.entry_maps[&r.map].watch()),
                entries: r.map,
                node: r.node,
            })
            .collect()
    }

    // kqueue.go:468
    // cleanupEntriesLocked closes fds for all entries that have been opened.
    // Called on subscribe failure to avoid fd leaks. Must be called under b.mu.
    fn cleanup_entries_locked(&mut self, entries: &mut EntryMap) {
        for e in entries.values_mut() {
            if let Some(fd) = e.state.take() {
                close_fd(fd);
                self.fd_to_entry.remove(&fd);
            }
        }
    }
}

fn path_of(maps: &FxHashMap<EntriesID, EntryMap>, r: SubRef) -> &str {
    &maps[&r.map].node(r.node).entry.path
}

struct DiskEntry {
    name: Box<str>,
    is_dir: bool,
    #[cfg(not(target_os = "dragonfly"))]
    is_symlink: bool,
    ino: u64,
}

// kqueue.go:732
// readEntries lists directory entries (excluding "." and "..") at path. (Go `os.ReadDir` sorts by name.)
fn read_entries(path: &str) -> Result<Vec<DiskEntry>, Error> {
    use std::os::unix::fs::DirEntryExt;
    let mut out = Vec::new();
    out = (std::fs::read_dir(path).map_err(Error::from_io)?).try_fold(out, |mut out, ent| {
        let ent = ent.map_err(Error::from_io)?;
        let file_type = ent.file_type();
        out.push(DiskEntry {
            name: ent
                .file_name()
                .into_string()
                .map_or_else(|name| name.to_string_lossy().into(), String::into_boxed_str),
            is_dir: file_type.as_ref().is_ok_and(std::fs::FileType::is_dir),
            #[cfg(not(target_os = "dragonfly"))]
            is_symlink: file_type.is_ok_and(|t| t.is_symlink()),
            ino: ent.ino(),
        });
        Ok::<_, Error>(out)
    })?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

impl WatcherImpl for KqueueBackend {
    fn base(&self) -> &WatcherBase {
        &self.base
    }

    // kqueue.go:146
    fn start(self: Arc<Self>) -> Result<(), Error> {
        raise_open_file_limit();
        let result = self.run_loop();
        self.close_subscriptions();
        self.close_fds();
        *self.ended.lock().unwrap() = true;
        self.ended_cv.notify_all();
        result
    }

    // kqueue.go:256
    fn shutdown(&self) {
        let fd = self.pipe_write_fd.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        // SAFETY: writing one byte from a live buffer to our own pipe.
        unsafe { libc::write(fd, b"X".as_ptr().cast::<libc::c_void>(), 1) };
        let ended = self.ended.lock().unwrap();
        let _ended = self.ended_cv.wait_while(ended, |e| !*e).unwrap();
    }

    // kqueue.go:455
    // subscribe mirrors `kqueueBackend::subscribe`. Called under watcherBase.mu
    // via watchAdd.
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), Error> {
        // Build the entries map without registering any watches or
        // subscriptions, so the event loop never sees a partially built map.
        let mut walked = Vec::new();
        walk_dir(&w.physical_dir, w.recursive, &mut |watch_path, is_dir, ino| {
            walked.push((watch_path.to_string(), is_dir, ino));
            Ok(())
        })?;
        let counts = child_counts(&walked);
        let mut entries = EntryMap::with_capacity(w, walked.len());
        for ((watch_path, is_dir, ino), count) in walked.into_iter().zip(counts) {
            let path = w.display_path(&watch_path);
            entries.insert(DirEntry::new(path, watch_path, is_dir, ino), count);
        }

        // Open fds, register kevents, and publish subscriptions under b.mu.
        let mut st = self.mu.lock().unwrap();
        let map_id = self.next_id();
        let kq = self.kq.load(Ordering::SeqCst);

        for id in 0..entries.nodes.len() as NodeId {
            let Some(node) = entries.nodes[id as usize].as_ref().filter(|n| n.present) else {
                continue;
            };
            let e = &node.entry;
            let is_root = *e.path == w.dir;
            let fd = match open_for_events(e.watch_path()) {
                Ok(fd) => fd,
                Err(err) => {
                    if is_root {
                        st.cleanup_entries_locked(&mut entries);
                        return Err(Error::wrap(
                            format!("error watching {}: {}", w.dir, err),
                            &err,
                        ));
                    }
                    if is_out_of_descriptors(&err) {
                        let err = err_out_of_descriptors(e.watch_path(), &err);
                        st.cleanup_entries_locked(&mut entries);
                        return Err(err);
                    }
                    entries.remove_id(id);
                    continue;
                }
            };
            let token = self.next_id() as usize;
            if let Err(err) = watch_fd(kq, fd, token) {
                close_fd(fd);
                if is_root {
                    st.cleanup_entries_locked(&mut entries);
                    return Err(Error::wrap(format!("error watching {}: {}", w.dir, err), &err));
                }
                entries.remove_id(id);
                continue;
            }
            entries.node_mut(id).entry.state = Some(fd);
            st.fd_to_entry.insert(fd, (map_id, id, token));
        }

        st.insert_map(map_id, entries);
        Ok(())
    }

    // kqueue.go:737
    // closeWatch mirrors `kqueueBackend::closeWatch`.
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), Error> {
        let mut st = self.mu.lock().unwrap();
        // Go finds the watch's maps through its subscriptions: a map left with none stays.
        let maps_of_watch: Vec<EntriesID> = st
            .entry_maps
            .iter()
            .filter(|(_, m)| m.subscribed > 0 && Arc::ptr_eq(m.watch(), w))
            .map(|(&id, _)| id)
            .collect();
        // tsrs-only: each subscribe owns an independent entries map, including overlapping watches. Close its
        // current descriptors when its last subscription leaves; cached subscription fds can be stale after rewatch.
        for map_id in maps_of_watch {
            let entries = st.entry_maps.remove(&map_id).unwrap();
            for (id, node) in entries.nodes.iter().enumerate() {
                let Some(node) = node else {
                    continue;
                };
                let r = SubRef { map: map_id, node: id as NodeId };
                if node.subscribed
                    && let Ok(slot) = st.subs.find_entry(hash_str(&node.entry.path), |x| *x == r)
                {
                    slot.remove();
                }
                if node.present
                    && let Some(fd) = node.entry.state
                {
                    close_fd(fd);
                    st.fd_to_entry.remove(&fd);
                }
            }
        }
        Ok(())
    }
}
