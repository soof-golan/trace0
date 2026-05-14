use crate::event::Event;
use crate::tls::PerThread;
use parking_lot::{Condvar, Mutex};
use rtrb::{Consumer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const MIN_PER_THREAD_CAPACITY: usize = 1024;

/// Per-thread SPSC sharded event queue.
///
/// Each Python thread lazily creates its own rtrb ring on first event;
/// the matching Consumer is registered with the shared `consumers`
/// vector that the exporter drains. The hot push path is pure
/// thread-local work — no shared atomics, no producer contention.
/// The exporter polls every 20ms and wakes immediately on `close()`.
pub struct EventQueue {
    consumers: Mutex<Vec<Consumer<Event>>>,
    per_thread_capacity: usize,
    dropped: AtomicU64,
    closed: AtomicBool,
    wake_lock: Mutex<()>,
    wake_cv: Condvar,
}

impl EventQueue {
    pub fn new(total_capacity: usize) -> Self {
        let per_thread = total_capacity.max(MIN_PER_THREAD_CAPACITY);
        Self {
            consumers: Mutex::new(Vec::new()),
            per_thread_capacity: per_thread,
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            wake_lock: Mutex::new(()),
            wake_cv: Condvar::new(),
        }
    }

    /// Push using the caller's `PerThread` context (already borrowed).
    /// Avoids a second TLS lookup on the hot path.
    #[inline]
    pub fn push_with_ctx(&self, ctx: &mut PerThread, ev: Event) {
        let q_id = self as *const _ as usize;
        let stale = match &ctx.producer {
            Some((id, _)) => *id != q_id,
            None => true,
        };
        if stale {
            let (prod, cons) = RingBuffer::<Event>::new(self.per_thread_capacity);
            self.consumers.lock().push(cons);
            ctx.producer = Some((q_id, prod));
        }
        let (_, prod) = ctx.producer.as_mut().unwrap();
        if prod.push(ev).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn drain_blocking(&self, out: &mut Vec<Event>) -> bool {
        loop {
            let mut got = 0;
            {
                let mut consumers = self.consumers.lock();
                'outer: for c in consumers.iter_mut() {
                    while let Ok(ev) = c.pop() {
                        out.push(ev);
                        got += 1;
                        if got >= 4096 {
                            break 'outer;
                        }
                    }
                }
            }
            if got > 0 {
                return true;
            }
            if self.closed.load(Ordering::Acquire) {
                let mut consumers = self.consumers.lock();
                for c in consumers.iter_mut() {
                    while let Ok(ev) = c.pop() {
                        out.push(ev);
                    }
                }
                return !out.is_empty();
            }
            let mut g = self.wake_lock.lock();
            self.wake_cv.wait_for(&mut g, Duration::from_millis(20));
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _g = self.wake_lock.lock();
        self.wake_cv.notify_all();
    }
}
