use crate::event::Event;
use ahash::AHashMap;
use std::io;

/// What an exporter needs to know about a Python code object.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CodeInfo {
    pub qualname: String,
    pub filename: String,
    pub firstlineno: u32,
}

/// Resolves interned code ids back to source information.
///
/// The tracer's real implementation is backed by live Python code
/// objects; [`CodeTable`] is the plain in-memory one used by tests and
/// by anything replaying a trace.
pub trait CodeLookup: Send + Sync {
    fn code(&self, id: u32) -> Option<CodeInfo>;
}

/// Resolves OS thread ids to thread names.
pub trait ThreadNames: Send + Sync {
    fn name(&self, tid: u32) -> Option<String>;
    fn snapshot(&self) -> Vec<(u32, String)>;
}

/// Consumes decoded events and writes them out in some trace format.
pub trait Exporter: Send {
    fn write_batch(
        &mut self,
        events: &[Event],
        codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
    ) -> io::Result<()>;

    /// Flush and close out the file. `dropped` is the number of events
    /// lost to queue overflow across the whole run.
    fn finish(
        &mut self,
        codes: &dyn CodeLookup,
        threads: &dyn ThreadNames,
        dropped: u64,
    ) -> io::Result<()>;
}

/// Straightforward id→[`CodeInfo`] storage.
#[derive(Default)]
pub struct CodeTable {
    info: Vec<CodeInfo>,
}

impl CodeTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, info: CodeInfo) -> u32 {
        self.info.push(info);
        (self.info.len() - 1) as u32
    }

    /// Convenience for callers that only care about the name.
    pub fn push_named(&mut self, qualname: &str) -> u32 {
        self.push(CodeInfo {
            qualname: qualname.to_string(),
            ..Default::default()
        })
    }

    pub fn len(&self) -> usize {
        self.info.len()
    }

    pub fn is_empty(&self) -> bool {
        self.info.is_empty()
    }
}

impl CodeLookup for CodeTable {
    fn code(&self, id: u32) -> Option<CodeInfo> {
        self.info.get(id as usize).cloned()
    }
}

/// Straightforward tid→name storage.
#[derive(Default)]
pub struct ThreadTable {
    names: AHashMap<u32, String>,
}

impl ThreadTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, tid: u32, name: &str) {
        self.names.insert(tid, name.to_string());
    }
}

impl ThreadNames for ThreadTable {
    fn name(&self, tid: u32) -> Option<String> {
        self.names.get(&tid).cloned()
    }

    fn snapshot(&self) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> =
            self.names.iter().map(|(k, v)| (*k, v.clone())).collect();
        out.sort_by_key(|(tid, _)| *tid);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_table_hands_back_what_was_pushed() {
        let mut t = CodeTable::new();
        let id = t.push(CodeInfo {
            qualname: "fib".into(),
            filename: "demo.py".into(),
            firstlineno: 12,
        });
        assert_eq!(id, 0);
        let info = t.code(0).unwrap();
        assert_eq!(info.qualname, "fib");
        assert_eq!(info.filename, "demo.py");
        assert_eq!(info.firstlineno, 12);
    }

    #[test]
    fn unknown_code_ids_resolve_to_nothing() {
        let t = CodeTable::new();
        assert_eq!(t.code(7), None);
    }

    #[test]
    fn thread_table_snapshot_is_ordered() {
        let mut t = ThreadTable::new();
        t.insert(30, "cpu-b");
        t.insert(10, "MainThread");
        t.insert(20, "cpu-a");
        assert_eq!(
            t.snapshot(),
            vec![
                (10, "MainThread".to_string()),
                (20, "cpu-a".to_string()),
                (30, "cpu-b".to_string()),
            ]
        );
    }
}
