use crate::evqueue::EventQueue;
use crate::event::{Event, EventKind, os_tid, now_us};
use crate::intern::Interner;
use crate::threads::ThreadRegistry;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use std::sync::Arc;
use std::time::Instant;

pub struct State {
    pub queue: Arc<EventQueue>,
    pub interner: Arc<Interner>,
    pub threads: Arc<ThreadRegistry>,
    pub start: Instant,
}

#[inline]
fn record(py: Python<'_>, state: &State, code: &Bound<'_, PyAny>, kind: EventKind) {
    let key = code.as_ptr() as usize;
    let code_id = match state.interner.lookup(key) {
        Some(id) => id,
        None => state.interner.insert(py, code, key),
    };

    let tid = os_tid();
    state.threads.ensure(py, tid);

    py.detach(move || {
        let q = state.queue.clone();
        let ts_us = now_us(state.start);
        let ev = Event { ts_us, tid, code_id, kind };
        q.push(ev);
    });
}

#[pyclass]
pub struct Callbacks {
    state: Arc<State>,
}

#[pymethods]
impl Callbacks {
    fn on_py_start(&self, py: Python<'_>, code: Bound<'_, PyAny>, _offset: i64) {
        record(py, &self.state, &code, EventKind::Begin);
    }
    fn on_py_return(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        _offset: i64,
        _retval: Bound<'_, PyAny>,
    ) {
        record(py, &self.state, &code, EventKind::End);
    }
    fn on_py_yield(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        _offset: i64,
        _retval: Bound<'_, PyAny>,
    ) {
        record(py, &self.state, &code, EventKind::Yield);
    }
    fn on_py_resume(&self, py: Python<'_>, code: Bound<'_, PyAny>, _offset: i64) {
        record(py, &self.state, &code, EventKind::Resume);
    }
    fn on_py_unwind(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        _offset: i64,
        _exc: Bound<'_, PyAny>,
    ) {
        record(py, &self.state, &code, EventKind::Unwind);
    }
    fn on_py_throw(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        _offset: i64,
        _exc: Bound<'_, PyAny>,
    ) {
        record(py, &self.state, &code, EventKind::Throw);
    }
}

pub struct MonitoringHandle {
    pub tool_id: u8,
    pub callbacks: Py<Callbacks>,
}

pub fn enable(py: Python<'_>, state: Arc<State>) -> PyResult<MonitoringHandle> {
    let monitoring = py.import("sys")?.getattr("monitoring")?;
    let tool_id: u8 = monitoring.getattr("PROFILER_ID")?.extract()?;
    monitoring.call_method1("use_tool_id", (tool_id, "useful_tracer"))?;

    let events = monitoring.getattr("events")?;
    let pairs = [
        ("PY_START", "on_py_start"),
        ("PY_RETURN", "on_py_return"),
        ("PY_YIELD", "on_py_yield"),
        ("PY_RESUME", "on_py_resume"),
        ("PY_UNWIND", "on_py_unwind"),
        ("PY_THROW", "on_py_throw"),
    ];

    let callbacks = Py::new(py, Callbacks { state })?;
    let cb_bound = callbacks.bind(py);

    let mut mask: i32 = 0;
    for (event_name, method_name) in pairs.iter() {
        let event_val: i32 = events.getattr(*event_name)?.extract()?;
        let method = cb_bound.getattr(*method_name)?;
        monitoring.call_method1("register_callback", (tool_id, event_val, method))?;
        mask |= event_val;
    }
    monitoring.call_method1("set_events", (tool_id, mask))?;

    Ok(MonitoringHandle { tool_id, callbacks })
}

pub fn disable(py: Python<'_>, handle: &MonitoringHandle) -> PyResult<()> {
    let monitoring = py.import("sys")?.getattr("monitoring")?;
    monitoring.call_method1("set_events", (handle.tool_id, 0))?;

    let events = monitoring.getattr("events")?;
    let names = [
        "PY_START", "PY_RETURN", "PY_YIELD", "PY_RESUME", "PY_UNWIND", "PY_THROW",
    ];
    let none = py.None();
    for n in names.iter() {
        let ev: i32 = events.getattr(*n)?.extract()?;
        monitoring.call_method1("register_callback", (handle.tool_id, ev, &none))?;
    }
    monitoring.call_method1("free_tool_id", (handle.tool_id,))?;
    Ok(())
}
