use crate::evqueue::EventQueue;
use crate::exporter::{make_exporter, run_exporter};
use crate::intern::Interner;
use crate::monitoring::{self, MonitoringHandle, State};
use crate::threads::ThreadRegistry;
use parking_lot::Mutex;
use pyo3::exceptions::{PyIOError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const DEFAULT_CAPACITY: usize = 1_000_000;

struct RunningHandle {
    queue: Arc<EventQueue>,
    monitoring: MonitoringHandle,
    exporter_thread: Option<thread::JoinHandle<std::io::Result<()>>>,
}

#[pyclass(module = "useful_tracer._core")]
pub struct Tracer {
    output: String,
    format: String,
    capacity: usize,
    handle: Mutex<Option<RunningHandle>>,
}

#[pymethods]
impl Tracer {
    #[new]
    #[pyo3(signature = (output, format = None, capacity = None))]
    pub(crate) fn new(output: String, format: Option<String>, capacity: Option<usize>) -> Self {
        Self {
            output,
            format: format.unwrap_or_else(|| "json".to_string()),
            capacity: capacity.unwrap_or(DEFAULT_CAPACITY),
            handle: Mutex::new(None),
        }
    }

    pub(crate) fn start(&self, py: Python<'_>) -> PyResult<()> {
        let mut slot = self.handle.lock();
        if slot.is_some() {
            return Err(PyRuntimeError::new_err("tracer already started"));
        }

        let queue = Arc::new(EventQueue::new(self.capacity));
        let interner = Arc::new(Interner::new());
        let threads = Arc::new(ThreadRegistry::new());
        let state = Arc::new(State {
            queue: queue.clone(),
            interner: interner.clone(),
            threads: threads.clone(),
            start: Instant::now(),
        });

        let exporter = make_exporter(&self.format, &self.output)
            .map_err(|e| PyIOError::new_err(e.to_string()))?;

        let exporter_thread = {
            let queue = queue.clone();
            let interner = interner.clone();
            let threads = threads.clone();
            thread::Builder::new()
                .name("useful-tracer-exporter".into())
                .spawn(move || run_exporter(queue, interner, threads, exporter))
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
