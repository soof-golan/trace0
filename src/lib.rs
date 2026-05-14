use pyo3::prelude::*;

mod cli;
mod event;
mod evqueue;
mod exporter;
mod intern;
mod monitoring;
mod threads;
mod tls;
mod tracer;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<tracer::Tracer>()?;
    m.add_function(wrap_pyfunction!(cli::cli_main, m)?)?;
    Ok(())
}
