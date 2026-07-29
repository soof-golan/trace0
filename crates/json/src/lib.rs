//! Chrome Trace Event Format exporter.
//!
//! Correctness-first: this format exists so a trace can be eyeballed,
//! diffed, and asserted on. The protobuf exporter is the one tuned for
//! throughput.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use trace0_core::{CodeLookup, Event, Exporter, ThreadNames};

pub struct JsonExporter<W: Write + Send> {
    out: W,
    first: bool,
    pid: u32,
}

impl JsonExporter<BufWriter<File>> {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let f = File::create(path)?;
        Self::new(BufWriter::with_capacity(1 << 16, f))
    }
}

impl<W: Write + Send> JsonExporter<W> {
    pub fn new(mut out: W) -> io::Result<Self> {
        out.write_all(b"{\"traceEvents\":[")?;
        Ok(Self {
            out,
            first: true,
            pid: std::process::id(),
        })
    }

    /// Override the recorded pid. Tests use this so output is stable.
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = pid;
        self
    }

    fn separator(&mut self) -> io::Result<()> {
        if !self.first {
            self.out.write_all(b",")?;
        }
        self.first = false;
        Ok(())
    }
}

impl<W: Write + Send> Exporter for JsonExporter<W> {
    fn write_batch(
        &mut self,
        events: &[Event],
        codes: &dyn CodeLookup,
        _threads: &dyn ThreadNames,
    ) -> io::Result<()> {
        for ev in events {
            self.separator()?;
            let info = codes.code(ev.code_id()).unwrap_or_default();
            let kind = ev.kind();
            let entry = serde_json::json!({
                "name": if info.qualname.is_empty() { "<unknown>" } else { &info.qualname },
                "cat": "py",
                "ph": if kind.opens_slice() { "B" } else { "E" },
                "ts": ev.ts_ns as f64 / 1000.0,
                "pid": self.pid,
                "tid": ev.tid,
                "args": {
                    "file": info.filename,
                    "line": info.firstlineno,
                    "kind": kind.as_str(),
                }
            });
            serde_json::to_writer(&mut self.out, &entry)?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        _codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
        dropped: u64,
    ) -> io::Result<()> {
        let pid = self.pid;
        for (tid, name) in threads.snapshot() {
            self.separator()?;
            let entry = serde_json::json!({
                "name": "thread_name",
                "ph": "M",
                "pid": pid,
                "tid": tid,
                "args": { "name": name },
            });
            serde_json::to_writer(&mut self.out, &entry)?;
        }
        write!(self.out, "],\"droppedEvents\":{dropped}}}")?;
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
        serde_json::from_slice(&buf).expect("exporter must emit parseable JSON")
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
        // 1500 ns is 1.5 us — integer division used to round this to 1.
        assert_eq!(ev[1]["ts"], 1.5);
    }

    #[test]
    fn sub_microsecond_slices_keep_their_duration() {
        // Back-to-back calls are hundreds of ns apart. Truncating to
        // whole microseconds would collapse them to zero-width.
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
            .map(|e| (e["tid"].as_u64().unwrap(), e["args"]["name"].as_str().unwrap().to_string()))
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
