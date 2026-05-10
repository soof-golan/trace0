use pyo3::prelude::*;

mod event;
mod evqueue;
mod exporter;
mod intern;
mod monitoring;
mod threads;
mod tracer;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<tracer::Tracer>()?;
    Ok(())
}
