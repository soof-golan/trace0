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

pub(crate) fn parse_duration_ns(text: &str) -> Result<u64, String> {
    let (number, scale) = match text {
        t if t.ends_with("ns") => (&t[..t.len() - 2], 1.0),
        t if t.ends_with("us") => (&t[..t.len() - 2], 1e3),
        t if t.ends_with("ms") => (&t[..t.len() - 2], 1e6),
        t if t.ends_with('s') => (&t[..t.len() - 1], 1e9),
        _ => return Err(format!("duration {text:?} needs a unit: ns, us, ms, or s")),
    };
    match number.trim().parse::<f64>() {
        Ok(value) if value >= 0.0 => Ok((value * scale) as u64),
        _ => Err(format!("duration {text:?} does not start with a number")),
    }
}

pub(crate) struct DirSink {
    pub dir: PathBuf,
    pub format: Format,
    pub pid: u32,
}

impl DumpSink for DirSink {
    fn open(&mut self, reason: &str) -> io::Result<Box<dyn Exporter>> {
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
                return self.format.open(path, self.pid, false);
            }
        }
        unreachable!("every serial number was taken")
    }
}

#[pyclass(module = "trace0._core", frozen)]
pub struct Snapshot {
    pub(crate) tracer: Py<Tracer>,
    pub(crate) reason: String,
    pub(crate) slower_than_ns: Option<u64>,
    pub(crate) window: Mutex<Option<(u64, u64)>>,
}

#[pymethods]
impl Snapshot {
    fn __enter__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, Self>> {
        let me = slf.get();
        me.tracer.get().with_recorder(|running, control| {
            let id = next_window();
            let start_ticks = running.queue.clock().raw();
            control
                .send(Control::Open { id, start_ticks })
                .map_err(|_| PyRuntimeError::new_err("the recorder is gone"))?;
            *me.window.lock() = Some((id, start_ticks));
            Ok(())
        })?;
        Ok(slf.clone())
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<Bound<'_, PyType>>,
        _exc_val: Option<Bound<'_, PyAny>>,
        _exc_tb: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let Some((id, start_ticks)) = self.window.lock().take() else {
            return Ok(false);
        };
        let tagged = match &exc_type {
            Some(t) => Some(format!(
                "{}-{}",
                self.reason,
                t.getattr("__name__")?.extract::<String>()?
            )),
            None => None,
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut wants_ack = false;
        self.tracer.get().with_recorder(|running, control| {
            let clock = running.queue.clock();
            let end_ticks = clock.raw();
            let elapsed = clock
                .ns_since_start(end_ticks)
                .saturating_sub(clock.ns_since_start(start_ticks));
            let reason = match tagged {
                Some(tagged) => Some(tagged),
                None if self.slower_than_ns.is_some_and(|min| elapsed < min) => None,
                None => Some(self.reason.clone()),
            };
            let msg = match reason {
                Some(reason) => {
                    wants_ack = true;
                    Control::Dump {
                        id,
                        start_ticks,
                        end_ticks,
                        reason,
                        done: Some(done_tx),
                    }
                }
                None => Control::Cancel { id },
            };
            control
                .send(msg)
                .map_err(|_| PyRuntimeError::new_err("the recorder is gone"))
        })?;
        if wants_ack {
            py.detach(move || done_rx.recv())
                .map_err(|_| PyRuntimeError::new_err("the recorder did not finish the dump"))?;
        }
        Ok(false)
    }
}
