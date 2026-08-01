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
    /// Empty only while a fork is in flight, when callbacks must not run.
    monitoring: Option<MonitoringHandle>,
    exporter: thread::JoinHandle<std::io::Result<()>>,
}

/// The tracer a forking process must hand over to its child. A fork clones one
/// thread, so the child inherits an exporter thread that does not exist and
/// locks no one will release; every hook below exists to keep the child from
/// touching any of it.
static ACTIVE: Mutex<Option<Py<Tracer>>> = Mutex::new(None);
static HOOKS_REGISTERED: std::sync::Once = std::sync::Once::new();

#[pyclass(module = "trace0._core", frozen)]
pub struct Tracer {
    output: String,
    format: Format,
    running: Mutex<Option<Running>>,
}

impl Tracer {
    pub(crate) fn begin(&self, py: Python<'_>) -> PyResult<Running> {
        self.start(py, 0, false)
    }

    /// A child adds to the file its parent started, under a packet-sequence
    /// slot of its own so the two streams stay separable once interleaved.
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

/// A process started by exec shares no memory with its parent, so the only
/// thing that reaches it is the environment. The `.pth` shipped with the
/// package reads these on interpreter startup.
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

/// Run `f` against the tracer that is currently tracing, if any. A fork hook
/// fires for every fork in the process, including forks by programs that never
/// started a tracer.
fn with_active<T>(f: impl FnOnce(&Tracer) -> PyResult<T>) -> PyResult<Option<T>> {
    let active = ACTIVE.lock();
    match active.as_ref() {
        Some(tracer) => f(tracer.get()).map(Some),
        None => Ok(None),
    }
}

/// Stop delivering callbacks before the address space is cloned, so that no
/// callback can run in the child against state the child is about to abandon.
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

/// Start the child's own trace. The inherited run is deliberately leaked: its
/// exporter thread did not survive the fork, and any lock it held at that
/// instant is still held by nobody, so reading or dropping that state could
/// block forever.
#[pyfunction]
pub fn _after_fork_in_child(py: Python<'_>) -> PyResult<()> {
    with_active(|tracer| {
        let mut slot = tracer.running.lock();
        std::mem::forget(slot.take());
        tls::forget_other_threads();
        *slot = Some(tracer.begin_child(py)?);
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
        let mut slot = slf.get().running.lock();
        debug_assert!(slot.is_none(), "entered a run that was still going");
        *slot = Some(running);
        drop(slot);
        *ACTIVE.lock() = Some(slf.clone().unbind());
        Ok(slf.clone())
    }
}

/// Registered once per process: `os.register_at_fork` stacks handlers, and a
/// child inherits the ones its parent registered.
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
    #[pyo3(signature = (output, format = "protobuf".to_string()))]
    pub(crate) fn new(output: String, format: String) -> PyResult<Self> {
        Ok(Self {
            output,
            format: Format::parse(&format).map_err(PyValueError::new_err)?,
            running: Mutex::new(None),
        })
    }

    pub(crate) fn __enter__<'py>(
        slf: &Bound<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, Self>> {
        let me = slf.get();
        advertise_to_spawned_children(py, &me.output, me.format)?;
        Self::install(slf, py, me.begin(py)?)
    }

    /// Enter as a process that exec'd out of a traced one: same bookkeeping,
    /// but writing beside the trace its parent named rather than over it.
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
        stop_advertising(py)?;
        match self.running.lock().take() {
            Some(running) => Tracer::end(py, running)?,
            None => return Err(PyRuntimeError::new_err("tracer is not tracing")),
        }
        Ok(false)
    }
}
