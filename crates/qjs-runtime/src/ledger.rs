use std::cell::Cell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocError {
    Limit,
    Failpoint,
    Allocator,
}

pub type AllocResult<T> = Result<T, AllocError>;

#[derive(Debug)]
struct Account {
    used_bytes: Cell<usize>,
    peak_bytes: Cell<usize>,
    limit_bytes: usize,
    gc_threshold: Cell<usize>,
    reserve_bytes: usize,
    failpoint_countdown: Cell<Option<usize>>,
}

#[derive(Clone, Debug)]
pub struct HeapLedger(Rc<Account>);

impl HeapLedger {
    pub fn new(limit_bytes: usize, reserve_bytes: usize) -> AllocResult<Self> {
        let used_bytes = size_of::<Account>() + 2 * size_of::<usize>();
        if used_bytes > limit_bytes.saturating_sub(reserve_bytes) {
            return Err(AllocError::Limit);
        }
        Ok(Self(Rc::new(Account {
            used_bytes: Cell::new(used_bytes),
            peak_bytes: Cell::new(used_bytes),
            limit_bytes,
            gc_threshold: Cell::new((limit_bytes - reserve_bytes) / 2),
            reserve_bytes,
            failpoint_countdown: Cell::new(None),
        })))
    }

    pub(crate) fn same_account(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub fn used_bytes(&self) -> usize {
        self.0.used_bytes.get()
    }
    pub fn peak_bytes(&self) -> usize {
        self.0.peak_bytes.get()
    }
    pub fn limit_bytes(&self) -> usize {
        self.0.limit_bytes
    }
    pub fn reserve_bytes(&self) -> usize {
        self.0.reserve_bytes
    }
    pub fn gc_threshold(&self) -> usize {
        self.0.gc_threshold.get()
    }
    pub fn set_gc_threshold(&self, bytes: usize) {
        self.0.gc_threshold.set(bytes);
    }
    pub fn fail_after(&self, successful_charges: Option<usize>) {
        self.0.failpoint_countdown.set(successful_charges);
    }
    pub fn failpoint_armed(&self) -> bool {
        self.0.failpoint_countdown.get().is_some()
    }

    pub fn charge_bytes(&self, bytes: usize) -> AllocResult<()> {
        self.charge(bytes, false)
    }

    fn charge(&self, bytes: usize, emergency: bool) -> AllocResult<()> {
        if bytes == 0 {
            return Ok(());
        }
        if !emergency && self.0.failpoint_countdown.get() == Some(0) {
            self.0.failpoint_countdown.set(None);
            return Err(AllocError::Failpoint);
        }
        let limit = if emergency {
            self.limit_bytes()
        } else {
            self.limit_bytes() - self.reserve_bytes()
        };
        let used = self
            .used_bytes()
            .checked_add(bytes)
            .filter(|used| *used <= limit)
            .ok_or(AllocError::Limit)?;
        if !emergency && let Some(remaining) = self.0.failpoint_countdown.get() {
            self.0.failpoint_countdown.set(Some(remaining - 1));
        }
        self.0.used_bytes.set(used);
        self.0.peak_bytes.set(self.peak_bytes().max(used));
        Ok(())
    }

    pub fn release_bytes(&self, bytes: usize) {
        self.0.used_bytes.set(
            self.used_bytes()
                .checked_sub(bytes)
                .expect("heap charge underflow"),
        );
    }
}

pub struct TrackedVec<T> {
    data: Vec<T>,
    charge: Option<HeapLedger>,
}

impl<T> TrackedVec<T> {
    pub const fn new() -> Self {
        Self {
            data: Vec::new(),
            charge: None,
        }
    }
    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }
    pub fn clear(&mut self) {
        self.data.clear();
    }
    pub fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.data.pop()
    }
    pub fn remove(&mut self, index: usize) -> T {
        self.data.remove(index)
    }
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.data.swap_remove(index)
    }
    pub fn retain(&mut self, f: impl FnMut(&T) -> bool) {
        self.data.retain(f);
    }

    pub fn try_reserve(&mut self, ledger: &HeapLedger, additional: usize) -> AllocResult<()> {
        let required = self
            .len()
            .checked_add(additional)
            .ok_or(AllocError::Limit)?;
        if required <= self.capacity() {
            return Ok(());
        }
        let target = required.max(self.capacity().saturating_mul(2)).max(4);
        match self.grow(ledger, target, false) {
            Err(AllocError::Limit) if target != required => {
                // The old buffer remains charged until the copy finishes. Use the
                // space that fits beside it instead of copying for each new item.
                let ceiling = ledger.limit_bytes() - ledger.reserve_bytes();
                let available = ceiling.saturating_sub(ledger.used_bytes());
                let capacity = available
                    .min(isize::MAX as usize)
                    .checked_div(size_of::<T>())
                    .unwrap_or(required);
                self.grow(ledger, capacity.max(required), false)
            }
            result => result,
        }
    }

    pub fn try_reserve_exact(&mut self, ledger: &HeapLedger, additional: usize) -> AllocResult<()> {
        let required = self
            .len()
            .checked_add(additional)
            .ok_or(AllocError::Limit)?;
        self.grow(ledger, required, false)
    }

    pub(crate) fn try_reserve_emergency(
        &mut self,
        ledger: &HeapLedger,
        additional: usize,
    ) -> AllocResult<()> {
        let required = self
            .len()
            .checked_add(additional)
            .ok_or(AllocError::Limit)?;
        self.grow(ledger, required, true)
    }

    fn grow(&mut self, ledger: &HeapLedger, target: usize, emergency: bool) -> AllocResult<()> {
        if let Some(previous) = &self.charge {
            assert!(Rc::ptr_eq(&previous.0, &ledger.0), "mismatched heap ledger");
        }
        let old = self.capacity();
        if target <= old {
            return Ok(());
        }
        let bytes = target
            .checked_mul(size_of::<T>())
            .filter(|bytes| *bytes <= isize::MAX as usize)
            .ok_or(AllocError::Limit)?;
        ledger.charge(bytes, emergency)?;
        let mut next = Vec::new();
        if next.try_reserve_exact(target).is_err() {
            ledger.release_bytes(bytes);
            return Err(AllocError::Allocator);
        }
        debug_assert_eq!(next.capacity(), target);
        next.append(&mut self.data);
        self.data = next;
        self.charge = Some(ledger.clone());
        ledger.release_bytes(old * size_of::<T>());
        Ok(())
    }

    pub fn push(&mut self, ledger: &HeapLedger, value: T) -> AllocResult<()> {
        self.try_reserve(ledger, 1)?;
        self.push_reserved(value);
        Ok(())
    }

    pub(crate) fn push_reserved(&mut self, value: T) {
        assert!(self.len() < self.capacity());
        self.data.push(value);
    }

    pub fn extend_from_slice(&mut self, ledger: &HeapLedger, items: &[T]) -> AllocResult<()>
    where
        T: Copy,
    {
        self.try_reserve(ledger, items.len())?;
        self.data.extend_from_slice(items);
        Ok(())
    }

    pub fn from_slice(ledger: &HeapLedger, items: &[T]) -> AllocResult<Self>
    where
        T: Copy,
    {
        let mut result = Self::new();
        result.try_reserve_exact(ledger, items.len())?;
        result.data.extend_from_slice(items);
        Ok(result)
    }
}

impl<T> Default for TrackedVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for TrackedVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.data.fmt(f)
    }
}

impl<T> Deref for TrackedVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.data
    }
}

impl<T> DerefMut for TrackedVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.data
    }
}

impl<T> Drop for TrackedVec<T> {
    fn drop(&mut self) {
        if let Some(ledger) = &self.charge {
            let bytes = self.capacity() * size_of::<T>();
            drop(std::mem::take(&mut self.data));
            ledger.release_bytes(bytes);
        }
    }
}

#[derive(Debug)]
pub struct TrackedBox<T>(TrackedVec<T>);

impl<T> TrackedBox<T> {
    pub fn new(ledger: &HeapLedger, value: T) -> AllocResult<Self> {
        let mut slot = TrackedVec::new();
        slot.try_reserve_exact(ledger, 1)?;
        slot.push_reserved(value);
        Ok(Self(slot))
    }
}

impl<T> Deref for TrackedBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0[0]
    }
}

impl<T> DerefMut for TrackedBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_charges_simultaneous_buffers() {
        let ledger = HeapLedger::new(1024, 32).unwrap();
        let base = ledger.used_bytes();
        {
            let mut buf = TrackedVec::<u64>::new();
            buf.try_reserve_exact(&ledger, 8).unwrap();
            buf.push_reserved(42);
            buf.try_reserve_exact(&ledger, 15).unwrap();
            assert_eq!(buf.capacity(), 16);
            assert_eq!(buf[0], 42);
            assert_eq!(ledger.used_bytes(), base + 128);
            assert_eq!(ledger.peak_bytes(), base + 64 + 128);
        }
        assert_eq!(ledger.used_bytes(), base);
    }

    #[test]
    fn growth_uses_available_capacity_when_doubling_cannot_fit() {
        let ledger = HeapLedger::new(1024, 32).unwrap();
        let base = ledger.used_bytes();
        let mut buf = TrackedVec::<u64>::new();
        buf.try_reserve_exact(&ledger, 50).unwrap();
        for i in 0..50 {
            buf.push_reserved(i);
        }
        let before = ledger.used_bytes();
        let capacity = (ledger.limit_bytes() - ledger.reserve_bytes() - before) / size_of::<u64>();
        assert!((51..100).contains(&capacity));
        buf.push(&ledger, 50).unwrap();
        assert_eq!(buf.capacity(), capacity);
        assert!(buf.iter().copied().eq(0..51));
        assert_eq!(ledger.peak_bytes(), before + capacity * size_of::<u64>());
        assert_eq!(ledger.used_bytes(), base + capacity * size_of::<u64>());
    }

    #[test]
    fn failures_leave_capacity_and_contents_unchanged() {
        let ledger = HeapLedger::new(256, 32).unwrap();
        let mut buf = TrackedVec::<u64>::new();
        buf.try_reserve_exact(&ledger, 8).unwrap();
        buf.push_reserved(42);
        let before = ledger.used_bytes();
        assert_eq!(buf.try_reserve_exact(&ledger, 20), Err(AllocError::Limit));
        ledger.fail_after(Some(0));
        assert_eq!(
            buf.try_reserve_exact(&ledger, 8),
            Err(AllocError::Failpoint)
        );
        assert_eq!(buf.capacity(), 8);
        assert_eq!(&*buf, &[42]);
        assert_eq!(ledger.used_bytes(), before);
        assert!(ledger.peak_bytes() <= ledger.limit_bytes());
    }
}
