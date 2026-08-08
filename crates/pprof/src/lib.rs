use prost::Message;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use trace0_core::{CodeLookup, Event, Exporter, SharedFile, ThreadNames};

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/perftools.profiles.rs"));
}

struct Fold {
    stack: Vec<u32>,
    last_ts: u64,
}

pub struct PprofExporter<W: Write + Send> {
    out: W,
    folds: ahash::AHashMap<u32, Fold>,
    self_ns: BTreeMap<Vec<u32>, i64>,
}

impl PprofExporter<SharedFile> {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self::new(SharedFile::create(path)?))
    }
}

impl<W: Write + Send> PprofExporter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            folds: ahash::AHashMap::new(),
            self_ns: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
struct Strings {
    table: Vec<String>,
    index: ahash::AHashMap<String, i64>,
}

impl Strings {
    fn with_empty_head() -> Self {
        let mut strings = Self::default();
        strings.id("");
        strings
    }

    fn id(&mut self, text: &str) -> i64 {
        if let Some(&i) = self.index.get(text) {
            return i;
        }
        let i = self.table.len() as i64;
        self.table.push(text.to_string());
        self.index.insert(text.to_string(), i);
        i
    }
}

fn location_id(code_id: u32) -> u64 {
    code_id as u64 + 1
}

impl<W: Write + Send> Exporter for PprofExporter<W> {
    fn write_batch(
        &mut self,
        events: &[Event],
        _codes: &dyn CodeLookup,
        _threads: &dyn ThreadNames,
    ) -> io::Result<()> {
        for ev in events {
            let fold = self.folds.entry(ev.tid).or_insert(Fold {
                stack: Vec::new(),
                last_ts: ev.ts_ns,
            });
            let elapsed = ev.ts_ns.saturating_sub(fold.last_ts);
            fold.last_ts = ev.ts_ns;
            if elapsed > 0 && !fold.stack.is_empty() {
                match self.self_ns.get_mut(fold.stack.as_slice()) {
                    Some(ns) => *ns += elapsed as i64,
                    None => {
                        self.self_ns.insert(fold.stack.clone(), elapsed as i64);
                    }
                }
            }
            if ev.kind().opens_slice() {
                fold.stack.push(ev.code_id());
            } else {
                fold.stack.pop();
            }
        }
        Ok(())
    }

    fn finish(
        &mut self,
        codes: &dyn CodeLookup,
        _threads: &dyn ThreadNames,
        dropped: u64,
    ) -> io::Result<()> {
        let mut strings = Strings::with_empty_head();
        let mut profile = pb::Profile {
            sample_type: vec![pb::ValueType {
                r#type: strings.id("wall"),
                unit: strings.id("nanoseconds"),
            }],
            ..Default::default()
        };

        let mut used: BTreeSet<u32> = BTreeSet::new();
        for stack in self.self_ns.keys() {
            used.extend(stack);
        }
        for &code_id in &used {
            let info = codes.code(code_id).unwrap_or_default();
            let name = strings.id(if info.qualname.is_empty() {
                "<unknown>"
            } else {
                &info.qualname
            });
            let filename = strings.id(&info.filename);
            let id = location_id(code_id);
            profile.function.push(pb::Function {
                id,
                name,
                system_name: name,
                filename,
                start_line: info.firstlineno as i64,
            });
            profile.location.push(pb::Location {
                id,
                line: vec![pb::Line {
                    function_id: id,
                    line: info.firstlineno as i64,
                    column: 0,
                }],
                ..Default::default()
            });
        }

        for (stack, ns) in std::mem::take(&mut self.self_ns) {
            profile.sample.push(pb::Sample {
                location_id: stack.iter().rev().map(|&c| location_id(c)).collect(),
                value: vec![ns],
                label: vec![],
            });
        }

        if dropped > 0 {
            profile
                .comment
                .push(strings.id(&format!("trace0: {dropped} events dropped")));
        }
        profile.string_table = strings.table;

        let mut buf = Vec::with_capacity(profile.encoded_len());
        profile
            .encode(&mut buf)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.out.write_all(&buf)?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace0_core::{CodeInfo, CodeTable, EventKind, ThreadTable};

    fn export(events: &[Event], codes: &CodeTable, dropped: u64) -> pb::Profile {
        export_batched(&[events], codes, dropped)
    }

    fn export_batched(batches: &[&[Event]], codes: &CodeTable, dropped: u64) -> pb::Profile {
        let mut buf = Vec::new();
        {
            let threads = ThreadTable::new();
            let mut ex = PprofExporter::new(&mut buf);
            for batch in batches {
                ex.write_batch(batch, codes, &threads).unwrap();
            }
            ex.finish(codes, &threads, dropped).unwrap();
        }
        let profile = pb::Profile::decode(&buf[..]).expect("decodable profile");
        check_references(&profile);
        profile
    }

    fn check_references(p: &pb::Profile) {
        assert_eq!(p.string_table.first().map(String::as_str), Some(""));
        let in_table = |i: i64| (i as usize) < p.string_table.len();
        for s in &p.sample {
            assert_eq!(s.value.len(), p.sample_type.len());
            for id in &s.location_id {
                let loc = p
                    .location
                    .iter()
                    .find(|l| l.id == *id)
                    .expect("sample points at a missing location");
                for line in &loc.line {
                    let f = p
                        .function
                        .iter()
                        .find(|f| f.id == line.function_id)
                        .expect("location points at a missing function");
                    assert!(in_table(f.name));
                    assert!(in_table(f.filename));
                }
            }
        }
        for vt in &p.sample_type {
            assert!(in_table(vt.r#type));
            assert!(in_table(vt.unit));
        }
        for c in &p.comment {
            assert!(in_table(*c));
        }
    }

    fn s(p: &pb::Profile, i: i64) -> &str {
        &p.string_table[i as usize]
    }

    fn stack_names(p: &pb::Profile, sample: &pb::Sample) -> Vec<String> {
        sample
            .location_id
            .iter()
            .map(|id| {
                let loc = p.location.iter().find(|l| l.id == *id).unwrap();
                let f = p
                    .function
                    .iter()
                    .find(|f| f.id == loc.line[0].function_id)
                    .unwrap();
                s(p, f.name).to_string()
            })
            .collect()
    }

    fn samples(p: &pb::Profile) -> Vec<(Vec<String>, i64)> {
        p.sample
            .iter()
            .map(|sample| (stack_names(p, sample), sample.value[0]))
            .collect()
    }

    fn self_ns_of(p: &pb::Profile, leaf_first: &[&str]) -> i64 {
        samples(p)
            .into_iter()
            .find(|(names, _)| names == leaf_first)
            .unwrap_or_else(|| panic!("no sample for stack {leaf_first:?}"))
            .1
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

    fn outer_inner_table() -> CodeTable {
        let mut t = CodeTable::new();
        t.push_named("outer");
        t.push_named("inner");
        t
    }

    #[test]
    fn an_empty_trace_still_declares_wall_nanoseconds() {
        let p = export(&[], &CodeTable::new(), 0);
        assert_eq!(p.sample_type.len(), 1);
        assert_eq!(s(&p, p.sample_type[0].r#type), "wall");
        assert_eq!(s(&p, p.sample_type[0].unit), "nanoseconds");
        assert!(p.sample.is_empty());
    }

    #[test]
    fn a_call_charges_the_time_between_begin_and_end_to_itself() {
        let events = [
            Event::new(1_000, 1, 0, EventKind::Begin),
            Event::new(1_100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert_eq!(samples(&p), vec![(vec!["fib".to_string()], 100)]);
    }

    #[test]
    fn a_callee_takes_its_time_out_of_the_callers_self_time() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(30, 1, 1, EventKind::Begin),
            Event::new(80, 1, 1, EventKind::End),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &outer_inner_table(), 0);
        assert_eq!(self_ns_of(&p, &["outer"]), 50);
        assert_eq!(self_ns_of(&p, &["inner", "outer"]), 50);
    }

    #[test]
    fn the_leaf_comes_first_in_a_samples_location_ids() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(10, 1, 1, EventKind::Begin),
            Event::new(20, 1, 1, EventKind::End),
            Event::new(30, 1, 0, EventKind::End),
        ];
        let p = export(&events, &outer_inner_table(), 0);
        let deepest = p.sample.iter().max_by_key(|s| s.location_id.len()).unwrap();
        assert_eq!(stack_names(&p, deepest), vec!["inner", "outer"]);
    }

    #[test]
    fn two_calls_through_one_stack_merge_into_one_sample() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(100, 1, 0, EventKind::End),
            Event::new(200, 1, 0, EventKind::Begin),
            Event::new(240, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert_eq!(samples(&p), vec![(vec!["fib".to_string()], 140)]);
    }

    #[test]
    fn a_yielded_generator_is_not_charged_while_suspended() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(10, 1, 0, EventKind::Yield),
            Event::new(50, 1, 0, EventKind::Resume),
            Event::new(60, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert_eq!(self_ns_of(&p, &["fib"]), 20);
    }

    #[test]
    fn a_throw_reopens_the_frame_and_an_unwind_pops_it() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(10, 1, 1, EventKind::Throw),
            Event::new(30, 1, 1, EventKind::Unwind),
            Event::new(50, 1, 0, EventKind::End),
        ];
        let p = export(&events, &outer_inner_table(), 0);
        assert_eq!(self_ns_of(&p, &["outer"]), 30);
        assert_eq!(self_ns_of(&p, &["inner", "outer"]), 20);
    }

    #[test]
    fn threads_are_folded_apart_then_summed_together() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(50, 2, 0, EventKind::Begin),
            Event::new(70, 2, 0, EventKind::End),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert_eq!(samples(&p), vec![(vec!["fib".to_string()], 120)]);
    }

    #[test]
    fn one_thread_never_charges_time_to_another_threads_stack() {
        let mut codes = CodeTable::new();
        codes.push_named("on_one");
        codes.push_named("on_two");
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(10, 2, 1, EventKind::Begin),
            Event::new(40, 2, 1, EventKind::End),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &codes, 0);
        assert_eq!(self_ns_of(&p, &["on_one"]), 100);
        assert_eq!(self_ns_of(&p, &["on_two"]), 30);
    }

    #[test]
    fn folding_state_survives_batch_boundaries() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(30, 1, 1, EventKind::Begin),
            Event::new(80, 1, 1, EventKind::End),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let whole = export(&events, &outer_inner_table(), 0);
        let split = export_batched(&[&events[..2], &events[2..]], &outer_inner_table(), 0);
        assert_eq!(samples(&whole), samples(&split));
    }

    #[test]
    fn time_before_a_threads_first_event_is_not_charged() {
        let events = [
            Event::new(5_000, 1, 0, EventKind::Begin),
            Event::new(5_100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert_eq!(self_ns_of(&p, &["fib"]), 100);
    }

    #[test]
    fn an_end_with_no_begin_charges_nothing() {
        let events = [
            Event::new(0, 1, 0, EventKind::End),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert!(p.sample.is_empty());
    }

    #[test]
    fn functions_carry_qualname_filename_and_first_line() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert_eq!(p.function.len(), 1);
        let f = &p.function[0];
        assert_eq!(s(&p, f.name), "fib");
        assert_eq!(s(&p, f.system_name), "fib");
        assert_eq!(s(&p, f.filename), "demo.py");
        assert_eq!(f.start_line, 12);
        assert_eq!(p.location.len(), 1);
        assert_eq!(p.location[0].line[0].line, 12);
    }

    #[test]
    fn a_shared_function_is_tabled_once_across_stacks() {
        let mut codes = CodeTable::new();
        codes.push_named("a");
        codes.push_named("b");
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(10, 1, 1, EventKind::Begin),
            Event::new(20, 1, 1, EventKind::End),
            Event::new(30, 1, 0, EventKind::End),
            Event::new(40, 1, 1, EventKind::Begin),
            Event::new(50, 1, 1, EventKind::End),
        ];
        let p = export(&events, &codes, 0);
        assert_eq!(p.function.len(), 2);
        assert_eq!(p.location.len(), 2);
        assert_eq!(p.sample.len(), 3);
    }

    #[test]
    fn location_and_function_ids_are_nonzero() {
        let events = [
            Event::new(0, 1, 0, EventKind::Begin),
            Event::new(100, 1, 0, EventKind::End),
        ];
        let p = export(&events, &fib_table(), 0);
        assert!(p.location.iter().all(|l| l.id != 0));
        assert!(p.function.iter().all(|f| f.id != 0));
    }

    #[test]
    fn unresolvable_code_ids_become_unknown() {
        let events = [
            Event::new(0, 1, 999, EventKind::Begin),
            Event::new(100, 1, 999, EventKind::End),
        ];
        let p = export(&events, &CodeTable::new(), 0);
        assert_eq!(self_ns_of(&p, &["<unknown>"]), 100);
    }

    #[test]
    fn dropped_events_are_reported_in_a_comment() {
        let p = export(&[], &CodeTable::new(), 77);
        let comments: Vec<&str> = p.comment.iter().map(|&i| s(&p, i)).collect();
        assert_eq!(comments, vec!["trace0: 77 events dropped"]);
    }

    #[test]
    fn a_lossless_profile_carries_no_comment() {
        let p = export(&[], &CodeTable::new(), 0);
        assert!(p.comment.is_empty());
    }

    #[test]
    fn the_profile_is_written_uncompressed() {
        let mut buf = Vec::new();
        {
            let threads = ThreadTable::new();
            let mut ex = PprofExporter::new(&mut buf);
            ex.write_batch(
                &[
                    Event::new(0, 1, 0, EventKind::Begin),
                    Event::new(100, 1, 0, EventKind::End),
                ],
                &fib_table(),
                &threads,
            )
            .unwrap();
            ex.finish(&fib_table(), &threads, 0).unwrap();
        }
        assert!(!buf.starts_with(&[0x1f, 0x8b]));
        pb::Profile::decode(&buf[..]).expect("raw bytes decode without gunzip");
    }
}
