//! Format-agnostic core of the tracer: the clock, the event model, the
//! lock-free per-thread event queue, and the [`sink::Exporter`] contract
//! the format crates implement.
//!
//! Nothing here depends on pyo3 or on Python, which is what makes the
//! whole record → drain → export path testable against a synthetic
//! [`clock::Clock`].

pub mod clock;
pub mod event;
pub mod evqueue;
pub mod pipeline;
pub mod sink;
pub mod tls;

pub use clock::{Clock, now_raw};
pub use event::{Event, EventKind, PackedEvent, os_tid};
pub use evqueue::EventQueue;
pub use pipeline::run_pipeline;
pub use sink::{CodeInfo, CodeLookup, CodeTable, Exporter, ThreadNames, ThreadTable};
