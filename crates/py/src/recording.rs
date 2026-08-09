use crate::format::Format;
use crate::tracer::Tracer;
use parking_lot::Mutex;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use trace0_core::recorder::{Control, DumpSink};
use trace0_core::sink::Exporter;

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_window() -> u64 {
    NEXT_WINDOW.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct DirSink {
    pub dir: PathBuf,
    pub format: Format,
    pub pid: u32,
}

impl DumpSink for DirSink {
    fn open(&mut self, reason: &str) -> io::Result<(String, Box<dyn Exporter>)> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis();
        let safe: String = reason
            .chars()
            .map(|c| match c {
                c if c.is_ascii_alphanumeric() => c,
                '.' | '_' | '-' => c,
                _ => '-',
            })
            .collect();
        let ext = match self.format {
            Format::Json => "json",
            Format::Protobuf => "pb",
            Format::Pprof => "pprof",
        };
        for serial in 1u32.. {
            let tag = match serial {
                1 => String::new(),
                n => format!("-{n}"),
            };
            let name = match self.pid {
                0 => format!("{stamp}-{safe}{tag}.{ext}"),
                pid => format!("{stamp}-{safe}{tag}.{pid}.{ext}"),
            };
            let path = self.dir.join(name);
            if !path.exists() {
                let path = path
                    .to_str()
                    .ok_or_else(|| io::Error::other("the output path is not unicode"))?;
                let exporter = self.format.open(path, self.pid, false)?;
                return Ok((path.to_string(), exporter));
            }
        }
        unreachable!("every serial number was taken")
    }
}

#[pyclass(module = "trace0._core", frozen)]
pub struct Snapshot {
    written: Mutex<Option<String>>,
}

#[pymethods]
impl Snapshot {
    #[getter]
    fn path(&self) -> PyResult<String> {
        self.written
            .lock()
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("the snapshot block is still open"))
    }
}

enum Stage {
    Ready,
    Open {
        id: u64,
        start_ticks: u64,
        result: Py<Snapshot>,
    },
    Done,
}

#[pyclass(module = "trace0._core", frozen)]
pub struct SnapshotBlock {
    tracer: Py<Tracer>,
    reason: String,
    stage: Mutex<Stage>,
}

impl SnapshotBlock {
    pub(crate) fn new(tracer: Py<Tracer>, reason: String) -> Self {
        Self {
            tracer,
            reason,
            stage: Mutex::new(Stage::Ready),
        }
    }
}

#[pymethods]
impl SnapshotBlock {
    fn __enter__(&self, py: Python<'_>) -> PyResult<Py<Snapshot>> {
        let mut stage = self.stage.lock();
        match &*stage {
            Stage::Ready => {}
            Stage::Open { .. } | Stage::Done => {
                return Err(PyRuntimeError::new_err(
                    "a snapshot block runs exactly once",
                ));
            }
        }
        let result = Py::new(
            py,
            Snapshot {
                written: Mutex::new(None),
            },
        )?;
        self.tracer.get().with_recorder(|running, control| {
            let id = next_window();
            let start_ticks = running.queue.clock().raw();
            control
                .send(Control::Open { id, start_ticks })
                .map_err(|_| PyRuntimeError::new_err("the recorder is gone"))?;
            *stage = Stage::Open {
                id,
                start_ticks,
                result: result.clone_ref(py),
            };
            Ok(())
        })?;
        Ok(result)
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<Bound<'_, PyType>>,
        _exc_val: Option<Bound<'_, PyAny>>,
        _exc_tb: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let opened = {
            let mut stage = self.stage.lock();
            match std::mem::replace(&mut *stage, Stage::Done) {
                Stage::Open {
                    id,
                    start_ticks,
                    result,
                } => Some((id, start_ticks, result)),
                Stage::Ready => {
                    *stage = Stage::Ready;
                    None
                }
                Stage::Done => None,
            }
        };
        let Some((id, start_ticks, result)) = opened else {
            return Ok(false);
        };
        let reason = match &exc_type {
            Some(t) => format!(
                "{}-{}",
                self.reason,
                t.getattr("__name__")?.extract::<String>()?
            ),
            None => self.reason.clone(),
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        self.tracer.get().with_recorder(|running, control| {
            control
                .send(Control::Dump {
                    id,
                    start_ticks,
                    end_ticks: running.queue.clock().raw(),
                    reason,
                    done: Some(done_tx),
                })
                .map_err(|_| PyRuntimeError::new_err("the recorder is gone"))
        })?;
        let path = py
            .detach(move || done_rx.recv())
            .map_err(|_| PyRuntimeError::new_err("the recorder did not finish the dump"))?;
        *result.get().written.lock() = Some(path);
        Ok(false)
    }
}
