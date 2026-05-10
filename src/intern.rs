use ahash::AHashMap;
use parking_lot::RwLock;
use pyo3::prelude::*;
use pyo3::types::PyAny;

#[derive(Clone, Debug)]
pub struct CodeInfo {
    pub qualname: String,
    pub filename: String,
    pub firstlineno: u32,
}

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
    /// Caller must follow up with `insert` (which extracts strings via Python).
    #[inline]
    pub fn lookup(&self, key: usize) -> Option<u32> {
        self.inner.read().map.get(&key).copied()
    }

    /// Slow path. Materializes qualname/filename/firstlineno from the code
    /// object while still attached to the interpreter, then takes the write
    /// lock. Idempotent on key.
    pub fn insert(&self, _py: Python<'_>, code: &Bound<'_, PyAny>, key: usize) -> u32 {
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
            return id;
        }
        let id = g.info.len() as u32;
        g.info.push(CodeInfo {
            qualname,
            filename,
            firstlineno,
        });
        g.refs.push(py_ref);
        g.map.insert(key, id);
        id
    }

    pub fn get(&self, id: u32) -> Option<CodeInfo> {
        self.inner.read().info.get(id as usize).cloned()
    }
}
