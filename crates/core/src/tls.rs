use crate::codecache::CodeCache;
use crate::event::PackedEvent;
use crate::evqueue::{BATCH_N, EventBatch};
use rtrb::{Producer, PushError};
use std::cell::{RefCell, UnsafeCell};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering, fence};

pub const NOT_CACHED: usize = usize::MAX;

pub struct Hot {
    pub cursor: AtomicPtr<PackedEvent>,
    pub end: AtomicPtr<PackedEvent>,
    pub base_ticks: AtomicU64,
    pub queue_id: AtomicU64,
    pub epoch: AtomicU64,
    pub code_gen: u64,
    pub clock_direct: AtomicBool,
    pub last_code_key: usize,
    pub last_code_id: u32,
    pub tid: AtomicU32,
    pub ensured: bool,
    pub name_retries: u32,
}

const _: () = assert!(!std::mem::needs_drop::<Hot>());

impl Hot {
    pub const fn empty() -> Self {
        Self {
            cursor: AtomicPtr::new(std::ptr::null_mut()),
            end: AtomicPtr::new(std::ptr::null_mut()),
            base_ticks: AtomicU64::new(0),
            queue_id: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            code_gen: 0,
            clock_direct: AtomicBool::new(false),
            last_code_key: NOT_CACHED,
            last_code_id: u32::MAX,
            tid: AtomicU32::new(u32::MAX),
            ensured: false,
            name_retries: 0,
        }
    }
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
        let cursor = hot.cursor.load(Ordering::Relaxed);
        let end = hot.end.load(Ordering::Relaxed);
        debug_assert!(
            end == unsafe { start.add(batch.events.capacity()) },
            "commit: hot is armed for a different batch than this one"
        );
        debug_assert!(
            cursor >= start && cursor <= end,
            "commit: cursor has left the batch it is measured against"
        );
        let len = unsafe { cursor.offset_from(start) } as usize;
        unsafe { batch.events.set_len(len) };
    }

    pub fn arm(&mut self, hot: &Hot) {
        let epoch = hot.epoch.load(Ordering::Relaxed);
        hot.epoch.store(epoch.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        match self.batch.as_mut() {
            None => {
                hot.cursor.store(std::ptr::null_mut(), Ordering::Relaxed);
                hot.end.store(std::ptr::null_mut(), Ordering::Relaxed);
            }
            Some(batch) => {
                hot.base_ticks.store(batch.base_ticks, Ordering::Relaxed);
                hot.tid.store(batch.tid, Ordering::Relaxed);
                let start = batch.events.as_mut_ptr();
                hot.cursor.store(start, Ordering::Relaxed);
                hot.end
                    .store(unsafe { start.add(BATCH_N) }, Ordering::Relaxed);
            }
        }
        hot.epoch.store(epoch.wrapping_add(2), Ordering::Release);
    }

    #[must_use]
    pub fn flush_partial(&mut self, hot: &Hot) -> u64 {
        self.commit(hot);
        let Some(batch) = self.batch.take() else {
            return 0;
        };
        if self.producer.is_none() || batch.events.is_empty() {
            self.batch = Some(batch);
            self.arm(hot);
            return 0;
        }
        self.batch = Some(Box::new(EventBatch::with_capacity(
            BATCH_N,
            batch.base_ticks,
            batch.tid,
        )));
        self.arm(hot);
        let (_, prod) = self.producer.as_mut().unwrap();
        match prod.push(batch) {
            Ok(()) => 0,
            Err(PushError::Full(lost)) => lost.events.len() as u64,
        }
    }
}

impl Drop for Cold {
    fn drop(&mut self) {
        forget(self as *mut Cold);
        let _no_queue_left_to_charge = self.flush_partial(hot());
    }
}

struct Recorder {
    cold: *mut Cold,
    hot: *mut Hot,
}

unsafe impl Send for Recorder {}

static RECORDERS: parking_lot::Mutex<Vec<Recorder>> = parking_lot::Mutex::new(Vec::new());

pub fn register_current() {
    let cold = COLD.with(|c| c.as_ptr());
    let hot: *mut Hot = hot();
    let mut recorders = RECORDERS.lock();
    if !recorders.iter().any(|r| r.cold == cold) {
        recorders.push(Recorder { cold, hot });
    }
}

fn forget(cold: *mut Cold) {
    RECORDERS.lock().retain(|r| r.cold != cold);
}

pub fn forget_other_threads() {
    let cold = COLD.with(|c| c.as_ptr());
    RECORDERS.lock().retain(|r| r.cold == cold);
}

const TAIL_RETRIES: usize = 8;

pub fn read_tails(queue_id: u64) -> Vec<EventBatch> {
    let recorders = RECORDERS.lock();
    recorders
        .iter()
        .filter_map(|r| unsafe { read_tail(&*r.hot, queue_id) })
        .collect()
}

unsafe fn read_tail(hot: &Hot, queue_id: u64) -> Option<EventBatch> {
    for _ in 0..TAIL_RETRIES {
        let epoch = hot.epoch.load(Ordering::Acquire);
        if epoch & 1 == 1 {
            std::hint::spin_loop();
            continue;
        }
        if hot.queue_id.load(Ordering::Relaxed) != queue_id {
            return None;
        }
        let cursor = hot.cursor.load(Ordering::Acquire);
        let end = hot.end.load(Ordering::Relaxed);
        if cursor.is_null() || end.is_null() {
            return None;
        }
        let start = unsafe { end.sub(BATCH_N) };
        let len = unsafe { cursor.offset_from(start) };
        if len <= 0 || len as usize > BATCH_N {
            continue;
        }
        let base_ticks = hot.base_ticks.load(Ordering::Relaxed);
        let tid = hot.tid.load(Ordering::Relaxed);
        let mut events = Vec::with_capacity(len as usize);
        for i in 0..len as usize {
            let slot = unsafe { &*start.add(i).cast::<std::sync::atomic::AtomicU64>() };
            events.push(PackedEvent::from_bits(slot.load(Ordering::Relaxed)));
        }
        fence(Ordering::Acquire);
        if hot.epoch.load(Ordering::Relaxed) != epoch {
            continue;
        }
        return Some(EventBatch {
            base_ticks,
            tid,
            events,
        });
    }
    None
}

pub fn flush_every_thread() -> u64 {
    let recorders = RECORDERS.lock();
    let mut lost = 0;
    for r in recorders.iter() {
        lost += unsafe { (*r.cold).flush_partial(&*r.hot) };
    }
    lost
}

thread_local! {
    static HOT: UnsafeCell<Hot> = const { UnsafeCell::new(Hot::empty()) };
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

    fn walk(hot: &Hot, n: usize) {
        for i in 0..n {
            let cursor = hot.cursor.load(Ordering::Relaxed);
            unsafe {
                (*cursor.cast::<AtomicU64>()).store(
                    PackedEvent {
                        delta_ticks: i as u32,
                        code_kind: 0,
                    }
                    .to_bits(),
                    Ordering::Relaxed,
                );
            }
            hot.cursor
                .store(unsafe { cursor.add(1) }, Ordering::Release);
        }
    }

    #[test]
    fn an_untouched_thread_holds_no_recording_state() {
        let h = Hot::empty();
        let cursor = h.cursor.load(Ordering::Relaxed);
        assert!(cursor.is_null());
        assert!(
            cursor >= h.end.load(Ordering::Relaxed),
            "the empty cursor must fail the push guard"
        );
        assert_eq!(h.last_code_key, NOT_CACHED);
        assert_eq!(h.tid.load(Ordering::Relaxed), u32::MAX);
        assert_eq!(h.queue_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn flush_partial_leaves_a_writable_cursor() {
        on_a_fresh_thread(|| {
            let hot = hot();
            let mut cold = armed(hot);
            walk(hot, 3);
            assert_eq!(
                cold.flush_partial(hot),
                0,
                "a thread with no producer has nowhere to lose events"
            );
            let cursor = hot.cursor.load(Ordering::Relaxed);
            assert!(!cursor.is_null());
            assert!(
                cursor < hot.end.load(Ordering::Relaxed),
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
            assert_eq!(
                hot.cursor.load(Ordering::Relaxed),
                hot.end.load(Ordering::Relaxed)
            );
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
            hot.cursor.store(
                hot.cursor.load(Ordering::Relaxed).wrapping_add(1),
                Ordering::Relaxed,
            );
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
