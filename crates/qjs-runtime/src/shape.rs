use std::ops::{BitOr, BitOrAssign};

use crate::arena::ArenaItem;
use crate::atom::{Atom, KeyKind, PropKey};
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedBox, TrackedVec};
use crate::value::{ObjId, ShapeId, Value};

pub(crate) const LINEAR_LOOKUP_LIMIT: usize = 8;
pub(crate) const MAX_SHARED_SHAPE_PROPS: usize = 128;
const LOOKUP_TOMBSTONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PropFlags(u8);

impl PropFlags {
    pub const WRITABLE: Self = Self(1);
    pub const ENUMERABLE: Self = Self(2);
    pub const CONFIGURABLE: Self = Self(4);
    pub const ACCESSOR: Self = Self(8);
    pub(crate) const DELETED: Self = Self(16);
    pub const DEFAULT_DATA: Self = Self(7);

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub fn set(&mut self, flag: Self, on: bool) {
        if on {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

impl BitOr for PropFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for PropFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShapeProp {
    pub key: PropKey,
    pub flags: PropFlags,
}

impl ShapeProp {
    // Non-index static builtin atom so deleted dictionary slots never trace GC refs
    // or match array-index checks on prototype chains.
    pub(crate) const DELETED: Self = Self {
        key: PropKey::atom(Atom::LENGTH),
        flags: PropFlags::DELETED,
    };

    #[inline]
    pub const fn is_deleted(self) -> bool {
        self.flags.contains(PropFlags::DELETED)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShapeFlags(u8);

impl ShapeFlags {
    pub(crate) const DICTIONARY: Self = Self(1);
    pub(crate) const BUILDING: Self = Self(2);
    pub(crate) const HAS_INDEX_KEY: Self = Self(4);

    #[inline]
    pub(crate) const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    #[inline]
    pub(crate) fn set(&mut self, flag: Self, on: bool) {
        if on {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

#[inline]
const fn is_index_key(key: PropKey) -> bool {
    matches!(key.decode(), KeyKind::Index(_))
}

#[derive(Debug)]
struct ShapeLookup {
    buckets: TrackedVec<u32>,
    live: u32,
    tombstones: u32,
}

impl ShapeLookup {
    fn build(ledger: &HeapLedger, props: &[ShapeProp], min_live: usize) -> AllocResult<Self> {
        let live = props.iter().filter(|p| !p.is_deleted()).count();
        let target = live.max(min_live);
        let len = target
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .filter(|_| u32::try_from(props.len()).is_ok_and(|n| n < LOOKUP_TOMBSTONE))
            .ok_or(AllocError::Limit)?;
        let mut buckets = TrackedVec::new();
        buckets.try_reserve_exact(ledger, len)?;
        for _ in 0..len {
            buckets.push_reserved(0);
        }
        let mask = len - 1;
        for (index, prop) in props.iter().enumerate() {
            if prop.is_deleted() {
                continue;
            }
            let mut pos = hash(prop.key) & mask;
            while buckets[pos] != 0 {
                pos = (pos + 1) & mask;
            }
            buckets[pos] = index as u32 + 1;
        }
        Ok(Self {
            buckets,
            live: live as u32,
            tombstones: 0,
        })
    }

    fn insert_slot(&mut self, key: PropKey, index: usize) {
        let mask = self.buckets.len() - 1;
        let mut pos = hash(key) & mask;
        loop {
            let entry = self.buckets[pos];
            if entry == 0 {
                self.buckets[pos] = index as u32 + 1;
                self.live += 1;
                return;
            }
            if entry == LOOKUP_TOMBSTONE {
                self.buckets[pos] = index as u32 + 1;
                self.live += 1;
                self.tombstones -= 1;
                return;
            }
            pos = (pos + 1) & mask;
        }
    }

    fn rebuild_in_place(&mut self, ledger: &HeapLedger, props: &[ShapeProp], extra: usize) {
        let live = props.len();
        let desired = (live + extra).saturating_mul(2).next_power_of_two().max(16);
        if desired < self.buckets.len() {
            self.buckets.truncate(desired);
            self.buckets.shrink_to(ledger, desired);
        }
        self.buckets.fill(0);
        let mask = self.buckets.len() - 1;
        for (index, prop) in props.iter().enumerate() {
            debug_assert!(!prop.is_deleted());
            let mut pos = hash(prop.key) & mask;
            while self.buckets[pos] != 0 {
                pos = (pos + 1) & mask;
            }
            self.buckets[pos] = index as u32 + 1;
        }
        self.live = live as u32;
        self.tombstones = 0;
    }
}

#[derive(Debug)]
pub struct Shape {
    pub proto: Option<ObjId>,
    pub props: TrackedVec<ShapeProp>,
    lookup: Option<TrackedBox<ShapeLookup>>,
    transitions: Option<TrackedBox<TrackedVec<(ShapeProp, ShapeId)>>>,
    flags: ShapeFlags,
    pub(crate) churn: u8,
}

impl ArenaItem for Shape {
    type Id = ShapeId;
}
impl Trace for Shape {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.proto.trace(tracer);
        for prop in self.props.iter() {
            if !prop.is_deleted() {
                prop.key.trace(tracer);
            }
        }
    }
}

impl Shape {
    pub const fn empty(proto: Option<ObjId>) -> Self {
        Self {
            proto,
            props: TrackedVec::new(),
            lookup: None,
            transitions: None,
            flags: ShapeFlags(0),
            churn: 0,
        }
    }

    #[inline]
    pub const fn is_dictionary(&self) -> bool {
        self.flags.contains(ShapeFlags::DICTIONARY)
    }

    #[inline]
    pub const fn is_building(&self) -> bool {
        self.flags.contains(ShapeFlags::BUILDING)
    }

    #[inline]
    pub const fn is_mutable(&self) -> bool {
        (self.flags.0 & (ShapeFlags::DICTIONARY.0 | ShapeFlags::BUILDING.0)) != 0
    }

    #[inline]
    pub const fn has_index_key(&self) -> bool {
        self.flags.contains(ShapeFlags::HAS_INDEX_KEY)
    }

    #[inline]
    pub fn live_count(&self) -> usize {
        if let Some(table) = self.lookup.as_deref() {
            table.live as usize
        } else if self.is_dictionary() {
            self.props.iter().filter(|p| !p.is_deleted()).count()
        } else {
            self.props.len()
        }
    }

    #[inline]
    pub fn live_props(&self) -> impl Iterator<Item = (usize, ShapeProp)> + '_ {
        self.props
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, prop)| !prop.is_deleted())
    }

    #[inline]
    pub fn find(&self, key: PropKey) -> Option<(usize, PropFlags)> {
        if is_index_key(key) && !self.has_index_key() {
            return None;
        }
        let Some(table) = self.lookup.as_deref() else {
            for (index, prop) in self.props.iter().enumerate() {
                if !prop.is_deleted() && prop.key == key {
                    return Some((index, prop.flags));
                }
            }
            return None;
        };
        let mask = table.buckets.len() - 1;
        let mut pos = hash(key) & mask;
        loop {
            let raw = table.buckets[pos];
            if raw == 0 {
                return None;
            }
            if raw != LOOKUP_TOMBSTONE {
                let entry = (raw - 1) as usize;
                let prop = self.props[entry];
                if prop.key == key {
                    return Some((entry, prop.flags));
                }
            }
            pos = (pos + 1) & mask;
        }
    }

    pub fn build(
        ledger: &HeapLedger,
        proto: Option<ObjId>,
        count: usize,
        props: impl Iterator<Item = ShapeProp>,
        churn: u8,
        dictionary: bool,
    ) -> AllocResult<Self> {
        let mut shape = Self::empty(proto);
        if dictionary || churn >= 8 || count > MAX_SHARED_SHAPE_PROPS {
            shape.flags.set(ShapeFlags::DICTIONARY, true);
        }
        shape.churn = churn;
        if count == 0 {
            return Ok(shape);
        }
        if u32::try_from(count).is_err_and(|_| true) || count as u32 >= LOOKUP_TOMBSTONE {
            return Err(AllocError::Limit);
        }
        shape.props.try_reserve_exact(ledger, count)?;
        for prop in props {
            debug_assert!(!prop.is_deleted());
            if is_index_key(prop.key) {
                shape.flags.set(ShapeFlags::HAS_INDEX_KEY, true);
            }
            shape.props.push_reserved(prop);
        }
        if shape.props.len() > LINEAR_LOOKUP_LIMIT {
            let table = ShapeLookup::build(ledger, &shape.props, shape.props.len())?;
            shape.lookup = Some(TrackedBox::new(ledger, table)?);
        }
        Ok(shape)
    }

    pub(crate) fn build_building(
        ledger: &HeapLedger,
        proto: Option<ObjId>,
        existing: &[ShapeProp],
        added: Option<ShapeProp>,
        churn: u8,
    ) -> AllocResult<Self> {
        let mut shape = Self::empty(proto);
        shape.flags.set(ShapeFlags::BUILDING, true);
        shape.churn = churn;
        let needed = existing.len() + usize::from(added.is_some());
        if needed > 0 {
            shape.props.try_reserve(ledger, needed)?;
            for &prop in existing {
                if !prop.is_deleted() {
                    if is_index_key(prop.key) {
                        shape.flags.set(ShapeFlags::HAS_INDEX_KEY, true);
                    }
                    shape.props.push_reserved(prop);
                }
            }
            if let Some(prop) = added {
                if is_index_key(prop.key) {
                    shape.flags.set(ShapeFlags::HAS_INDEX_KEY, true);
                }
                shape.props.push_reserved(prop);
            }
        }
        Ok(shape)
    }

    pub(crate) fn push_building_prop(
        &mut self,
        ledger: &HeapLedger,
        values: &mut TrackedVec<Value>,
        prop: ShapeProp,
        value: Value,
    ) -> AllocResult<()> {
        debug_assert!(self.is_building());
        self.props.try_reserve(ledger, 1)?;
        values.try_reserve(ledger, 1)?;
        if is_index_key(prop.key) {
            self.flags.set(ShapeFlags::HAS_INDEX_KEY, true);
        }
        self.props.push_reserved(prop);
        values.push_reserved(value);
        Ok(())
    }

    pub(crate) fn clear_building(&mut self, ledger: &HeapLedger) {
        if !self.is_building() {
            return;
        }
        self.flags.set(ShapeFlags::BUILDING, false);
        self.props.shrink_to(ledger, self.props.len());
    }

    pub(crate) fn ensure_lookup(&mut self, ledger: &HeapLedger) -> AllocResult<()> {
        if self.props.len() > LINEAR_LOOKUP_LIMIT && self.lookup.is_none() {
            let table = ShapeLookup::build(ledger, &self.props, self.props.len())?;
            self.lookup = Some(TrackedBox::new(ledger, table)?);
        }
        Ok(())
    }

    pub(crate) fn find_transition(
        &self,
        prop: ShapeProp,
        is_alive: impl Fn(ShapeId) -> bool,
    ) -> Option<ShapeId> {
        let transitions = self.transitions.as_deref()?;
        transitions
            .iter()
            .find_map(|&(p, target)| (p == prop && is_alive(target)).then_some(target))
    }

    pub(crate) fn add_transition(
        &mut self,
        ledger: &HeapLedger,
        prop: ShapeProp,
        target: ShapeId,
    ) -> AllocResult<()> {
        if let Some(transitions) = self.transitions.as_deref_mut() {
            transitions.push(ledger, (prop, target))
        } else {
            let mut vec = TrackedVec::new();
            vec.try_reserve(ledger, 1)?;
            vec.push_reserved((prop, target));
            self.transitions = Some(TrackedBox::new(ledger, vec)?);
            Ok(())
        }
    }

    pub(crate) fn retain_transitions(&mut self, mut keep: impl FnMut(ShapeId) -> bool) {
        if let Some(transitions) = self.transitions.as_deref_mut() {
            transitions.retain(|&(_, id)| keep(id));
            if transitions.is_empty() {
                self.transitions = None;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn transitions_len(&self) -> usize {
        self.transitions.as_deref().map_or(0, |t| t.len())
    }

    /// Inserts a new property in-place into a dictionary shape and its owner's value vector.
    /// Atomic on OOM/failpoint: on error, both `self.props` and `values` remain unchanged.
    pub(crate) fn dict_insert(
        &mut self,
        ledger: &HeapLedger,
        values: &mut TrackedVec<Value>,
        prop: ShapeProp,
        value: Value,
    ) -> AllocResult<()> {
        debug_assert!(self.is_dictionary());
        debug_assert_eq!(self.props.len(), values.len());
        if self.props.len() as u64 + 1 >= LOOKUP_TOMBSTONE as u64 {
            return Err(AllocError::Limit);
        }

        // Maintain hash lookup for > LINEAR_LOOKUP_LIMIT properties before mutating vectors.
        let action = if let Some(table) = self.lookup.as_deref() {
            let used = table.live as usize + table.tombstones as usize + 1;
            if used.saturating_mul(2) > table.buckets.len() {
                if table.tombstones >= table.live {
                    1 // compact in place
                } else {
                    2 // grow lookup
                }
            } else {
                0
            }
        } else {
            0
        };
        if action == 1 {
            self.compact_in_place(ledger, values, 1);
        } else if action == 2 {
            let live = self.lookup.as_deref().unwrap().live as usize;
            let next = ShapeLookup::build(ledger, &self.props, live + 1)?;
            *self.lookup.as_deref_mut().unwrap() = next;
        }
        if self.lookup.is_none() && self.props.len() >= LINEAR_LOOKUP_LIMIT {
            let table = ShapeLookup::build(ledger, &self.props, self.props.len() + 1)?;
            self.lookup = Some(TrackedBox::new(ledger, table)?);
        }

        self.props.try_reserve(ledger, 1)?;
        values.try_reserve(ledger, 1)?;
        let index = self.props.len();
        if is_index_key(prop.key) {
            self.flags.set(ShapeFlags::HAS_INDEX_KEY, true);
        }
        self.props.push_reserved(prop);
        values.push_reserved(value);
        if let Some(table) = self.lookup.as_deref_mut() {
            table.insert_slot(prop.key, index);
        }
        Ok(())
    }

    /// Single-probe lookup and delete for dictionary shapes.
    /// Returns `None` if absent, `Some(false)` if non-configurable, `Some(true)` if deleted.
    pub(crate) fn dict_delete_if_configurable(
        &mut self,
        ledger: &HeapLedger,
        values: &mut TrackedVec<Value>,
        key: PropKey,
    ) -> Option<bool> {
        debug_assert!(self.is_dictionary());
        debug_assert_eq!(self.props.len(), values.len());
        if is_index_key(key) && !self.has_index_key() {
            return None;
        }
        let Some(table) = self.lookup.as_deref_mut() else {
            let index = self
                .props
                .iter()
                .position(|p| !p.is_deleted() && p.key == key)?;
            if !self.props[index].flags.contains(PropFlags::CONFIGURABLE) {
                return Some(false);
            }
            self.props.remove(index);
            values.remove(index);
            return Some(true);
        };
        let mask = table.buckets.len() - 1;
        let mut pos = hash(key) & mask;
        let index = loop {
            let raw = table.buckets[pos];
            if raw == 0 {
                return None;
            }
            if raw != LOOKUP_TOMBSTONE {
                let entry = (raw - 1) as usize;
                if self.props[entry].key == key {
                    break entry;
                }
            }
            pos = (pos + 1) & mask;
        };
        if !self.props[index].flags.contains(PropFlags::CONFIGURABLE) {
            return Some(false);
        }
        self.props[index] = ShapeProp::DELETED;
        values[index] = Value::EMPTY;
        table.buckets[pos] = LOOKUP_TOMBSTONE;
        table.live -= 1;
        table.tombstones += 1;
        if table.live == 0 {
            self.props.clear();
            values.clear();
            self.lookup = None;
            self.flags.set(ShapeFlags::HAS_INDEX_KEY, false);
            if self.props.capacity() > 32 {
                self.props.shrink_to(ledger, 0);
            }
            if values.capacity() > 32 {
                values.shrink_to(ledger, 0);
            }
        } else if self.props.len() >= 16 && (table.live as usize) * 4 <= self.props.len() {
            self.compact_in_place(ledger, values, 0);
        }
        Some(true)
    }

    /// Deletes an existing property in-place from a dictionary shape and its owner's value vector.
    /// Infallible: reuses buffers in-place or shrinks.
    pub(crate) fn dict_delete(
        &mut self,
        ledger: &HeapLedger,
        values: &mut TrackedVec<Value>,
        key: PropKey,
    ) {
        let deleted = self.dict_delete_if_configurable(ledger, values, key);
        debug_assert_eq!(deleted, Some(true));
    }

    fn compact_in_place(
        &mut self,
        ledger: &HeapLedger,
        values: &mut TrackedVec<Value>,
        extra: usize,
    ) {
        let mut write = 0;
        for read in 0..self.props.len() {
            let prop = self.props[read];
            if !prop.is_deleted() {
                if write != read {
                    self.props[write] = prop;
                    values[write] = values[read];
                }
                write += 1;
            }
        }
        self.props.truncate(write);
        values.truncate(write);
        if write * 4 <= self.props.capacity() && self.props.capacity() > 32 {
            self.props
                .shrink_to(ledger, write.next_power_of_two().max(16));
        }
        if write * 4 <= values.capacity() && values.capacity() > 32 {
            values.shrink_to(ledger, write.next_power_of_two().max(16));
        }
        if write + extra <= LINEAR_LOOKUP_LIMIT {
            self.lookup = None;
        } else if let Some(table) = self.lookup.as_deref_mut() {
            table.rebuild_in_place(ledger, &self.props, extra);
        }
    }
}

fn hash(key: PropKey) -> usize {
    let bits = key.bits();
    ((bits ^ (bits >> 16)).wrapping_mul(0x9e37_79b9) ^ (bits >> 8)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_shape_size_and_dictionary_operations() {
        assert!(
            size_of::<Shape>() <= 72,
            "Shape grew unexpectedly: {} bytes",
            size_of::<Shape>()
        );
        let ledger = HeapLedger::new(1024 * 1024, 4096).unwrap();
        let mut shape = Shape::build(&ledger, None, 0, std::iter::empty(), 0, true).unwrap();
        let mut values = TrackedVec::new();
        for i in 0..200u32 {
            let prop = ShapeProp {
                key: PropKey::index(i).unwrap(),
                flags: PropFlags::DEFAULT_DATA,
            };
            shape
                .dict_insert(&ledger, &mut values, prop, Value::int32(i as i32))
                .unwrap();
        }
        assert_eq!(shape.live_count(), 200);
        for i in 0..200u32 {
            let (slot, flags) = shape.find(PropKey::index(i).unwrap()).unwrap();
            assert_eq!(flags, PropFlags::DEFAULT_DATA);
            assert_eq!(values[slot], Value::int32(i as i32));
        }
        for i in 0..200u32 {
            shape.dict_delete(&ledger, &mut values, PropKey::index(i).unwrap());
            assert_eq!(shape.find(PropKey::index(i).unwrap()), None);
        }
        assert_eq!(shape.live_count(), 0);
        assert!(shape.props.is_empty());
        assert!(values.is_empty());
    }
}
