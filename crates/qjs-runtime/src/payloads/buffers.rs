use std::collections::LinkedList;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::value::ObjId;

#[derive(Debug, Default)]
pub struct ArrayBufferData {
    pub(crate) bytes: TrackedVec<u8>,
    pub(crate) max_byte_length: Option<usize>,
    pub(crate) detached: bool,
}

#[derive(Debug)]
pub struct SharedArrayBufferData {
    pub backing: Arc<SharedBufferBacking>,
    _charge: BackingCharge,
}

#[derive(Debug)]
struct BackingCharge {
    ledger: HeapLedger,
    bytes: usize,
}

impl BackingCharge {
    fn new(ledger: &HeapLedger, bytes: usize) -> AllocResult<Self> {
        ledger.charge_bytes(bytes)?;
        Ok(Self {
            ledger: ledger.clone(),
            bytes,
        })
    }
}

impl Drop for BackingCharge {
    fn drop(&mut self) {
        self.ledger.release_bytes(self.bytes);
    }
}

impl SharedArrayBufferData {
    pub(crate) fn new(ledger: &HeapLedger, len: usize, max: Option<usize>) -> AllocResult<Self> {
        let capacity = max.unwrap_or(len);
        let charge = BackingCharge::new(ledger, SharedBufferBacking::allocation_size(capacity)?)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| AllocError::Allocator)?;
        bytes.resize_with(capacity, || AtomicU8::new(0));
        Ok(Self {
            backing: Arc::new(SharedBufferBacking {
                bytes,
                len: AtomicUsize::new(len),
                max,
                state: Mutex::default(),
                changed: Condvar::new(),
            }),
            _charge: charge,
        })
    }

    pub(crate) fn from_arc(
        ledger: &HeapLedger,
        backing: Arc<SharedBufferBacking>,
    ) -> AllocResult<Self> {
        let bytes = SharedBufferBacking::allocation_size(backing.bytes.capacity())?;
        Ok(Self {
            backing,
            _charge: BackingCharge::new(ledger, bytes)?,
        })
    }
}

#[derive(Debug)]
pub struct SharedBufferBacking {
    bytes: Vec<AtomicU8>,
    len: AtomicUsize,
    max: Option<usize>,
    // Word accesses and the waiter list use the same lock. Coercions and
    // interrupt callbacks run outside it.
    state: Mutex<SharedState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct SharedState {
    waiters: LinkedList<Waiter>,
    next_id: u64,
}

#[derive(Debug)]
struct Waiter {
    id: u64,
    offset: usize,
    notified: bool,
}

pub(crate) enum WaitResult {
    Ok,
    NotEqual,
    TimedOut,
    Interrupted,
}

impl SharedBufferBacking {
    fn allocation_size(capacity: usize) -> AllocResult<usize> {
        capacity
            .checked_add(size_of::<Self>() + 2 * size_of::<usize>())
            .ok_or(AllocError::Limit)
    }

    pub fn byte_length(&self) -> usize {
        self.len.load(SeqCst)
    }

    pub fn max_byte_length(&self) -> Option<usize> {
        self.max
    }

    pub(crate) fn grow(&self, len: usize) -> bool {
        self.max.is_some_and(|max| {
            len <= max
                && self
                    .len
                    .fetch_update(SeqCst, SeqCst, |old| (len >= old).then_some(len))
                    .is_ok()
        })
    }

    fn read_locked(&self, offset: usize, out: &mut [u8]) {
        let bytes = &self.bytes[offset..][..out.len()];
        for (out, byte) in out.iter_mut().zip(bytes) {
            *out = byte.load(SeqCst);
        }
    }

    fn write_locked(&self, offset: usize, input: &[u8]) {
        for (byte, &value) in self.bytes[offset..][..input.len()].iter().zip(input) {
            byte.store(value, SeqCst);
        }
    }

    pub(crate) fn read(&self, offset: usize, out: &mut [u8]) {
        let _state = self.state.lock().unwrap();
        self.read_locked(offset, out);
    }

    pub(crate) fn write(&self, offset: usize, input: &[u8]) {
        let _state = self.state.lock().unwrap();
        self.write_locked(offset, input);
    }

    pub(crate) fn update_word(
        &self,
        offset: usize,
        size: usize,
        op: impl FnOnce(u64) -> Option<u64>,
    ) -> u64 {
        let _state = self.state.lock().unwrap();
        let old = self.word_locked(offset, size);
        if let Some(new) = op(old) {
            self.write_locked(offset, &new.to_le_bytes()[..size]);
        }
        old
    }

    fn word_locked(&self, offset: usize, size: usize) -> u64 {
        let mut bytes = [0; 8];
        self.read_locked(offset, &mut bytes[..size]);
        u64::from_le_bytes(bytes)
    }

    pub(crate) fn notify(&self, offset: usize, count: usize) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut n = 0;
        for waiter in &mut state.waiters {
            if n == count {
                break;
            }
            if waiter.offset == offset && !waiter.notified {
                waiter.notified = true;
                n += 1;
            }
        }
        if n != 0 {
            self.changed.notify_all();
        }
        n
    }

    pub(crate) fn wait(
        &self,
        ledger: &HeapLedger,
        offset: usize,
        size: usize,
        expected: u64,
        timeout: Option<Duration>,
        mut interrupted: impl FnMut() -> bool,
    ) -> AllocResult<WaitResult> {
        let start = Instant::now();
        let mut state = self.state.lock().unwrap();
        if self.word_locked(offset, size) != expected {
            return Ok(WaitResult::NotEqual);
        }
        if timeout == Some(Duration::ZERO) {
            return Ok(WaitResult::TimedOut);
        }
        let id = state.next_id;
        state.next_id = id.wrapping_add(1);
        let _charge = BackingCharge::new(ledger, size_of::<Waiter>() + 2 * size_of::<usize>())?;
        state.waiters.push_back(Waiter {
            id,
            offset,
            notified: false,
        });
        let result = loop {
            if state.waiters.iter().any(|w| w.id == id && w.notified) {
                break WaitResult::Ok;
            }
            let remaining = timeout.map(|limit| limit.saturating_sub(start.elapsed()));
            if remaining == Some(Duration::ZERO) {
                break WaitResult::TimedOut;
            }
            let poll_after = Duration::from_millis(10);
            let delay = remaining.map_or(poll_after, |d| d.min(poll_after));
            let (guard, _) = self.changed.wait_timeout(state, delay).unwrap();
            drop(guard);
            let stop = interrupted();
            state = self.state.lock().unwrap();
            if stop {
                break WaitResult::Interrupted;
            }
        };
        let index = state.waiters.iter().position(|w| w.id == id).unwrap();
        let mut tail = state.waiters.split_off(index);
        tail.pop_front();
        state.waiters.append(&mut tail);
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TypedArrayKind {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    BigInt64,
    BigUint64,
    Float16,
    Float32,
    Float64,
}

impl TypedArrayKind {
    pub(crate) const ALL: [Self; 12] = [
        Self::Int8,
        Self::Uint8,
        Self::Uint8Clamped,
        Self::Int16,
        Self::Uint16,
        Self::Int32,
        Self::Uint32,
        Self::BigInt64,
        Self::BigUint64,
        Self::Float16,
        Self::Float32,
        Self::Float64,
    ];

    pub(crate) fn name(self) -> &'static str {
        [
            "Int8Array",
            "Uint8Array",
            "Uint8ClampedArray",
            "Int16Array",
            "Uint16Array",
            "Int32Array",
            "Uint32Array",
            "BigInt64Array",
            "BigUint64Array",
            "Float16Array",
            "Float32Array",
            "Float64Array",
        ][self as usize]
    }

    pub(crate) const fn size(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 | Self::Uint8Clamped => 1,
            Self::Int16 | Self::Uint16 | Self::Float16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::BigInt64 | Self::BigUint64 | Self::Float64 => 8,
        }
    }

    pub(crate) const fn is_bigint(self) -> bool {
        matches!(self, Self::BigInt64 | Self::BigUint64)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TypedArrayData {
    pub(crate) buffer: ObjId,
    pub(crate) byte_offset: usize,
    pub(crate) length: Option<usize>,
    pub(crate) kind: TypedArrayKind,
}

#[derive(Clone, Copy, Debug)]
pub struct DataViewData {
    pub(crate) buffer: ObjId,
    pub(crate) byte_offset: usize,
    pub(crate) byte_length: Option<usize>,
}

impl Trace for ArrayBufferData {
    fn trace(&self, _: &Tracer<'_>) {}
}
impl Trace for SharedArrayBufferData {
    fn trace(&self, _: &Tracer<'_>) {}
}
impl Trace for DataViewData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.buffer.trace(tracer);
    }
}
impl Trace for TypedArrayData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.buffer.trace(tracer);
    }
}
