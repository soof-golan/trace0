use crate::evqueue::{EventBatch, EventQueue};
use crate::pipeline::{IDLE_MIN, backoff};
use crate::ring::Ring;
use crate::sink::{CodeLookup, Exporter, ThreadNames};
use crate::snapshot::assemble;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

const DRAIN_LIMIT: usize = 256;
const WRITE_CHUNK: usize = 4096;
const FLOOR_EVERY: u32 = 1024;

pub enum Control {
    Open {
        id: u64,
        start_ticks: u64,
    },
    Cancel {
        id: u64,
    },
    Dump {
        id: u64,
        start_ticks: u64,
        end_ticks: u64,
        reason: String,
        done: Option<std::sync::mpsc::Sender<()>>,
    },
    DumpAll {
        end_ticks: u64,
        reason: String,
        done: Option<std::sync::mpsc::Sender<()>>,
    },
}

pub trait DumpSink: Send {
    fn open(&mut self, reason: &str) -> io::Result<Box<dyn Exporter>>;
}

pub fn run_recorder(
    inbound: Arc<EventQueue>,
    codes: Arc<dyn CodeLookup>,
    threads: Arc<dyn ThreadNames>,
    mut sinks: Box<dyn DumpSink>,
    capacity_bytes: usize,
    control: Receiver<Control>,
) -> io::Result<()> {
    let mut recorder = Recorder {
        inbound,
        ring: Ring::new(capacity_bytes),
        windows: Vec::new(),
        batches: Vec::new(),
    };
    let mut idle = IDLE_MIN;
    let mut ticker = 0u32;
    loop {
        let mut worked = false;
        while let Ok(msg) = control.try_recv() {
            worked = true;
            recorder.handle(msg, codes.as_ref(), threads.as_ref(), sinks.as_mut())?;
        }
        ticker += 1;
        if ticker == FLOOR_EVERY {
            ticker = 0;
            recorder.update_recycle_floor();
        }
        if recorder.absorb(DRAIN_LIMIT) > 0 {
            worked = true;
        } else if recorder.inbound.is_closed() {
            recorder.absorb_all();
            while let Ok(msg) = control.try_recv() {
                recorder.handle(msg, codes.as_ref(), threads.as_ref(), sinks.as_mut())?;
            }
            return Ok(());
        }
        if worked {
            idle = IDLE_MIN;
        } else {
            backoff(&mut idle);
        }
    }
}

struct Recorder {
    inbound: Arc<EventQueue>,
    ring: Ring,
    windows: Vec<(u64, u64)>,
    #[allow(clippy::vec_box)]
    batches: Vec<Box<EventBatch>>,
}

impl Recorder {
    fn absorb(&mut self, limit: usize) -> usize {
        self.batches.clear();
        let drained = self.inbound.drain_batches(&mut self.batches, limit);
        for batch in self.batches.drain(..) {
            self.ring.push(batch);
        }
        drained
    }

    fn absorb_all(&mut self) {
        while self.absorb(usize::MAX) > 0 {}
    }

    fn refloor(&mut self) {
        let floor = self.windows.iter().map(|(_, start)| *start).min();
        self.ring.set_floor(floor);
    }

    fn update_recycle_floor(&mut self) {
        let now = self.inbound.clock().raw();
        let tails = self.inbound.read_tails();
        self.absorb_all();
        let oldest_tail = tails.iter().map(|t| t.base_ticks).min();
        let oldest_batch = self.ring.iter().map(|b| b.base_ticks).min();
        let floor = [Some(now), oldest_tail, oldest_batch]
            .into_iter()
            .flatten()
            .min()
            .unwrap();
        self.inbound.set_recycle_floor(floor);
    }

    fn handle(
        &mut self,
        msg: Control,
        codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
        sinks: &mut dyn DumpSink,
    ) -> io::Result<()> {
        match msg {
            Control::Open { id, start_ticks } => {
                self.windows.push((id, start_ticks));
                self.refloor();
            }
            Control::Cancel { id } => {
                self.windows.retain(|(window, _)| *window != id);
                self.refloor();
            }
            Control::Dump {
                id,
                start_ticks,
                end_ticks,
                reason,
                done,
            } => {
                self.dump(codes, threads, sinks, start_ticks, end_ticks, &reason)?;
                self.windows.retain(|(window, _)| *window != id);
                self.refloor();
                self.update_recycle_floor();
                if let Some(done) = done {
                    done.send(()).ok();
                }
            }
            Control::DumpAll {
                end_ticks,
                reason,
                done,
            } => {
                self.dump(codes, threads, sinks, 0, end_ticks, &reason)?;
                self.update_recycle_floor();
                if let Some(done) = done {
                    done.send(()).ok();
                }
            }
        }
        Ok(())
    }

    fn dump(
        &mut self,
        codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
        sinks: &mut dyn DumpSink,
        start_ticks: u64,
        end_ticks: u64,
        reason: &str,
    ) -> io::Result<()> {
        self.absorb_all();
        let tails = self.inbound.read_tails();
        self.absorb_all();
        let events = assemble(
            self.ring.iter(),
            &tails,
            self.inbound.clock(),
            self.ring.horizon_ticks(),
            start_ticks,
            end_ticks,
        );
        let mut exporter = sinks.open(reason)?;
        for chunk in events.chunks(WRITE_CHUNK) {
            exporter.write_batch(chunk, codes, threads)?;
            self.absorb(usize::MAX);
        }
        exporter.finish(codes, threads, self.inbound.dropped())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::event::{Event, EventKind};
    use crate::evqueue::BATCH_N;
    use crate::sink::{CodeTable, ThreadTable};
    use crate::tls::{COLD, hot};
    use parking_lot::Mutex;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    type Dumps = Arc<Mutex<Vec<(String, Vec<Event>)>>>;

    struct Collector {
        reason: String,
        events: Vec<Event>,
        out: Dumps,
    }

    impl Exporter for Collector {
        fn write_batch(
            &mut self,
            events: &[Event],
            _: &dyn CodeLookup,
            _: &dyn ThreadNames,
        ) -> io::Result<()> {
            self.events.extend_from_slice(events);
            Ok(())
        }

        fn finish(
            &mut self,
            _: &dyn CodeLookup,
            _: &dyn ThreadNames,
            _dropped: u64,
        ) -> io::Result<()> {
            self.out.lock().push((
                std::mem::take(&mut self.reason),
                std::mem::take(&mut self.events),
            ));
            Ok(())
        }
    }

    struct Factory(Dumps);

    impl DumpSink for Factory {
        fn open(&mut self, reason: &str) -> io::Result<Box<dyn Exporter>> {
            Ok(Box::new(Collector {
                reason: reason.to_string(),
                events: Vec::new(),
                out: self.0.clone(),
            }))
        }
    }

    struct Session {
        queue: Arc<EventQueue>,
        control: mpsc::Sender<Control>,
        dumps: Dumps,
        recorder: thread::JoinHandle<io::Result<()>>,
        syncs: u64,
        mock: Arc<crate::clock::Mock>,
    }

    fn start(capacity_bytes: usize) -> Session {
        let (clock, mock) = Clock::mock();
        let queue = Arc::new(EventQueue::new(clock));
        let (control, rx) = mpsc::channel();
        let dumps: Dumps = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let queue = queue.clone();
            let factory = Box::new(Factory(dumps.clone()));
            thread::Builder::new()
                .name("recorder".into())
                .spawn(move || {
                    run_recorder(
                        queue,
                        Arc::new(CodeTable::new()),
                        Arc::new(ThreadTable::new()),
                        factory,
                        capacity_bytes,
                        rx,
                    )
                })
                .unwrap()
        };
        Session {
            queue,
            control,
            dumps,
            recorder,
            syncs: 0,
            mock,
        }
    }

    impl Session {
        fn produce(&self, tid: u32, events: Vec<(u64, EventKind)>) {
            let queue = self.queue.clone();
            thread::spawn(move || {
                let hot = hot();
                for (ticks, kind) in events {
                    queue.push_with_ctx(hot, queue.id(), ticks, tid, 7, kind);
                }
                queue.record_dropped(COLD.with_borrow_mut(|cold| cold.flush_partial(hot)));
            })
            .join()
            .unwrap();
        }

        fn wait_for(&self, reason: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.dumps.lock().iter().any(|(r, _)| r == reason) {
                assert!(
                    Instant::now() < deadline,
                    "the recorder never wrote the dump named {reason:?}"
                );
                thread::yield_now();
            }
        }

        fn sync(&mut self) {
            self.syncs += 1;
            let reason = format!("sync-{}", self.syncs);
            let (done, written) = mpsc::channel();
            self.control
                .send(Control::DumpAll {
                    end_ticks: 0,
                    reason,
                    done: Some(done),
                })
                .unwrap();
            written.recv().unwrap();
        }

        fn finish(self) -> Vec<(String, Vec<Event>)> {
            self.queue.close();
            self.recorder.join().unwrap().unwrap();
            let dumps = Arc::try_unwrap(self.dumps).ok().unwrap().into_inner();
            dumps
                .into_iter()
                .filter(|(reason, _)| !reason.starts_with("sync-"))
                .collect()
        }
    }

    fn hold_after<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> (mpsc::Sender<()>, thread::JoinHandle<T>, mpsc::Receiver<()>) {
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            let out = work();
            done_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            out
        });
        (go_tx, handle, done_rx)
    }

    fn full_batch() -> usize {
        std::mem::size_of::<EventBatch>() + BATCH_N * 8
    }

    fn begins(ticks: std::ops::RangeInclusive<u64>) -> Vec<(u64, EventKind)> {
        ticks.map(|t| (t, EventKind::Begin)).collect()
    }

    fn timestamps(events: &[Event]) -> Vec<u64> {
        events.iter().map(|e| e.ts_ns).collect()
    }

    #[test]
    fn a_dump_captures_the_slice_between_its_window_marks() {
        let s = start(usize::MAX);
        s.produce(
            1,
            vec![
                (1, EventKind::Begin),
                (2, EventKind::End),
                (11, EventKind::Begin),
                (12, EventKind::End),
                (21, EventKind::Begin),
                (22, EventKind::End),
                (31, EventKind::Begin),
                (32, EventKind::End),
            ],
        );
        s.control
            .send(Control::Dump {
                id: 1,
                start_ticks: 10,
                end_ticks: 25,
                reason: "slice".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].0, "slice");
        assert_eq!(timestamps(&dumps[0].1), [11, 12, 21, 22]);
    }

    #[test]
    fn a_dump_reads_events_still_in_a_live_thread_tail() {
        let s = start(usize::MAX);
        let (go, producer, ready) = {
            let queue = s.queue.clone();
            hold_after(move || {
                let hot = hot();
                queue.push_with_ctx(hot, queue.id(), 1, 3, 7, EventKind::Begin);
                queue.push_with_ctx(hot, queue.id(), 2, 3, 7, EventKind::End);
            })
        };
        ready.recv().unwrap();
        s.control
            .send(Control::Dump {
                id: 1,
                start_ticks: 1,
                end_ticks: 10,
                reason: "tail".into(),
                done: None,
            })
            .unwrap();
        s.wait_for("tail");
        go.send(()).unwrap();
        producer.join().unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(timestamps(&dumps[0].1), [1, 2]);
        assert!(dumps[0].1.iter().all(|e| e.tid == 3));
    }

    #[test]
    fn an_open_window_protects_history_beyond_capacity() {
        let mut s = start(2 * full_batch());
        s.control
            .send(Control::Open {
                id: 1,
                start_ticks: 1,
            })
            .unwrap();
        s.sync();
        s.produce(1, begins(1..=4 * BATCH_N as u64));
        s.control
            .send(Control::Dump {
                id: 1,
                start_ticks: 1,
                end_ticks: u64::MAX,
                reason: "kept".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].1.len(), 4 * BATCH_N);
        assert_eq!(dumps[0].1[0].ts_ns, 1);
    }

    #[test]
    fn without_a_window_the_ring_keeps_only_its_capacity() {
        let s = start(2 * full_batch());
        s.produce(1, begins(1..=4 * BATCH_N as u64));
        s.control
            .send(Control::DumpAll {
                end_ticks: u64::MAX,
                reason: "rest".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].1.len(), 2 * BATCH_N);
        assert_eq!(dumps[0].1[0].ts_ns, 2 * BATCH_N as u64 + 1);
    }

    #[test]
    fn a_canceled_window_releases_its_history() {
        let mut s = start(2 * full_batch());
        s.control
            .send(Control::Open {
                id: 1,
                start_ticks: 1,
            })
            .unwrap();
        s.sync();
        s.produce(1, begins(1..=4 * BATCH_N as u64));
        s.sync();
        s.control.send(Control::Cancel { id: 1 }).unwrap();
        s.control
            .send(Control::DumpAll {
                end_ticks: u64::MAX,
                reason: "after-cancel".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].1.len(), 2 * BATCH_N);
        assert_eq!(dumps[0].1[0].ts_ns, 2 * BATCH_N as u64 + 1);
    }

    #[test]
    fn the_floor_follows_the_oldest_open_window() {
        let mut s = start(2 * full_batch());
        s.control
            .send(Control::Open {
                id: 1,
                start_ticks: 1,
            })
            .unwrap();
        s.control
            .send(Control::Open {
                id: 2,
                start_ticks: 3_000,
            })
            .unwrap();
        s.sync();
        s.produce(1, begins(1..=4 * BATCH_N as u64));
        s.sync();
        s.control.send(Control::Cancel { id: 1 }).unwrap();
        s.sync();
        s.produce(1, begins(4 * BATCH_N as u64 + 1..=6 * BATCH_N as u64));
        s.control
            .send(Control::Dump {
                id: 2,
                start_ticks: 3_000,
                end_ticks: u64::MAX,
                reason: "second".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].1.len(), 4 * BATCH_N);
        assert_eq!(dumps[0].1[0].ts_ns, 2 * BATCH_N as u64 + 1);
    }

    #[test]
    fn the_recorder_writes_nothing_unless_asked() {
        let s = start(usize::MAX);
        s.produce(1, begins(1..=64));
        let dumps = s.finish();
        assert!(dumps.is_empty());
    }

    #[test]
    fn a_dump_larger_than_one_write_chunk_keeps_every_event() {
        let s = start(usize::MAX);
        s.produce(1, begins(1..=2 * WRITE_CHUNK as u64));
        s.control
            .send(Control::DumpAll {
                end_ticks: u64::MAX,
                reason: "big".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].1.len(), 2 * WRITE_CHUNK);
        assert_eq!(
            timestamps(&dumps[0].1),
            Vec::from_iter(1..=2 * WRITE_CHUNK as u64)
        );
    }

    #[test]
    fn a_dump_publishes_the_recycle_floor() {
        let mut s = start(2 * full_batch());
        s.produce(1, begins(1..=4 * BATCH_N as u64));
        s.mock.increment(1_000_000u64);
        s.sync();
        assert_eq!(s.queue.recycle_floor(), 2 * BATCH_N as u64 + 1);
        s.finish();
    }

    #[test]
    fn a_live_tail_holds_the_recycle_floor_down() {
        let mut s = start(usize::MAX);
        let (go, producer, ready) = {
            let queue = s.queue.clone();
            hold_after(move || {
                let hot = hot();
                queue.push_with_ctx(hot, queue.id(), 5, 3, 7, EventKind::Begin);
                queue.push_with_ctx(hot, queue.id(), 6, 3, 7, EventKind::End);
            })
        };
        ready.recv().unwrap();
        s.mock.increment(1_000_000u64);
        s.sync();
        assert_eq!(s.queue.recycle_floor(), 5);
        go.send(()).unwrap();
        producer.join().unwrap();
        s.finish();
    }

    #[test]
    fn an_idle_recorder_floors_at_the_present() {
        let mut s = start(usize::MAX);
        s.mock.increment(777u64);
        s.sync();
        assert_eq!(s.queue.recycle_floor(), 777);
        s.finish();
    }

    #[test]
    fn a_dump_acknowledges_only_after_it_is_written() {
        let s = start(usize::MAX);
        s.produce(1, vec![(1, EventKind::Begin), (2, EventKind::End)]);
        let (done, written) = mpsc::channel();
        s.control
            .send(Control::DumpAll {
                end_ticks: 10,
                reason: "acked".into(),
                done: Some(done),
            })
            .unwrap();
        written.recv().unwrap();
        assert!(s.dumps.lock().iter().any(|(reason, _)| reason == "acked"));
        s.finish();
    }

    #[test]
    fn a_dump_sent_with_close_still_lands() {
        let s = start(usize::MAX);
        s.produce(1, vec![(1, EventKind::Begin), (2, EventKind::End)]);
        s.control
            .send(Control::DumpAll {
                end_ticks: 10,
                reason: "exit".into(),
                done: None,
            })
            .unwrap();
        let dumps = s.finish();
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].0, "exit");
        assert_eq!(timestamps(&dumps[0].1), [1, 2]);
    }
}
