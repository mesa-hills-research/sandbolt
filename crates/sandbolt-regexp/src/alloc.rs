use crate::{RegExpError, RegExpHost};
use std::mem::size_of;

type Result<T> = std::result::Result<T, RegExpError>;

pub(crate) struct Budget<'a> {
    pub(crate) host: &'a mut dyn RegExpHost,
    charged: usize,
}

impl<'a> Budget<'a> {
    pub(crate) fn new(host: &'a mut dyn RegExpHost) -> Self {
        Self { host, charged: 0 }
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> Result<()> {
        let total = self
            .charged
            .checked_add(bytes)
            .ok_or(RegExpError::OutOfMemory)?;
        if bytes != 0 {
            self.host.charge(bytes)?;
        }
        self.charged = total;
        Ok(())
    }

    pub(crate) fn release(&mut self, bytes: usize) {
        if bytes != 0 {
            self.host.release(bytes);
        }
        self.charged -= bytes;
    }

    pub(crate) fn reserve<T>(&mut self, vec: &mut Vec<T>, additional: usize) -> Result<()> {
        let need = vec
            .len()
            .checked_add(additional)
            .ok_or(RegExpError::OutOfMemory)?;
        let old = vec.capacity();
        if need <= old || size_of::<T>() == 0 {
            return Ok(());
        }
        let target = need.max(old + old / 2).max(4);
        let bytes = (target - old)
            .checked_mul(size_of::<T>())
            .ok_or(RegExpError::OutOfMemory)?;
        self.charge(bytes)?;
        if vec.try_reserve_exact(target - vec.len()).is_err() {
            self.release(bytes);
            return Err(RegExpError::OutOfMemory);
        }
        Ok(())
    }

    pub(crate) fn push<T>(&mut self, vec: &mut Vec<T>, value: T) -> Result<()> {
        self.reserve(vec, 1)?;
        vec.push(value);
        Ok(())
    }

    pub(crate) fn extend<T: Copy>(&mut self, vec: &mut Vec<T>, values: &[T]) -> Result<()> {
        self.reserve(vec, values.len())?;
        vec.extend_from_slice(values);
        Ok(())
    }

    pub(crate) fn free<T>(&mut self, vec: Vec<T>) {
        let bytes = vec.capacity().saturating_mul(size_of::<T>());
        drop(vec);
        self.release(bytes);
    }
}

impl Drop for Budget<'_> {
    fn drop(&mut self) {
        if self.charged != 0 {
            self.host.release(self.charged);
        }
    }
}
