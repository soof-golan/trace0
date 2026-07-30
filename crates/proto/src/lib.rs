use prost::Message;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use trace0_core::{CodeLookup, Event, Exporter, ThreadNames};

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

const PACKET_TAG: u8 = (1 << 3) | 2;
const PROCESS_SEQUENCE_ID: u32 = 1;
const CATEGORY: &str = "py";

mod tag {
    pub const TIMESTAMP: u8 = 8 << 3;
    pub const TRACK_EVENT: u8 = (11 << 3) | 2;
    pub const TYPE: u8 = 9 << 3;
    pub const CATEGORY_IIDS: u8 = 3 << 3;
    pub const NAME_IID: u8 = 10 << 3;
    pub const SEQUENCE_FLAGS: u8 = 13 << 3;
    pub const SEQUENCE_ID: u8 = 10 << 3;
}

const SEQ_INCREMENTAL_STATE_CLEARED: u32 = 1;
const SEQ_NEEDS_INCREMENTAL_STATE: u32 = 2;

const NEEDS_STATE_FIELD: [u8; 2] = [tag::SEQUENCE_FLAGS, SEQ_NEEDS_INCREMENTAL_STATE as u8];

const CATEGORY_IID: u64 = 1;

/// One packet sequence per thread. Perfetto lets a sequence declare a default
/// track, so events on it need not name their own -- which is what makes a
/// globally unique track uuid affordable, since the uuid travels once per
/// thread instead of once per event.
struct Sequence {
    id: u32,
    header: Vec<u8>,
    name_iids: ahash::AHashMap<u32, u64>,
}

/// Sequence ids must not collide between processes merged into one trace,
/// because Perfetto scopes interned names to them and will interleave two
/// processes that ran at the same time. A slot of 0 belongs to the process
/// the user launched, whose ids stay one varint byte wide; a child passes its
/// pid, which is unique among live processes and costs it a wider id.
const SEQUENCES_PER_SLOT: u32 = 128;

pub struct ProtoExporter<W: Write + Send> {
    out: W,
    scratch: Vec<u8>,
    buf: Vec<u8>,
    templates: ahash::AHashMap<u64, (u32, u32)>,
    template_bytes: Vec<u8>,
    sequences: ahash::AHashMap<u32, Sequence>,
    process_emitted: bool,
    pid: i32,
    process_uuid: u64,
    slot: u32,
}

/// Track uuids must not collide between processes whose traces are merged
/// into one file, so each process owns the uuid block named by its pid.
fn process_uuid_of(pid: i32) -> u64 {
    ((pid as u32 as u64) << 32) | 1
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
            sequences: ahash::AHashMap::new(),
            process_emitted: false,
            pid: std::process::id() as i32,
            process_uuid: process_uuid_of(std::process::id() as i32),
            slot: 0,
        }
    }

    pub fn with_pid(mut self, pid: i32) -> Self {
        self.pid = pid;
        self.process_uuid = process_uuid_of(pid);
        self
    }

    pub fn with_slot(mut self, slot: u32) -> Self {
        self.slot = slot;
        self
    }

    fn write_packet(&mut self, packet: &pb::TracePacket) -> io::Result<()> {
        self.scratch.clear();
        packet
            .encode(&mut self.scratch)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let body_len = Varint::of(self.scratch.len() as u64);
        let mut header = [0u8; 1 + Varint::MAX];
        header[0] = PACKET_TAG;
        let n = 1 + body_len.as_slice().len();
        header[1..n].copy_from_slice(body_len.as_slice());
        self.out.write_all(&header[..n])?;
        self.out.write_all(&self.scratch)
    }

    fn name_iid(&mut self, tid: u32, code_id: u32, codes: &dyn CodeLookup) -> io::Result<u64> {
        let seq = &self.sequences[&tid];
        if let Some(&iid) = seq.name_iids.get(&code_id) {
            return Ok(iid);
        }
        let (iid, seq_id) = (seq.name_iids.len() as u64 + 1, seq.id);
        let name = codes
            .code(code_id)
            .map(|i| i.qualname)
            .unwrap_or_else(|| "<unknown>".into());
        self.write_packet(&pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(seq_id),
            sequence_flags: Some(SEQ_NEEDS_INCREMENTAL_STATE),
            trace_packet_defaults: None,
            interned_data: Some(pb::InternedData {
                event_names: vec![pb::EventName {
                    iid: Some(iid),
                    name: Some(name),
                }],
                event_categories: vec![],
            }),
            data: None,
        })?;
        self.sequences
            .get_mut(&tid)
            .expect("sequence exists")
            .name_iids
            .insert(code_id, iid);
        Ok(iid)
    }

    fn template(
        &mut self,
        tid: u32,
        code_id: u32,
        opens: bool,
        codes: &dyn CodeLookup,
    ) -> io::Result<(u32, u32)> {
        let keyed_code = if opens { code_id } else { 0 };
        let key = ((tid as u64) << 32) | ((keyed_code as u64) << 1) | opens as u64;
        if let Some(&range) = self.templates.get(&key) {
            return Ok(range);
        }

        let mut event = Vec::with_capacity(32);
        event.push(tag::TYPE);
        event.push(if opens {
            pb::track_event::Type::SliceBegin as u8
        } else {
            pb::track_event::Type::SliceEnd as u8
        });
        if opens {
            let name_iid = self.name_iid(tid, code_id, codes)?;
            event.push(tag::CATEGORY_IIDS);
            event.extend_from_slice(Varint::of(CATEGORY_IID).as_slice());
            event.push(tag::NAME_IID);
            event.extend_from_slice(Varint::of(name_iid).as_slice());
        }

        let start = self.template_bytes.len() as u32;
        let header = self.sequences[&tid].header.clone();
        self.template_bytes.extend_from_slice(&header);
        self.template_bytes.push(tag::TRACK_EVENT);
        self.template_bytes
            .extend_from_slice(Varint::of(event.len() as u64).as_slice());
        self.template_bytes.extend_from_slice(&event);
        let range = (start, self.template_bytes.len() as u32 - start);

        self.templates.insert(key, range);
        Ok(range)
    }

    fn push_event(&mut self, ts_ns: u64, template: (u32, u32)) {
        let ts = Varint::of(ts_ns);
        let (start, len) = template;

        let body_len = 1 + ts.as_slice().len() + len as usize;
        self.buf.push(PACKET_TAG);
        self.buf
            .extend_from_slice(Varint::of(body_len as u64).as_slice());
        self.buf.push(tag::TIMESTAMP);
        self.buf.extend_from_slice(ts.as_slice());
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
            trusted_packet_sequence_id: Some(self.process_sequence_id()),
            sequence_flags: Some(SEQ_INCREMENTAL_STATE_CLEARED),
            trace_packet_defaults: None,
            interned_data: None,
            data: Some(pb::trace_packet::Data::TrackDescriptor(
                pb::TrackDescriptor {
                    uuid: Some(self.process_uuid),
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

    fn process_sequence_id(&self) -> u32 {
        self.slot * SEQUENCES_PER_SLOT + PROCESS_SEQUENCE_ID
    }

    fn ensure_sequence(&mut self, tid: u32, threads: &dyn ThreadNames) -> io::Result<()> {
        if self.sequences.contains_key(&tid) {
            return Ok(());
        }
        let index = self.sequences.len() as u64;
        debug_assert!(
            index as u32 + PROCESS_SEQUENCE_ID < SEQUENCES_PER_SLOT,
            "more threads than a slot has sequence ids"
        );
        let id = self.slot * SEQUENCES_PER_SLOT + PROCESS_SEQUENCE_ID + 1 + index as u32;
        let track_uuid = self.process_uuid + 1 + index;

        let mut header = Vec::with_capacity(4);
        header.push(tag::SEQUENCE_ID);
        header.extend_from_slice(Varint::of(id as u64).as_slice());
        header.extend_from_slice(&NEEDS_STATE_FIELD);

        let pid = self.pid;
        let pkt = pb::TracePacket {
            timestamp: Some(0),
            trusted_packet_sequence_id: Some(id),
            sequence_flags: Some(SEQ_INCREMENTAL_STATE_CLEARED),
            trace_packet_defaults: Some(pb::TracePacketDefaults {
                track_event_defaults: Some(pb::TrackEventDefaults {
                    track_uuid: Some(track_uuid),
                }),
            }),
            interned_data: Some(pb::InternedData {
                event_categories: vec![pb::EventCategory {
                    iid: Some(CATEGORY_IID),
                    name: Some(CATEGORY.into()),
                }],
                event_names: vec![],
            }),
            data: Some(pb::trace_packet::Data::TrackDescriptor(
                pb::TrackDescriptor {
                    uuid: Some(track_uuid),
                    parent_uuid: Some(self.process_uuid),
                    thread: Some(pb::ThreadDescriptor {
                        pid: Some(pid),
                        tid: Some(tid as i32),
                        thread_name: threads.name(tid),
                    }),
                    ..Default::default()
                },
            )),
        };
        self.write_packet(&pkt)?;
        self.sequences.insert(
            tid,
            Sequence {
                id,
                header,
                name_iids: ahash::AHashMap::new(),
            },
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Varint {
    bytes: [u8; Varint::MAX],
    len: usize,
}

impl Varint {
    const MAX: usize = 10;

    fn of(mut v: u64) -> Self {
        let mut bytes = [0u8; Self::MAX];
        let mut len = 0;
        while v >= 0x80 {
            bytes[len] = (v as u8) | 0x80;
            v >>= 7;
            len += 1;
        }
        bytes[len] = v as u8;
        Self {
            bytes,
            len: len + 1,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
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
        self.buf.clear();
        for ev in events {
            self.ensure_sequence(ev.tid, threads)?;
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
        if dropped > 0 {
            self.ensure_process()?;
            let pkt = pb::TracePacket {
                timestamp: Some(0),
                trusted_packet_sequence_id: Some(self.process_sequence_id()),
                interned_data: None,
                sequence_flags: None,
                trace_packet_defaults: None,
                data: Some(pb::trace_packet::Data::TrackEvent(pb::TrackEvent {
                    r#type: Some(pb::track_event::Type::Instant as i32),
                    track_uuid: Some(self.process_uuid),
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

    /// Interned ids are scoped to their sequence: iid 1 on one thread's
    /// sequence names a different function than iid 1 on another's.
    fn interned_names(pkts: &[pb::TracePacket]) -> std::collections::HashMap<(u32, u64), String> {
        pkts.iter()
            .flat_map(|p| {
                let seq = p.trusted_packet_sequence_id.unwrap();
                p.interned_data
                    .iter()
                    .flat_map(|d| d.event_names.iter())
                    .map(move |n| ((seq, n.iid.unwrap()), n.name.clone().unwrap()))
            })
            .collect()
    }

    fn event_name(pkts: &[pb::TracePacket], seq: u32, te: &pb::TrackEvent) -> String {
        let names = interned_names(pkts);
        let key = (seq, te.name_iid.unwrap());
        names
            .get(&key)
            .unwrap_or_else(|| panic!("name_iid {key:?} was never interned"))
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

    /// The track each sequence declared as its default, which is where every
    /// event on that sequence lands.
    fn sequence_tracks(pkts: &[pb::TracePacket]) -> std::collections::HashMap<u32, u64> {
        pkts.iter()
            .filter_map(|p| {
                let uuid = p
                    .trace_packet_defaults
                    .as_ref()?
                    .track_event_defaults
                    .as_ref()?
                    .track_uuid?;
                Some((p.trusted_packet_sequence_id?, uuid))
            })
            .collect()
    }

    fn events_by_sequence(pkts: &[pb::TracePacket]) -> Vec<(u32, &pb::TrackEvent)> {
        pkts.iter()
            .filter_map(|p| match p.data.as_ref() {
                Some(pb::trace_packet::Data::TrackEvent(te)) => {
                    Some((p.trusted_packet_sequence_id.unwrap(), te))
                }
                _ => None,
            })
            .collect()
    }

    /// Where an event actually lands: its sequence's default track.
    fn track_of(pkts: &[pb::TracePacket], seq: u32) -> u64 {
        sequence_tracks(pkts)[&seq]
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
        let resolved: Vec<_> = events_by_sequence(&pkts)
            .iter()
            .filter(|(_, te)| te.r#type == Some(pb::track_event::Type::SliceBegin as i32))
            .map(|(seq, te)| names[&(*seq, te.name_iid.unwrap())].clone())
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
        assert_eq!(end.name_iid, None);
        assert!(end.category_iids.is_empty());
        assert!(end.track_uuid.is_none());
    }

    #[test]
    fn an_event_carries_no_track_of_its_own() {
        let raw = export(
            &[
                Event::new(0, 4242, 0, EventKind::Begin),
                Event::new(5, 4242, 0, EventKind::End),
            ],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let evs = events_by_sequence(&pkts);
        assert_eq!(evs.len(), 2);
        for (seq, te) in &evs {
            assert!(te.track_uuid.is_none(), "track repeated on the event");
            assert_ne!(track_of(&pkts, *seq), 0, "sequence declared no track");
        }
    }

    #[test]
    fn sequence_ids_stay_small_enough_to_encode_in_one_byte() {
        let events: Vec<_> = (0..40)
            .map(|i| Event::new(i, i as u32, 0, EventKind::Begin))
            .collect();
        let raw = export(&events, &fib_table(), &ThreadTable::new(), 0);
        for p in packets(&raw) {
            let seq = p.trusted_packet_sequence_id.unwrap();
            assert!(seq < 128, "sequence {seq} needs more than one varint byte");
        }
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
    fn interleaved_threads_and_names_never_borrow_each_others_track() {
        let mut codes = CodeTable::new();
        for n in ["a", "b", "c"] {
            codes.push_named(n);
        }
        let mut threads = ThreadTable::new();
        for tid in 1..=3u32 {
            threads.insert(tid, &format!("t{tid}"));
        }
        let events: Vec<Event> = (0..60)
            .map(|i| {
                let tid = 1 + (i % 3) as u32;
                let code = (i * 7 % 3) as u32;
                let kind = if i % 2 == 0 {
                    EventKind::Begin
                } else {
                    EventKind::End
                };
                Event::new(i as u64 * 100, tid, code, kind)
            })
            .collect();

        let raw = export(&events, &codes, &threads, 0);
        let pkts = packets(&raw);
        let uuid_of: std::collections::HashMap<u32, u64> = descriptors(&pkts)
            .iter()
            .filter_map(|d| d.thread.as_ref().map(|t| (t.tid() as u32, d.uuid())))
            .collect();

        let tes = events_by_sequence(&pkts);
        assert_eq!(tes.len(), events.len());
        for (ev, (seq, te)) in events.iter().zip(&tes) {
            assert_eq!(
                track_of(&pkts, *seq),
                uuid_of[&ev.tid],
                "an event landed on another thread's track"
            );
            if ev.kind().opens_slice() {
                let want = ["a", "b", "c"][ev.code_id() as usize];
                assert_eq!(
                    event_name(&pkts, *seq, te),
                    want,
                    "a memo served a stale name"
                );
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
        let (seq, te) = events_by_sequence(&pkts)[0];
        assert_eq!(event_name(&pkts, seq, te).len(), 400);
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
    fn a_varint_round_trips_at_every_width() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let encoded = Varint::of(v);
            let (decoded, used) = get_varint(encoded.as_slice());
            assert_eq!(decoded, v);
            assert_eq!(used, encoded.as_slice().len(), "trailing bytes for {v}");
        }
    }

    #[test]
    fn a_varint_is_never_wider_than_the_value_needs() {
        assert_eq!(Varint::of(0).as_slice().len(), 1);
        assert_eq!(Varint::of(127).as_slice().len(), 1);
        assert_eq!(Varint::of(128).as_slice().len(), 2);
        assert_eq!(Varint::of(u64::MAX).as_slice().len(), Varint::MAX);
    }

    #[test]
    fn interned_ids_start_at_one() {
        let mut codes = CodeTable::new();
        codes.push_named("alpha");
        codes.push_named("beta");
        let raw = export(
            &[
                Event::new(0, 7, 0, EventKind::Begin),
                Event::new(10, 7, 1, EventKind::Begin),
            ],
            &codes,
            &ThreadTable::new(),
            0,
        );
        let mut iids: Vec<u64> = interned_names(&packets(&raw))
            .into_keys()
            .map(|(_, iid)| iid)
            .collect();
        iids.sort();
        assert_eq!(iids, vec![1, 2], "zero is Perfetto's \"unset\"");
    }

    #[test]
    fn the_process_is_uuid_one_and_threads_follow_in_first_seen_order() {
        let raw = export(
            &[
                Event::new(0, 30, 0, EventKind::Begin),
                Event::new(10, 10, 0, EventKind::Begin),
                Event::new(20, 20, 0, EventKind::Begin),
            ],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let ds = descriptors(&pkts);
        let proc = ds.iter().find(|d| d.process.is_some()).unwrap();
        let base = proc.uuid.unwrap();
        let assigned: Vec<(i32, u64)> = ds
            .iter()
            .filter_map(|d| Some((d.thread.as_ref()?.tid?, d.uuid?)))
            .collect();
        assert_eq!(
            assigned,
            vec![(30, base + 1), (10, base + 2), (20, base + 3)]
        );
    }

    #[test]
    fn a_child_slot_keeps_its_sequences_clear_of_the_root() {
        let events = [
            Event::new(0, 10, 0, EventKind::Begin),
            Event::new(1, 20, 0, EventKind::Begin),
        ];
        let sequences = |slot: u32| -> std::collections::HashSet<u32> {
            let mut buf = Vec::new();
            {
                let mut ex = ProtoExporter::new(&mut buf).with_pid(7).with_slot(slot);
                ex.write_batch(&events, &fib_table(), &ThreadTable::new())
                    .unwrap();
                ex.finish(&fib_table(), &ThreadTable::new(), 0).unwrap();
            }
            packets(&buf)
                .iter()
                .filter_map(|p| p.trusted_packet_sequence_id)
                .collect()
        };
        let root = sequences(0);
        assert!(
            root.iter().all(|&s| s < 128),
            "the root process must keep one-byte sequence ids: {root:?}"
        );
        assert!(root.is_disjoint(&sequences(4242)));
        assert!(sequences(4242).is_disjoint(&sequences(4243)));
    }

    #[test]
    fn two_processes_never_share_a_track_uuid() {
        let events = [
            Event::new(0, 10, 0, EventKind::Begin),
            Event::new(1, 20, 0, EventKind::Begin),
        ];
        let uuids = |pid: i32| -> std::collections::HashSet<u64> {
            let mut buf = Vec::new();
            {
                let mut ex = ProtoExporter::new(&mut buf).with_pid(pid);
                ex.write_batch(&events, &fib_table(), &ThreadTable::new())
                    .unwrap();
                ex.finish(&fib_table(), &ThreadTable::new(), 0).unwrap();
            }
            descriptors(&packets(&buf))
                .iter()
                .filter_map(|d| d.uuid)
                .collect()
        };
        let (a, b) = (uuids(4242), uuids(4243));
        assert_eq!(a.len(), 3, "one process track and two thread tracks");
        assert!(
            a.is_disjoint(&b),
            "two processes claimed the same track: {:?}",
            a.intersection(&b).collect::<Vec<_>>()
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
        let (seq, _) = events_by_sequence(&pkts)[0];
        assert_eq!(track_of(&pkts, seq), thread.uuid.unwrap());
    }

    #[test]
    fn each_thread_gets_its_own_sequence() {
        let raw = export(
            &[
                Event::new(0, 10, 0, EventKind::Begin),
                Event::new(1, 20, 0, EventKind::Begin),
                Event::new(2, 10, 0, EventKind::End),
            ],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let evs = events_by_sequence(&pkts);
        assert_eq!(evs[0].0, evs[2].0, "one thread, one sequence");
        assert_ne!(evs[0].0, evs[1].0, "two threads shared a sequence");
        assert_ne!(track_of(&pkts, evs[0].0), track_of(&pkts, evs[1].0));
    }

    #[test]
    fn every_sequence_opens_by_clearing_incremental_state() {
        let raw = export(
            &[
                Event::new(0, 10, 0, EventKind::Begin),
                Event::new(1, 20, 0, EventKind::Begin),
            ],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let pkts = packets(&raw);
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for p in &pkts {
            let seq = p.trusted_packet_sequence_id.unwrap();
            if seen.insert(seq) {
                assert_eq!(
                    p.sequence_flags,
                    Some(SEQ_INCREMENTAL_STATE_CLEARED),
                    "sequence {seq} started without clearing its state"
                );
            }
        }
        assert!(seen.len() >= 3, "a process sequence and one per thread");
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
        let (seq, te) = events_by_sequence(&pkts)[0];
        assert_eq!(event_name(&pkts, seq, te), "<unknown>");
    }
}
