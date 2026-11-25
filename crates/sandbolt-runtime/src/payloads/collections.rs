use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::num::NonZeroUsize;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::gc::{Trace, Tracer};
use crate::heap::Heap;
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::ops::compare::same_value_raw;
use crate::value::{ObjId, Value, ValueKind};

payloads!(
    MapData {
        entries: OrderedCollection
    },
    SetData {
        entries: OrderedCollection
    },
);

#[derive(Clone, Copy, Debug)]
struct Bucket {
    hash: u64,
    row_plus_one: NonZeroUsize,
}

impl Bucket {
    fn row(self) -> usize {
        self.row_plus_one.get() - 1
    }
}

fn encode_row(row: usize) -> NonZeroUsize {
    NonZeroUsize::new(row + 1).unwrap()
}

#[derive(Debug, Default)]
pub(crate) struct HashIndex {
    buckets: TrackedVec<Option<Bucket>>,
    len: usize,
}

impl HashIndex {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn find(&self, hash: u64, matches: impl FnMut(usize) -> bool) -> Option<usize> {
        self.locate(hash, matches)
            .map(|bucket| self.buckets[bucket].unwrap().row())
    }

    fn locate(&self, hash: u64, mut matches: impl FnMut(usize) -> bool) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let mask = self.buckets.len() - 1;
        let mut offset = hash as usize & mask;
        while let Some(bucket) = self.buckets[offset] {
            if bucket.hash == hash && matches(bucket.row()) {
                return Some(offset);
            }
            offset = (offset + 1) & mask;
        }
        None
    }

    pub(crate) fn has_spare_capacity(&self) -> bool {
        self.len < self.buckets.len() / 2
    }

    pub(crate) fn reserve(&mut self, ledger: &HeapLedger) -> AllocResult<()> {
        if self.has_spare_capacity() {
            return Ok(());
        }
        let size = self
            .buckets
            .len()
            .checked_mul(2)
            .ok_or(AllocError::Limit)?
            .max(8);
        let mut buckets = TrackedVec::new();
        buckets.try_reserve_exact(ledger, size)?;
        for _ in 0..size {
            buckets.push_reserved(None);
        }
        for bucket in self.buckets.iter().flatten().copied() {
            Self::place(&mut buckets, bucket);
        }
        self.buckets = buckets;
        Ok(())
    }

    pub(crate) fn insert(&mut self, hash: u64, row: usize) {
        Self::place(
            &mut self.buckets,
            Bucket {
                hash,
                row_plus_one: encode_row(row),
            },
        );
        self.len += 1;
    }

    fn place(buckets: &mut [Option<Bucket>], bucket: Bucket) {
        let mask = buckets.len() - 1;
        let mut offset = bucket.hash as usize & mask;
        while buckets[offset].is_some() {
            offset = (offset + 1) & mask;
        }
        buckets[offset] = Some(bucket);
    }

    pub(crate) fn relocate(&mut self, hash: u64, from: usize, to: usize) {
        let offset = self.locate(hash, |row| row == from).unwrap();
        self.buckets[offset].as_mut().unwrap().row_plus_one = encode_row(to);
    }

    pub(crate) fn remove(
        &mut self,
        hash: u64,
        matches: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let hole = self.locate(hash, matches)?;
        Some(self.remove_at(hole))
    }

    fn remove_at(&mut self, mut hole: usize) -> usize {
        let removed = self.buckets[hole].unwrap();
        let mask = self.buckets.len() - 1;
        let mut offset = (hole + 1) & mask;
        // Move entries whose probe runs through the hole so lookup can still stop at an empty bucket.
        while let Some(bucket) = self.buckets[offset] {
            let home = bucket.hash as usize & mask;
            if hole.wrapping_sub(home) & mask < offset.wrapping_sub(home) & mask {
                self.buckets[hole] = Some(bucket);
                hole = offset;
            }
            offset = (offset + 1) & mask;
        }
        self.buckets[hole] = None;
        self.len -= 1;
        removed.row()
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    order: u64,
    hash: u64,
    pair: Option<(Value, Value)>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CollectionCursor {
    order: u64,
    row: usize,
    epoch: u64,
}

#[derive(Debug)]
pub struct OrderedCollection {
    entries: TrackedVec<Entry>,
    index: HashIndex,
    seed: u64,
    next_order: u64,
    epoch: u64,
}

impl Default for OrderedCollection {
    fn default() -> Self {
        Self {
            entries: TrackedVec::new(),
            index: HashIndex::default(),
            seed: collection_seed(),
            next_order: 0,
            epoch: 0,
        }
    }
}

pub(super) fn collection_seed() -> u64 {
    static PROCESS_SEED: OnceLock<u64> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seed = *PROCESS_SEED.get_or_init(|| RandomState::new().hash_one(0u64));
    mix64(seed ^ NEXT.fetch_add(1, Ordering::Relaxed))
}

#[inline]
pub(super) fn mix64(word: u64) -> u64 {
    let word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    word ^ (word >> 31)
}

impl Trace for OrderedCollection {
    fn trace(&self, tracer: &Tracer<'_>) {
        for entry in self.entries.iter() {
            entry.pair.trace(tracer);
        }
    }
}

impl OrderedCollection {
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn hash(&self, heap: &Heap, key: Value) -> u64 {
        match key.decode() {
            ValueKind::Int32(n) => mix64(self.seed ^ f64::from(n).to_bits()),
            ValueKind::Float64(n) => mix64(self.seed ^ if n == 0.0 { 0 } else { n.to_bits() }),
            ValueKind::String(id) => {
                let string = heap.string(id);
                let state = self.seed ^ 0x9e37_79b9_7f4a_7c15 ^ u64::from(string.len());
                if let Some(bytes) = string.as_latin1() {
                    hash_units(state, bytes, u64::from)
                } else if let Some(units) = string.as_utf16() {
                    hash_units(state, units, u64::from)
                } else {
                    hash_iter(state, heap.string_units(id).map(u64::from))
                }
            }
            ValueKind::ShortBigInt(n) => {
                let magnitude = n.unsigned_abs();
                hash_bigint(
                    self.seed,
                    n < 0,
                    &[magnitude as u32, (magnitude >> 32) as u32],
                )
            }
            ValueKind::BigInt(id) => {
                let data = heap.bigints.get(id).unwrap();
                hash_bigint(self.seed, data.negative, &data.limbs)
            }
            _ => mix64(self.seed ^ key.bits() ^ 0xd1b5_4a32_d192_ed03),
        }
    }

    pub(crate) fn locate(&self, heap: &Heap, key: Value, hash: u64) -> Option<usize> {
        self.locate_bucket(heap, key, hash)
            .map(|bucket| self.index.buckets[bucket].unwrap().row())
    }

    pub(crate) fn locate_bucket(&self, heap: &Heap, key: Value, hash: u64) -> Option<usize> {
        let key = canonical_key(key);
        self.index.locate(hash, |row| {
            let stored = self.entries[row].pair.unwrap().0;
            key == stored || same_value_raw(heap, key, stored)
        })
    }

    pub(crate) fn value(&self, row: usize) -> Value {
        self.entries[row].pair.unwrap().1
    }

    pub(crate) fn update(&mut self, row: usize, value: Value) {
        self.entries[row].pair.as_mut().unwrap().1 = value;
    }

    pub(crate) fn has_spare_capacity(&self) -> bool {
        self.entries.len() < self.entries.capacity() && self.index.has_spare_capacity()
    }

    pub(crate) fn insert(
        &mut self,
        ledger: &HeapLedger,
        hash: u64,
        key: Value,
        value: Value,
    ) -> AllocResult<()> {
        if self.next_order == u64::MAX {
            return Err(AllocError::Limit);
        }
        self.entries.try_reserve(ledger, 1)?;
        self.index.reserve(ledger)?;
        self.insert_reserved(hash, key, value)
    }

    pub(crate) fn insert_reserved(
        &mut self,
        hash: u64,
        key: Value,
        value: Value,
    ) -> AllocResult<()> {
        let next_order = self.next_order.checked_add(1).ok_or(AllocError::Limit)?;
        self.index.insert(hash, self.entries.len());
        self.entries.push_reserved(Entry {
            order: self.next_order,
            hash,
            pair: Some((canonical_key(key), value)),
        });
        self.next_order = next_order;
        Ok(())
    }

    pub(crate) fn remove_bucket(&mut self, bucket: usize) {
        let row = self.index.remove_at(bucket);
        self.entries[row].pair = None;
        if self.entries.len() - self.len() > self.len() {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut to = 0;
        for from in 0..self.entries.len() {
            let entry = self.entries[from];
            if entry.pair.is_some() {
                if from != to {
                    self.index.relocate(entry.hash, from, to);
                    self.entries[to] = entry;
                }
                to += 1;
            }
        }
        self.entries.truncate(to);
        self.epoch += 1;
    }

    pub(crate) fn clear(&mut self) {
        self.entries = TrackedVec::new();
        self.index = HashIndex::default();
        self.epoch += 1;
    }

    pub(crate) fn next(&self, cursor: &mut CollectionCursor) -> Option<(Value, Value)> {
        // Entry order survives compaction. A cursor only needs to seek after its row moves.
        if cursor.epoch != self.epoch {
            cursor.row = self
                .entries
                .partition_point(|entry| entry.order < cursor.order);
            cursor.epoch = self.epoch;
        }
        while let Some(entry) = self.entries.get(cursor.row) {
            cursor.row += 1;
            cursor.order = entry.order + 1;
            if entry.pair.is_some() {
                return entry.pair;
            }
        }
        None
    }
}

pub(crate) fn canonical_key(key: Value) -> Value {
    if key == Value::number(0.0) || key == Value::number(-0.0) {
        Value::int32(0)
    } else {
        key
    }
}

// Pack four code units into one word regardless of the backing representation.
#[inline]
fn hash_units<T: Copy>(mut state: u64, units: &[T], word: impl Fn(T) -> u64) -> u64 {
    let (chunks, tail) = units.as_chunks::<4>();
    for &[a, b, c, d] in chunks {
        let packed = word(a) | (word(b) << 16) | (word(c) << 32) | (word(d) << 48);
        state = mix64(state ^ packed);
    }
    hash_iter(state, tail.iter().copied().map(word))
}

fn hash_iter(mut state: u64, units: impl Iterator<Item = u64>) -> u64 {
    let mut packed = 0;
    let mut shift = 0;
    for unit in units {
        packed |= unit << shift;
        shift += 16;
        if shift == 64 {
            state = mix64(state ^ packed);
            packed = 0;
            shift = 0;
        }
    }
    mix64(state ^ packed)
}

fn hash_bigint(seed: u64, negative: bool, limbs: &[u32]) -> u64 {
    let len = limbs
        .iter()
        .rposition(|&limb| limb != 0)
        .map_or(0, |i| i + 1);
    let mut state =
        seed ^ 0xa076_1d64_78bd_642f ^ ((len as u64) << 1) ^ u64::from(negative && len != 0);
    let (chunks, tail) = limbs[..len].as_chunks::<2>();
    for &[low, high] in chunks {
        state = mix64(state ^ (u64::from(low) | (u64::from(high) << 32)));
    }
    mix64(state ^ tail.first().copied().map_or(0, u64::from))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CollectionIteration {
    Keys,
    Values,
    Entries,
}

#[derive(Debug)]
pub struct CollectionIteratorData {
    pub(crate) target: Option<ObjId>,
    pub(crate) is_set: bool,
    pub(crate) kind: CollectionIteration,
    pub(crate) cursor: CollectionCursor,
}

impl Trace for CollectionIteratorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.target.trace(tracer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bucket_uses_the_empty_row_as_its_option_tag() {
        assert_eq!(size_of::<Option<Bucket>>(), size_of::<Bucket>());
    }

    #[test]
    fn hash_agrees_across_string_and_number_representations() {
        use crate::string::JsString;

        let mut heap = Heap::new(1024 * 1024).unwrap();
        let ledger = heap.ledger.clone();
        let text = b"abc\xE9\x00xyz!";
        let left = heap
            .strings
            .insert(&ledger, JsString::from_latin1(&ledger, &text[..5]).unwrap())
            .unwrap();
        let right = heap
            .strings
            .insert(&ledger, JsString::from_latin1(&ledger, &text[5..]).unwrap())
            .unwrap();
        let flat = heap
            .strings
            .insert(&ledger, JsString::from_latin1(&ledger, text).unwrap())
            .unwrap();
        let wide = heap
            .strings
            .insert(
                &ledger,
                JsString::Utf16(TrackedVec::from_slice(&ledger, &text.map(u16::from)).unwrap()),
            )
            .unwrap();
        let rope = heap
            .strings
            .insert(
                &ledger,
                JsString::Rope {
                    left,
                    right,
                    len: 9,
                    is_wide: false,
                    depth: 1,
                },
            )
            .unwrap();
        let backing = heap
            .strings
            .insert(
                &ledger,
                JsString::from_latin1(&ledger, b".abc\xE9\x00xyz!.").unwrap(),
            )
            .unwrap();
        let slice = heap
            .strings
            .insert(
                &ledger,
                JsString::Slice {
                    base: backing,
                    start: 1,
                    len: 9,
                    is_wide: false,
                },
            )
            .unwrap();
        let map = OrderedCollection::default();
        let hash = |value| map.hash(&heap, value);
        for id in [wide, rope, slice] {
            assert_eq!(hash(Value::string(id)), hash(Value::string(flat)));
        }
        assert_eq!(hash(Value::int32(-42)), hash(Value::number(-42.0)));
        assert_eq!(hash(Value::int32(0)), hash(Value::number(-0.0)));
    }

    #[test]
    fn hash_index_removes_wrapped_collisions_and_keeps_entries_if_growth_fails() {
        let ledger = HeapLedger::new(4096, 0).unwrap();
        let mut index = HashIndex::default();
        let pairs = [(7, 10), (15, 11), (8, 12), (7, 13)];
        for (hash, row) in pairs {
            index.reserve(&ledger).unwrap();
            index.insert(hash, row);
        }
        ledger.fail_after(Some(0));
        assert_eq!(index.reserve(&ledger), Err(AllocError::Failpoint));
        for (hash, row) in pairs {
            assert_eq!(index.find(hash, |i| i == row), Some(row));
        }
        assert_eq!(index.remove(7, |i| i == 10), Some(10));
        assert_eq!(index.find(7, |i| i == 10), None);
        for (hash, row) in &pairs[1..] {
            assert_eq!(index.find(*hash, |i| i == *row), Some(*row));
        }
    }
}
