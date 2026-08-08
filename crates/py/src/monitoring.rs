use crate::intern::Interner;
use crate::threads::ThreadRegistry;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use std::ffi::CStr;
use std::sync::Arc;
use trace0_core::clock::read_counter;
use trace0_core::codecache::CodeCache;
use trace0_core::event::{EventKind, os_tid};
use trace0_core::{
    CodeInfo, CodeLookup, EventQueue, ThreadNames,
    tls::{codes, hot},
};

pub struct State {
    pub run: u64,
    pub queue: Arc<EventQueue>,
    pub interner: Interner,
    pub threads: ThreadRegistry,
}

impl CodeLookup for State {
    fn code(&self, id: u32) -> Option<CodeInfo> {
        self.interner.code(id)
    }
}

impl ThreadNames for State {
    fn name(&self, tid: u32) -> Option<String> {
        self.threads.name(tid)
    }

    fn snapshot(&self) -> Vec<(u32, String)> {
        self.threads.snapshot()
    }
}

const NAME_RETRIES: u32 = 64;

#[cold]
#[inline(never)]
fn resolve_cold(
    py: Python<'_>,
    state: &State,
    code: pyo3::Borrowed<'_, '_, PyAny>,
    key: usize,
    hot: &mut trace0_core::tls::Hot,
) -> Option<u32> {
    let generation = crate::codewatch::generation();
    if hot.queue_id != state.run {
        *codes() = CodeCache::EMPTY;
        hot.last_code_key = trace0_core::tls::NOT_CACHED;
        hot.ensured = false;
        hot.name_retries = NAME_RETRIES;
    }
    if hot.code_gen != generation {
        *codes() = CodeCache::EMPTY;
        hot.code_gen = generation;
    }
    if hot.tid == u32::MAX {
        hot.tid = os_tid();
    }
    if !hot.ensured && hot.name_retries > 0 {
        hot.name_retries -= 1;
        hot.ensured = state.threads.ensure(py, hot.tid);
    }
    let id = match codes().get(key) {
        Some(id) => id,
        None => {
            let id = state
                .interner
                .lookup(key)
                .or_else(|| state.interner.insert(py, &code, key))?;
            codes().put(key, id);
            id
        }
    };
    hot.last_code_id = id;
    hot.last_code_key = if hot.ensured || hot.name_retries == 0 {
        key
    } else {
        trace0_core::tls::NOT_CACHED
    };
    Some(id)
}

#[inline]
fn record(py: Python<'_>, state: &State, code: pyo3::Borrowed<'_, '_, PyAny>, kind: EventKind) {
    let key = code.as_ptr() as usize;
    let hot = hot();
    let ticks = if hot.clock_direct {
        read_counter()
    } else {
        state.queue.clock().raw()
    };
    let code_id = if hot.last_code_key == key
        && hot.queue_id == state.run
        && hot.code_gen == crate::codewatch::generation()
    {
        hot.last_code_id
    } else {
        match resolve_cold(py, state, code, key, hot) {
            Some(id) => id,
            None => return,
        }
    };
    state
        .queue
        .push_with_ctx(hot, state.run, ticks, hot.tid, code_id, kind);
}

#[pyclass(module = "trace0._core", frozen)]
pub struct Callbacks {
    state: Arc<State>,
}

fn fastcall_record(
    slf: *mut ffi::PyObject,
    args: *mut *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kind: EventKind,
) -> *mut ffi::PyObject {
    if nargs >= 1 {
        let py = unsafe { Python::assume_attached() };
        let cb_obj = unsafe { pyo3::Borrowed::<'_, '_, PyAny>::from_ptr(py, slf) };
        let b = unsafe { cb_obj.cast_unchecked::<Callbacks>() };
        let cb: &Callbacks = b.get();
        let code = unsafe { pyo3::Borrowed::<'_, '_, PyAny>::from_ptr(py, *args) };
        record(py, &cb.state, code, kind);
    }
    unsafe { ffi::Py_NewRef(ffi::Py_None()) }
}

macro_rules! make_cb {
    ($name:ident, $kind:expr) => {
        unsafe extern "C" fn $name(
            slf: *mut ffi::PyObject,
            args: *mut *mut ffi::PyObject,
            nargs: ffi::Py_ssize_t,
        ) -> *mut ffi::PyObject {
            fastcall_record(slf, args, nargs, $kind)
        }
    };
}

make_cb!(cb_py_start, EventKind::Begin);
make_cb!(cb_py_return, EventKind::End);
make_cb!(cb_py_yield, EventKind::Yield);
make_cb!(cb_py_resume, EventKind::Resume);
make_cb!(cb_py_unwind, EventKind::Unwind);
make_cb!(cb_py_throw, EventKind::Throw);

#[repr(transparent)]
struct MethodDef(ffi::PyMethodDef);
unsafe impl Sync for MethodDef {}

const fn method_def(name: &'static CStr, fp: ffi::PyCFunctionFast) -> MethodDef {
    MethodDef(ffi::PyMethodDef {
        ml_name: name.as_ptr(),
        ml_meth: ffi::PyMethodDefPointer {
            PyCFunctionFast: fp,
        },
        ml_flags: ffi::METH_FASTCALL,
        ml_doc: std::ptr::null(),
    })
}

static MD_PY_START: MethodDef = method_def(c"_t0_py_start", cb_py_start);
static MD_PY_RETURN: MethodDef = method_def(c"_t0_py_return", cb_py_return);
static MD_PY_YIELD: MethodDef = method_def(c"_t0_py_yield", cb_py_yield);
static MD_PY_RESUME: MethodDef = method_def(c"_t0_py_resume", cb_py_resume);
static MD_PY_UNWIND: MethodDef = method_def(c"_t0_py_unwind", cb_py_unwind);
static MD_PY_THROW: MethodDef = method_def(c"_t0_py_throw", cb_py_throw);

const PAIRS: [(&str, &MethodDef); 6] = [
    ("PY_START", &MD_PY_START),
    ("PY_RETURN", &MD_PY_RETURN),
    ("PY_YIELD", &MD_PY_YIELD),
    ("PY_RESUME", &MD_PY_RESUME),
    ("PY_UNWIND", &MD_PY_UNWIND),
    ("PY_THROW", &MD_PY_THROW),
];

pub struct MonitoringHandle {
    pub tool_id: u8,
    #[allow(dead_code)]
    pub callbacks: Py<Callbacks>,
    #[allow(dead_code)]
    pub registered: Vec<Py<PyAny>>,
}

pub fn enable(py: Python<'_>, state: Arc<State>) -> PyResult<MonitoringHandle> {
    let monitoring = py.import("sys")?.getattr("monitoring")?;
    let tool_id: u8 = monitoring.getattr("PROFILER_ID")?.extract()?;
    monitoring.call_method1("use_tool_id", (tool_id, "trace0"))?;

    let events = monitoring.getattr("events")?;
    let cb_obj = Py::new(py, Callbacks { state })?;

    let mut mask: i32 = 0;
    let mut registered: Vec<Py<PyAny>> = Vec::with_capacity(PAIRS.len());

    for (event_name, md) in PAIRS.iter() {
        let event_val: i32 = events.getattr(*event_name)?.extract()?;
        let cfn = unsafe {
            ffi::PyCFunction_NewEx(
                &md.0 as *const _ as *mut _,
                cb_obj.as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if cfn.is_null() {
            return Err(PyErr::fetch(py));
        }
        let cfn_bound = unsafe { Bound::from_owned_ptr(py, cfn) };
        monitoring.call_method1("register_callback", (tool_id, event_val, &cfn_bound))?;
        registered.push(cfn_bound.unbind());
        mask |= event_val;
    }
    monitoring.call_method1("set_events", (tool_id, mask))?;

    Ok(MonitoringHandle {
        tool_id,
        callbacks: cb_obj,
        registered,
    })
}

pub fn disable(py: Python<'_>, handle: &MonitoringHandle) -> PyResult<()> {
    let monitoring = py.import("sys")?.getattr("monitoring")?;
    monitoring.call_method1("set_events", (handle.tool_id, 0))?;

    let events = monitoring.getattr("events")?;
    let none = py.None();
    for (event_name, _) in PAIRS.iter() {
        let ev: i32 = events.getattr(*event_name)?.extract()?;
        monitoring.call_method1("register_callback", (handle.tool_id, ev, &none))?;
    }
    monitoring.call_method1("free_tool_id", (handle.tool_id,))?;
    Ok(())
}
