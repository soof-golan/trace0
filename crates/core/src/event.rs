use crate::clock::Clock;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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

    /// Short tag used by the exporters' `args.kind` / debug output.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Begin => "start",
            EventKind::End => "return",
            EventKind::Yield => "yield",
            EventKind::Resume => "resume",
            EventKind::Unwind => "unwind",
            EventKind::Throw => "throw",
        }
    }

    /// Whether this event opens a slice (as opposed to closing one).
    /// `Throw` opens: it fires when a generator is resumed via
    /// `.throw()`, which re-enters the frame just as `Resume` does.
    pub fn opens_slice(self) -> bool {
        matches!(
            self,
            EventKind::Begin | EventKind::Resume | EventKind::Throw
        )
    }
}

pub const CODE_ID_MASK: u32 = 0x00FF_FFFF;
pub const CODE_ID_MAX: u32 = CODE_ID_MASK;

#[inline]
pub fn pack_code_kind(code_id: u32, kind: EventKind) -> u32 {
    ((kind as u32) << 24) | (code_id & CODE_ID_MASK)
}

/// 8-byte packed event living inside an `EventBatch`. `delta_ticks` is a
/// raw-tick offset from the batch's `base_ticks`; `code_kind` carries the
/// event kind in the top 8 bits and the interned code id in the low 24.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct PackedEvent {
    pub delta_ticks: u32,
    pub code_kind: u32,
}

const _: () = assert!(std::mem::size_of::<PackedEvent>() == 8);

/// 16-byte reconstructed event surfaced to the exporters, with the
/// timestamp already converted to nanoseconds relative to the trace
/// start. The hot path never builds one of these; batch decode produces
/// them at drain time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
    pub fn from_packed(clock: &Clock, base_ticks: u64, tid: u32, p: PackedEvent) -> Self {
        Self {
            ts_ns: clock.ns_since_start(base_ticks + p.delta_ticks as u64),
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
    #[cfg(target_os = "windows")]
    unsafe {
        windows_sys::Win32::System::Threading::GetCurrentThreadId()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_id_and_kind_survive_a_round_trip() {
        for kind in [
            EventKind::Begin,
            EventKind::End,
            EventKind::Yield,
            EventKind::Resume,
            EventKind::Unwind,
            EventKind::Throw,
        ] {
            let ev = Event::new(0, 0, 12_345, kind);
            assert_eq!(ev.code_id(), 12_345);
            assert_eq!(ev.kind(), kind);
        }
    }

    #[test]
    fn the_largest_code_id_does_not_collide_with_the_kind_bits() {
        let ev = Event::new(0, 0, CODE_ID_MAX, EventKind::Throw);
        assert_eq!(ev.code_id(), CODE_ID_MAX);
        assert_eq!(ev.kind(), EventKind::Throw);
    }

    #[test]
    fn unpacking_measures_from_the_clock_anchor() {
        let (clock, _m) = Clock::mock_starting_at(1_000);
        let p = PackedEvent {
            delta_ticks: 24,
            code_kind: pack_code_kind(7, EventKind::Begin),
        };
        let ev = Event::from_packed(&clock, 1_000, 42, p);
        // Base 1000 is the anchor itself, so only the delta remains.
        assert_eq!(ev.ts_ns, 24);
        assert_eq!(ev.tid, 42);
        assert_eq!(ev.code_id(), 7);
        assert_eq!(ev.kind(), EventKind::Begin);
    }

    #[test]
    fn batch_base_offsets_accumulate_onto_the_anchor() {
        let (clock, _m) = Clock::mock();
        let p = PackedEvent {
            delta_ticks: 24,
            code_kind: pack_code_kind(0, EventKind::End),
        };
        let ev = Event::from_packed(&clock, 240, 1, p);
        assert_eq!(ev.ts_ns, 264);
    }

    #[test]
    fn each_thread_reports_its_own_stable_id() {
        let here = os_tid();
        assert_ne!(here, 0, "no thread id here: every thread shares one track");
        assert_eq!(here, os_tid(), "the id changed between two calls");
        let there = std::thread::spawn(os_tid).join().unwrap();
        assert_ne!(here, there, "two live threads reported the same id");
    }

    #[test]
    fn slice_polarity_matches_the_kind() {
        assert!(EventKind::Begin.opens_slice());
        assert!(EventKind::Resume.opens_slice());
        assert!(EventKind::Throw.opens_slice());
        assert!(!EventKind::End.opens_slice());
        assert!(!EventKind::Yield.opens_slice());
        assert!(!EventKind::Unwind.opens_slice());
    }
}
