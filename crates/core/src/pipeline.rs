use crate::clock::Clock;
use crate::event::Event;
use crate::evqueue::{BATCH_N, EventBatch, EventQueue};
use crate::ring::Ring;
use crate::sink::{CodeLookup, Exporter, ThreadNames};
use std::io;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const FANIN_BATCHES: usize = 256;
pub(crate) const IDLE_MIN: Duration = Duration::from_micros(20);
const IDLE_MAX: Duration = Duration::from_micros(200);

pub(crate) fn backoff(idle: &mut Duration) {
    thread::sleep(*idle);
    *idle = (*idle * 2).min(IDLE_MAX);
}

fn ship(full: &mut rtrb::Producer<Box<EventBatch>>, batch: Box<EventBatch>, dropped: &mut u64) {
    let events = batch.events.len() as u64;
    if full.push(batch).is_err() {
        *dropped += events;
    }
}

struct Writer<'a> {
    clock: &'a Clock,
    codes: &'a dyn CodeLookup,
    threads: &'a dyn ThreadNames,
    exporter: &'a mut dyn Exporter,
    decoded: Vec<Event>,
}

impl Writer<'_> {
    fn drain(&mut self, ring: &mut Ring) -> io::Result<()> {
        while let Some(batch) = ring.pop() {
            self.decoded.clear();
            batch.decode_into(self.clock, &mut self.decoded);
            self.exporter
                .write_batch(&self.decoded, self.codes, self.threads)?;
        }
        Ok(())
    }
}

pub fn run_pipeline(
    inbound: Arc<EventQueue>,
    codes: Arc<dyn CodeLookup>,
    threads: Arc<dyn ThreadNames>,
    mut exporter: Box<dyn Exporter>,
) -> io::Result<()> {
    let (mut full_tx, mut full_rx) = rtrb::RingBuffer::<Box<EventBatch>>::new(FANIN_BATCHES);

    let serializer = {
        let inbound = inbound.clone();
        thread::Builder::new()
            .name("trace0-serializer".into())
            .spawn(move || {
                let mut dropped: u64 = 0;
                let mut idle = IDLE_MIN;
                let mut batches: Vec<Box<EventBatch>> = Vec::new();
                loop {
                    batches.clear();
                    if inbound.drain_batches(&mut batches, FANIN_BATCHES) == 0 {
                        if inbound.is_closed() {
                            break;
                        }
                        backoff(&mut idle);
                        continue;
                    }
                    idle = IDLE_MIN;
                    for batch in batches.drain(..) {
                        ship(&mut full_tx, batch, &mut dropped);
                    }
                }
                loop {
                    batches.clear();
                    if inbound.drain_batches(&mut batches, usize::MAX) == 0 {
                        break;
                    }
                    for batch in batches.drain(..) {
                        ship(&mut full_tx, batch, &mut dropped);
                    }
                }
                dropped
            })
            .map_err(io::Error::other)?
    };

    let mut ring = Ring::new(usize::MAX);
    let mut writer = Writer {
        clock: inbound.clock(),
        codes: codes.as_ref(),
        threads: threads.as_ref(),
        exporter: exporter.as_mut(),
        decoded: Vec::with_capacity(BATCH_N),
    };
    let mut idle = IDLE_MIN;
    loop {
        match full_rx.pop() {
            Ok(batch) => {
                idle = IDLE_MIN;
                ring.push(batch);
                writer.drain(&mut ring)?;
            }
            Err(_) => {
                if serializer.is_finished() {
                    while let Ok(batch) = full_rx.pop() {
                        ring.push(batch);
                    }
                    writer.drain(&mut ring)?;
                    break;
                }
                backoff(&mut idle);
            }
        }
    }

    let fanin_dropped = serializer
        .join()
        .map_err(|_| io::Error::other("serializer panicked"))?;
    let total_dropped = inbound.dropped() + fanin_dropped;
    exporter.finish(codes.as_ref(), threads.as_ref(), total_dropped)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::event::EventKind;
    use crate::evqueue::{BATCH_N, BATCHES_CAPACITY};
    use crate::sink::{CodeTable, ThreadTable};
    use crate::tls::{COLD, hot};
    use parking_lot::Mutex;
    use std::time::Instant;

    #[derive(Default)]
    struct Recording {
        events: Vec<Event>,
        dropped: Option<u64>,
    }

    struct Collector(Arc<Mutex<Recording>>);

    impl Exporter for Collector {
        fn write_batch(
            &mut self,
            events: &[Event],
            _: &dyn CodeLookup,
            _: &dyn ThreadNames,
        ) -> io::Result<()> {
            self.0.lock().events.extend_from_slice(events);
            Ok(())
        }

        fn finish(
            &mut self,
            _: &dyn CodeLookup,
            _: &dyn ThreadNames,
            dropped: u64,
        ) -> io::Result<()> {
            self.0.lock().dropped = Some(dropped);
            Ok(())
        }
    }

    fn queue() -> Arc<EventQueue> {
        Arc::new(EventQueue::new(Clock::mock().0))
    }

    fn produce(queue: &Arc<EventQueue>, n: u32) -> thread::JoinHandle<()> {
        let queue = queue.clone();
        thread::Builder::new()
            .name("producer".into())
            .spawn(move || {
                let hot = hot();
                for i in 0..n {
                    queue.push_with_ctx(hot, queue.id(), i as u64, 7, i % 16, EventKind::Begin);
                }
                queue.record_dropped(COLD.with_borrow_mut(|cold| cold.flush_partial(hot)));
            })
            .unwrap()
    }

    fn drain(queue: Arc<EventQueue>, into: Arc<Mutex<Recording>>) {
        run_pipeline(
            queue,
            Arc::new(CodeTable::new()),
            Arc::new(ThreadTable::new()),
            Box::new(Collector(into)),
        )
        .unwrap();
    }

    fn drain_all(queue: Arc<EventQueue>) -> Recording {
        let seen = Arc::new(Mutex::new(Recording::default()));
        drain(queue, seen.clone());
        Arc::try_unwrap(seen).ok().unwrap().into_inner()
    }

    #[test]
    fn every_event_survives_the_fanin() {
        let queue = queue();
        produce(&queue, 5000).join().unwrap();
        queue.close();
        let seen = drain_all(queue);
        assert_eq!(seen.events.len(), 5000);
        assert_eq!(seen.dropped, Some(0));
    }

    #[test]
    fn events_pushed_while_the_exporter_runs_are_not_lost() {
        let queue = queue();
        let producer = produce(&queue, 20_000);
        let closer = {
            let queue = queue.clone();
            thread::spawn(move || {
                producer.join().unwrap();
                queue.close();
            })
        };
        let seen = drain_all(queue.clone());
        closer.join().unwrap();
        assert_eq!(seen.events.len() as u64 + queue.dropped(), 20_000);
        assert_eq!(seen.dropped, Some(queue.dropped()));
    }

    #[test]
    fn the_exporter_is_told_how_many_events_were_dropped() {
        let queue = queue();
        let n = ((BATCHES_CAPACITY + 8) * BATCH_N) as u32;
        produce(&queue, n).join().unwrap();
        queue.close();
        let seen = drain_all(queue.clone());
        assert!(
            queue.dropped() > 0,
            "a queue this small should have overflowed"
        );
        assert_eq!(seen.dropped, Some(queue.dropped()));
        assert_eq!(seen.events.len() as u64 + queue.dropped(), n as u64);
    }

    #[test]
    fn the_exporter_writes_before_the_queue_closes() {
        let queue = queue();
        produce(&queue, 4096).join().unwrap();
        let seen = Arc::new(Mutex::new(Recording::default()));
        let pipeline = {
            let queue = queue.clone();
            let seen = seen.clone();
            thread::spawn(move || drain(queue, seen))
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().events.is_empty() {
            assert!(
                Instant::now() < deadline,
                "nothing was written before close"
            );
            thread::yield_now();
        }
        assert!(seen.lock().dropped.is_none(), "finish ran before close");

        queue.close();
        pipeline.join().unwrap();
        assert_eq!(seen.lock().events.len(), 4096);
    }
}
