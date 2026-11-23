use std::cell::Cell;
use std::num::NonZeroU32;

use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::value::ArenaId;

pub trait ArenaItem {
    type Id: ArenaId;
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mark {
    White,
    Gray,
    Black,
    Retired,
}

pub(crate) type SlotLink = Option<NonZeroU32>;

fn link(index: u32) -> SlotLink {
    NonZeroU32::new(index + 1)
}

fn index(link: NonZeroU32) -> usize {
    (link.get() - 1) as usize
}

struct Slot<T> {
    value: Option<T>,
    generation: u16,
    // Free slots link to one another. Live slots use the same link for GC lists.
    next: Cell<SlotLink>,
    mark: Cell<Mark>,
}

pub struct Arena<T: ArenaItem> {
    slots: TrackedVec<Slot<T>>,
    free: SlotLink,
    gray: Cell<SlotLink>,
    live: usize,
    // A stack of generations for trimmed indices, with the next index on top.
    tail_generations: TrackedVec<u16>,
}

impl<T: ArenaItem> Arena<T> {
    pub const fn new() -> Self {
        Self {
            slots: TrackedVec::new(),
            free: None,
            gray: Cell::new(None),
            live: 0,
            tail_generations: TrackedVec::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.live
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    pub fn try_reserve_slot(&mut self, ledger: &HeapLedger) -> AllocResult<()> {
        if self.free.is_none() {
            if self.slots.len() >= (1 << 31) {
                return Err(AllocError::Limit);
            }
            self.slots.try_reserve(ledger, 1)?;
        }
        Ok(())
    }

    pub fn insert(&mut self, ledger: &HeapLedger, value: T) -> AllocResult<T::Id> {
        self.try_reserve_slot(ledger)?;
        Ok(self.insert_reserved(value))
    }

    pub(crate) fn insert_reserved(&mut self, value: T) -> T::Id {
        self.live += 1;
        if let Some(free) = self.free {
            let i = index(free);
            let slot = &mut self.slots[i];
            self.free = slot.next.take();
            slot.value = Some(value);
            T::Id::from_parts(i as u32, slot.generation)
        } else {
            let index = self.slots.len() as u32;
            let generation = self.tail_generations.pop().unwrap_or(0);
            self.slots.push_reserved(Slot {
                value: Some(value),
                generation,
                next: Cell::new(None),
                mark: Cell::new(Mark::White),
            });
            T::Id::from_parts(index, generation)
        }
    }

    fn slot(&self, id: T::Id) -> Option<&Slot<T>> {
        self.slots
            .get(id.index() as usize)
            .filter(|slot| slot.generation == id.generation() && slot.value.is_some())
    }

    pub fn get(&self, id: T::Id) -> Option<&T> {
        let value = self.get_weak(id);
        #[cfg(feature = "gc_stress")]
        assert!(value.is_some(), "USE-AFTER-GC: unrooted or stale handle");
        value
    }

    pub fn get_weak(&self, id: T::Id) -> Option<&T> {
        self.slot(id)?.value.as_ref()
    }

    pub fn get_mut(&mut self, id: T::Id) -> Option<&mut T> {
        let value = self
            .slots
            .get_mut(id.index() as usize)
            .filter(|slot| slot.generation == id.generation())
            .and_then(|slot| slot.value.as_mut());
        #[cfg(feature = "gc_stress")]
        assert!(value.is_some(), "USE-AFTER-GC: unrooted or stale handle");
        value
    }

    pub fn remove(&mut self, id: T::Id) -> Option<T> {
        let slot = self.slots.get_mut(id.index() as usize)?;
        if slot.generation != id.generation() {
            return None;
        }
        let value = slot.value.take()?;
        self.live -= 1;
        if slot.generation < u16::MAX {
            slot.mark.set(Mark::White);
            slot.generation += 1;
            slot.next.set(self.free);
            self.free = link(id.index());
        } else {
            slot.mark.set(Mark::Retired);
            slot.next.set(None);
        }
        Some(value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (T::Id, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            Some((
                T::Id::from_parts(i as u32, slot.generation),
                slot.value.as_ref()?,
            ))
        })
    }

    pub(crate) fn for_each_live_mut(
        &mut self,
        mut visit: impl FnMut(&mut T, &dyn Fn(T::Id) -> bool),
    ) {
        for index in 0..self.slots.len() {
            self.visit_index_mut(index, &mut visit);
        }
    }

    pub(crate) fn for_each_linked_mut(
        &mut self,
        mut head: SlotLink,
        mut visit: impl FnMut(&mut T, &dyn Fn(T::Id) -> bool),
    ) {
        while let Some(entry) = head {
            let i = index(entry);
            head = self.slots[i].next.take();
            self.visit_index_mut(i, &mut visit);
        }
    }

    fn visit_index_mut(
        &mut self,
        index: usize,
        visit: &mut impl FnMut(&mut T, &dyn Fn(T::Id) -> bool),
    ) {
        let (before, rest) = self.slots.split_at_mut(index);
        let (slot, after) = rest.split_first_mut().unwrap();
        let generation = slot.generation;
        if let Some(value) = slot.value.as_mut() {
            visit(value, &|id| {
                let i = id.index() as usize;
                if i == index {
                    return id.generation() == generation;
                }
                let other = if i < index {
                    before.get(i)
                } else {
                    after.get(i - index - 1)
                };
                other.is_some_and(|s| s.generation == id.generation() && s.value.is_some())
            });
        }
    }

    pub(crate) fn mark(&self, id: T::Id) {
        if let Some(slot) = self.slot(id) {
            if slot.mark.get() == Mark::White {
                slot.mark.set(Mark::Gray);
                slot.next.set(self.gray.replace(link(id.index())));
            }
        } else {
            #[cfg(feature = "gc_stress")]
            panic!("USE-AFTER-GC: unrooted or stale handle");
        }
    }

    pub(crate) fn is_marked(&self, id: T::Id) -> bool {
        self.slot(id)
            .is_some_and(|slot| slot.mark.get() != Mark::White)
    }

    pub(crate) fn pop_gray(&self) -> Option<(T::Id, &T)> {
        let i = index(self.gray.get()?);
        let slot = &self.slots[i];
        debug_assert!(slot.mark.get() == Mark::Gray);
        self.gray.set(slot.next.take());
        slot.mark.set(Mark::Black);
        Some((
            T::Id::from_parts(i as u32, slot.generation),
            slot.value.as_ref().expect("marked slot without value"),
        ))
    }

    pub(crate) fn link_black(&self, id: T::Id, head: &mut SlotLink) {
        let slot = &self.slots[id.index() as usize];
        debug_assert!(slot.mark.get() == Mark::Black);
        slot.next.set(std::mem::replace(head, link(id.index())));
    }

    pub(crate) fn linked(&self, mut head: SlotLink) -> impl Iterator<Item = &T> {
        std::iter::from_fn(move || {
            let slot = &self.slots[index(head?)];
            head = slot.next.get();
            slot.value.as_ref()
        })
    }

    pub(crate) fn sweep(&mut self) {
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            if slot.value.is_some() && slot.mark.replace(Mark::White) == Mark::White {
                let id = T::Id::from_parts(index as u32, slot.generation);
                self.remove(id);
            }
        }
    }

    pub(crate) fn shrink_after_gc(&mut self, ledger: &HeapLedger) {
        let new_len = self
            .slots
            .iter()
            .rposition(|slot| slot.value.is_some() || slot.mark.get() == Mark::Retired)
            .map_or(0, |i| i + 1);
        let trimmed = self.slots.len() - new_len;
        if trimmed != 0 {
            let room = self.tail_generations.capacity() - self.tail_generations.len();
            // Preserving generations is mandatory when trimming. Leave the tail
            // in place if we cannot record it without taking a failpoint.
            if room >= trimmed
                || (!ledger.failpoint_armed()
                    && self
                        .tail_generations
                        .try_reserve_exact(ledger, trimmed)
                        .is_ok())
            {
                for slot in self.slots[new_len..].iter().rev() {
                    self.tail_generations.push_reserved(slot.generation);
                }
                self.slots.truncate(new_len);
                self.free = None;
                for (i, slot) in self.slots.iter().enumerate().rev() {
                    if slot.value.is_none() && slot.mark.get() != Mark::Retired {
                        slot.next.set(self.free);
                        self.free = link(i as u32);
                    }
                }
            }
        }
        let min = self.slots.len().max(64);
        if self.capacity() > min.saturating_mul(2) {
            self.slots.shrink_to(ledger, min.next_power_of_two());
        }
    }

    pub(crate) fn slack_bytes(&self) -> usize {
        (self.capacity() - self.live) * size_of::<Slot<T>>()
            + self.tail_generations.capacity() * size_of::<u16>()
    }
}

impl<T: ArenaItem> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ObjId;

    struct Item;
    impl ArenaItem for Item {
        type Id = ObjId;
    }

    #[test]
    fn generations_retire_on_exhaustion() {
        let ledger = HeapLedger::new(4096, 0).unwrap();
        let mut arena = Arena::new();
        let first = arena.insert(&ledger, Item).unwrap();
        assert_eq!(first.index(), 0);
        arena.remove(first).unwrap();
        for generation in 1..=u16::MAX {
            let id = arena.insert(&ledger, Item).unwrap();
            assert_eq!(id, ObjId::from_parts(0, generation));
            arena.remove(id).unwrap();
            if generation >= u16::MAX - 1 {
                arena.shrink_after_gc(&ledger);
            }
        }
        assert_eq!(
            arena.insert(&ledger, Item).unwrap(),
            ObjId::from_parts(1, 0)
        );
        assert!(arena.get_weak(first).is_none());
    }

    #[test]
    fn trims_dead_tail_and_reuses_holes_without_reviving_ids() {
        let ledger = HeapLedger::new(65536, 0).unwrap();
        let mut arena = Arena::new();
        let ids: Vec<_> = (0..1024)
            .map(|_| arena.insert(&ledger, Item).unwrap())
            .collect();
        arena.remove(ids[1023]).unwrap();
        let newer = arena.insert(&ledger, Item).unwrap();
        let old_used = ledger.used_bytes();
        for live in [ids[0], ids[2]] {
            arena.mark(live);
            arena.mark(live);
        }
        assert_eq!(arena.pop_gray().map(|(id, _)| id), Some(ids[2]));
        assert_eq!(arena.pop_gray().map(|(id, _)| id), Some(ids[0]));
        assert!(arena.pop_gray().is_none());
        arena.sweep();
        arena.shrink_after_gc(&ledger);
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.capacity(), 64);
        assert!(ledger.used_bytes() < old_used / 2);
        for index in [1, 3] {
            assert_eq!(
                arena.insert(&ledger, Item).unwrap(),
                ObjId::from_parts(index, 1)
            );
        }
        for _ in 4..1023 {
            arena.insert(&ledger, Item).unwrap();
        }
        assert_eq!(
            arena.insert(&ledger, Item).unwrap(),
            ObjId::from_parts(1023, 2)
        );
        assert!(arena.get_weak(ids[1]).is_none());
        assert!(arena.get_weak(ids[1023]).is_none());
        assert!(arena.get_weak(newer).is_none());
    }

    #[cfg(feature = "gc_stress")]
    #[test]
    #[should_panic(expected = "USE-AFTER-GC: unrooted or stale handle")]
    fn stale_strong_access_panics() {
        let ledger = HeapLedger::new(4096, 0).unwrap();
        let mut arena = Arena::new();
        let id = arena.insert(&ledger, Item).unwrap();
        arena.remove(id);
        arena.insert(&ledger, Item).unwrap();
        arena.get(id);
    }
}
