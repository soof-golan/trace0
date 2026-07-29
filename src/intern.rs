use ahash::AHashMap;
use parking_lot::RwLock;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use trace0_core::event::CODE_ID_MAX;
use trace0_core::{CodeInfo, CodeLookup};

pub struct Interner {
    inner: RwLock<InternerInner>,
}

struct InternerInner {
    map: AHashMap<usize, u32>,
    info: Vec<CodeInfo>,
    refs: Vec<Py<PyAny>>,
}

impl Interner {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(InternerInner {
                map: AHashMap::new(),
                info: Vec::new(),
                refs: Vec::new(),
            }),
        }
    }

    /// Fast path: lookup by code-object pointer. Returns None on miss.
    #[inline]
    pub fn lookup(&self, key: usize) -> Option<u32> {
        self.inner.read().map.get(&key).copied()
    }

    /// Slow path. Materializes qualname/filename/firstlineno from the code
    /// object. Runs once per code object.
    ///
    /// Returns `None` past `CODE_ID_MAX`: a code id shares its u32 with
    /// the event kind, so the 2^24-th distinct code object has no
    /// representation. Refusing it keeps the overflow from silently
    /// rewriting the kind bits of every subsequent event.
    pub fn insert(&self, _py: Python<'_>, code: &Bound<'_, PyAny>, key: usize) -> Option<u32> {
        let qualname = code
            .getattr("co_qualname")
            .and_then(|x| x.extract::<String>())
            .unwrap_or_else(|_| "<unknown>".to_string());
        let filename = code
            .getattr("co_filename")
            .and_then(|x| x.extract::<String>())
            .unwrap_or_default();
        let firstlineno = code
            .getattr("co_firstlineno")
            .and_then(|x| x.extract::<u32>())
            .unwrap_or(0);
        let py_ref = code.clone().unbind();

        let mut g = self.inner.write();
        if let Some(&id) = g.map.get(&key) {
            return Some(id);
        }
        if g.info.len() as u64 > CODE_ID_MAX as u64 {
            return None;
        }
        let id = g.info.len() as u32;
        g.info.push(CodeInfo {
            qualname,
            filename,
            firstlineno,
        });
        g.refs.push(py_ref);
        g.map.insert(key, id);
        Some(id)
    }
}

impl CodeLookup for Interner {
    fn code(&self, id: u32) -> Option<CodeInfo> {
        self.inner.read().info.get(id as usize).cloned()
    }
}
