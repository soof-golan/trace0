use pyo3::prelude::*;

mod cli;
mod format;
mod intern;
mod monitoring;
mod threads;
mod tracer;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<tracer::Tracer>()?;
    m.add_function(wrap_pyfunction!(cli::cli_main, m)?)?;
    m.add_function(wrap_pyfunction!(tracer::_before_fork, m)?)?;
    m.add_function(wrap_pyfunction!(tracer::_after_fork_in_parent, m)?)?;
    m.add_function(wrap_pyfunction!(tracer::_after_fork_in_child, m)?)?;
    Ok(())
}
