use crate::format::Format;
use crate::tracer::Tracer;
use clap::{Args, Parser, Subcommand};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

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
    /// Run a library module as a script, like `python -m`. Every following
    /// argument belongs to the module.
    #[arg(
        short = 'm',
        long = "module",
        value_name = "MODULE",
        num_args = 1..,
        allow_hyphen_values = true,
        conflicts_with = "script"
    )]
    module: Vec<String>,
    /// Keep the last N megabytes of events in memory and write only dumps.
    /// The output becomes a directory of dump files.
    #[arg(long, value_name = "MB")]
    record_last_mb: Option<usize>,
    /// Trace only this process, leaving the ones it starts alone.
    #[arg(long)]
    no_trace_subprocesses: bool,
    /// Python script to run.
    #[arg(required_unless_present = "module")]
    script: Option<String>,
    /// Arguments forwarded to the script.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    script_args: Vec<String>,
}

enum Target {
    Script(String),
    Module(String),
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

/// Split the target from the arguments that belong to it, matching how
/// `python` treats `-m`: the module name is the first of its own arguments.
/// The returned string is `sys.argv[0]`, which `runpy` overwrites with the
/// module's file once it resolves one.
fn split_target(
    module: Vec<String>,
    script: Option<String>,
    script_args: Vec<String>,
) -> (Target, Vec<String>, String) {
    match script {
        Some(script) => (Target::Script(script.clone()), script_args, script),
        None => {
            let mut it = module.into_iter();
            let name = it.next().expect("clap requires a module or a script");
            (Target::Module(name.clone()), it.collect(), name)
        }
    }
}

fn run(py: Python<'_>, a: RunArgs) -> PyResult<i32> {
    let RunArgs {
        output,
        format,
        module,
        record_last_mb,
        no_trace_subprocesses,
        script,
        script_args,
    } = a;
    let tracer = Py::new(
        py,
        Tracer::new(
            output,
            format.as_str().to_string(),
            !no_trace_subprocesses,
            record_last_mb,
        )?,
    )?;
    let (target, args, argv0) = split_target(module, script, script_args);

    let sys = py.import("sys")?;
    let saved_argv: Py<PyAny> = sys.getattr("argv")?.unbind();
    let mut argv_items: Vec<String> = Vec::with_capacity(1 + args.len());
    argv_items.push(argv0);
    argv_items.extend(args);
    let new_argv = PyList::new(py, argv_items)?;
    sys.setattr("argv", &new_argv)?;

    let runpy = py.import("runpy")?;
    let tracer = tracer.bind(py);
    Tracer::__enter__(tracer, py)?;
    let run_result = match &target {
        Target::Script(path) => {
            runpy.call_method1("run_path", (path.as_str(), py.None(), "__main__"))
        }
        Target::Module(name) => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("run_name", "__main__").and_then(|()| {
                kwargs
                    .set_item("alter_sys", true)
                    .and_then(|()| runpy.call_method("run_module", (name.as_str(),), Some(&kwargs)))
            })
        }
    };
    let stop_result = tracer.get().__exit__(py, None, None, None);

    sys.setattr("argv", saved_argv)?;

    run_result?;
    stop_result?;
    Ok(0)
}
