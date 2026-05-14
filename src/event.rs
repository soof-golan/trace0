use std::time::Instant;

#[derive(Copy, Clone, Debug)]
#[repr(u8)]
pub enum EventKind {
    Begin = 1,
    End = 2,
    Yield = 3,
    Resume = 4,
    Unwind = 5,
    Throw = 6,
}

#[derive(Copy, Clone, Debug)]
pub struct Event {
    /// Raw timestamp at push time. `mach_absolute_time()` on macOS
    /// (= ns on Apple Silicon), elapsed-ns since start on other
    /// platforms. Exporters convert at write time.
    pub ts_ns: u64,
    pub tid: u64,
    pub code_id: u32,
    pub kind: EventKind,
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
pub fn os_tid() -> u64 {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(libc::SYS_gettid) as u64
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut tid: u64 = 0;
        libc::pthread_threadid_np(0, &mut tid);
        tid
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}
