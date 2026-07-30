use crate::clock::Clock;
use crate::event::{Event, EventKind, PackedEvent, pack_code_kind};
use crate::tls::{COLD, Cold, Hot};
use parking_lot::{Condvar, Mutex};
use rtrb::{Consumer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub const BATCH_N: usize = 1024;
pub const BATCHES_CAPACITY: usize = 64;

const DELTA_OVERFLOW: u64 = u32::MAX as u64;

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

const CACHE_LINE: usize = 64;

#[repr(align(64))]
struct Shared {
    consumers: Mutex<Vec<Consumer<Box<EventBatch>>>>,
    dropped: AtomicU64,
    closed: AtomicBool,
    wake_lock: Mutex<()>,
    wake_cv: Condvar,
}

pub struct EventQueue {
    id: u64,
    clock: Clock,
    shared: Shared,
}

const _: () = assert!(std::mem::offset_of!(EventQueue, shared) % CACHE_LINE == 0);
const _: () = assert!(std::mem::size_of::<Shared>().is_multiple_of(CACHE_LINE));

static NEXT_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

impl EventQueue {
    pub fn new(clock: Clock) -> Self {
        Self {
            id: NEXT_QUEUE_ID.fetch_add(1, Ordering::Relaxed),
            clock,
            shared: Shared {
                consumers: Mutex::new(Vec::new()),
                dropped: AtomicU64::new(0),
                closed: AtomicBool::new(false),
                wake_lock: Mutex::new(()),
                wake_cv: Condvar::new(),
            },
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[inline]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[inline]
    pub fn push_with_ctx(
        &self,
        hot: &mut Hot,
        run: u64,
        ticks: u64,
        tid: u32,
        code_id: u32,
        kind: EventKind,
    ) {
        let code_kind = pack_code_kind(code_id, kind);
        let delta = ticks.wrapping_sub(hot.base_ticks);
        if hot.cursor < hot.end && hot.queue_id == run && delta <= DELTA_OVERFLOW {
            unsafe {
                hot.cursor.write(PackedEvent {
                    delta_ticks: delta as u32,
                    code_kind,
                });
                hot.cursor = hot.cursor.add(1);
            }
            return;
        }
        self.slow_path(hot, ticks, tid, code_kind);
    }

    #[cold]
    #[inline(never)]
    fn init_producer(&self, cold: &mut Cold, hot: &mut Hot, ticks: u64, tid: u32) {
        let (prod, cons) = RingBuffer::<Box<EventBatch>>::new(BATCHES_CAPACITY);
        self.shared.consumers.lock().push(cons);
        cold.producer = Some((self.id, prod));
        cold.batch = Some(Box::new(EventBatch::with_capacity(BATCH_N, ticks, tid)));
        hot.queue_id = self.id;
        hot.clock_direct = self.clock.is_direct();
        cold.arm(hot);
    }

    #[cold]
    #[inline(never)]
    fn slow_path(&self, hot: &mut Hot, ticks: u64, tid: u32, code_kind: u32) {
        COLD.with(|cold| {
            let cold = &mut *cold.borrow_mut();
            let stale = match &cold.producer {
                Some((id, _)) => *id != self.id,
                None => true,
            };
            if stale {
                self.init_producer(cold, hot, ticks, tid);
            } else {
                self.ship_and_renew(cold, hot, ticks, tid);
            }
        });
        unsafe {
            hot.cursor.write(PackedEvent {
                delta_ticks: 0,
                code_kind,
            });
            hot.cursor = hot.cursor.add(1);
        }
    }

    #[inline]
    fn ship_and_renew(&self, cold: &mut Cold, hot: &mut Hot, base_ticks: u64, tid: u32) {
        cold.commit(hot);
        let full = cold.batch.take().unwrap();
        let (_, prod) = cold.producer.as_mut().unwrap();
        let dropped = full.events.len();
        if let Err(rtrb::PushError::Full(returned)) = prod.push(full) {
            self.shared
                .dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
            let mut reused = returned;
            reused.events.clear();
            reused.base_ticks = base_ticks;
            reused.tid = tid;
            cold.batch = Some(reused);
        } else {
            cold.batch = Some(Box::new(EventBatch::with_capacity(
                BATCH_N, base_ticks, tid,
            )));
        }
        cold.arm(hot);
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

    pub fn drain_nonblocking(&self, out: &mut Vec<Event>, limit: usize) -> usize {
        let mut consumers = self.shared.consumers.lock();
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
        self.shared.closed.load(Ordering::Acquire)
    }

    pub fn drain_blocking(&self, out: &mut Vec<Event>) -> bool {
        loop {
            let mut got = 0;
            {
                let mut consumers = self.shared.consumers.lock();
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
            if self.shared.closed.load(Ordering::Acquire) {
                let mut consumers = self.shared.consumers.lock();
                for c in consumers.iter_mut() {
                    while let Ok(batch) = c.pop() {
                        self.decode_into(&batch, out);
                    }
                }
                return !out.is_empty();
            }
            let mut g = self.shared.wake_lock.lock();
            self.shared
                .wake_cv
                .wait_for(&mut g, Duration::from_millis(20));
        }
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    pub fn record_dropped(&self, events: u64) {
        self.shared.dropped.fetch_add(events, Ordering::Relaxed);
    }

    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        let _g = self.shared.wake_lock.lock();
        self.shared.wake_cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::hot;

    fn test_clock() -> Clock {
        Clock::mock().0
    }

    fn push_and_drain(clock: Clock, pushes: Vec<(u64, u32, u32, EventKind)>) -> Vec<Event> {
        std::thread::spawn(move || {
            let q = EventQueue::new(clock);
            let hot = hot();
            for (ticks, tid, code, kind) in pushes {
                q.push_with_ctx(hot, q.id(), ticks, tid, code, kind);
            }
            COLD.with_borrow_mut(|cold| cold.flush_partial(hot));
            let mut out = Vec::new();
            q.drain_nonblocking(&mut out, usize::MAX);
            out
        })
        .join()
        .unwrap()
    }

    #[test]
    fn a_second_queue_on_the_same_thread_records_its_own_events() {
        let out = std::thread::spawn(|| {
            let hot = hot();
            let first = EventQueue::new(test_clock());
            first.push_with_ctx(hot, first.id(), 10, 1, 7, EventKind::Begin);
            COLD.with_borrow_mut(|cold| cold.flush_partial(hot));
            let mut discard = Vec::new();
            first.drain_nonblocking(&mut discard, usize::MAX);
            first.close();

            let second = EventQueue::new(test_clock());
            second.push_with_ctx(hot, second.id(), 20, 1, 7, EventKind::End);
            COLD.with_borrow_mut(|cold| cold.flush_partial(hot));
            let mut out = Vec::new();
            second.drain_nonblocking(&mut out, usize::MAX);
            out
        })
        .join()
        .unwrap();
        assert_eq!(out.len(), 1, "the second queue saw none of its own events");
        assert_eq!(out[0].kind(), EventKind::End);
    }

    #[test]
    fn a_new_queue_never_reuses_a_dropped_queues_id() {
        let first = EventQueue::new(test_clock());
        let id = first.id();
        drop(first);
        assert_ne!(EventQueue::new(test_clock()).id(), id);
    }

    #[test]
    fn no_queue_takes_the_id_a_fresh_thread_starts_with() {
        assert_eq!(Hot::EMPTY.queue_id, 0);
        for _ in 0..8 {
            assert_ne!(EventQueue::new(test_clock()).id(), 0);
        }
    }

    #[test]
    fn a_wrong_run_id_costs_the_fast_path_but_not_the_trace() {
        let out = std::thread::spawn(|| {
            let q = EventQueue::new(test_clock());
            let hot = hot();
            let wrong = q.id().wrapping_add(1);
            for i in 0..4u64 {
                q.push_with_ctx(hot, wrong, i * 1_000, 1, 3, EventKind::Begin);
            }
            COLD.with_borrow_mut(|cold| cold.flush_partial(hot));
            let mut out = Vec::new();
            q.drain_nonblocking(&mut out, usize::MAX);
            out
        })
        .join()
        .unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[3].ts_ns, 3_000);
        assert_eq!(out[3].code_id(), 3);
    }

    #[test]
    fn the_last_batch_is_counted_when_the_ring_has_no_room() {
        let n = BATCH_N * (BATCHES_CAPACITY + 1);
        let (kept, dropped) = std::thread::spawn(move || {
            let q = EventQueue::new(test_clock());
            let hot = hot();
            for i in 0..n {
                q.push_with_ctx(hot, q.id(), i as u64, 1, 0, EventKind::Begin);
            }
            q.record_dropped(COLD.with_borrow_mut(|cold| cold.flush_partial(hot)));
            let mut out = Vec::new();
            q.drain_nonblocking(&mut out, usize::MAX);
            (out.len() as u64, q.dropped())
        })
        .join()
        .unwrap();
        assert_eq!(kept, (BATCH_N * BATCHES_CAPACITY) as u64);
        assert_eq!(
            kept + dropped,
            n as u64,
            "the flushed batch vanished without being counted"
        );
    }

    #[test]
    fn a_full_set_of_batches_is_buffered_without_dropping() {
        let n = BATCH_N * BATCHES_CAPACITY;
        let out = push_and_drain(
            test_clock(),
            (0..n).map(|i| (i as u64, 1, 0, EventKind::Begin)).collect(),
        );
        assert_eq!(out.len(), n);
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
