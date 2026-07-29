use crate::clock::Clock;
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
/// (`base_ticks`, `tid`) is written once at allocation; each per-event
/// slot is 8 bytes (`PackedEvent { delta_ticks, code_kind }`). Drain
/// rehydrates nanosecond timestamps + tid into `Event` for the writer.
pub struct EventBatch {
    pub base_ticks: u64,
    pub tid: u32,
    pub events: Vec<PackedEvent>,
}

impl EventBatch {
    #[inline]
    pub fn with_capacity(cap: usize, base_ticks: u64, tid: u32) -> Self {
        Self {
            base_ticks,
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
/// BATCH_N events. Raw ticks go in; the `Clock` converts to
/// nanoseconds on the way out.
pub struct EventQueue {
    clock: Clock,
    consumers: Mutex<Vec<Consumer<Box<EventBatch>>>>,
    dropped: AtomicU64,
    closed: AtomicBool,
    wake_lock: Mutex<()>,
    wake_cv: Condvar,
}

impl EventQueue {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            consumers: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            wake_lock: Mutex::new(()),
            wake_cv: Condvar::new(),
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Push using the caller's `PerThread` context (already borrowed).
    /// Computes the per-batch tick delta inline; force-closes the batch
    /// on rare > u32::MAX-tick gaps so deltas always fit in u32.
    #[inline]
    pub fn push_with_ctx(
        &self,
        ctx: &mut PerThread,
        ticks: u64,
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
            self.init_producer(ctx, ticks, tid);
        }
        let code_kind = pack_code_kind(code_id, kind);
        // Hot path: single mut borrow of batch, push, branch on len.
        let batch = ctx.batch.as_mut().unwrap();
        let delta = ticks.wrapping_sub(batch.base_ticks);
        if delta <= DELTA_OVERFLOW {
            batch.events.push(PackedEvent {
                delta_ticks: delta as u32,
                code_kind,
            });
            if batch.events.len() < BATCH_N {
                return;
            }
        }
        // Cold paths: batch full or delta overflow.
        self.slow_path(ctx, ticks, tid, code_kind, delta > DELTA_OVERFLOW);
    }

    #[cold]
    #[inline(never)]
    fn init_producer(&self, ctx: &mut PerThread, ticks: u64, tid: u32) {
        let (prod, cons) = RingBuffer::<Box<EventBatch>>::new(BATCHES_CAPACITY);
        self.consumers.lock().push(cons);
        ctx.producer = Some((self as *const _ as usize, prod));
        ctx.batch = Some(Box::new(EventBatch::with_capacity(BATCH_N, ticks, tid)));
    }

    #[cold]
    #[inline(never)]
    fn slow_path(
        &self,
        ctx: &mut PerThread,
        ticks: u64,
        tid: u32,
        code_kind: u32,
        was_overflow: bool,
    ) {
        self.ship_and_renew(ctx, ticks, tid);
        if was_overflow {
            // After overflow the previous batch was shipped without
            // recording this event; push it into the fresh batch.
            ctx.batch.as_mut().unwrap().events.push(PackedEvent {
                delta_ticks: 0,
                code_kind,
            });
        }
    }

    #[inline]
    fn ship_and_renew(&self, ctx: &mut PerThread, base_ticks: u64, tid: u32) {
        let full = ctx.batch.take().unwrap();
        let (_, prod) = ctx.producer.as_mut().unwrap();
        let dropped = full.events.len();
        if let Err(rtrb::PushError::Full(returned)) = prod.push(full) {
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
            let mut reused = returned;
            reused.events.clear();
            reused.base_ticks = base_ticks;
            reused.tid = tid;
            ctx.batch = Some(reused);
        } else {
            ctx.batch = Some(Box::new(EventBatch::with_capacity(
                BATCH_N, base_ticks, tid,
            )));
        }
    }

    #[inline]
    fn decode_into(&self, batch: &EventBatch, out: &mut Vec<Event>) {
        let base = batch.base_ticks;
        let tid = batch.tid;
        out.extend(
            batch
                .events
                .iter()
                .map(|p| Event::from_packed(&self.clock, base, tid, *p)),
        );
    }

    /// Non-blocking drain. Reconstructs full `Event`s from packed
    /// per-batch storage. Returns the number of events drained.
    pub fn drain_nonblocking(&self, out: &mut Vec<Event>, limit: usize) -> usize {
        let mut consumers = self.consumers.lock();
        let mut got = 0;
        for c in consumers.iter_mut() {
            while let Ok(batch) = c.pop() {
                got += batch.events.len();
                self.decode_into(&batch, out);
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
                        self.decode_into(&batch, out);
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
                        self.decode_into(&batch, out);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::CTX;

    /// Ticks are nanoseconds on a mock clock, so a test can state the
    /// timestamps it expects directly.
    fn test_clock() -> Clock {
        Clock::mock().0
    }

    /// Push through the real thread-local context, then drain. Runs on
    /// its own thread so each test gets a fresh `CTX`.
    fn push_and_drain(clock: Clock, pushes: Vec<(u64, u32, u32, EventKind)>) -> Vec<Event> {
        std::thread::spawn(move || {
            let q = EventQueue::new(clock);
            CTX.with_borrow_mut(|ctx| {
                for (ticks, tid, code, kind) in pushes {
                    q.push_with_ctx(ctx, ticks, tid, code, kind);
                }
                ctx.flush_partial();
            });
            let mut out = Vec::new();
            q.drain_nonblocking(&mut out, usize::MAX);
            out
        })
        .join()
        .unwrap()
    }

    #[test]
    fn drained_timestamps_preserve_the_gaps_between_events() {
        let out = push_and_drain(
            test_clock(),
            vec![
                (0, 7, 0, EventKind::Begin),
                (1_000, 7, 0, EventKind::End),
                (10_000, 7, 0, EventKind::Begin),
            ],
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].ts_ns, 0);
        assert_eq!(out[1].ts_ns, 1_000);
        assert_eq!(out[2].ts_ns, 10_000);
    }

    #[test]
    fn a_synthetic_second_measures_as_a_second() {
        let out = push_and_drain(
            test_clock(),
            vec![
                (0, 1, 0, EventKind::Begin),
                (1_000_000_000, 1, 0, EventKind::End),
            ],
        );
        assert_eq!(out[1].ts_ns - out[0].ts_ns, 1_000_000_000);
    }

    #[test]
    fn the_clock_anchor_shifts_timestamps_to_trace_relative() {
        // A boot-relative counter that is already far along.
        let (clock, _m) = Clock::mock_starting_at(9_000_000_000);
        let out = push_and_drain(clock, vec![(9_000_001_000, 1, 0, EventKind::Begin)]);
        assert_eq!(out[0].ts_ns, 1_000);
    }

    #[test]
    fn timestamps_are_relative_to_the_first_event_not_the_batch() {
        let out = push_and_drain(
            test_clock(),
            vec![(0, 1, 0, EventKind::Begin), (1_000, 1, 0, EventKind::End)],
        );
        assert_eq!(out[1].ts_ns, 1_000);
    }

    #[test]
    fn events_survive_a_full_batch_boundary() {
        let pushes: Vec<_> = (0..BATCH_N + 5)
            .map(|i| (i as u64 * 1_000, 3, 0, EventKind::Begin))
            .collect();
        let out = push_and_drain(test_clock(), pushes);
        assert_eq!(out.len(), BATCH_N + 5);
        // Timestamps stay correct across the base_ticks re-anchoring.
        assert_eq!(out[BATCH_N + 4].ts_ns, (BATCH_N as u64 + 4) * 1_000);
    }

    #[test]
    fn a_tick_gap_wider_than_u32_still_lands_at_the_right_time() {
        let big = DELTA_OVERFLOW + 1_000;
        let out = push_and_drain(
            test_clock(),
            vec![(0, 1, 0, EventKind::Begin), (big, 1, 0, EventKind::End)],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ts_ns, 0);
        assert_eq!(out[1].ts_ns, big);
    }

    #[test]
    fn code_id_and_kind_come_back_out_of_the_queue() {
        let out = push_and_drain(
            test_clock(),
            vec![(0, 5, 11, EventKind::Begin), (24, 5, 22, EventKind::Yield)],
        );
        assert_eq!((out[0].code_id(), out[0].kind()), (11, EventKind::Begin));
        assert_eq!((out[1].code_id(), out[1].kind()), (22, EventKind::Yield));
        assert_eq!(out[0].tid, 5);
    }

    #[test]
    fn nothing_is_dropped_below_the_ring_capacity() {
        let out = push_and_drain(
            test_clock(),
            (0..BATCH_N * 4)
                .map(|i| (i as u64, 1, 0, EventKind::Begin))
                .collect(),
        );
        assert_eq!(out.len(), BATCH_N * 4);
    }
}
