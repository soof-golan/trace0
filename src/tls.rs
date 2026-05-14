use crate::event::Event;
use rtrb::Producer;
use std::cell::RefCell;

/// Combined per-thread state accessed once per event.
///
/// Coalesces what used to be three independent `thread_local!`s
/// (last-code cache, ensured-thread flag, producer ring) into a single
/// TLS slot. One `.with(...)` per event.
pub struct PerThread {
    pub last_code_key: usize,
    pub last_code_id: u32,
    pub ensured: bool,
    pub producer: Option<(usize, Producer<Event>)>,
}

thread_local! {
    pub static CTX: RefCell<PerThread> = const {
        RefCell::new(PerThread {
            last_code_key: 0,
            last_code_id: u32::MAX,
            ensured: false,
            producer: None,
        })
    };
}
