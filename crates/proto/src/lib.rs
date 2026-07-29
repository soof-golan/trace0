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
use trace0_core::{CodeLookup, Event, Exporter, ThreadNames};

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

/// `Trace.packet` is field 1, wire type 2 (length-delimited).
const PACKET_TAG: u8 = (1 << 3) | 2;
const SEQUENCE_ID: u32 = 1;
const CATEGORY: &str = "py";

/// Field tags, pre-resolved. A tag is `(field_number << 3) | wire_type`,
/// varint-encoded, so fields past 15 need two bytes.
mod tag {
    /// `TracePacket.timestamp`, field 8, varint.
    pub const TIMESTAMP: u8 = 8 << 3;
    /// `TracePacket.track_event`, field 11, length-delimited.
    pub const TRACK_EVENT: u8 = (11 << 3) | 2;
    /// `TrackEvent.type`, field 9, varint.
    pub const TYPE: u8 = 9 << 3;
    /// `TrackEvent.track_uuid`, field 11, varint.
    pub const TRACK_UUID: u8 = 11 << 3;
    /// `TrackEvent.categories`, field 22, length-delimited.
    pub const CATEGORIES: [u8; 2] = [0xb2, 0x01];
    /// `TrackEvent.name`, field 23, length-delimited.
    pub const NAME: [u8; 2] = [0xba, 0x01];
}

/// `TracePacket.trusted_packet_sequence_id`, field 10, varint, always 1.
const SEQUENCE_FIELD: [u8; 2] = [10 << 3, SEQUENCE_ID as u8];

pub struct ProtoExporter<W: Write + Send> {
    out: W,
    scratch: Vec<u8>,
    /// Event packets for the current batch, written out in one call.
    buf: Vec<u8>,
    /// Encoded `TrackEvent` submessages, keyed by what determines them.
    /// Only the timestamp differs between two events sharing a key, so
    /// the rest is encoded once and copied thereafter.
    templates: ahash::AHashMap<u64, (u32, u32)>,
    template_bytes: Vec<u8>,
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
            buf: Vec::with_capacity(1 << 18),
            templates: ahash::AHashMap::new(),
            template_bytes: Vec::with_capacity(1 << 12),
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

    /// Byte range in `template_bytes` holding the encoded `track_event`
    /// field for this combination, building it on first sight.
    fn template(
        &mut self,
        tid: u32,
        code_id: u32,
        opens: bool,
        codes: &dyn CodeLookup,
    ) -> (u32, u32) {
        let key = ((tid as u64) << 32) | ((code_id as u64) << 1) | opens as u64;
        if let Some(&range) = self.templates.get(&key) {
            return range;
        }

        let name = codes
            .code(code_id)
            .map(|i| i.qualname)
            .unwrap_or_else(|| "<unknown>".into());

        let mut event = Vec::with_capacity(32 + name.len());
        event.push(tag::TYPE);
        event.push(if opens {
            pb::track_event::Type::SliceBegin as u8
        } else {
            pb::track_event::Type::SliceEnd as u8
        });
        event.push(tag::TRACK_UUID);
        push_varint(&mut event, thread_uuid(tid));
        event.extend_from_slice(&tag::CATEGORIES);
        push_varint(&mut event, CATEGORY.len() as u64);
        event.extend_from_slice(CATEGORY.as_bytes());
        event.extend_from_slice(&tag::NAME);
        push_varint(&mut event, name.len() as u64);
        event.extend_from_slice(name.as_bytes());

        let start = self.template_bytes.len() as u32;
        self.template_bytes.push(tag::TRACK_EVENT);
        push_varint(&mut self.template_bytes, event.len() as u64);
        self.template_bytes.extend_from_slice(&event);
        let range = (start, self.template_bytes.len() as u32 - start);

        self.templates.insert(key, range);
        range
    }

    /// Append one event packet. prost is bypassed here: it would build a
    /// `TracePacket` value, walk it to compute a length, then walk it
    /// again to encode. The shape is known, so this writes the bytes.
    fn push_event(&mut self, ts_ns: u64, template: (u32, u32)) {
        let mut ts = [0u8; 10];
        let ts_len = put_varint(&mut ts, ts_ns);
        let (start, len) = template;

        let body_len = 1 + ts_len + SEQUENCE_FIELD.len() + len as usize;
        self.buf.push(PACKET_TAG);
        push_varint(&mut self.buf, body_len as u64);
        self.buf.push(tag::TIMESTAMP);
        self.buf.extend_from_slice(&ts[..ts_len]);
        self.buf.extend_from_slice(&SEQUENCE_FIELD);
        let (a, b) = (start as usize, (start + len) as usize);
        self.buf.extend_from_slice(&self.template_bytes[a..b]);
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
            data: Some(pb::trace_packet::Data::TrackDescriptor(
                pb::TrackDescriptor {
                    uuid: Some(process_uuid(pid)),
                    name: Some(format!("python:{pid}")),
                    process: Some(pb::ProcessDescriptor {
                        pid: Some(pid),
                        process_name: Some("python".into()),
                    }),
                    ..Default::default()
                },
            )),
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
            data: Some(pb::trace_packet::Data::TrackDescriptor(
                pb::TrackDescriptor {
                    uuid: Some(thread_uuid(tid)),
                    parent_uuid: Some(process_uuid(pid)),
                    thread: Some(pb::ThreadDescriptor {
                        pid: Some(pid),
                        tid: Some(tid as i32),
                        thread_name: threads.name(tid),
                    }),
                    ..Default::default()
                },
            )),
        };
        self.write_packet(&pkt)
    }
}

fn push_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
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

impl<W: Write + Send> Exporter for ProtoExporter<W> {
    fn write_batch(
        &mut self,
        events: &[Event],
        codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
    ) -> io::Result<()> {
        self.ensure_process()?;
        self.buf.clear();
        for ev in events {
            self.ensure_thread(ev.tid, threads)?;
            let template = self.template(ev.tid, ev.code_id(), ev.kind().opens_slice(), codes);
            self.push_event(ev.ts_ns, template);
        }
        self.out.write_all(&self.buf)
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
    use trace0_core::{CodeInfo, CodeTable, EventKind, ThreadTable};

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
