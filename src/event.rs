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

/// 16-byte event. Packed: kind in top 8 bits of `code_kind`, code id
/// in low 24 bits. `tid` is the truncated OS thread id (low 32 bits).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Event {
    /// Raw timestamp at push time. `mach_absolute_time()` on macOS
    /// (= ns on Apple Silicon), elapsed-ns since start on other
    /// platforms. Exporters convert at write time.
    pub ts_ns: u64,
    pub tid: u32,
    code_kind: u32,
}

const CODE_ID_MASK: u32 = 0x00FF_FFFF;
const CODE_ID_MAX: u32 = CODE_ID_MASK;

impl Event {
    #[inline]
    pub fn new(ts_ns: u64, tid: u32, code_id: u32, kind: EventKind) -> Self {
        debug_assert!(code_id <= CODE_ID_MAX, "code_id exceeds 24-bit range");
        Self {
            ts_ns,
            tid,
            code_kind: ((kind as u32) << 24) | (code_id & CODE_ID_MASK),
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
