use crate::evqueue::{BATCH_N, EventBatch};
use rtrb::Producer;
use std::cell::RefCell;

/// Combined per-thread state accessed once per event.
///
/// Events are packed into the thread-local `batch.events`
/// (`Vec<PackedEvent>` with pre-reserved capacity). When full, the
/// whole `Box<EventBatch>` is handed off via `producer` in one SPSC
/// push — one atomic per BATCH_N events instead of one per event.
pub struct PerThread {
    pub last_code_key: usize,
    pub last_code_id: u32,
    /// OS thread id, resolved once. `u32::MAX` means not yet resolved.
    /// Asking libc for it on every event costs more than the push does.
    pub tid: u32,
    pub ensured: bool,
    pub producer: Option<(usize, Producer<Box<EventBatch>>)>,
    pub batch: Option<Box<EventBatch>>,
}

impl PerThread {
    /// Push the in-flight partial batch into the SPSC ring so the
    /// consumer sees events accumulated since the last full-batch
    /// boundary. Best-effort: if the ring is full, the partial is
    /// dropped along with everything else.
    pub fn flush_partial(&mut self) {
        let Some(batch) = self.batch.take() else {
            return;
        };
        let Some((_, prod)) = self.producer.as_mut() else {
            self.batch = Some(batch);
            return;
        };
        if batch.events.is_empty() {
            self.batch = Some(batch);
            return;
        }
        // Leave a fresh writable batch in place. PEP 669 does not
        // guarantee zero callbacks after `set_events(0)` returns;
        // a straggler PY_START on this thread would otherwise hit
        // `unwrap` on `None` in `push_with_ctx`.
        self.batch = Some(Box::new(EventBatch::with_capacity(
            BATCH_N,
            batch.base_ticks,
            batch.tid,
        )));
        let _ = prod.push(batch);
    }
}

impl Drop for PerThread {
    fn drop(&mut self) {
        self.flush_partial();
    }
}

thread_local! {
    pub static CTX: RefCell<PerThread> = const {
        RefCell::new(PerThread {
            last_code_key: 0,
            last_code_id: u32::MAX,
            tid: u32::MAX,
            ensured: false,
            producer: None,
            batch: None,
        })
    };
}
