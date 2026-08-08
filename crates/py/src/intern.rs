use ahash::AHashMap;
use parking_lot::RwLock;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use trace0_core::event::CODE_ID_MAX;
use trace0_core::{CodeInfo, CodeLookup};

pub struct Interner {
    inner: RwLock<InternerInner>,
    capacity: u32,
}

struct InternerInner {
    map: AHashMap<usize, u32>,
    info: Vec<CodeInfo>,
    free: Vec<(u32, u64)>,
}

impl Interner {
    pub fn new() -> Self {
        let capacity = std::env::var("TRACE0_CODE_CAPACITY")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v.clamp(1, CODE_ID_MAX + 1))
            .unwrap_or(CODE_ID_MAX + 1);
        Self {
            inner: RwLock::new(InternerInner {
                map: AHashMap::new(),
                info: Vec::new(),
                free: Vec::new(),
            }),
            capacity,
        }
    }

    #[inline]
    pub fn lookup(&self, key: usize) -> Option<u32> {
        self.inner.read().map.get(&key).copied()
    }

    pub fn forget(&self, key: usize, freed_at_ticks: u64) -> bool {
        let mut g = self.inner.write();
        match g.map.remove(&key) {
            Some(id) => {
                g.free.push((id, freed_at_ticks));
                true
            }
            None => false,
        }
    }

    pub fn insert(
        &self,
        _py: Python<'_>,
        code: &Bound<'_, PyAny>,
        key: usize,
        recycle_floor: u64,
    ) -> Option<u32> {
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

        let info = CodeInfo {
            qualname,
            filename,
            firstlineno,
        };
        let mut g = self.inner.write();
        if let Some(&id) = g.map.get(&key) {
            return Some(id);
        }
        let id = if (g.info.len() as u32) < self.capacity {
            let id = g.info.len() as u32;
            g.info.push(info);
            id
        } else {
            let slot = g
                .free
                .iter()
                .position(|(_, freed_at)| *freed_at < recycle_floor)?;
            let (id, _) = g.free.swap_remove(slot);
            g.info[id as usize] = info;
            id
        };
        g.map.insert(key, id);
        Some(id)
    }
}

impl CodeLookup for Interner {
    fn code(&self, id: u32) -> Option<CodeInfo> {
        self.inner.read().info.get(id as usize).cloned()
    }
}
