const SLOTS: usize = 256;

const EMPTY_KEY: usize = 0;

const GOLDEN: usize = 0x9E37_79B9_7F4A_7C15;

#[derive(Clone, Copy)]
#[repr(C)]
struct Slot {
    key: usize,
    id: u32,
    _pad: u32,
}

pub struct CodeCache {
    slots: [Slot; SLOTS],
}

const _: () = assert!(std::mem::size_of::<Slot>() == 16);
const _: () = assert!(std::mem::size_of::<CodeCache>() == 4096);

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
    fn an_empty_cache_is_all_zero_bytes() {
        let empty = CodeCache::EMPTY;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &empty as *const CodeCache as *const u8,
                std::mem::size_of::<CodeCache>(),
            )
        };
        assert!(
            bytes.iter().all(|&b| b == 0),
            "a non-zero byte moves the table out of .tbss"
        );
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
        let mut cache = CodeCache::EMPTY;
        cache.put(0x1000, 0);
        assert_eq!(cache.get(0x1000), Some(0));
    }
}
