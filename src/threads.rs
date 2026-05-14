use ahash::AHashMap;
use parking_lot::RwLock;
use pyo3::prelude::*;

pub struct ThreadRegistry {
    inner: RwLock<AHashMap<u32, String>>,
}

impl ThreadRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(AHashMap::new()),
        }
    }

    pub fn ensure(&self, py: Python<'_>, tid: u32) {
        if self.inner.read().contains_key(&tid) {
            return;
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
        // before `_bootstrap_inner` registers it. Skip the insert; we'll
        // try again on the next event.
        if name.is_empty() || name.starts_with("Dummy-") {
            return;
        }

        self.inner.write().entry(tid).or_insert(name);
    }

    pub fn name(&self, tid: u32) -> Option<String> {
        self.inner.read().get(&tid).cloned()
    }

    pub fn snapshot(&self) -> Vec<(u32, String)> {
        self.inner
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }
}
