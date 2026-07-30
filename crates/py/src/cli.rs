use crate::format::Format;
use crate::tracer::Tracer;
use clap::{Args, Parser, Subcommand};
use pyo3::prelude::*;
use pyo3::types::PyList;

#[derive(Parser)]
#[command(
    name = "trace0",
    about = "Trace a Python script with sys.monitoring; emit Perfetto-compatible output."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a script under the tracer.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Output file path.
    #[arg(short, long)]
    output: String,
    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Protobuf)]
    format: Format,
    /// Python script to run.
    script: String,
    /// Arguments forwarded to the script.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    script_args: Vec<String>,
}

#[pyfunction]
pub fn cli_main(py: Python<'_>, argv: Vec<String>) -> PyResult<i32> {
    let cli = match Cli::try_parse_from(&argv) {
        Ok(c) => c,
        Err(e) => {
            let code = e.exit_code();
            e.print().ok();
            return Ok(code);
        }
    };
    match cli.cmd {
        Cmd::Run(a) => run(py, a),
    }
}

fn run(py: Python<'_>, a: RunArgs) -> PyResult<i32> {
    let tracer = Tracer::new(a.output, a.format.as_str().to_string())?;

    let sys = py.import("sys")?;
    let saved_argv: Py<PyAny> = sys.getattr("argv")?.unbind();
    let mut argv_items: Vec<String> = Vec::with_capacity(1 + a.script_args.len());
    argv_items.push(a.script.clone());
    argv_items.extend(a.script_args);
    let new_argv = PyList::new(py, argv_items)?;
    sys.setattr("argv", &new_argv)?;

    let running = tracer.begin(py)?;
    let run_result = py
        .import("runpy")
        .and_then(|m| m.call_method1("run_path", (a.script.as_str(), py.None(), "__main__")));
    let stop_result = Tracer::end(py, running);

    sys.setattr("argv", saved_argv)?;

    run_result?;
    stop_result?;
    Ok(0)
}
