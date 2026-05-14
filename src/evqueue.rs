use crate::event::Event;
use crate::tls::PerThread;
use parking_lot::{Condvar, Mutex};
use rtrb::{Consumer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub const BATCH_N: usize = 1024;
pub const BATCHES_CAPACITY: usize = 64;

/// Per-thread batched SPSC sharded event queue.
///
/// Each Python thread accumulates events in a thread-local
/// `Box<Vec<Event>>` of capacity BATCH_N. When full the box is handed
/// off via a per-thread `rtrb` ring sized for BATCHES_CAPACITY boxes
/// (≈ 64K events per thread of in-flight buffer). The hot push path
/// is pure thread-local memory writes — one atomic per BATCH_N events
/// instead of one per event.
pub struct EventQueue {
    consumers: Mutex<Vec<Consumer<Box<Vec<Event>>>>>,
    dropped: AtomicU64,
    closed: AtomicBool,
    wake_lock: Mutex<()>,
    wake_cv: Condvar,
}

impl EventQueue {
    pub fn new(_total_capacity: usize) -> Self {
        Self {
            consumers: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            wake_lock: Mutex::new(()),
            wake_cv: Condvar::new(),
        }
    }

    /// Push using the caller's `PerThread` context (already borrowed).
    /// Steady state: write event into thread-local batch (no atomic).
    /// Every BATCH_N pushes: ship the full batch through SPSC and
    /// allocate a fresh one.
    #[inline]
    pub fn push_with_ctx(&self, ctx: &mut PerThread, ev: Event) {
        let q_id = self as *const _ as usize;
        let stale = match &ctx.producer {
            Some((id, _)) => *id != q_id,
            None => true,
        };
        if stale {
            let (prod, cons) = RingBuffer::<Box<Vec<Event>>>::new(BATCHES_CAPACITY);
            self.consumers.lock().push(cons);
            ctx.producer = Some((q_id, prod));
            ctx.batch = Some(Box::new(Vec::with_capacity(BATCH_N)));
        }
        let batch = ctx.batch.as_mut().unwrap();
        batch.push(ev);
        if batch.len() >= BATCH_N {
            let full = ctx.batch.take().unwrap();
            let (_, prod) = ctx.producer.as_mut().unwrap();
            if let Err(rtrb::PushError::Full(returned)) = prod.push(full) {
                self.dropped.fetch_add(returned.len() as u64, Ordering::Relaxed);
                let mut reused = returned;
                reused.clear();
                ctx.batch = Some(reused);
            } else {
                ctx.batch = Some(Box::new(Vec::with_capacity(BATCH_N)));
            }
        }
    }

    /// Non-blocking drain. Pops up to `limit` events from all
    /// registered consumers into `out`. Returns the number drained.
    pub fn drain_nonblocking(&self, out: &mut Vec<Event>, limit: usize) -> usize {
        let mut consumers = self.consumers.lock();
        let mut got = 0;
        for c in consumers.iter_mut() {
            while let Ok(batch) = c.pop() {
                got += batch.len();
                out.extend_from_slice(&batch);
                if got >= limit {
                    return got;
                }
            }
        }
        got
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn drain_blocking(&self, out: &mut Vec<Event>) -> bool {
        loop {
            let mut got = 0;
            {
                let mut consumers = self.consumers.lock();
                'outer: for c in consumers.iter_mut() {
                    while let Ok(batch) = c.pop() {
                        got += batch.len();
                        out.extend_from_slice(&batch);
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
                    while let Ok(batch) = c.pop() {
                        out.extend_from_slice(&batch);
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
