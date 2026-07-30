//! Per-thread cache from code-object address to interned code id.
//!
//! The single cached code object in `tls::Hot` covers a thread that stays
//! inside one function, which recursion and tight loops both do. Anything
//! that alternates between functions misses it on every event and falls
//! through to the interner's `RwLock` -- uncontended in principle, but a
//! shared cache line that every traced thread then writes to in turn.
//!
//! This sits between the two, and shares nothing.
//!
//! Caching an address is only sound because the interner holds a strong
//! reference to every code object it has seen. An interned address is
//! pinned for the tracer's lifetime and cannot be recycled under us.

/// Entries per thread, 16 bytes each. Sized for a call-dense program's
/// hot set rather than its total: sqlglot reaches 2,232 code objects but
/// cycles through far fewer at any one moment.
const SLOTS: usize = 256;

/// Address zero marks a free slot. No code object lives there, and
/// picking a marker of zero keeps the table in `.tbss`, so a thread that
/// never records an event never faults the pages in.
const EMPTY_KEY: usize = 0;

/// Knuth's multiplicative constant. Code-object addresses are aligned and
/// often consecutive, so the low bits are nearly constant and the high
/// bits carry what little entropy there is -- multiply, then take from the
/// top.
const GOLDEN: usize = 0x9E37_79B9_7F4A_7C15;

#[derive(Clone, Copy)]
#[repr(C)]
struct Slot {
    key: usize,
    id: u32,
    _pad: u32,
}

/// A direct-mapped, per-thread address → code id table.
///
/// Collisions evict rather than probe. An evicted key costs one more trip
/// to the interner, which is what every lookup cost before this existed.
pub struct CodeCache {
    slots: [Slot; SLOTS],
}

impl CodeCache {
    pub const EMPTY: Self = CodeCache {
        slots: [Slot {
            key: EMPTY_KEY,
            id: 0,
            _pad: 0,
        }; SLOTS],
    };

    #[inline(always)]
    pub fn get(&self, key: usize) -> Option<u32> {
        if key == EMPTY_KEY {
            return None;
        }
        let slot = &self.slots[Self::slot_of(key)];
        (slot.key == key).then_some(slot.id)
    }

    #[inline(always)]
    pub fn put(&mut self, key: usize, id: u32) {
        if key == EMPTY_KEY {
            return;
        }
        self.slots[Self::slot_of(key)] = Slot { key, id, _pad: 0 };
    }

    #[inline(always)]
    fn slot_of(key: usize) -> usize {
        key.wrapping_mul(GOLDEN) >> (usize::BITS as usize - SLOTS.trailing_zeros() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two keys chosen to land in the same slot, for the eviction tests.
    fn colliding_pair() -> (usize, usize) {
        let first = 0x1000_usize;
        let slot = CodeCache::slot_of(first);
        let second = (0x2000_usize..0x100_0000)
            .step_by(16)
            .find(|&k| k != first && CodeCache::slot_of(k) == slot)
            .expect("some later key must share the slot");
        (first, second)
    }

    #[test]
    fn a_key_never_stored_is_a_miss() {
        let cache = CodeCache::EMPTY;
        assert_eq!(cache.get(0x1000), None);
    }

    #[test]
    fn a_stored_key_reads_back() {
        let mut cache = CodeCache::EMPTY;
        cache.put(0x1000, 7);
        assert_eq!(cache.get(0x1000), Some(7));
    }

    #[test]
    fn a_null_key_is_never_stored() {
        // Zero is the empty marker, which is what lets the whole table
        // live in .tbss and cost nothing to start a thread. No code
        // object lives at address zero, so refusing it costs nothing.
        let mut cache = CodeCache::EMPTY;
        cache.put(0, 7);
        assert_eq!(cache.get(0), None);
    }

    #[test]
    fn a_colliding_key_evicts_the_one_before_it() {
        let (first, second) = colliding_pair();
        let mut cache = CodeCache::EMPTY;
        cache.put(first, 1);
        cache.put(second, 2);
        assert_eq!(cache.get(second), Some(2));
        assert_eq!(cache.get(first), None, "evicted, not silently wrong");
    }

    #[test]
    fn an_evicted_key_can_be_stored_again() {
        let (first, second) = colliding_pair();
        let mut cache = CodeCache::EMPTY;
        cache.put(first, 1);
        cache.put(second, 2);
        cache.put(first, 1);
        assert_eq!(cache.get(first), Some(1));
        assert_eq!(cache.get(second), None);
    }

    #[test]
    fn distinct_keys_do_not_evict_each_other() {
        // Consecutive allocations, which is what a run of code objects
        // from one module looks like. A cache that degenerates on this
        // pattern is worse than no cache.
        let mut cache = CodeCache::EMPTY;
        let keys: Vec<usize> = (0..SLOTS / 2).map(|i| 0x1_0000 + i * 96).collect();
        for (i, &k) in keys.iter().enumerate() {
            cache.put(k, i as u32);
        }
        let hits = keys
            .iter()
            .enumerate()
            .filter(|&(i, &k)| cache.get(k) == Some(i as u32))
            .count();
        assert!(
            hits * 10 >= keys.len() * 8,
            "kept only {hits} of {} at half load",
            keys.len()
        );
    }

    #[test]
    fn every_slot_is_reachable() {
        let reached: std::collections::HashSet<usize> = (0..SLOTS * 64)
            .map(|i| CodeCache::slot_of(0x1_0000 + i * 16))
            .collect();
        assert_eq!(reached.len(), SLOTS, "some slot can never be used");
    }

    #[test]
    fn a_stored_id_is_not_confused_with_a_miss() {
        // Code ids start at zero, so the empty marker cannot be the id.
        let mut cache = CodeCache::EMPTY;
        cache.put(0x1000, 0);
        assert_eq!(cache.get(0x1000), Some(0));
    }
}
