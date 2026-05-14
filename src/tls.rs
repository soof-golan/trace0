use crate::event::Event;
use crate::evqueue::BATCH_N;
use rtrb::Producer;
use std::cell::RefCell;

/// Combined per-thread state accessed once per event.
///
/// Events are written into `batch` (a thread-local `Vec<Event>` with
/// pre-reserved capacity). When full, the whole `Box<Vec<Event>>` is
/// handed off via `producer` to the consumer in one SPSC push — one
/// atomic per BATCH_N events instead of one per event.
pub struct PerThread {
    pub last_code_key: usize,
    pub last_code_id: u32,
    pub ensured: bool,
    pub producer: Option<(usize, Producer<Box<Vec<Event>>>)>,
    pub batch: Option<Box<Vec<Event>>>,
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
        if batch.is_empty() {
            self.batch = Some(batch);
            return;
        }
        // Leave a fresh writable batch in place. PEP 669 does not
        // guarantee zero callbacks after `set_events(0)` returns;
        // a straggler PY_START on this thread would otherwise
        // hit `unwrap` on `None` in `push_with_ctx`.
        self.batch = Some(Box::new(Vec::with_capacity(BATCH_N)));
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
            ensured: false,
            producer: None,
            batch: None,
        })
    };
}
