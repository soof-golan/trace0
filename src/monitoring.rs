use crate::evqueue::EventQueue;
use crate::event::{Event, EventKind, now_ns, os_tid};
use crate::intern::Interner;
use crate::threads::ThreadRegistry;
use crate::tls::CTX;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use std::ffi::CStr;
use std::sync::Arc;
use std::time::Instant;

pub struct State {
    pub queue: Arc<EventQueue>,
    pub interner: Arc<Interner>,
    pub threads: Arc<ThreadRegistry>,
    pub start: Instant,
}

#[inline]
fn record(py: Python<'_>, state: &State, code: pyo3::Borrowed<'_, '_, PyAny>, kind: EventKind) {
    let key = code.as_ptr() as usize;
    let tid = os_tid();
    let ts_ns = now_ns(state.start);
    CTX.with_borrow_mut(|ctx| {
        let code_id = if ctx.last_code_key == key && ctx.last_code_id != u32::MAX {
            ctx.last_code_id
        } else {
            let id = match state.interner.lookup(key) {
                Some(id) => id,
                None => state.interner.insert(py, &code, key),
            };
            ctx.last_code_key = key;
            ctx.last_code_id = id;
            id
        };
        if !ctx.ensured {
            state.threads.ensure(py, tid);
            ctx.ensured = true;
        }
        state
            .queue
            .push_with_ctx(ctx, Event { ts_ns, tid, code_id, kind });
    });
}

#[pyclass(module = "useful_tracer._core", frozen)]
pub struct Callbacks {
    state: Arc<State>,
}

/// Shared `METH_FASTCALL` body for every event variant.
///
/// `slf` is the `Callbacks` pyclass instance bound at registration
/// (passed as the `self` arg of `PyCFunction_NewEx`). We read `args[0]`
/// (the code object) and ignore everything else — `instruction_offset`,
/// `retval`, `exc` — without ever wrapping them. Skips per-event
/// `Bound<PyAny>` construction and `i64` extraction that PyO3's
/// `#[pymethod]` thunk would otherwise do.
fn fastcall_record(
    slf: *mut ffi::PyObject,
    args: *mut *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kind: EventKind,
) -> *mut ffi::PyObject {
    Python::attach(|py| {
        if nargs >= 1 {
            let cb_obj = unsafe { pyo3::Borrowed::<'_, '_, PyAny>::from_ptr(py, slf) };
            let b = unsafe { cb_obj.downcast_unchecked::<Callbacks>() };
            let cb: &Callbacks = b.get();
            let code = unsafe { pyo3::Borrowed::<'_, '_, PyAny>::from_ptr(py, *args) };
            record(py, &cb.state, code, kind);
        }
        unsafe { ffi::Py_NewRef(ffi::Py_None()) }
    })
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
// Safe: PyMethodDef contains raw pointers, but for static method tables
// the pointers are to constant data and a never-mutated function entry.
unsafe impl Sync for MethodDef {}

const fn method_def(name: &'static CStr, fp: ffi::PyCFunctionFast) -> MethodDef {
    MethodDef(ffi::PyMethodDef {
        ml_name: name.as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunctionFast: fp },
        ml_flags: ffi::METH_FASTCALL,
        ml_doc: std::ptr::null(),
    })
}

static MD_PY_START: MethodDef = method_def(c"_uft_py_start", cb_py_start);
static MD_PY_RETURN: MethodDef = method_def(c"_uft_py_return", cb_py_return);
static MD_PY_YIELD: MethodDef = method_def(c"_uft_py_yield", cb_py_yield);
static MD_PY_RESUME: MethodDef = method_def(c"_uft_py_resume", cb_py_resume);
static MD_PY_UNWIND: MethodDef = method_def(c"_uft_py_unwind", cb_py_unwind);
static MD_PY_THROW: MethodDef = method_def(c"_uft_py_throw", cb_py_throw);

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
    pub callbacks: Py<Callbacks>,
    pub registered: Vec<Py<PyAny>>,
}

pub fn enable(py: Python<'_>, state: Arc<State>) -> PyResult<MonitoringHandle> {
    let monitoring = py.import("sys")?.getattr("monitoring")?;
    let tool_id: u8 = monitoring.getattr("PROFILER_ID")?.extract()?;
    monitoring.call_method1("use_tool_id", (tool_id, "useful_tracer"))?;

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
