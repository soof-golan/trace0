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

const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

impl Clock {
    pub fn new(start_ticks: u64, numer: u32, denom: u32) -> Self {
        assert!(numer > 0 && denom > 0, "timebase must be non-zero");
        // Reduced so the common timebases collapse to `denom == 1`, which
        // `ns_since_start` converts without dividing at all. Every ratio
        // trace0 sees in practice is n/n or 1/1.
        let g = gcd(numer, denom);
        Self {
            start_ticks,
            numer: numer / g,
            denom: denom / g,
        }
    }

    /// Clock anchored at the current instant, using the host timebase.
    pub fn starting_now() -> Self {
        let (numer, denom) = host_timebase();
        Self::new(now_raw(), numer, denom)
    }

    /// Nanoseconds elapsed between the clock's anchor and `ticks`.
    /// Saturates at zero for ticks recorded before the anchor.
    /// Nanoseconds elapsed between the clock's anchor and `ticks`.
    /// Saturates at zero for ticks recorded before the anchor.
    ///
    /// Dividing by a runtime denominator is a `__udivti3` libcall, so the
    /// reduced `denom == 1` case -- every platform trace0 currently runs
    /// on -- skips it. The division stays as the exact fallback for a
    /// timebase that genuinely is a fraction.
    #[inline]
    pub fn ns_since_start(&self, ticks: u64) -> u64 {
        let elapsed = ticks.saturating_sub(self.start_ticks);
        if self.denom == 1 {
            return elapsed.wrapping_mul(self.numer as u64);
        }
        ((elapsed as u128 * self.numer as u128) / self.denom as u128) as u64
    }

    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }
}

/// Raw monotonic counter. Ticks, not nanoseconds — see [`Clock`].
#[inline]
pub fn now_raw() -> u64 {
    // `mach_absolute_time` reads exactly this register, but as an
    // out-of-line libc call with a barrier, which measured at ~16ns per
    // event -- more than everything else the callback does put together.
    // Reading it directly gives the same tick values, and the timebase
    // below still applies unchanged.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    unsafe {
        let ticks: u64;
        std::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks, options(nomem, nostack, preserves_flags));
        ticks
    }
    #[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
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
    // Must match whatever counter `now_raw` reads. `cntvct_el0` runs at
    // `cntfrq_el0`, which is 1GHz on Apple silicon -- nanosecond ticks,
    // where `mach_absolute_time` only offers 24MHz (41.67ns per tick).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let freq: u64;
        unsafe {
            std::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack, preserves_flags));
        }
        (1_000_000_000, freq as u32)
    }
    #[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
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

    /// The multiply-only path must agree with the division it replaces,
    /// exactly, not merely closely -- an earlier fixed-point version of
    /// this drifted 7.7us over a tick range this test covers.
    #[test]
    fn reducible_timebases_convert_without_dividing() {
        // cntvct_el0 at its 1GHz frequency, as `host_timebase` reports it.
        let c = Clock::new(0, 1_000_000_000, 1_000_000_000);
        assert_eq!(c.denom, 1, "n/n must reduce to 1/1");
        for ticks in [0u64, 1, 999, 1 << 32, u64::MAX / 4, 4_000_000_000_000_000] {
            let exact = ticks as u128 * 1_000_000_000u128 / 1_000_000_000u128;
            assert_eq!(c.ns_since_start(ticks) as u128, exact, "at {ticks}");
        }
    }

    #[test]
    fn irreducible_timebases_stay_exact() {
        let c = Clock::new(0, APPLE.0, APPLE.1);
        assert_eq!(c.denom, 3, "125/3 has no common factor to remove");
        for ticks in [0u64, 3, 100, 4_000_000_000_000_000] {
            let exact = (ticks as u128 * 125 / 3) as u64;
            assert_eq!(c.ns_since_start(ticks), exact, "at {ticks}");
        }
    }

    #[test]
    fn reduction_preserves_the_ratio() {
        let reduced = Clock::new(0, 250, 6);
        let raw = Clock::new(0, 125, 3);
        assert_eq!((reduced.numer, reduced.denom), (raw.numer, raw.denom));
    }

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
