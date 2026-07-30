//! Per-thread recording state, split by how often it is touched.
//!
//! The split exists for one reason: a `thread_local!` whose type needs
//! dropping cannot be a plain TLS slot. Rust gives it lazy initialisation
//! and destructor registration, and `LocalKey::with` stops inlining --
//! every event then pays an out-of-line call plus spilling the closure's
//! captures to the stack.
//!
//! So [`Hot`] holds only `Copy` fields and owns nothing. Its
//! `thread_local!` needs no destructor, and access compiles to a direct
//! TLS address computation. [`Cold`] owns the batch and the ring
//! producer, keeps the destructor, and is reached only when a batch fills
//! or a thread records its first event.

use crate::codecache::CodeCache;
use crate::event::PackedEvent;
use crate::evqueue::{BATCH_N, EventBatch};
use rtrb::Producer;
use std::cell::{RefCell, UnsafeCell};

/// State on the per-event path. Every field is `Copy`, deliberately.
///
/// Events are written straight through `cursor`, rather than reached via
/// the batch, so a push touches this struct and the destination bytes --
/// not this struct, then a `Box<EventBatch>`, then a `Vec` header, then
/// the buffer.
///
/// A null cursor doubles as the uninitialised state, so "no batch yet"
/// and "batch full" collapse into one `cursor < end` test.
/// No real code object can live at this address, so it doubles as "the
/// fast path is not armed yet".
pub const NOT_CACHED: usize = usize::MAX;

#[derive(Clone, Copy)]
pub struct Hot {
    /// Next free slot, or null before the first batch is installed.
    pub cursor: *mut PackedEvent,
    /// One past the last writable slot.
    pub end: *mut PackedEvent,
    /// Anchor the current batch's deltas are measured from.
    pub base_ticks: u64,
    /// Which queue `cursor` points into. A tracer that is stopped and
    /// started again leaves this thread holding a cursor into the old
    /// run's batch; without this the events would be written and then
    /// shipped to a ring the new run never drains.
    pub queue_id: u64,
    /// Whether the counter can be read inline. Set from the queue's own
    /// clock, so it stays a decision that clock made -- not a second
    /// opinion about which counter to read.
    pub clock_direct: bool,
    /// Last code object resolved on this thread, or [`NOT_CACHED`].
    /// Armed only once the thread is fully settled -- tid resolved and
    /// name registered -- so one compare against it covers all three.
    pub last_code_key: usize,
    pub last_code_id: u32,
    /// OS thread id, resolved once. `u32::MAX` means not yet resolved.
    /// Asking libc for it on every event costs more than the push does.
    pub tid: u32,
    pub ensured: bool,
}

impl Hot {
    pub const EMPTY: Self = Self {
        cursor: std::ptr::null_mut(),
        end: std::ptr::null_mut(),
        base_ticks: 0,
        queue_id: 0,
        clock_direct: false,
        last_code_key: NOT_CACHED,
        last_code_id: u32::MAX,
        tid: u32::MAX,
        ensured: false,
    };
}

/// State reached only when a batch fills, or on a thread's first event.
/// Owns heap memory and the ring producer, which is why it lives apart
/// from [`Hot`].
pub struct Cold {
    pub producer: Option<(u64, Producer<Box<EventBatch>>)>,
    pub batch: Option<Box<EventBatch>>,
}

impl Cold {
    /// Publish the cursor's progress as the batch's length. Nothing may
    /// read the batch until this has run.
    pub fn commit(&mut self, hot: &Hot) {
        let Some(batch) = self.batch.as_mut() else {
            return;
        };
        let start = batch.events.as_mut_ptr();
        // SAFETY: `cursor` addresses this batch's buffer, between `start`
        // and one past its last slot. Every assignment to `self.batch` is
        // followed by [`Cold::arm`], which repoints both `cursor` and
        // `end`, and the push path advances `cursor` only while it is
        // below `end`.
        debug_assert!(
            hot.end == unsafe { start.add(batch.events.capacity()) },
            "commit: hot is armed for a different batch than this one"
        );
        debug_assert!(
            hot.cursor >= start && hot.cursor <= hot.end,
            "commit: cursor has left the batch it is measured against"
        );
        let len = unsafe { hot.cursor.offset_from(start) } as usize;
        unsafe { batch.events.set_len(len) };
    }

    /// Point the cursor at the current batch's buffer.
    pub fn arm(&mut self, hot: &mut Hot) {
        let Some(batch) = self.batch.as_mut() else {
            hot.cursor = std::ptr::null_mut();
            hot.end = std::ptr::null_mut();
            return;
        };
        hot.base_ticks = batch.base_ticks;
        let start = batch.events.as_mut_ptr();
        hot.cursor = start;
        // SAFETY: the buffer is allocated with exactly BATCH_N capacity.
        hot.end = unsafe { start.add(BATCH_N) };
    }

    /// Push the in-flight partial batch into the SPSC ring so the
    /// consumer sees events accumulated since the last full-batch
    /// boundary. Best-effort: if the ring is full, the partial is
    /// dropped along with everything else.
    pub fn flush_partial(&mut self, hot: &mut Hot) {
        self.commit(hot);
        let Some(batch) = self.batch.take() else {
            return;
        };
        if self.producer.is_none() || batch.events.is_empty() {
            self.batch = Some(batch);
            self.arm(hot);
            return;
        }
        // Leave a fresh writable batch in place. PEP 669 does not
        // guarantee zero callbacks after `set_events(0)` returns; a
        // straggler PY_START on this thread would otherwise write through
        // a stale cursor.
        self.batch = Some(Box::new(EventBatch::with_capacity(
            BATCH_N,
            batch.base_ticks,
            batch.tid,
        )));
        self.arm(hot);
        let (_, prod) = self.producer.as_mut().unwrap();
        let _ = prod.push(batch);
    }
}

impl Drop for Cold {
    fn drop(&mut self) {
        // `Hot` has no destructor, so its slot is still readable while
        // this one is being torn down.
        self.flush_partial(hot());
    }
}

thread_local! {
    static HOT: UnsafeCell<Hot> = const { UnsafeCell::new(Hot::EMPTY) };
    static CODES: UnsafeCell<CodeCache> = const { UnsafeCell::new(CodeCache::EMPTY) };
    pub static COLD: RefCell<Cold> = const {
        RefCell::new(Cold {
            producer: None,
            batch: None,
        })
    };
}

/// This thread's hot state.
///
/// Returns a pointer rather than taking a closure on purpose. macOS
/// resolves thread-locals through an opaque `tlv_get_addr` call that
/// never inlines, and a closure taken across it has its captures spilled
/// to the stack -- seven stores per event, measured. A nullary closure
/// returning a pointer leaves nothing to spill.
///
/// # Safety
///
/// [`Hot`] has no destructor, so the slot is valid for the whole life of
/// the thread and the reference may outlive the lookup. Callers must not
/// run Python while it is live: a `sys.monitoring` callback would
/// re-enter and alias it. Every path that touches the interpreter goes
/// through [`Cold`] instead.
#[inline(always)]
#[allow(clippy::mut_from_ref)]
pub fn hot() -> &'static mut Hot {
    let ptr = HOT.with(|cell| cell.get());
    unsafe { &mut *ptr }
}

/// This thread's code-object cache, consulted when [`Hot::last_code_key`]
/// misses.
///
/// A separate slot rather than a field of [`Hot`]: the table is 4 KiB, and
/// folding it in would put a second cache line on the path of every event,
/// including the ones that hit the single cached entry and never read the
/// table at all.
///
/// # Safety
///
/// As [`hot`]: no destructor, so the slot outlives any reference to it,
/// and no caller may run Python while the reference is live.
#[inline(always)]
#[allow(clippy::mut_from_ref)]
pub fn codes() -> &'static mut CodeCache {
    let ptr = CODES.with(|cell| cell.get());
    unsafe { &mut *ptr }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`Hot`] and [`Cold`] are thread-locals, and `Cold::drop` reads the
    /// `Hot` slot as it tears down, so each case needs a thread of its
    /// own. Panics are re-raised with their original payload so
    /// `should_panic` can match on the message.
    fn on_a_fresh_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        match std::thread::spawn(f).join() {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn armed(hot: &mut Hot) -> Cold {
        let mut cold = Cold {
            producer: None,
            batch: Some(Box::new(EventBatch::with_capacity(BATCH_N, 0, 1))),
        };
        cold.arm(hot);
        cold
    }

    /// Walk the cursor the way the push path does: straight through it,
    /// with nothing else updated.
    fn walk(hot: &mut Hot, n: usize) {
        for i in 0..n {
            unsafe {
                hot.cursor.write(PackedEvent {
                    delta_ticks: i as u32,
                    code_kind: 0,
                });
                hot.cursor = hot.cursor.add(1);
            }
        }
    }

    #[test]
    fn commit_publishes_the_events_the_cursor_walked_past() {
        let events = on_a_fresh_thread(|| {
            let hot = hot();
            let mut cold = armed(hot);
            walk(hot, 3);
            cold.commit(hot);
            cold.batch.as_ref().unwrap().events.clone()
        });
        let deltas: Vec<u32> = events.iter().map(|e| e.delta_ticks).collect();
        assert_eq!(deltas, [0, 1, 2]);
    }

    /// A batch filled to the last slot leaves `cursor == end`, one past
    /// the final event. That is in range, and all of it is committed.
    #[test]
    fn a_full_batch_commits_every_slot() {
        let len = on_a_fresh_thread(|| {
            let hot = hot();
            let mut cold = armed(hot);
            walk(hot, BATCH_N);
            assert_eq!(hot.cursor, hot.end);
            cold.commit(hot);
            cold.batch.as_ref().unwrap().events.len()
        });
        assert_eq!(len, BATCH_N);
    }

    /// One slot further is out of range, and committing it would publish
    /// a length past the end of the allocation.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "left the batch")]
    fn a_cursor_past_the_end_of_the_batch_is_caught() {
        on_a_fresh_thread(|| {
            let hot = hot();
            // Leaked rather than dropped: `Cold::drop` commits again, and
            // a second panic during this one's unwind aborts the process.
            let mut cold = std::mem::ManuallyDrop::new(armed(hot));
            walk(hot, BATCH_N);
            hot.cursor = hot.cursor.wrapping_add(1);
            cold.commit(hot);
        });
    }

    /// Every assignment to `batch` in this module is paired with `arm`.
    /// Unpaired, the length would be measured from an allocation the
    /// cursor never walked.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "different batch")]
    fn committing_against_a_batch_the_cursor_never_walked_is_caught() {
        on_a_fresh_thread(|| {
            let hot = hot();
            let mut cold = std::mem::ManuallyDrop::new(armed(hot));
            walk(hot, 3);
            cold.batch = Some(Box::new(EventBatch::with_capacity(BATCH_N, 0, 1)));
            cold.commit(hot);
        });
    }
}
