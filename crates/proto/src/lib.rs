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
    /// `TrackEvent.category_iids`, field 3, varint.
    pub const CATEGORY_IIDS: u8 = 3 << 3;
    /// `TrackEvent.name_iid`, field 10, varint.
    pub const NAME_IID: u8 = 10 << 3;
    /// `TracePacket.sequence_flags`, field 13, varint.
    pub const SEQUENCE_FLAGS: u8 = 13 << 3;
}

/// `SequenceFlags` from Perfetto: a reader joining the stream mid-way must
/// know these packets only make sense with the interning table that
/// preceded them.
const SEQ_INCREMENTAL_STATE_CLEARED: u32 = 1;
const SEQ_NEEDS_INCREMENTAL_STATE: u32 = 2;

/// Emitted on every event packet, since all of them reference interned ids.
const NEEDS_STATE_FIELD: [u8; 2] = [tag::SEQUENCE_FLAGS, SEQ_NEEDS_INCREMENTAL_STATE as u8];

/// The single category every event carries, interned once.
const CATEGORY_IID: u64 = 1;

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
    /// Interned name id per code object. Writing `Parser._parse_expression`
    /// into all ten million of its events is most of a trace's bytes.
    name_iids: ahash::AHashMap<u32, u64>,
    /// Dense track uuids. The value is opaque to Perfetto, and a sparse
    /// one like `0x7000_0000_0000_0000 | tid` costs a full 10-byte varint
    /// on every event -- a third of the trace.
    track_uuids: ahash::AHashMap<u32, u64>,
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
            name_iids: ahash::AHashMap::new(),
            track_uuids: ahash::AHashMap::new(),
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

    /// Interned id for this code object's name, emitting the name itself
    /// into the stream the first time it is seen.
    fn name_iid(&mut self, code_id: u32, codes: &dyn CodeLookup) -> io::Result<u64> {
        if let Some(&iid) = self.name_iids.get(&code_id) {
            return Ok(iid);
        }
        // 0 means "unset" in Perfetto's interning, so ids start at 1.
        let iid = self.name_iids.len() as u64 + 1;
        let name = codes
            .code(code_id)
            .map(|i| i.qualname)
            .unwrap_or_else(|| "<unknown>".into());
        self.write_packet(&pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(SEQUENCE_ID),
            sequence_flags: Some(SEQ_NEEDS_INCREMENTAL_STATE),
            interned_data: Some(pb::InternedData {
                event_names: vec![pb::EventName {
                    iid: Some(iid),
                    name: Some(name),
                }],
                event_categories: vec![],
            }),
            data: None,
        })?;
        self.name_iids.insert(code_id, iid);
        Ok(iid)
    }

    /// Byte range in `template_bytes` holding the encoded `track_event`
    /// field for this combination, building it on first sight.
    fn template(
        &mut self,
        tid: u32,
        code_id: u32,
        opens: bool,
        codes: &dyn CodeLookup,
    ) -> io::Result<(u32, u32)> {
        // A slice end carries no name, so every end on a thread encodes
        // identically and needs only one template.
        let keyed_code = if opens { code_id } else { 0 };
        let key = ((tid as u64) << 32) | ((keyed_code as u64) << 1) | opens as u64;
        if let Some(&range) = self.templates.get(&key) {
            return Ok(range);
        }

        let uuid = self.track_uuid(tid);

        let mut event = Vec::with_capacity(32);
        event.push(tag::TYPE);
        event.push(if opens {
            pb::track_event::Type::SliceBegin as u8
        } else {
            pb::track_event::Type::SliceEnd as u8
        });
        event.push(tag::TRACK_UUID);
        push_varint(&mut event, uuid);
        // A slice end closes whatever the track has open, so repeating the
        // name and category on it is pure duplication.
        if opens {
            let name_iid = self.name_iid(code_id, codes)?;
            event.push(tag::CATEGORY_IIDS);
            push_varint(&mut event, CATEGORY_IID);
            event.push(tag::NAME_IID);
            push_varint(&mut event, name_iid);
        }

        let start = self.template_bytes.len() as u32;
        self.template_bytes.push(tag::TRACK_EVENT);
        push_varint(&mut self.template_bytes, event.len() as u64);
        self.template_bytes.extend_from_slice(&event);
        let range = (start, self.template_bytes.len() as u32 - start);

        self.templates.insert(key, range);
        Ok(range)
    }

    /// Append one event packet. prost is bypassed here: it would build a
    /// `TracePacket` value, walk it to compute a length, then walk it
    /// again to encode. The shape is known, so this writes the bytes.
    fn push_event(&mut self, ts_ns: u64, template: (u32, u32)) {
        let mut ts = [0u8; 10];
        let ts_len = put_varint(&mut ts, ts_ns);
        let (start, len) = template;

        let body_len = 1 + ts_len + SEQUENCE_FIELD.len() + NEEDS_STATE_FIELD.len() + len as usize;
        self.buf.push(PACKET_TAG);
        push_varint(&mut self.buf, body_len as u64);
        self.buf.push(tag::TIMESTAMP);
        self.buf.extend_from_slice(&ts[..ts_len]);
        self.buf.extend_from_slice(&SEQUENCE_FIELD);
        self.buf.extend_from_slice(&NEEDS_STATE_FIELD);
        let (a, b) = (start as usize, (start + len) as usize);
        self.buf.extend_from_slice(&self.template_bytes[a..b]);
    }

    fn ensure_process(&mut self) -> io::Result<()> {
        if self.process_emitted {
            return Ok(());
        }
        self.process_emitted = true;
        let pid = self.pid;
        // First packet on the sequence: declares that interning state
        // starts here, and carries the one category every event shares.
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(SEQUENCE_ID),
            sequence_flags: Some(SEQ_INCREMENTAL_STATE_CLEARED),
            interned_data: Some(pb::InternedData {
                event_categories: vec![pb::EventCategory {
                    iid: Some(CATEGORY_IID),
                    name: Some(CATEGORY.into()),
                }],
                event_names: vec![],
            }),
            data: Some(pb::trace_packet::Data::TrackDescriptor(
                pb::TrackDescriptor {
                    uuid: Some(PROCESS_UUID),
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

    fn track_uuid(&mut self, tid: u32) -> u64 {
        let next = self.track_uuids.len() as u64 + PROCESS_UUID + 1;
        *self.track_uuids.entry(tid).or_insert(next)
    }

    fn ensure_thread(&mut self, tid: u32, threads: &dyn ThreadNames) -> io::Result<()> {
        if !self.seen_tids.insert(tid) {
            return Ok(());
        }
        let uuid = self.track_uuid(tid);
        let pid = self.pid;
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(SEQUENCE_ID),
            interned_data: None,
            sequence_flags: None,
            data: Some(pb::trace_packet::Data::TrackDescriptor(
                pb::TrackDescriptor {
                    uuid: Some(uuid),
                    parent_uuid: Some(PROCESS_UUID),
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

/// Uuid 1 is the process; threads take 2 upward in first-seen order.
const PROCESS_UUID: u64 = 1;

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
            let template = self.template(ev.tid, ev.code_id(), ev.kind().opens_slice(), codes)?;
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
                interned_data: None,
                sequence_flags: None,
                data: Some(pb::trace_packet::Data::TrackEvent(pb::TrackEvent {
                    r#type: Some(pb::track_event::Type::Instant as i32),
                    track_uuid: Some(PROCESS_UUID),
                    name: Some(format!("trace0: {dropped} events dropped")),
                    categories: vec!["py".into(), "dropped".into()],
                    category_iids: vec![],
                    name_iid: None,
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

    /// Resolve a track event's name through the interning table, the way
    /// a real consumer must. Names appear once in an `interned_data`
    /// packet; events carry only the id.
    fn interned_names(pkts: &[pb::TracePacket]) -> std::collections::HashMap<u64, String> {
        pkts.iter()
            .filter_map(|p| p.interned_data.as_ref())
            .flat_map(|d| d.event_names.iter())
            .map(|n| (n.iid.unwrap(), n.name.clone().unwrap()))
            .collect()
    }

    fn event_name(pkts: &[pb::TracePacket], te: &pb::TrackEvent) -> String {
        let names = interned_names(pkts);
        names
            .get(&te.name_iid.unwrap())
            .unwrap_or_else(|| panic!("name_iid {:?} was never interned", te.name_iid))
            .clone()
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
    fn a_repeated_name_is_written_once_however_many_events_use_it() {
        let events: Vec<_> = (0..500)
            .map(|i| Event::new(i * 10, 7, 0, EventKind::Begin))
            .collect();
        let raw = export(&events, &fib_table(), &ThreadTable::new(), 0);
        let pkts = packets(&raw);

        let interned: Vec<_> = pkts
            .iter()
            .filter_map(|p| p.interned_data.as_ref())
            .flat_map(|d| d.event_names.iter())
            .collect();
        assert_eq!(interned.len(), 1, "one code object, one interned name");
        assert_eq!(interned[0].name.as_deref(), Some("fib"));

        // The name must not appear in the byte stream once per event.
        let occurrences = raw.windows(3).filter(|w| *w == b"fib").count();
        assert_eq!(occurrences, 1, "name repeated in the encoded stream");

        for te in track_events(&pkts) {
            assert_eq!(te.name_iid, interned[0].iid);
            assert!(te.name.is_none(), "name should travel as an id only");
            assert_eq!(te.category_iids, vec![CATEGORY_IID]);
        }
    }

    #[test]
    fn distinct_code_objects_get_distinct_ids() {
        let mut codes = CodeTable::new();
        codes.push_named("alpha");
        codes.push_named("beta");
        let raw = export(
            &[
                Event::new(0, 1, 0, EventKind::Begin),
                Event::new(1, 1, 1, EventKind::Begin),
                Event::new(2, 1, 0, EventKind::End),
            ],
            &codes,
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let names = interned_names(&pkts);
        assert_eq!(names.len(), 2);
        let resolved: Vec<_> = track_events(&pkts)
            .iter()
            .filter(|te| te.r#type == Some(pb::track_event::Type::SliceBegin as i32))
            .map(|te| names[&te.name_iid.unwrap()].clone())
            .collect();
        assert_eq!(resolved, vec!["alpha", "beta"]);
    }

    #[test]
    fn slice_ends_carry_no_name_or_category() {
        let raw = export(
            &[
                Event::new(0, 1, 0, EventKind::Begin),
                Event::new(5, 1, 0, EventKind::End),
            ],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let evs = track_events(&pkts);
        let end = evs
            .iter()
            .find(|te| te.r#type == Some(pb::track_event::Type::SliceEnd as i32))
            .expect("no slice end");
        // The track already knows which slice is open; repeating its name
        // on the end would be duplication on half of every trace.
        assert_eq!(end.name_iid, None);
        assert!(end.category_iids.is_empty());
        assert!(end.track_uuid.is_some());
    }

    #[test]
    fn track_uuids_stay_small_enough_to_encode_in_one_byte() {
        let mut threads = ThreadTable::new();
        threads.insert(4242, "worker");
        let raw = export(
            &[Event::new(0, 4242, 0, EventKind::Begin)],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        for te in track_events(&pkts) {
            let uuid = te.track_uuid.unwrap();
            assert!(uuid < 128, "uuid {uuid} needs more than one varint byte");
        }
        let _ = threads;
    }

    #[test]
    fn events_declare_they_depend_on_the_interning_table() {
        let raw = export(
            &[Event::new(0, 1, 0, EventKind::Begin)],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        assert_eq!(
            pkts[0].sequence_flags,
            Some(SEQ_INCREMENTAL_STATE_CLEARED),
            "the stream must say where interning state begins"
        );
        for p in pkts.iter() {
            if matches!(p.data, Some(pb::trace_packet::Data::TrackEvent(_))) {
                assert_eq!(p.sequence_flags, Some(SEQ_NEEDS_INCREMENTAL_STATE));
            }
        }
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
        // process descriptor + interned name + thread descriptor + the event
        assert_eq!(pkts.len(), 4);
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
        assert_eq!(event_name(&pkts, te).len(), 400);
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
        let te = track_events(&pkts)[0];
        assert_eq!(event_name(&pkts, te), "<unknown>");
    }
}
