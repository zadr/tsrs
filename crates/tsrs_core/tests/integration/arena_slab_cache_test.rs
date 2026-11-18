use std::time::Duration;

use tsrs_core::arena::{Region, release_cached_slab_pages};
use tsrs_core::memsplit::process_memory;

/// A region whose first chunk is a whole standard slab, which goes to the slab cache when the region is freed.
const SLAB: usize = 1 << 20;
const WORDS: usize = 96 << 10;

fn fill(region: &Region, value: u64) -> &'static [u64] {
    let _scope = region.enter();
    tsrs_core::alloc_slice(&vec![value; WORDS])
}

/// The footprint on macOS, the resident set on Linux (re-read at most every 5 ms).
fn memory() -> usize {
    std::thread::sleep(Duration::from_millis(10));
    process_memory()
}

/// The cached slabs' pages go back to the system (`release_cached_slab_pages`, after each watch cycle) and the slabs
/// are still handed out: a region on a slab taken back keeps what it writes, its pages count as the process's memory
/// again (macOS: a page written before `MADV_FREE_REUSE` stays free for the system to drop), and slabs in use are
/// not touched.
#[test]
fn cached_slabs_give_back_their_pages_and_come_back_whole() {
    if !crate::isolated::in_child(
        "arena_slab_cache_test::cached_slabs_give_back_their_pages_and_come_back_whole",
    ) {
        return;
    }
    let bytes = WORDS * 8;
    let live = Region::new(SLAB);
    let kept = fill(&live, 1);
    let freed = Region::new(SLAB);
    fill(&freed, 2);
    let slab = freed.chunks()[0];
    assert_eq!(slab.1, SLAB);
    drop(freed);

    let before = memory();
    release_cached_slab_pages();
    let after = memory();
    assert!(before.saturating_sub(after) >= bytes, "given back: {before} -> {after}");

    let reused = Region::new(SLAB);
    assert_eq!(reused.chunks()[0], slab, "the cached slab is taken again");
    let data = fill(&reused, 3);
    let taken = memory();
    assert!(taken.saturating_sub(after) >= bytes, "taken again: {after} -> {taken}");

    release_cached_slab_pages();
    assert!(memory() + bytes / 2 > taken, "slabs in use keep their pages");
    assert!(kept.iter().all(|&w| w == 1));
    assert!(data.iter().all(|&w| w == 3));
}
