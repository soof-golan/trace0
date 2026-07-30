use crate::event::Event;
use crate::evqueue::EventQueue;
use crate::sink::{CodeLookup, Exporter, ThreadNames};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const FANIN_CAPACITY: usize = 1 << 22;
const BATCH: usize = 4096;

pub fn run_pipeline(
    inbound: Arc<EventQueue>,
    codes: Arc<dyn CodeLookup>,
    threads: Arc<dyn ThreadNames>,
    mut exporter: Box<dyn Exporter>,
) -> io::Result<()> {
    let (mut fanin_tx, mut fanin_rx) = rtrb::RingBuffer::<Event>::new(FANIN_CAPACITY);
    let fanin_dropped = Arc::new(AtomicU64::new(0));
    let fanin_closed = Arc::new(AtomicBool::new(false));

    let serializer = {
        let inbound = inbound.clone();
        let fanin_dropped = fanin_dropped.clone();
        let fanin_closed = fanin_closed.clone();
        thread::Builder::new()
            .name("trace0-serializer".into())
            .spawn(move || {
                let mut buf: Vec<Event> = Vec::with_capacity(BATCH);
                let mut empty_spins: u32 = 0;
                const SPIN_BEFORE_YIELD: u32 = 1024;
                loop {
                    let n = inbound.drain_nonblocking(&mut buf, BATCH);
                    if n == 0 {
                        empty_spins += 1;
                        if empty_spins >= SPIN_BEFORE_YIELD {
                            if inbound.is_closed() {
                                break;
                            }
                            thread::yield_now();
                            empty_spins = 0;
                        } else {
                            std::hint::spin_loop();
                        }
                        continue;
                    }
                    empty_spins = 0;
                    for ev in buf.drain(..) {
                        if fanin_tx.push(ev).is_err() {
                            fanin_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                loop {
                    let n = inbound.drain_nonblocking(&mut buf, BATCH);
                    if n == 0 {
                        break;
                    }
                    for ev in buf.drain(..) {
                        if fanin_tx.push(ev).is_err() {
                            fanin_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                fanin_closed.store(true, Ordering::Release);
            })
            .map_err(io::Error::other)?
    };

    let mut buf: Vec<Event> = Vec::with_capacity(BATCH);
    loop {
        while let Ok(ev) = fanin_rx.pop() {
            buf.push(ev);
            if buf.len() >= BATCH {
                break;
            }
        }
        if !buf.is_empty() {
            exporter.write_batch(&buf, codes.as_ref(), threads.as_ref())?;
            buf.clear();
            continue;
        }
        if fanin_closed.load(Ordering::Acquire) {
            while let Ok(ev) = fanin_rx.pop() {
                buf.push(ev);
            }
            if !buf.is_empty() {
                exporter.write_batch(&buf, codes.as_ref(), threads.as_ref())?;
            }
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    serializer
        .join()
        .map_err(|_| io::Error::other("serializer panicked"))?;
    let total_dropped = inbound.dropped() + fanin_dropped.load(Ordering::Relaxed);
    exporter.finish(codes.as_ref(), threads.as_ref(), total_dropped)?;
    Ok(())
}
