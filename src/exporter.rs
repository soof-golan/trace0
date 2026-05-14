use crate::event::{Event, EventKind};
use crate::intern::Interner;
use crate::threads::ThreadRegistry;
use prost::Message;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

pub trait Exporter: Send {
    fn write_batch(
        &mut self,
        events: &[Event],
        interner: &Interner,
        threads: &ThreadRegistry,
    ) -> io::Result<()>;
    fn finish(
        &mut self,
        interner: &Interner,
        threads: &ThreadRegistry,
        dropped: u64,
    ) -> io::Result<()>;
}

pub fn make_exporter(format: &str, path: &str) -> io::Result<Box<dyn Exporter>> {
    match format {
        "json" => Ok(Box::new(JsonExporter::new(path)?)),
        "protobuf" | "proto" | "pb" => Ok(Box::new(ProtoExporter::new(path)?)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown format: {other}"),
        )),
    }
}

pub struct JsonExporter {
    out: BufWriter<File>,
    first: bool,
}

impl JsonExporter {
    pub fn new(path: &str) -> io::Result<Self> {
        let f = File::create(path)?;
        let mut out = BufWriter::with_capacity(1 << 16, f);
        out.write_all(b"{\"traceEvents\":[")?;
        Ok(Self { out, first: true })
    }
}

fn phase(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Begin | EventKind::Resume => "B",
        EventKind::End | EventKind::Yield | EventKind::Unwind => "E",
        EventKind::Throw => "B",
    }
}

impl Exporter for JsonExporter {
    fn write_batch(
        &mut self,
        events: &[Event],
        interner: &Interner,
        _threads: &ThreadRegistry,
    ) -> io::Result<()> {
        for ev in events {
            if !self.first {
                self.out.write_all(b",")?;
            }
            self.first = false;
            let info = interner.get(ev.code_id);
            let (name, file, line) = match info {
                Some(i) => (i.qualname, i.filename, i.firstlineno),
                None => ("<unknown>".into(), String::new(), 0),
            };
            let pid = std::process::id();
            let kind_str = match ev.kind {
                EventKind::Begin => "start",
                EventKind::End => "return",
                EventKind::Yield => "yield",
                EventKind::Resume => "resume",
                EventKind::Unwind => "unwind",
                EventKind::Throw => "throw",
            };
            let entry = serde_json::json!({
                "name": name,
                "cat": "py",
                "ph": phase(ev.kind),
                "ts": ev.ts_ns / 1000,
                "pid": pid,
                "tid": ev.tid,
                "args": {
                    "file": file,
                    "line": line,
                    "kind": kind_str,
                }
            });
            serde_json::to_writer(&mut self.out, &entry)?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        _interner: &Interner,
        threads: &ThreadRegistry,
        dropped: u64,
    ) -> io::Result<()> {
        let pid = std::process::id();
        for (tid, name) in threads.snapshot() {
            if !self.first {
                self.out.write_all(b",")?;
            }
            self.first = false;
            let entry = serde_json::json!({
                "name": "thread_name",
                "ph": "M",
                "pid": pid,
                "tid": tid,
                "args": { "name": name },
            });
            serde_json::to_writer(&mut self.out, &entry)?;
        }
        self.out.write_all(b"],\"droppedEvents\":")?;
        write!(self.out, "{}", dropped)?;
        self.out.write_all(b"}")?;
        self.out.flush()
    }
}

pub struct ProtoExporter {
    out: BufWriter<File>,
    seen_tids: ahash::AHashSet<u64>,
    process_emitted: bool,
}

impl ProtoExporter {
    pub fn new(path: &str) -> io::Result<Self> {
        let f = File::create(path)?;
        Ok(Self {
            out: BufWriter::with_capacity(1 << 16, f),
            seen_tids: ahash::AHashSet::new(),
            process_emitted: false,
        })
    }

    fn write_packet(&mut self, packet: &pb::TracePacket) -> io::Result<()> {
        let mut buf = Vec::with_capacity(packet.encoded_len() + 8);
        let trace = pb::Trace {
            packet: vec![packet.clone()],
        };
        trace.encode(&mut buf).map_err(io_err)?;
        self.out.write_all(&buf)
    }

    fn ensure_process(&mut self) -> io::Result<()> {
        if self.process_emitted {
            return Ok(());
        }
        self.process_emitted = true;
        let pid = std::process::id() as i32;
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(1),
            data: Some(pb::trace_packet::Data::TrackDescriptor(pb::TrackDescriptor {
                uuid: Some(process_uuid(pid)),
                name: Some(format!("python:{pid}")),
                process: Some(pb::ProcessDescriptor {
                    pid: Some(pid),
                    process_name: Some("python".into()),
                }),
                ..Default::default()
            })),
        };
        self.write_packet(&pkt)
    }

    fn ensure_thread(&mut self, tid: u64, threads: &ThreadRegistry) -> io::Result<()> {
        if !self.seen_tids.insert(tid) {
            return Ok(());
        }
        let pid = std::process::id() as i32;
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(1),
            data: Some(pb::trace_packet::Data::TrackDescriptor(pb::TrackDescriptor {
                uuid: Some(thread_uuid(tid)),
                parent_uuid: Some(process_uuid(pid)),
                thread: Some(pb::ThreadDescriptor {
                    pid: Some(pid),
                    tid: Some(tid as i32),
                    thread_name: threads.name(tid),
                }),
                ..Default::default()
            })),
        };
        self.write_packet(&pkt)
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

fn process_uuid(pid: i32) -> u64 {
    0x5000_0000_0000_0000u64 | (pid as u64)
}

fn thread_uuid(tid: u64) -> u64 {
    0x7000_0000_0000_0000u64 | (tid & 0x0FFF_FFFF_FFFF_FFFF)
}

fn proto_type(kind: EventKind) -> i32 {
    match kind {
        EventKind::Begin | EventKind::Resume | EventKind::Throw => {
            pb::track_event::Type::SliceBegin as i32
        }
        EventKind::End | EventKind::Yield | EventKind::Unwind => {
            pb::track_event::Type::SliceEnd as i32
        }
    }
}

impl Exporter for ProtoExporter {
    fn write_batch(
        &mut self,
        events: &[Event],
        interner: &Interner,
        threads: &ThreadRegistry,
    ) -> io::Result<()> {
        self.ensure_process()?;
        for ev in events {
            self.ensure_thread(ev.tid, threads)?;
            let name = interner
                .get(ev.code_id)
                .map(|i| i.qualname)
                .unwrap_or_else(|| "<unknown>".into());
            let pkt = pb::TracePacket {
                timestamp: Some(ev.ts_ns),
                trusted_packet_sequence_id: Some(1),
                data: Some(pb::trace_packet::Data::TrackEvent(pb::TrackEvent {
                    r#type: Some(proto_type(ev.kind)),
                    track_uuid: Some(thread_uuid(ev.tid)),
                    name: Some(name),
                    categories: vec!["py".into()],
                })),
            };
            self.write_packet(&pkt)?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        _interner: &Interner,
        _threads: &ThreadRegistry,
        _dropped: u64,
    ) -> io::Result<()> {
        self.out.flush()
    }
}

const FANIN_CAPACITY: usize = 1 << 22;
const BATCH: usize = 4096;

/// Two-thread pipeline. A serializer drains the per-thread SPSC rings
/// into a single 1M SPSC fan-in buffer; a writer pops the fan-in
/// buffer and runs the I/O-bound Exporter. Splitting separates the
/// fast-drain step (memory-bandwidth-bound) from the slow-write step
/// (JSON encoding / disk-bound) so the per-thread rings can stay
/// small without dropping during bursts.
pub fn run_pipeline(
    inbound: Arc<crate::evqueue::EventQueue>,
    interner: Arc<Interner>,
    threads: Arc<ThreadRegistry>,
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
            .name("useful-tracer-serializer".into())
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
                // final drain after close
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
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
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
            exporter.write_batch(&buf, &interner, &threads)?;
            buf.clear();
            continue;
        }
        if fanin_closed.load(Ordering::Acquire) {
            while let Ok(ev) = fanin_rx.pop() {
                buf.push(ev);
            }
            if !buf.is_empty() {
                exporter.write_batch(&buf, &interner, &threads)?;
            }
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    serializer
        .join()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "serializer panicked"))?;
    let total_dropped = inbound.dropped() + fanin_dropped.load(Ordering::Relaxed);
    exporter.finish(&interner, &threads, total_dropped)?;
    Ok(())
}
