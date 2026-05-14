use std::time::Instant;

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u8)]
pub enum EventKind {
    Begin = 1,
    End = 2,
    Yield = 3,
    Resume = 4,
    Unwind = 5,
    Throw = 6,
}

impl EventKind {
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => EventKind::Begin,
            2 => EventKind::End,
            3 => EventKind::Yield,
            4 => EventKind::Resume,
            5 => EventKind::Unwind,
            6 => EventKind::Throw,
            _ => EventKind::Begin,
        }
    }
}

const CODE_ID_MASK: u32 = 0x00FF_FFFF;
const CODE_ID_MAX: u32 = CODE_ID_MASK;

#[inline]
pub fn pack_code_kind(code_id: u32, kind: EventKind) -> u32 {
    debug_assert!(code_id <= CODE_ID_MAX, "code_id exceeds 24-bit range");
    ((kind as u32) << 24) | (code_id & CODE_ID_MASK)
}

/// 8-byte packed event living inside an `EventBatch`. `delta_ns` is
/// ns-offset from the batch's `base_ts`; `code_kind` carries the event
/// kind in the top 8 bits and the interned code id in the low 24.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct PackedEvent {
    pub delta_ns: u32,
    pub code_kind: u32,
}

const _: () = assert!(std::mem::size_of::<PackedEvent>() == 8);

/// 16-byte reconstructed event surfaced to the exporter. The hot path
/// never builds one of these; `EventBatch` decode produces them at
/// drain time.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Event {
    pub ts_ns: u64,
    pub tid: u32,
    code_kind: u32,
}

impl Event {
    #[inline]
    pub fn new(ts_ns: u64, tid: u32, code_id: u32, kind: EventKind) -> Self {
        Self {
            ts_ns,
            tid,
            code_kind: pack_code_kind(code_id, kind),
        }
    }

    #[inline]
    pub fn from_packed(base_ts: u64, tid: u32, p: PackedEvent) -> Self {
        Self {
            ts_ns: base_ts + p.delta_ns as u64,
            tid,
            code_kind: p.code_kind,
        }
    }

    #[inline]
    pub fn code_id(&self) -> u32 {
        self.code_kind & CODE_ID_MASK
    }

    #[inline]
    pub fn kind(&self) -> EventKind {
        EventKind::from_u8((self.code_kind >> 24) as u8)
    }
}

#[inline]
pub fn now_ns(_start: Instant) -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::mach_absolute_time()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Instant::now().duration_since(_start).as_nanos() as u64
    }
}

#[inline]
pub fn os_tid() -> u32 {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(libc::SYS_gettid) as u32
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut tid: u64 = 0;
        libc::pthread_threadid_np(0, &mut tid);
        tid as u32
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}
