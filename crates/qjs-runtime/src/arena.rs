use std::cell::Cell;

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
}

struct Slot<T> {
    value: Option<T>,
    generation: u16,
    next_free: Option<u32>,
    mark: Cell<Mark>,
}

pub struct Arena<T: ArenaItem> {
    slots: TrackedVec<Slot<T>>,
    free: Option<u32>,
    live: usize,
}

impl<T: ArenaItem> Arena<T> {
    pub const fn new() -> Self {
        Self {
            slots: TrackedVec::new(),
            free: None,
            live: 0,
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
        if let Some(index) = self.free {
            let slot = &mut self.slots[index as usize];
            self.free = slot.next_free.take();
            slot.value = Some(value);
            T::Id::from_parts(index, slot.generation)
        } else {
            let index = self.slots.len() as u32;
            self.slots.push_reserved(Slot {
                value: Some(value),
                generation: 0,
                next_free: None,
                mark: Cell::new(Mark::White),
            });
            T::Id::from_parts(index, 0)
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
        slot.mark.set(Mark::White);
        self.live -= 1;
        if slot.generation < u16::MAX {
            slot.generation += 1;
            slot.next_free = self.free;
            self.free = Some(id.index());
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
    }

    pub(crate) fn mark(&self, id: T::Id) -> bool {
        if let Some(slot) = self.slot(id) {
            if slot.mark.get() == Mark::White {
                slot.mark.set(Mark::Gray);
                return true;
            }
        } else {
            #[cfg(feature = "gc_stress")]
            panic!("USE-AFTER-GC: unrooted or stale handle");
        }
        false
    }

    pub(crate) fn is_marked(&self, id: T::Id) -> bool {
        self.slot(id)
            .is_some_and(|slot| slot.mark.get() != Mark::White)
    }

    pub(crate) fn scan_gray(&self, mut trace: impl FnMut(&T)) -> bool {
        let mut scanned = false;
        for slot in self.slots.iter() {
            if slot.mark.get() == Mark::Gray {
                slot.mark.set(Mark::Black);
                trace(slot.value.as_ref().expect("marked slot without value"));
                scanned = true;
            }
        }
        scanned
    }

    pub(crate) fn sweep(&mut self) {
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            if slot.mark.replace(Mark::White) == Mark::White && slot.value.is_some() {
                let id = T::Id::from_parts(index as u32, slot.generation);
                self.remove(id);
            }
        }
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
        }
        assert_eq!(
            arena.insert(&ledger, Item).unwrap(),
            ObjId::from_parts(1, 0)
        );
        assert!(arena.get_weak(first).is_none());
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
