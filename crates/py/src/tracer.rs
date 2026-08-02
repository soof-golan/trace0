use crate::format::Format;
use crate::intern::Interner;
use crate::monitoring::{self, MonitoringHandle, State};
use crate::threads::ThreadRegistry;
use parking_lot::Mutex;
use pyo3::exceptions::{PyIOError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::sync::Arc;
use std::thread;
use trace0_core::{Clock, CodeLookup, EventQueue, ThreadNames, run_pipeline, tls};

pub(crate) struct Running {
    queue: Arc<EventQueue>,
    state: Arc<State>,
    monitoring: Option<MonitoringHandle>,
    exporter: thread::JoinHandle<std::io::Result<()>>,
}

static ACTIVE: Mutex<Option<Py<Tracer>>> = Mutex::new(None);
static HOOKS_REGISTERED: std::sync::Once = std::sync::Once::new();
static REPLACED_HANDLERS: Mutex<Vec<(i32, Py<PyAny>)>> = Mutex::new(Vec::new());

const DEADLY_SIGNALS: [&str; 3] = ["SIGTERM", "SIGHUP", "SIGQUIT"];

#[pyclass(module = "trace0._core", frozen)]
pub struct Tracer {
    output: String,
    format: Format,
    trace_subprocesses: bool,
    running: Mutex<Option<Running>>,
}

impl Tracer {
    pub(crate) fn begin(&self, py: Python<'_>) -> PyResult<Running> {
        self.start(py, 0, false)
    }

    fn begin_child(&self, py: Python<'_>) -> PyResult<Running> {
        self.start(py, std::process::id(), true)
    }

    fn start(&self, py: Python<'_>, slot: u32, append: bool) -> PyResult<Running> {
        let queue = Arc::new(EventQueue::new(Clock::starting_now()));
        let state = Arc::new(State {
            run: queue.id(),
            queue: queue.clone(),
            interner: Interner::new(),
            threads: ThreadRegistry::new(),
        });

        let sink = self
            .format
            .open(&self.output, slot, append)
            .map_err(|e| PyIOError::new_err(e.to_string()))?;

        let exporter = {
            let queue = queue.clone();
            let codes: Arc<dyn CodeLookup> = state.clone();
            let names: Arc<dyn ThreadNames> = state.clone();
            thread::Builder::new()
                .name("trace0-exporter".into())
                .spawn(move || run_pipeline(queue, codes, names, sink))
                .map_err(|e| PyRuntimeError::new_err(format!("spawn exporter: {e}")))?
        };

        match monitoring::enable(py, state.clone()) {
            Ok(monitoring) => Ok(Running {
                queue,
                state,
                monitoring: Some(monitoring),
                exporter,
            }),
            Err(e) => {
                queue.close();
                let _ = py.detach(move || exporter.join());
                Err(e)
            }
        }
    }

    pub(crate) fn end(py: Python<'_>, running: Running) -> PyResult<()> {
        let Running {
            queue,
            state: _,
            monitoring,
            exporter,
        } = running;

        let disabled = match &monitoring {
            Some(h) => monitoring::disable(py, h),
            None => Ok(()),
        };
        queue.record_dropped(tls::flush_every_thread());
        queue.close();
        let joined = py.detach(move || exporter.join());

        disabled?;
        match joined {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(PyIOError::new_err(e.to_string())),
            Err(_) => Err(PyRuntimeError::new_err("exporter thread panicked")),
        }
    }
}

const CHILD_OUTPUT: &str = "TRACE0_CHILD_OUTPUT";
const CHILD_FORMAT: &str = "TRACE0_CHILD_FORMAT";

fn advertise_to_spawned_children(py: Python<'_>, output: &str, format: Format) -> PyResult<()> {
    let environ = py.import("os")?.getattr("environ")?;
    environ.set_item(CHILD_OUTPUT, output)?;
    environ.set_item(CHILD_FORMAT, format.as_str())?;
    Ok(())
}

fn stop_advertising(py: Python<'_>) -> PyResult<()> {
    let environ = py.import("os")?.getattr("environ")?;
    for key in [CHILD_OUTPUT, CHILD_FORMAT] {
        environ.call_method1("pop", (key, py.None()))?;
    }
    Ok(())
}

fn our_handler(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    py.import("trace0._core")?.getattr("_handle_deadly_signal")
}

fn take_over_deadly_signals(py: Python<'_>) -> PyResult<()> {
    let signal = py.import("signal")?;
    let ours = our_handler(py)?;
    let mut replaced = REPLACED_HANDLERS.lock();
    for name in DEADLY_SIGNALS {
        let Ok(sig) = signal.getattr(name) else {
            continue;
        };
        let Ok(previous) = signal.call_method1("getsignal", (&sig,)) else {
            continue;
        };
        if previous.is(&ours) {
            continue;
        }
        if signal.call_method1("signal", (&sig, &ours)).is_err() {
            continue;
        }
        let number: i32 = sig.extract()?;
        replaced.retain(|(n, _)| *n != number);
        replaced.push((number, previous.unbind()));
    }
    Ok(())
}

fn hand_back_deadly_signals(py: Python<'_>) -> PyResult<()> {
    let signal = py.import("signal")?;
    let ours = our_handler(py)?;
    for (number, previous) in REPLACED_HANDLERS.lock().drain(..) {
        let Ok(current) = signal.call_method1("getsignal", (number,)) else {
            continue;
        };
        if current.is(&ours) {
            signal.call_method1("signal", (number, previous)).ok();
        }
    }
    Ok(())
}

fn end_active_run(py: Python<'_>) -> PyResult<()> {
    let Some(active) = ACTIVE.try_lock() else {
        return Ok(());
    };
    let Some(tracer) = active.as_ref() else {
        return Ok(());
    };
    let Some(mut slot) = tracer.get().running.try_lock() else {
        return Ok(());
    };
    match slot.take() {
        Some(running) => Tracer::end(py, running),
        None => Ok(()),
    }
}

#[pyfunction]
pub fn _handle_deadly_signal(py: Python<'_>, signum: i32, frame: Py<PyAny>) -> PyResult<()> {
    let signal = py.import("signal")?;
    let previous = REPLACED_HANDLERS
        .lock()
        .iter()
        .find(|(number, _)| *number == signum)
        .map(|(_, handler)| handler.clone_ref(py));
    let Some(previous) = previous else {
        return Ok(());
    };

    if previous.bind(py).is_callable() {
        previous.call1(py, (signum, frame))?;
        return Ok(());
    }
    if previous.is(&signal.getattr("SIG_DFL")?) {
        end_active_run(py)?;
    }
    signal.call_method1("signal", (signum, previous))?;
    signal.call_method1("raise_signal", (signum,))?;
    Ok(())
}

fn with_active<T>(f: impl FnOnce(&Tracer) -> PyResult<T>) -> PyResult<Option<T>> {
    let active = ACTIVE.lock();
    match active.as_ref() {
        Some(tracer) => f(tracer.get()).map(Some),
        None => Ok(None),
    }
}

#[pyfunction]
pub fn _before_fork(py: Python<'_>) -> PyResult<()> {
    with_active(|tracer| {
        let mut slot = tracer.running.lock();
        if let Some(running) = slot.as_mut()
            && let Some(handle) = running.monitoring.take()
        {
            monitoring::disable(py, &handle)?;
        }
        Ok(())
    })?;
    Ok(())
}

#[pyfunction]
pub fn _after_fork_in_parent(py: Python<'_>) -> PyResult<()> {
    with_active(|tracer| {
        let mut slot = tracer.running.lock();
        if let Some(running) = slot.as_mut() {
            running.monitoring = Some(monitoring::enable(py, running.state.clone())?);
        }
        Ok(())
    })?;
    Ok(())
}

#[pyfunction]
pub fn _after_fork_in_child(py: Python<'_>) -> PyResult<()> {
    with_active(|tracer| {
        let mut slot = tracer.running.lock();
        std::mem::forget(slot.take());
        tls::forget_other_threads();
        if tracer.trace_subprocesses {
            *slot = Some(tracer.begin_child(py)?);
        }
        Ok(())
    })?;
    Ok(())
}

impl Tracer {
    fn install<'py>(
        slf: &Bound<'py, Self>,
        py: Python<'py>,
        running: Running,
    ) -> PyResult<Bound<'py, Self>> {
        register_fork_hooks(py)?;
        take_over_deadly_signals(py)?;
        let mut slot = slf.get().running.lock();
        debug_assert!(slot.is_none(), "entered a run that was still going");
        *slot = Some(running);
        drop(slot);
        *ACTIVE.lock() = Some(slf.clone().unbind());
        Ok(slf.clone())
    }
}

fn register_fork_hooks(py: Python<'_>) -> PyResult<()> {
    let mut result = Ok(());
    HOOKS_REGISTERED.call_once(|| {
        result = (|| {
            let module = py.import("trace0._core")?;
            let kwargs = pyo3::types::PyDict::new(py);
            kwargs.set_item("before", module.getattr("_before_fork")?)?;
            kwargs.set_item("after_in_parent", module.getattr("_after_fork_in_parent")?)?;
            kwargs.set_item("after_in_child", module.getattr("_after_fork_in_child")?)?;
            py.import("os")?
                .call_method("register_at_fork", (), Some(&kwargs))?;
            Ok(())
        })();
    });
    result
}

#[pymethods]
impl Tracer {
    #[new]
    #[pyo3(signature = (output, format = "protobuf".to_string(), trace_subprocesses = true))]
    pub(crate) fn new(output: String, format: String, trace_subprocesses: bool) -> PyResult<Self> {
        Ok(Self {
            output,
            format: Format::parse(&format).map_err(PyValueError::new_err)?,
            trace_subprocesses,
            running: Mutex::new(None),
        })
    }

    pub(crate) fn __enter__<'py>(
        slf: &Bound<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, Self>> {
        let me = slf.get();
        match me.trace_subprocesses {
            true => advertise_to_spawned_children(py, &me.output, me.format)?,
            false => stop_advertising(py)?,
        }
        Self::install(slf, py, me.begin(py)?)
    }

    fn _enter_as_child<'py>(slf: &Bound<'py, Self>, py: Python<'py>) -> PyResult<Bound<'py, Self>> {
        let running = slf.get().begin_child(py)?;
        Self::install(slf, py, running)
    }

    pub(crate) fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<Bound<'_, PyType>>,
        _exc_val: Option<Bound<'_, pyo3::types::PyAny>>,
        _exc_tb: Option<Bound<'_, pyo3::types::PyAny>>,
    ) -> PyResult<bool> {
        *ACTIVE.lock() = None;
        hand_back_deadly_signals(py)?;
        stop_advertising(py)?;
        if let Some(running) = self.running.lock().take() {
            Tracer::end(py, running)?;
        }
        Ok(false)
    }
}
