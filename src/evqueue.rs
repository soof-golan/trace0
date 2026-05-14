use crate::event::{Event, EventKind, PackedEvent, pack_code_kind};
use crate::tls::PerThread;
use parking_lot::{Condvar, Mutex};
use rtrb::{Consumer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub const BATCH_N: usize = 1024;
pub const BATCHES_CAPACITY: usize = 64;

const DELTA_OVERFLOW: u64 = u32::MAX as u64;

/// A contiguous run of events from a single thread. The batch header
/// (`base_ts`, `tid`) is written once at allocation; each per-event
/// slot is 8 bytes (`PackedEvent { delta_ns, code_kind }`). Drain
/// rehydrates absolute timestamps + tid into `Event` for the writer.
pub struct EventBatch {
    pub base_ts: u64,
    pub tid: u32,
    pub events: Vec<PackedEvent>,
}

impl EventBatch {
    #[inline]
    pub fn with_capacity(cap: usize, base_ts: u64, tid: u32) -> Self {
        Self {
            base_ts,
            tid,
            events: Vec::with_capacity(cap),
        }
    }
}

/// Per-thread batched SPSC sharded event queue.
///
/// Each Python thread accumulates events in a thread-local
/// `Box<EventBatch>` of cap BATCH_N. When full the box is handed off
/// via a per-thread `rtrb` ring sized for BATCHES_CAPACITY boxes
/// (≈ 64K events per thread of in-flight buffer). The hot push path
/// writes only 8 bytes per event and bumps a length — one atomic per
/// BATCH_N events.
pub struct EventQueue {
    consumers: Mutex<Vec<Consumer<Box<EventBatch>>>>,
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
    /// Computes the per-batch ts delta inline; force-closes the batch
    /// on rare > u32::MAX-ns gaps so deltas always fit in u32.
    #[inline]
    pub fn push_with_ctx(
        &self,
        ctx: &mut PerThread,
        ts_ns: u64,
        tid: u32,
        code_id: u32,
        kind: EventKind,
    ) {
        let q_id = self as *const _ as usize;
        let stale = match &ctx.producer {
            Some((id, _)) => *id != q_id,
            None => true,
        };
        if stale {
            self.init_producer(ctx, ts_ns, tid);
        }
        let code_kind = pack_code_kind(code_id, kind);
        // Hot path: single mut borrow of batch, push, branch on len.
        let batch = ctx.batch.as_mut().unwrap();
        let delta = ts_ns.wrapping_sub(batch.base_ts);
        if delta <= DELTA_OVERFLOW {
            batch.events.push(PackedEvent {
                delta_ns: delta as u32,
                code_kind,
            });
            if batch.events.len() < BATCH_N {
                return;
            }
        }
        // Cold paths: batch full or delta overflow.
        self.slow_path(ctx, ts_ns, tid, code_kind, delta > DELTA_OVERFLOW);
    }

    #[cold]
    #[inline(never)]
    fn init_producer(&self, ctx: &mut PerThread, ts_ns: u64, tid: u32) {
        let (prod, cons) = RingBuffer::<Box<EventBatch>>::new(BATCHES_CAPACITY);
        self.consumers.lock().push(cons);
        ctx.producer = Some((self as *const _ as usize, prod));
        ctx.batch = Some(Box::new(EventBatch::with_capacity(BATCH_N, ts_ns, tid)));
    }

    #[cold]
    #[inline(never)]
    fn slow_path(
        &self,
        ctx: &mut PerThread,
        ts_ns: u64,
        tid: u32,
        code_kind: u32,
        was_overflow: bool,
    ) {
        self.ship_and_renew(ctx, ts_ns, tid);
        if was_overflow {
            // After overflow the previous batch was shipped without
            // recording this event; push it into the fresh batch.
            ctx.batch.as_mut().unwrap().events.push(PackedEvent {
                delta_ns: 0,
                code_kind,
            });
        }
    }

    #[inline]
    fn ship_and_renew(&self, ctx: &mut PerThread, base_ts: u64, tid: u32) {
        let full = ctx.batch.take().unwrap();
        let (_, prod) = ctx.producer.as_mut().unwrap();
        let dropped = full.events.len();
        if let Err(rtrb::PushError::Full(returned)) = prod.push(full) {
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
            let mut reused = returned;
            reused.events.clear();
            reused.base_ts = base_ts;
            reused.tid = tid;
            ctx.batch = Some(reused);
        } else {
            ctx.batch = Some(Box::new(EventBatch::with_capacity(BATCH_N, base_ts, tid)));
        }
    }

    /// Non-blocking drain. Reconstructs full `Event`s from packed
    /// per-batch storage. Returns the number of events drained.
    pub fn drain_nonblocking(&self, out: &mut Vec<Event>, limit: usize) -> usize {
        let mut consumers = self.consumers.lock();
        let mut got = 0;
        for c in consumers.iter_mut() {
            while let Ok(batch) = c.pop() {
                got += batch.events.len();
                let base = batch.base_ts;
                let tid = batch.tid;
                out.extend(
                    batch
                        .events
                        .iter()
                        .map(|p| Event::from_packed(base, tid, *p)),
                );
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
                        got += batch.events.len();
                        let base = batch.base_ts;
                        let tid = batch.tid;
                        out.extend(
                            batch
                                .events
                                .iter()
                                .map(|p| Event::from_packed(base, tid, *p)),
                        );
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
                        let base = batch.base_ts;
                        let tid = batch.tid;
                        out.extend(
                            batch
                                .events
                                .iter()
                                .map(|p| Event::from_packed(base, tid, *p)),
                        );
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
