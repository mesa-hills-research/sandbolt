use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};

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
    row: usize,
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
            .map(|bucket| self.buckets[bucket].unwrap().row)
    }

    fn locate(&self, hash: u64, mut matches: impl FnMut(usize) -> bool) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let mask = self.buckets.len() - 1;
        let mut offset = hash as usize & mask;
        while let Some(bucket) = self.buckets[offset] {
            if bucket.hash == hash && matches(bucket.row) {
                return Some(offset);
            }
            offset = (offset + 1) & mask;
        }
        None
    }

    pub(crate) fn reserve(&mut self, ledger: &HeapLedger) -> AllocResult<()> {
        if self.len < self.buckets.len() / 2 {
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
        Self::place(&mut self.buckets, Bucket { hash, row });
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
        self.buckets[offset].as_mut().unwrap().row = to;
    }

    pub(crate) fn remove(
        &mut self,
        hash: u64,
        matches: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let mut hole = self.locate(hash, matches)?;
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
        Some(removed.row)
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

#[derive(Debug, Default)]
pub struct OrderedCollection {
    entries: TrackedVec<Entry>,
    index: HashIndex,
    seed: RandomState,
    next_order: u64,
    epoch: u64,
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
        let mut hasher = self.seed.build_hasher();
        let key = canonical_key(key);
        match key.decode() {
            ValueKind::Int32(_) | ValueKind::Float64(_) => {
                0u8.hash(&mut hasher);
                key.as_number().unwrap().to_bits().hash(&mut hasher);
            }
            ValueKind::String(id) => {
                1u8.hash(&mut hasher);
                for unit in heap.string_units(id) {
                    unit.hash(&mut hasher);
                }
            }
            ValueKind::ShortBigInt(n) => {
                let magnitude = n.unsigned_abs();
                hash_bigint(
                    n < 0,
                    &[magnitude as u32, (magnitude >> 32) as u32],
                    &mut hasher,
                );
            }
            ValueKind::BigInt(id) => {
                let data = heap.bigints.get(id).unwrap();
                hash_bigint(data.negative, &data.limbs, &mut hasher);
            }
            _ => {
                3u8.hash(&mut hasher);
                key.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    pub(crate) fn locate(&self, heap: &Heap, key: Value, hash: u64) -> Option<usize> {
        let key = canonical_key(key);
        self.index.find(hash, |row| {
            same_value_raw(heap, key, self.entries[row].pair.unwrap().0)
        })
    }

    pub(crate) fn value(&self, row: usize) -> Value {
        self.entries[row].pair.unwrap().1
    }

    pub(crate) fn update(&mut self, row: usize, value: Value) {
        self.entries[row].pair.as_mut().unwrap().1 = value;
    }

    pub(crate) fn insert(
        &mut self,
        ledger: &HeapLedger,
        hash: u64,
        key: Value,
        value: Value,
    ) -> AllocResult<()> {
        let next_order = self.next_order.checked_add(1).ok_or(AllocError::Limit)?;
        self.entries.try_reserve(ledger, 1)?;
        self.index.reserve(ledger)?;
        self.index.insert(hash, self.entries.len());
        self.entries.push_reserved(Entry {
            order: self.next_order,
            hash,
            pair: Some((canonical_key(key), value)),
        });
        self.next_order = next_order;
        Ok(())
    }

    pub(crate) fn remove(&mut self, hash: u64, row: usize) {
        self.index
            .remove(hash, |candidate| candidate == row)
            .unwrap();
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
    if key.as_number() == Some(0.0) {
        Value::int32(0)
    } else {
        key
    }
}

fn hash_bigint(negative: bool, limbs: &[u32], hasher: &mut impl Hasher) {
    let len = limbs
        .iter()
        .rposition(|&limb| limb != 0)
        .map_or(0, |i| i + 1);
    2u8.hash(hasher);
    (negative && len != 0).hash(hasher);
    limbs[..len].hash(hasher);
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
