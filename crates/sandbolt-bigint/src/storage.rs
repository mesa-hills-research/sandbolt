use super::{BigIntError, BigIntHost};
use std::cell::{Cell, RefCell};
use std::mem;
use std::ops::{Deref, DerefMut};

const POLL_INTERVAL: usize = 256;

pub(super) struct Scope<'h> {
    host: RefCell<&'h mut dyn BigIntHost>,
    ticks: Cell<usize>,
}

impl<'h> Scope<'h> {
    pub(super) fn new(host: &'h mut dyn BigIntHost) -> Self {
        Self {
            host: RefCell::new(host),
            ticks: Cell::new(0),
        }
    }

    pub(super) fn poll(&self) -> Result<(), BigIntError> {
        if self.host.borrow_mut().poll_interrupt() {
            Err(BigIntError::Interrupted)
        } else {
            Ok(())
        }
    }

    pub(super) fn step(&self) -> Result<(), BigIntError> {
        let ticks = (self.ticks.get() + 1) % POLL_INTERVAL;
        self.ticks.set(ticks);
        if ticks == 0 {
            self.poll()?;
        }
        Ok(())
    }

    pub(super) fn buffer<T>(&self, capacity: usize) -> Result<Buffer<'_, 'h, T>, BigIntError> {
        let mut buffer = Buffer {
            scope: self,
            vec: Vec::new(),
        };
        buffer.reserve(capacity)?;
        Ok(buffer)
    }

    pub(super) fn filled<T: Copy>(
        &self,
        len: usize,
        value: T,
    ) -> Result<Buffer<'_, 'h, T>, BigIntError> {
        let mut buffer = self.buffer(len)?;
        buffer.vec.resize(len, value);
        Ok(buffer)
    }

    pub(super) fn copy<T: Copy>(&self, values: &[T]) -> Result<Buffer<'_, 'h, T>, BigIntError> {
        let mut buffer = self.buffer(values.len())?;
        buffer.vec.extend_from_slice(values);
        Ok(buffer)
    }
}

pub(super) struct Buffer<'s, 'h, T> {
    scope: &'s Scope<'h>,
    vec: Vec<T>,
}

impl<T> Buffer<'_, '_, T> {
    pub(super) fn capacity(&self) -> usize {
        self.vec.capacity()
    }

    pub(super) fn reserve(&mut self, capacity: usize) -> Result<(), BigIntError> {
        if capacity > self.capacity() {
            let bytes = (capacity - self.capacity()) * size_of::<T>();
            self.scope.host.borrow_mut().charge(bytes)?;
            if self
                .vec
                .try_reserve_exact(capacity - self.vec.len())
                .is_err()
            {
                self.scope.host.borrow_mut().release(bytes);
                return Err(BigIntError::OutOfMemory);
            }
        }
        Ok(())
    }

    pub(super) fn push(&mut self, value: T) -> Result<(), BigIntError> {
        self.reserve(self.len() + 1)?;
        self.vec.push(value);
        Ok(())
    }

    pub(super) fn truncate(&mut self, len: usize) {
        self.vec.truncate(len);
    }

    // The caller keeps the charge for storage that escapes the scope.
    pub(super) fn into_vec(mut self) -> Vec<T> {
        mem::take(&mut self.vec)
    }
}

impl<T> Deref for Buffer<'_, '_, T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.vec
    }
}

impl<T> DerefMut for Buffer<'_, '_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.vec
    }
}

impl<T> Drop for Buffer<'_, '_, T> {
    fn drop(&mut self) {
        let bytes = self.capacity() * size_of::<T>();
        drop(mem::take(&mut self.vec));
        if bytes != 0 {
            self.scope.host.borrow_mut().release(bytes);
        }
    }
}
