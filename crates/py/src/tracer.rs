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
use trace0_core::{
    Clock, CodeLookup, EventQueue, ThreadNames, run_pipeline,
    tls::{COLD, hot},
};

pub(crate) struct Running {
    queue: Arc<EventQueue>,
    monitoring: MonitoringHandle,
    exporter: thread::JoinHandle<std::io::Result<()>>,
}

#[pyclass(module = "trace0._core", frozen)]
pub struct Tracer {
    output: String,
    format: Format,
    running: Mutex<Option<Running>>,
}

impl Tracer {
    pub(crate) fn begin(&self, py: Python<'_>) -> PyResult<Running> {
        let queue = Arc::new(EventQueue::new(Clock::starting_now()));
        let state = Arc::new(State {
            run: queue.id(),
            queue: queue.clone(),
            interner: Interner::new(),
            threads: ThreadRegistry::new(),
        });

        let sink = self
            .format
            .open(&self.output)
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

        match monitoring::enable(py, state) {
            Ok(monitoring) => Ok(Running {
                queue,
                monitoring,
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
            monitoring,
            exporter,
        } = running;

        let disabled = monitoring::disable(py, &monitoring);
        queue.record_dropped(COLD.with_borrow_mut(|cold| cold.flush_partial(hot())));
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

    fn __enter__<'py>(slf: PyRef<'py, Self>, py: Python<'py>) -> PyResult<PyRef<'py, Self>> {
        let running = slf.begin(py)?;
        let mut slot = slf.running.lock();
        debug_assert!(slot.is_none(), "__enter__ replaced a run still going");
        *slot = Some(running);
        drop(slot);
        Ok(slf)
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<Bound<'_, PyType>>,
        _exc_val: Option<Bound<'_, pyo3::types::PyAny>>,
        _exc_tb: Option<Bound<'_, pyo3::types::PyAny>>,
    ) -> PyResult<bool> {
        match self.running.lock().take() {
            Some(running) => Tracer::end(py, running)?,
            None => return Err(PyRuntimeError::new_err("tracer is not tracing")),
        }
        Ok(false)
    }
}
