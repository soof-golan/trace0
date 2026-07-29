/// Monotonic clock with an explicit tick→nanosecond conversion.
///
/// The hot path records whatever the platform's cheapest monotonic
/// counter returns — on macOS that is `mach_absolute_time`, whose ticks
/// are *not* nanoseconds (Apple silicon runs 125/3 ns per tick). The
/// conversion happens here, at drain time, once per event.
///
/// `numer`/`denom` and `start_ticks` are plain fields so tests can build
/// a synthetic clock with an exact, host-independent timebase.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Clock {
    start_ticks: u64,
    numer: u32,
    denom: u32,
}

impl Clock {
    pub fn new(start_ticks: u64, numer: u32, denom: u32) -> Self {
        assert!(numer > 0 && denom > 0, "timebase must be non-zero");
        Self {
            start_ticks,
            numer,
            denom,
        }
    }

    /// Clock anchored at the current instant, using the host timebase.
    pub fn starting_now() -> Self {
        let (numer, denom) = host_timebase();
        Self::new(now_raw(), numer, denom)
    }

    /// Nanoseconds elapsed between the clock's anchor and `ticks`.
    /// Saturates at zero for ticks recorded before the anchor.
    pub fn ns_since_start(&self, ticks: u64) -> u64 {
        let elapsed = ticks.saturating_sub(self.start_ticks) as u128;
        (elapsed * self.numer as u128 / self.denom as u128) as u64
    }

    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }
}

/// Raw monotonic counter. Ticks, not nanoseconds — see [`Clock`].
#[inline]
pub fn now_raw() -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        // Declared directly; libc's binding is deprecated in favour of
        // the mach2 crate, which we don't need a dependency on.
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
        }
        mach_absolute_time()
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
}

/// Nanoseconds per tick, as the fraction `(numer, denom)`.
pub fn host_timebase() -> (u32, u32) {
    #[cfg(target_os = "macos")]
    {
        // Declared directly: libc's binding is deprecated in favour of
        // the mach2 crate, and this is the only symbol we'd want from it.
        #[repr(C)]
        struct TimebaseInfo {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut TimebaseInfo) -> libc::c_int;
        }
        let mut info = TimebaseInfo { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut info) };
        (info.numer, info.denom)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // clock_gettime already yields nanoseconds.
        (1, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Apple-silicon timebase: 125/3 == 41.666… ns per tick.
    const APPLE: (u32, u32) = (125, 3);

    #[test]
    fn anchor_reads_as_zero() {
        let c = Clock::new(1_000, APPLE.0, APPLE.1);
        assert_eq!(c.ns_since_start(1_000), 0);
    }

    #[test]
    fn converts_ticks_to_nanoseconds() {
        let c = Clock::new(1_000, APPLE.0, APPLE.1);
        // 24 ticks * 125/3 == exactly 1000 ns.
        assert_eq!(c.ns_since_start(1_024), 1_000);
        // One second of real time is 24_000_000 ticks at this timebase.
        assert_eq!(c.ns_since_start(1_000 + 24_000_000), 1_000_000_000);
    }

    #[test]
    fn identity_timebase_passes_nanoseconds_through() {
        let c = Clock::new(500, 1, 1);
        assert_eq!(c.ns_since_start(1_500), 1_000);
    }

    #[test]
    fn ticks_before_the_anchor_saturate() {
        let c = Clock::new(1_000, APPLE.0, APPLE.1);
        assert_eq!(c.ns_since_start(999), 0);
    }

    #[test]
    fn wide_tick_values_do_not_overflow() {
        // A machine up for ~50 days: 1e14 ticks. The u128 intermediate
        // keeps `ticks * numer` from wrapping.
        let c = Clock::new(0, APPLE.0, APPLE.1);
        assert_eq!(c.ns_since_start(100_000_000_000_000), 4_166_666_666_666_666);
    }

    #[test]
    fn host_timebase_is_usable() {
        let (numer, denom) = host_timebase();
        assert!(numer > 0 && denom > 0);
    }
}
