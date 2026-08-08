use crate::monitoring::State;
use parking_lot::Mutex;
use pyo3::exceptions::PyRuntimeError;
use pyo3::ffi;
use pyo3::prelude::*;
use std::ffi::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

const PY_CODE_EVENT_DESTROY: c_int = 1;

unsafe extern "C" {
    fn PyCode_AddWatcher(
        callback: unsafe extern "C" fn(c_int, *mut ffi::PyCodeObject) -> c_int,
    ) -> c_int;
}

static GENERATION: AtomicU64 = AtomicU64::new(0);
static WATCHED: Mutex<Vec<Weak<State>>> = Mutex::new(Vec::new());
static WATCHER: OnceLock<c_int> = OnceLock::new();

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn watch(py: Python<'_>, state: &Arc<State>) -> PyResult<()> {
    let id = *WATCHER.get_or_init(|| unsafe { PyCode_AddWatcher(on_code_event) });
    if id < 0 {
        return Err(
            PyErr::take(py).unwrap_or_else(|| PyRuntimeError::new_err("PyCode_AddWatcher failed"))
        );
    }
    let mut watched = WATCHED.lock();
    watched.retain(|w| w.strong_count() > 0);
    watched.push(Arc::downgrade(state));
    Ok(())
}

pub fn forget_watched() {
    WATCHED.lock().clear();
}

unsafe extern "C" fn on_code_event(event: c_int, code: *mut ffi::PyCodeObject) -> c_int {
    if event != PY_CODE_EVENT_DESTROY {
        return 0;
    }
    let states: Vec<Arc<State>> = WATCHED.lock().iter().filter_map(Weak::upgrade).collect();
    let key = code as usize;
    let mut forgotten = false;
    for state in &states {
        forgotten |= state.interner.forget(key, state.queue.clock().raw());
    }
    if forgotten {
        GENERATION.fetch_add(1, Ordering::Release);
    }
    0
}
