use ahash::AHashMap;
use parking_lot::RwLock;
use pyo3::prelude::*;
use trace0_core::ThreadNames;

pub struct ThreadRegistry {
    inner: RwLock<AHashMap<u32, String>>,
}

impl ThreadRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(AHashMap::new()),
        }
    }

    pub fn ensure(&self, py: Python<'_>, tid: u32) -> bool {
        if self.inner.read().contains_key(&tid) {
            return true;
        }

        let name = py
            .import("threading")
            .and_then(|m| m.call_method0("current_thread"))
            .and_then(|t| t.getattr("name"))
            .and_then(|n| n.extract::<String>())
            .unwrap_or_default();

        if name.is_empty() || name.starts_with("Dummy-") {
            return false;
        }

        self.inner.write().entry(tid).or_insert(name);
        true
    }
}

impl ThreadNames for ThreadRegistry {
    fn name(&self, tid: u32) -> Option<String> {
        self.inner.read().get(&tid).cloned()
    }

    fn snapshot(&self) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = self
            .inner
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        out.sort_by_key(|(tid, _)| *tid);
        out
    }
}
