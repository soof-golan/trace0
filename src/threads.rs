use ahash::AHashMap;
use parking_lot::RwLock;
use pyo3::prelude::*;

struct Entry {
    name: String,
    definitive: bool,
}

pub struct ThreadRegistry {
    inner: RwLock<AHashMap<u64, Entry>>,
}

impl ThreadRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(AHashMap::new()),
        }
    }

    pub fn ensure(&self, py: Python<'_>, tid: u64) {
        if let Some(e) = self.inner.read().get(&tid) {
            if e.definitive {
                return;
            }
        }

        let name = py
            .import("threading")
            .and_then(|m| m.call_method0("current_thread"))
            .and_then(|t| t.getattr("name"))
            .and_then(|n| n.extract::<String>())
            .unwrap_or_default();

        // `current_thread()` returns a `_DummyThread` named `"Dummy-N"`
        // when the calling thread isn't yet in `threading._active` — which
        // happens for the first PY_START frames of every new `Thread`,
        // before `_bootstrap_inner` registers it. Treat as provisional and
        // keep retrying until the user-supplied name appears.
        let definitive = !name.is_empty() && !name.starts_with("Dummy-");

        let mut g = self.inner.write();
        match g.get_mut(&tid) {
            Some(e) if e.definitive => {}
            Some(e) => {
                e.name = name;
                e.definitive = definitive;
            }
            None => {
                g.insert(tid, Entry { name, definitive });
            }
        }
    }

    /// Returns the thread's name only if we've seen the definitive
    /// (non-Dummy, non-empty) value. Provisional names are not exposed.
    pub fn name(&self, tid: u64) -> Option<String> {
        self.inner
            .read()
            .get(&tid)
            .filter(|e| e.definitive)
            .map(|e| e.name.clone())
    }

    /// Snapshot of all definitively-named threads.
    pub fn snapshot(&self) -> Vec<(u64, String)> {
        self.inner
            .read()
            .iter()
            .filter(|(_, e)| e.definitive)
            .map(|(k, e)| (*k, e.name.clone()))
            .collect()
    }
}
