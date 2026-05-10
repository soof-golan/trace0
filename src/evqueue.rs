use crate::event::Event;
use crossbeam_queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const NOTIFY_EVERY: u64 = 256;
const MIN_CAPACITY: usize = 1024;

pub struct EventQueue {
    queue: ArrayQueue<Event>,
    dropped: AtomicU64,
    push_count: AtomicU64,
    closed: AtomicBool,
    wake_lock: Mutex<()>,
    wake_cv: Condvar,
}

impl EventQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity.max(MIN_CAPACITY)),
            dropped: AtomicU64::new(0),
            push_count: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            wake_lock: Mutex::new(()),
            wake_cv: Condvar::new(),
        }
    }

    /// Lock-free push. Drops the new event if the queue is full.
    /// Safe under no-GIL Python — concurrent producers welcome.
    #[inline]
    pub fn push(&self, ev: Event) {
        if self.queue.push(ev).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let n = self.push_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n % NOTIFY_EVERY == 0 {
            let _g = self.wake_lock.lock();
            self.wake_cv.notify_one();
        }
    }

    pub fn drain_blocking(&self, out: &mut Vec<Event>) -> bool {
        loop {
            let mut got = 0;
            while let Some(ev) = self.queue.pop() {
                out.push(ev);
                got += 1;
                if got >= 4096 {
                    return true;
                }
            }
            if got > 0 {
                return true;
            }
            if self.closed.load(Ordering::Acquire) {
                while let Some(ev) = self.queue.pop() {
                    out.push(ev);
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
