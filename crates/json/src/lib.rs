use std::io::{self, Write};
use std::path::Path;
use trace0_core::{CodeInfo, CodeLookup, Event, Exporter, SharedFile, ThreadNames};

/// Chrome's JSON Array Format: a bare array of entries, each followed by a
/// comma, with no closing bracket. Every traced process can append to the same
/// file that way, and a process killed before it finishes still leaves a file
/// that reads back to the last whole entry.
pub struct JsonExporter<W: Write + Send> {
    out: W,
    buf: Vec<u8>,
    pid: u32,
    templates: ahash::AHashMap<u32, Template>,
}

struct Template {
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl Template {
    fn of(info: &CodeInfo) -> Self {
        let mut head = Vec::new();
        head.extend_from_slice(b"{\"name\":");
        push_string(
            &mut head,
            if info.qualname.is_empty() {
                "<unknown>"
            } else {
                &info.qualname
            },
        );
        head.extend_from_slice(b",\"cat\":\"py\",\"ph\":\"");

        let mut tail = Vec::new();
        tail.extend_from_slice(b",\"args\":{\"file\":");
        push_string(&mut tail, &info.filename);
        tail.extend_from_slice(b",\"line\":");
        push_u64(&mut tail, info.firstlineno as u64);
        tail.extend_from_slice(b",\"kind\":\"");

        Self { head, tail }
    }
}

fn push_u64(out: &mut Vec<u8>, mut v: u64) {
    let mut digits = [0u8; 20];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        out.push(digits[n]);
    }
}

fn push_micros(out: &mut Vec<u8>, ts_ns: u64) {
    push_u64(out, ts_ns / 1_000);
    let frac = ts_ns % 1_000;
    out.push(b'.');
    out.push(b'0' + (frac / 100) as u8);
    out.push(b'0' + (frac / 10 % 10) as u8);
    out.push(b'0' + (frac % 10) as u8);
}

fn push_string(out: &mut Vec<u8>, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for b in s.bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x09 => out.extend_from_slice(b"\\t"),
            0x0a => out.extend_from_slice(b"\\n"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0d => out.extend_from_slice(b"\\r"),
            0x00..=0x1f => {
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(b >> 4) as usize]);
                out.push(HEX[(b & 0x0f) as usize]);
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

impl JsonExporter<SharedFile> {
    /// The process the user launched opens the array the others append into.
    /// The bracket is committed before this returns: a child that forks away
    /// and commits first would otherwise land ahead of it in the file.
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut out = SharedFile::create(path)?;
        out.write_all(b"[")?;
        out.flush()?;
        Self::new(out)
    }

    pub fn append(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::new(SharedFile::append(path)?)
    }
}

impl<W: Write + Send> JsonExporter<W> {
    pub fn new(mut out: W) -> io::Result<Self> {
        let _ = &mut out;
        Ok(Self {
            out,
            buf: Vec::with_capacity(1 << 18),
            pid: std::process::id(),
            templates: ahash::AHashMap::new(),
        })
    }

    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = pid;
        self
    }

}

impl<W: Write + Send> Exporter for JsonExporter<W> {
    fn write_batch(
        &mut self,
        events: &[Event],
        codes: &dyn CodeLookup,
        _threads: &dyn ThreadNames,
    ) -> io::Result<()> {
        self.buf.clear();
        for ev in events {
            let id = ev.code_id();
            let template = self
                .templates
                .entry(id)
                .or_insert_with(|| Template::of(&codes.code(id).unwrap_or_default()));
            let kind = ev.kind();

            self.buf.extend_from_slice(&template.head);
            self.buf.push(if kind.opens_slice() { b'B' } else { b'E' });
            self.buf.extend_from_slice(b"\",\"ts\":");
            push_micros(&mut self.buf, ev.ts_ns);
            self.buf.extend_from_slice(b",\"pid\":");
            push_u64(&mut self.buf, self.pid as u64);
            self.buf.extend_from_slice(b",\"tid\":");
            push_u64(&mut self.buf, ev.tid as u64);
            self.buf.extend_from_slice(&template.tail);
            self.buf.extend_from_slice(kind.as_str().as_bytes());
            self.buf.extend_from_slice(b"\"}},");
        }
        self.out.write_all(&self.buf)?;
        self.out.flush()
    }

    fn finish(
        &mut self,
        _codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
        dropped: u64,
    ) -> io::Result<()> {
        let pid = self.pid;
        for (tid, name) in threads.snapshot() {
            let entry = serde_json::json!({
                "name": "thread_name",
                "ph": "M",
                "pid": pid,
                "tid": tid,
                "args": { "name": name },
            });
            serde_json::to_writer(&mut self.out, &entry)?;
            self.out.write_all(b",")?;
        }
        if dropped > 0 {
            let entry = serde_json::json!({
                "name": "trace0_dropped_events",
                "ph": "M",
                "pid": pid,
                "tid": 0,
                "args": { "count": dropped },
            });
            serde_json::to_writer(&mut self.out, &entry)?;
            self.out.write_all(b",")?;
        }
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use trace0_core::{CodeInfo, CodeTable, EventKind, ThreadTable};

    fn export(events: &[Event], codes: &CodeTable, threads: &ThreadTable, dropped: u64) -> Value {
        let mut buf = Vec::new();
        {
            let mut ex = JsonExporter::new(&mut buf).unwrap().with_pid(4242);
            ex.write_batch(events, codes, threads).unwrap();
            ex.finish(codes, threads, dropped).unwrap();
        }
        parse_array(&buf)
    }

    /// The exporter writes a bare comma-separated array so several processes
    /// can append to one file. Rebuild the document the assertions expect.
    fn parse_array(buf: &[u8]) -> Value {
        let text = std::str::from_utf8(buf).unwrap();
        let body = text.trim_end().trim_end_matches(',');
        let events: Value = serde_json::from_str(&format!("[{body}]"))
            .expect("exporter must emit parseable JSON");
        let dropped = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "trace0_dropped_events")
            .and_then(|e| e["args"]["count"].as_u64())
            .unwrap_or(0);
        serde_json::json!({ "traceEvents": events, "droppedEvents": dropped })
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

    #[test]
    fn empty_trace_is_still_valid_json() {
        let v = export(&[], &CodeTable::new(), &ThreadTable::new(), 0);
        assert_eq!(v["traceEvents"].as_array().unwrap().len(), 0);
        assert_eq!(v["droppedEvents"], 0);
    }

    #[test]
    fn timestamps_are_microseconds() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(1_500, 1, 0, EventKind::End),
        ];
        let v = export(&events, &fib_table(), &ThreadTable::new(), 0);
        let ev = v["traceEvents"].as_array().unwrap();
        assert_eq!(ev[0]["ts"], 0.0);
        assert_eq!(ev[1]["ts"], 1.5);
    }

    #[test]
    fn sub_microsecond_slices_keep_their_duration() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(300, 1, 0, EventKind::End),
        ];
        let v = export(&events, &fib_table(), &ThreadTable::new(), 0);
        let ev = v["traceEvents"].as_array().unwrap();
        assert!(ev[1]["ts"].as_f64().unwrap() > ev[0]["ts"].as_f64().unwrap());
    }

    #[test]
    fn slice_phases_follow_event_kind() {
        let kinds = [
            (EventKind::Begin, "B"),
            (EventKind::Resume, "B"),
            (EventKind::Throw, "B"),
            (EventKind::End, "E"),
            (EventKind::Yield, "E"),
            (EventKind::Unwind, "E"),
        ];
        for (kind, phase) in kinds {
            let v = export(
                &[Event::new(0, 1, 0, kind)],
                &fib_table(),
                &ThreadTable::new(),
                0,
            );
            let ev = &v["traceEvents"][0];
            assert_eq!(ev["ph"], phase, "{kind:?} should be phase {phase}");
            assert_eq!(ev["args"]["kind"], kind.as_str());
        }
    }

    #[test]
    fn code_info_reaches_the_output() {
        let v = export(
            &[Event::new(0, 9, 0, EventKind::Begin)],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let ev = &v["traceEvents"][0];
        assert_eq!(ev["name"], "fib");
        assert_eq!(ev["args"]["file"], "demo.py");
        assert_eq!(ev["args"]["line"], 12);
        assert_eq!(ev["tid"], 9);
        assert_eq!(ev["cat"], "py");
    }

    #[test]
    fn unresolvable_code_ids_do_not_break_the_trace() {
        let v = export(
            &[Event::new(0, 1, 999, EventKind::Begin)],
            &CodeTable::new(),
            &ThreadTable::new(),
            0,
        );
        assert_eq!(v["traceEvents"][0]["name"], "<unknown>");
    }

    #[test]
    fn every_named_thread_gets_a_metadata_record() {
        let mut threads = ThreadTable::new();
        threads.insert(1, "MainThread");
        threads.insert(2, "cpu-a");
        threads.insert(3, "cpu-b");
        let v = export(
            &[Event::new(0, 1, 0, EventKind::Begin)],
            &fib_table(),
            &threads,
            0,
        );
        let meta: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "M")
            .map(|e| {
                (
                    e["tid"].as_u64().unwrap(),
                    e["args"]["name"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            meta,
            vec![
                (1, "MainThread".into()),
                (2, "cpu-a".into()),
                (3, "cpu-b".into())
            ]
        );
    }

    #[test]
    fn names_that_need_escaping_survive_the_writer() {
        let mut t = CodeTable::new();
        t.push(CodeInfo {
            qualname: "say(\"hi\")\n\tand\\stop\u{1}\u{1f600}".into(),
            filename: "C:\\tmp\\a\"b\r.py".into(),
            firstlineno: 7,
        });
        let v = export(
            &[Event::new(0, 1, 0, EventKind::Begin)],
            &t,
            &ThreadTable::new(),
            0,
        );
        let ev = &v["traceEvents"][0];
        assert_eq!(ev["name"], "say(\"hi\")\n\tand\\stop\u{1}\u{1f600}");
        assert_eq!(ev["args"]["file"], "C:\\tmp\\a\"b\r.py");
        assert_eq!(ev["args"]["line"], 7);
    }

    #[test]
    fn timestamps_keep_nanosecond_precision() {
        let events = [
            Event::new(1_234_567, 1, 0, EventKind::Begin),
            Event::new(1_234_568, 1, 0, EventKind::End),
        ];
        let v = export(&events, &fib_table(), &ThreadTable::new(), 0);
        let ev = v["traceEvents"].as_array().unwrap();
        assert_eq!(ev[0]["ts"].as_f64().unwrap(), 1234.567);
        assert_eq!(ev[1]["ts"].as_f64().unwrap(), 1234.568);
    }

    #[test]
    fn every_event_carries_the_full_chrome_trace_shape() {
        let v = export(
            &[Event::new(5_000, 3, 0, EventKind::Begin)],
            &fib_table(),
            &ThreadTable::new(),
            0,
        );
        let ev = &v["traceEvents"][0];
        let keys: Vec<&str> = ev.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, ["args", "cat", "name", "ph", "pid", "tid", "ts"]);
        let args: Vec<&str> = ev["args"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(args, ["file", "kind", "line"]);
    }

    #[test]
    fn dropped_events_are_reported() {
        let v = export(&[], &CodeTable::new(), &ThreadTable::new(), 77);
        assert_eq!(v["droppedEvents"], 77);
    }

    #[test]
    fn nesting_stays_balanced_through_the_exporter() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(100, 1, 0, EventKind::Begin),
            Event::new(200, 1, 0, EventKind::End),
            Event::new(300, 1, 0, EventKind::End),
        ];
        let v = export(&events, &fib_table(), &ThreadTable::new(), 0);
        let mut depth = 0i32;
        for e in v["traceEvents"].as_array().unwrap() {
            depth += if e["ph"] == "B" { 1 } else { -1 };
            assert!(depth >= 0, "slice stack underflowed");
        }
        assert_eq!(depth, 0);
    }
}
