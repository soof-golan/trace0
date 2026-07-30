use crate::format::make_exporter;
use crate::intern::Interner;
use crate::monitoring::{self, MonitoringHandle, State};
use crate::threads::ThreadRegistry;
use parking_lot::Mutex;
use pyo3::exceptions::{PyIOError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::sync::Arc;
use std::thread;
use trace0_core::{
    Clock, CodeLookup, EventQueue, ThreadNames, run_pipeline,
    tls::{COLD, hot},
};

struct RunningHandle {
    queue: Arc<EventQueue>,
    monitoring: MonitoringHandle,
    exporter_thread: Option<thread::JoinHandle<std::io::Result<()>>>,
}

#[pyclass(module = "trace0._core")]
pub struct Tracer {
    output: String,
    format: String,
    handle: Mutex<Option<RunningHandle>>,
}

#[pymethods]
impl Tracer {
    #[new]
    #[pyo3(signature = (output, format = None))]
    pub(crate) fn new(output: String, format: Option<String>) -> Self {
        Self {
            output,
            format: format.unwrap_or_else(|| "json".to_string()),
            handle: Mutex::new(None),
        }
    }

    pub(crate) fn start(&self, py: Python<'_>) -> PyResult<()> {
        let mut slot = self.handle.lock();
        if slot.is_some() {
            return Err(PyRuntimeError::new_err("tracer already started"));
        }

        // Anchor the clock before any event can be recorded, so every
        // timestamp is a non-negative offset from the trace start.
        let queue = Arc::new(EventQueue::new(Clock::starting_now()));
        let interner = Arc::new(Interner::new());
        let threads = Arc::new(ThreadRegistry::new());
        let state = Arc::new(State {
            run: queue.id(),
            queue: queue.clone(),
            interner: interner.clone(),
            threads: threads.clone(),
        });

        let exporter = make_exporter(&self.format, &self.output)
            .map_err(|e| PyIOError::new_err(e.to_string()))?;

        let exporter_thread = {
            let queue = queue.clone();
            let codes: Arc<dyn CodeLookup> = interner.clone();
            let names: Arc<dyn ThreadNames> = threads.clone();
            thread::Builder::new()
                .name("trace0-exporter".into())
                .spawn(move || run_pipeline(queue, codes, names, exporter))
                .map_err(|e| PyRuntimeError::new_err(format!("spawn exporter: {e}")))?
        };

        let mh = monitoring::enable(py, state)?;

        *slot = Some(RunningHandle {
            queue,
            monitoring: mh,
            exporter_thread: Some(exporter_thread),
        });
        Ok(())
    }

    pub(crate) fn stop(&self, py: Python<'_>) -> PyResult<()> {
        let mut slot = self.handle.lock();
        let Some(mut h) = slot.take() else {
            return Err(PyRuntimeError::new_err("tracer not running"));
        };

        monitoring::disable(py, &h.monitoring)?;
        // Drain the calling thread's partial batch (worker threads
        // do this via Cold::Drop on exit; the thread that runs
        // `stop` is still alive). Safe to touch its TLS now —
        // `monitoring::disable` returned so no callbacks fire here
        // anymore.
        COLD.with_borrow_mut(|cold| cold.flush_partial(hot()));
        h.queue.close();

        let join = h.exporter_thread.take();
        let joined: Option<std::io::Result<()>> =
            py.detach(move || join.and_then(|t| t.join().ok()));

        match joined {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(PyIOError::new_err(e.to_string())),
            None => Err(PyRuntimeError::new_err("exporter thread panicked")),
        }
    }

    fn __enter__<'py>(slf: PyRef<'py, Self>, py: Python<'py>) -> PyResult<PyRef<'py, Self>> {
        slf.start(py)?;
        Ok(slf)
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<Bound<'_, PyType>>,
        _exc_val: Option<Bound<'_, pyo3::types::PyAny>>,
        _exc_tb: Option<Bound<'_, pyo3::types::PyAny>>,
    ) -> PyResult<bool> {
        self.stop(py)?;
        Ok(false)
    }
}
