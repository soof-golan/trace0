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
    EventQueue,
    tls::{codes, hot},
};

pub struct State {
    /// `queue.id()`, copied here because this struct is on the hot path
    /// and the queue's own cache line is not.
    pub run: u64,
    pub queue: Arc<EventQueue>,
    pub interner: Arc<Interner>,
    pub threads: Arc<ThreadRegistry>,
}

/// Resolve a code object this thread has not seen, and name the thread if
/// it still needs naming.
///
/// Outlined deliberately. Inlined, its interner lock and Python calls put
/// a 480-byte frame and twelve register spills into the callback's
/// prologue -- paid on every event, to run on almost none of them.
///
/// Returns the code id, or `None` past the 24-bit ceiling, where dropping
/// the event beats corrupting the kind bits of every later one.
#[cold]
#[inline(never)]
fn resolve_cold(
    py: Python<'_>,
    state: &State,
    code: pyo3::Borrowed<'_, '_, PyAny>,
    key: usize,
    hot: &mut trace0_core::tls::Hot,
) -> Option<u32> {
    // Everything this thread learned belongs to whichever run taught it.
    // A restarted tracer has a fresh interner numbering from zero and a
    // fresh registry that has never heard of this thread.
    if hot.queue_id != state.run {
        *codes() = CodeCache::EMPTY;
        hot.last_code_key = trace0_core::tls::NOT_CACHED;
        hot.ensured = false;
    }
    if hot.tid == u32::MAX {
        hot.tid = os_tid();
    }
    // `ensure` is best-effort: a brand-new thread reports itself as
    // `Dummy-N` until `threading` registers it, so latching on the first
    // attempt would leave every worker thread unnamed.
    if !hot.ensured {
        hot.ensured = state.threads.ensure(py, hot.tid);
    }
    // The interner is shared, so a miss here is a lock every traced
    // thread queues behind. `codes()` is this thread's alone.
    //
    // Never held across the Python calls above or below: a callback that
    // re-entered would alias it.
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
    // Arm the fast path only once the thread is fully settled, so a
    // single compare against `last_code_key` also covers naming.
    hot.last_code_key = if hot.ensured {
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
    // The counter and the scale factor that converts it must come from
    // one source. `clock_direct` is that clock's own answer about whether
    // its counter can be read inline, cached here to save chasing the
    // queue on every event.
    let ticks = if hot.clock_direct {
        read_counter()
    } else {
        state.queue.clock().raw()
    };
    // Two compares cover everything a settled thread has already done:
    // same code object as last time, id assigned, thread named, and all
    // of it learned during *this* tracer run rather than a previous one.
    let code_id = if hot.last_code_key == key && hot.queue_id == state.run {
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
    if nargs >= 1 {
        // SAFETY: sys.monitoring dispatches callbacks from the interpreter
        // loop, on a thread that is attached for the whole call. Nothing
        // derived from this token escapes `record`.
        //
        // `Python::attach` would re-establish that state at ~35ns per
        // event, several times what this callback's actual work costs.
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
// Safe: PyMethodDef contains raw pointers, but for static method tables
// the pointers are to constant data and a never-mutated function entry.
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
    // Held, not read: these keep the callback objects and the shared
    // `Callbacks` instance alive for as long as sys.monitoring can
    // still invoke them.
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
