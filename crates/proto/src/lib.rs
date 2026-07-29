//! Perfetto protobuf exporter — the throughput-oriented format.
//!
//! Output is a stream of length-delimited `Trace.packet` entries, which
//! is what Perfetto expects and what makes concatenation valid. Packets
//! are framed by hand into a reused scratch buffer rather than by
//! building a fresh single-packet `Trace` message per event.

use prost::Message;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use trace0_core::{CodeLookup, Event, EventKind, Exporter, ThreadNames};

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

/// `Trace.packet` is field 1, wire type 2 (length-delimited).
const PACKET_TAG: u8 = (1 << 3) | 2;
const SEQUENCE_ID: u32 = 1;

pub struct ProtoExporter<W: Write + Send> {
    out: W,
    scratch: Vec<u8>,
    seen_tids: ahash::AHashSet<u32>,
    process_emitted: bool,
    pid: i32,
}

impl ProtoExporter<BufWriter<File>> {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let f = File::create(path)?;
        Ok(Self::new(BufWriter::with_capacity(1 << 16, f)))
    }
}

impl<W: Write + Send> ProtoExporter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            scratch: Vec::with_capacity(1 << 12),
            seen_tids: ahash::AHashSet::new(),
            process_emitted: false,
            pid: std::process::id() as i32,
        }
    }

    /// Override the recorded pid. Tests use this so output is stable.
    pub fn with_pid(mut self, pid: i32) -> Self {
        self.pid = pid;
        self
    }

    /// Frame one packet as `field 1, length-delimited` straight into the
    /// output. No per-event `Trace` allocation, no packet clone.
    fn write_packet(&mut self, packet: &pb::TracePacket) -> io::Result<()> {
        self.scratch.clear();
        packet
            .encode(&mut self.scratch)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut header = [0u8; 11];
        header[0] = PACKET_TAG;
        let n = 1 + put_varint(&mut header[1..], self.scratch.len() as u64);
        self.out.write_all(&header[..n])?;
        self.out.write_all(&self.scratch)
    }

    fn ensure_process(&mut self) -> io::Result<()> {
        if self.process_emitted {
            return Ok(());
        }
        self.process_emitted = true;
        let pid = self.pid;
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(SEQUENCE_ID),
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

    fn ensure_thread(&mut self, tid: u32, threads: &dyn ThreadNames) -> io::Result<()> {
        if !self.seen_tids.insert(tid) {
            return Ok(());
        }
        let pid = self.pid;
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(SEQUENCE_ID),
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

fn put_varint(buf: &mut [u8], mut v: u64) -> usize {
    let mut n = 0;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf[n] = byte;
            return n + 1;
        }
        buf[n] = byte | 0x80;
        n += 1;
    }
}

fn process_uuid(pid: i32) -> u64 {
    0x5000_0000_0000_0000u64 | (pid as u32 as u64)
}

fn thread_uuid(tid: u32) -> u64 {
    0x7000_0000_0000_0000u64 | (tid as u64)
}

fn proto_type(kind: EventKind) -> i32 {
    if kind.opens_slice() {
        pb::track_event::Type::SliceBegin as i32
    } else {
        pb::track_event::Type::SliceEnd as i32
    }
}

impl<W: Write + Send> Exporter for ProtoExporter<W> {
    fn write_batch(
        &mut self,
        events: &[Event],
        codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
    ) -> io::Result<()> {
        self.ensure_process()?;
        for ev in events {
            self.ensure_thread(ev.tid, threads)?;
            let name = codes
                .code(ev.code_id())
                .map(|i| i.qualname)
                .unwrap_or_else(|| "<unknown>".into());
            let pkt = pb::TracePacket {
                timestamp: Some(ev.ts_ns),
                trusted_packet_sequence_id: Some(SEQUENCE_ID),
                data: Some(pb::trace_packet::Data::TrackEvent(pb::TrackEvent {
                    r#type: Some(proto_type(ev.kind())),
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
        _codes: &dyn CodeLookup,
        _threads: &dyn ThreadNames,
        dropped: u64,
    ) -> io::Result<()> {
        // Surface loss in-band. Without this the protobuf path gives a
        // reader no way to tell a complete trace from a truncated one.
        if dropped > 0 {
            self.ensure_process()?;
            let pkt = pb::TracePacket {
                timestamp: Some(0),
                trusted_packet_sequence_id: Some(SEQUENCE_ID),
                data: Some(pb::trace_packet::Data::TrackEvent(pb::TrackEvent {
                    r#type: Some(pb::track_event::Type::Instant as i32),
                    track_uuid: Some(process_uuid(self.pid)),
                    name: Some(format!("trace0: {dropped} events dropped")),
                    categories: vec!["py".into(), "dropped".into()],
                })),
            };
            self.write_packet(&pkt)?;
        }
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace0_core::{CodeInfo, CodeTable, ThreadTable};

    /// Split a raw stream back into `TracePacket`s by walking the
    /// field-1 framing this exporter writes.
    fn packets(raw: &[u8]) -> Vec<pb::TracePacket> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            assert_eq!(raw[i], PACKET_TAG, "unexpected framing byte at {i}");
            i += 1;
            let (len, used) = get_varint(&raw[i..]);
            i += used;
            out.push(pb::TracePacket::decode(&raw[i..i + len as usize]).expect("decodable packet"));
            i += len as usize;
        }
        out
    }

    fn get_varint(b: &[u8]) -> (u64, usize) {
        let (mut v, mut shift, mut n) = (0u64, 0, 0);
        loop {
            let byte = b[n];
            v |= ((byte & 0x7f) as u64) << shift;
            n += 1;
            if byte & 0x80 == 0 {
                return (v, n);
            }
            shift += 7;
        }
    }

    fn export(events: &[Event], codes: &CodeTable, threads: &ThreadTable, dropped: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut ex = ProtoExporter::new(&mut buf).with_pid(4242);
            ex.write_batch(events, codes, threads).unwrap();
            ex.finish(codes, threads, dropped).unwrap();
        }
        buf
    }

    fn fib_table() -> CodeTable {
        let mut t = CodeTable::new();
        t.push(CodeInfo {
            qualname: "fib".into(),
            filename: "demo.py".into(),
            firstlineno: 12,
        });
        t
    }

    fn track_events(pkts: &[pb::TracePacket]) -> Vec<&pb::TrackEvent> {
        pkts.iter()
            .filter_map(|p| match p.data.as_ref() {
                Some(pb::trace_packet::Data::TrackEvent(te)) => Some(te),
                _ => None,
            })
            .collect()
    }

    fn descriptors(pkts: &[pb::TracePacket]) -> Vec<&pb::TrackDescriptor> {
        pkts.iter()
            .filter_map(|p| match p.data.as_ref() {
                Some(pb::trace_packet::Data::TrackDescriptor(td)) => Some(td),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn hand_framing_round_trips_through_a_decoder() {
        let raw = export(
            &[Event::new(1_000, 7, 0, EventKind::Begin)],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        // process descriptor + thread descriptor + the event itself
        assert_eq!(pkts.len(), 3);
    }

    #[test]
    fn varint_framing_handles_packets_over_127_bytes() {
        let mut codes = CodeTable::new();
        codes.push_named(&"n".repeat(400));
        let raw = export(
            &[Event::new(0, 1, 0, EventKind::Begin)],
            &codes,
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let te = track_events(&pkts)[0];
        assert_eq!(te.name.as_deref().unwrap().len(), 400);
    }

    #[test]
    fn timestamps_are_carried_through_verbatim() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(1_000_000_000, 1, 0, EventKind::End),
        ];
        let pkts = packets(&export(&events, &fib_table(), &ThreadTable::new(), 0));
        let ts: Vec<u64> = pkts
            .iter()
            .filter(|p| matches!(p.data, Some(pb::trace_packet::Data::TrackEvent(_))))
            .map(|p| p.timestamp.unwrap())
            .collect();
        assert_eq!(ts, vec![0, 1_000_000_000]);
    }

    #[test]
    fn slice_polarity_matches_event_kind() {
        for (kind, want) in [
            (EventKind::Begin, pb::track_event::Type::SliceBegin),
            (EventKind::Resume, pb::track_event::Type::SliceBegin),
            (EventKind::Throw, pb::track_event::Type::SliceBegin),
            (EventKind::End, pb::track_event::Type::SliceEnd),
            (EventKind::Yield, pb::track_event::Type::SliceEnd),
            (EventKind::Unwind, pb::track_event::Type::SliceEnd),
        ] {
            let raw = export(
                &[Event::new(0, 1, 0, kind)],
                &fib_table(),
                &ThreadTable::new(),
                0,
            );
            let pkts = packets(&raw);
            let te = track_events(&pkts)[0];
            assert_eq!(te.r#type, Some(want as i32), "wrong polarity for {kind:?}");
        }
    }

    #[test]
    fn each_thread_is_described_once_and_carries_its_name() {
        let mut threads = ThreadTable::new();
        threads.insert(1, "MainThread");
        threads.insert(2, "cpu-a");
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(1, 2, 0, EventKind::Begin),
            Event::new(2, 1, 0, EventKind::End),
            Event::new(3, 2, 0, EventKind::End),
        ];
        let pkts = packets(&export(&events, &fib_table(), &threads, 0));
        let named: Vec<_> = descriptors(&pkts)
            .iter()
            .filter_map(|d| d.thread.as_ref())
            .map(|t| (t.tid.unwrap(), t.thread_name.clone().unwrap_or_default()))
            .collect();
        assert_eq!(
            named,
            vec![(1, "MainThread".to_string()), (2, "cpu-a".to_string())]
        );
    }

    #[test]
    fn threads_are_parented_to_the_process() {
        let raw = export(
            &[Event::new(0, 9, 0, EventKind::Begin)],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let ds = descriptors(&pkts);
        let proc = ds.iter().find(|d| d.process.is_some()).unwrap();
        let thread = ds.iter().find(|d| d.thread.is_some()).unwrap();
        assert_eq!(thread.parent_uuid, proc.uuid);
        assert_eq!(track_events(&pkts)[0].track_uuid, thread.uuid);
    }

    #[test]
    fn dropped_events_are_reported_in_band() {
        let raw = export(&[], &CodeTable::new(), &ThreadTable::new(), 77);
        let pkts = packets(&raw);
        let marker = track_events(&pkts)
            .into_iter()
            .find(|te| te.r#type == Some(pb::track_event::Type::Instant as i32))
            .expect("a dropped-event marker must be emitted");
        assert!(marker.name.as_deref().unwrap().contains("77"));
    }

    #[test]
    fn a_lossless_trace_carries_no_marker() {
        let raw = export(&[], &CodeTable::new(), &ThreadTable::new(), 0);
        assert!(track_events(&packets(&raw)).is_empty());
    }

    #[test]
    fn unresolvable_code_ids_do_not_break_the_stream() {
        let raw = export(
            &[Event::new(0, 1, 999, EventKind::Begin)],
            &CodeTable::new(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        assert_eq!(track_events(&pkts)[0].name.as_deref(), Some("<unknown>"));
    }
}
