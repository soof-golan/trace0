use crate::codecache::CodeCache;
use crate::event::PackedEvent;
use crate::evqueue::{BATCH_N, EventBatch};
use rtrb::Producer;
use std::cell::{RefCell, UnsafeCell};

pub const NOT_CACHED: usize = usize::MAX;

#[derive(Clone, Copy)]
pub struct Hot {
    pub cursor: *mut PackedEvent,
    pub end: *mut PackedEvent,
    pub base_ticks: u64,
    pub queue_id: u64,
    pub clock_direct: bool,
    pub last_code_key: usize,
    pub last_code_id: u32,
    pub tid: u32,
    pub ensured: bool,
}

const _: () = assert!(!std::mem::needs_drop::<Hot>());

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

pub struct Cold {
    pub producer: Option<(u64, Producer<Box<EventBatch>>)>,
    pub batch: Option<Box<EventBatch>>,
}

impl Cold {
    pub fn commit(&mut self, hot: &Hot) {
        let Some(batch) = self.batch.as_mut() else {
            return;
        };
        let start = batch.events.as_mut_ptr();
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

    pub fn arm(&mut self, hot: &mut Hot) {
        let Some(batch) = self.batch.as_mut() else {
            hot.cursor = std::ptr::null_mut();
            hot.end = std::ptr::null_mut();
            return;
        };
        hot.base_ticks = batch.base_ticks;
        let start = batch.events.as_mut_ptr();
        hot.cursor = start;
        hot.end = unsafe { start.add(BATCH_N) };
    }

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

#[inline(always)]
#[allow(clippy::mut_from_ref)]
pub fn hot() -> &'static mut Hot {
    let ptr = HOT.with(|cell| cell.get());
    unsafe { &mut *ptr }
}

#[inline(always)]
#[allow(clippy::mut_from_ref)]
pub fn codes() -> &'static mut CodeCache {
    let ptr = CODES.with(|cell| cell.get());
    unsafe { &mut *ptr }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_untouched_thread_holds_no_recording_state() {
        let h = Hot::EMPTY;
        assert!(h.cursor.is_null());
        assert!(
            h.cursor >= h.end,
            "the empty cursor must fail the push guard"
        );
        assert_eq!(h.last_code_key, NOT_CACHED);
        assert_eq!(h.tid, u32::MAX);
        assert_eq!(h.queue_id, 0);
    }

    #[test]
    fn flush_partial_leaves_a_writable_cursor() {
        on_a_fresh_thread(|| {
            let hot = hot();
            let mut cold = armed(hot);
            walk(hot, 3);
            cold.flush_partial(hot);
            assert!(!hot.cursor.is_null());
            assert!(
                hot.cursor < hot.end,
                "a straggler event would write through a dead cursor"
            );
        });
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

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "left the batch")]
    fn a_cursor_past_the_end_of_the_batch_is_caught() {
        on_a_fresh_thread(|| {
            let hot = hot();
            let mut cold = std::mem::ManuallyDrop::new(armed(hot));
            walk(hot, BATCH_N);
            hot.cursor = hot.cursor.wrapping_add(1);
            cold.commit(hot);
        });
    }

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
