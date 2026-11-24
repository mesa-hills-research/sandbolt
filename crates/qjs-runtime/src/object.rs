use std::ops::{BitOr, BitOrAssign, Deref, DerefMut};

use crate::arena::ArenaItem;
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedBox};
use crate::value::{AccId, ObjId, ShapeId, Value};

pub use crate::payloads::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObjectFlags(u16);

impl ObjectFlags {
    pub const EXTENSIBLE: Self = Self(1);
    pub const IS_PROTOTYPE: Self = Self(2);
    pub const IS_CONSTRUCTOR: Self = Self(4);
    pub const IS_EXOTIC: Self = Self(8);
    pub const FAST_ARRAY: Self = Self(16);
    pub const IS_HTML_DDA: Self = Self(32);
    pub const IMMUTABLE_PROTO: Self = Self(64);

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

impl BitOr for ObjectFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for ObjectFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

const INLINE_PROPS: usize = 2;
const SLOT_BYTES: usize = size_of::<Value>();

// The ledger's pointer niche leaves room for the tag without growing JsObject.
#[derive(Debug)]
enum SlotStorage {
    Inline {
        len: u8,
        slots: [Value; INLINE_PROPS],
    },
    Heap(Vec<Value>, HeapLedger),
}

#[derive(Debug)]
pub struct PropSlots(SlotStorage);

impl PropSlots {
    pub(crate) const fn new() -> Self {
        Self(SlotStorage::Inline {
            len: 0,
            slots: [Value::EMPTY; INLINE_PROPS],
        })
    }

    pub(crate) const fn one(value: Value) -> Self {
        Self(SlotStorage::Inline {
            len: 1,
            slots: [value, Value::EMPTY],
        })
    }

    pub(crate) const fn two(first: Value, second: Value) -> Self {
        Self(SlotStorage::Inline {
            len: 2,
            slots: [first, second],
        })
    }

    pub fn capacity(&self) -> usize {
        match &self.0 {
            SlotStorage::Inline { .. } => INLINE_PROPS,
            SlotStorage::Heap(data, _) => data.capacity(),
        }
    }

    pub fn heap_capacity(&self) -> usize {
        match &self.0 {
            SlotStorage::Inline { .. } => 0,
            SlotStorage::Heap(data, _) => data.capacity(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.truncate(0);
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        match &mut self.0 {
            SlotStorage::Inline { len: old, .. } => *old = len.min(usize::from(*old)) as u8,
            SlotStorage::Heap(data, _) => data.truncate(len),
        }
    }

    pub(crate) fn remove(&mut self, index: usize) -> Value {
        match &mut self.0 {
            SlotStorage::Inline { len, slots } => {
                let live = &mut slots[..usize::from(*len)];
                let value = live[index];
                live.copy_within(index + 1.., index);
                *len -= 1;
                value
            }
            SlotStorage::Heap(data, _) => data.remove(index),
        }
    }

    pub(crate) fn push_reserved(&mut self, value: Value) {
        match &mut self.0 {
            SlotStorage::Inline { len, slots } => {
                slots[usize::from(*len)] = value;
                *len += 1;
            }
            SlotStorage::Heap(data, _) => {
                assert!(data.len() < data.capacity());
                data.push(value);
            }
        }
    }

    pub(crate) fn push(&mut self, ledger: &HeapLedger, value: Value) -> AllocResult<()> {
        self.try_reserve(ledger, 1)?;
        self.push_reserved(value);
        Ok(())
    }

    pub(crate) fn try_reserve(
        &mut self,
        ledger: &HeapLedger,
        additional: usize,
    ) -> AllocResult<()> {
        let required = self
            .len()
            .checked_add(additional)
            .ok_or(AllocError::Limit)?;
        let capacity = self.capacity();
        if required <= capacity {
            return Ok(());
        }
        let target = required.max(capacity.saturating_mul(2));
        match self.resize(ledger, target, false) {
            Err(AllocError::Limit) if target != required => {
                let available = (ledger.limit_bytes() - ledger.reserve_bytes())
                    .saturating_sub(ledger.used_bytes());
                let capacity = available.min(isize::MAX as usize) / SLOT_BYTES;
                self.resize(ledger, capacity.max(required), false)
            }
            result => result,
        }
    }

    pub(crate) fn try_reserve_exact(
        &mut self,
        ledger: &HeapLedger,
        additional: usize,
    ) -> AllocResult<()> {
        let required = self
            .len()
            .checked_add(additional)
            .ok_or(AllocError::Limit)?;
        if required > self.capacity() {
            self.resize(ledger, required, false)?;
        }
        Ok(())
    }

    pub(crate) fn shrink_to(&mut self, ledger: &HeapLedger, capacity: usize) {
        let target = capacity.max(self.len());
        if target >= self.capacity() {
            return;
        }
        if target <= INLINE_PROPS {
            if let SlotStorage::Heap(_, previous) = &self.0 {
                assert!(previous.same_account(ledger));
                let mut slots = [Value::EMPTY; INLINE_PROPS];
                slots[..self.len()].copy_from_slice(self);
                *self = Self(SlotStorage::Inline {
                    len: self.len() as u8,
                    slots,
                });
            }
        } else {
            let _ = self.resize(ledger, target, true);
        }
    }

    fn resize(&mut self, ledger: &HeapLedger, target: usize, emergency: bool) -> AllocResult<()> {
        if let SlotStorage::Heap(_, previous) = &self.0 {
            assert!(previous.same_account(ledger), "mismatched heap ledger");
        }
        let bytes = target
            .checked_mul(SLOT_BYTES)
            .filter(|&bytes| bytes <= isize::MAX as usize)
            .ok_or(AllocError::Limit)?;
        // Both buffers stay charged until the copy finishes.
        ledger.charge(bytes, emergency)?;
        let mut next = Vec::new();
        if next.try_reserve_exact(target).is_err() {
            ledger.release_bytes(bytes);
            return Err(AllocError::Allocator);
        }
        debug_assert_eq!(next.capacity(), target);
        next.extend_from_slice(self);
        *self = Self(SlotStorage::Heap(next, ledger.clone()));
        Ok(())
    }
}

impl Deref for PropSlots {
    type Target = [Value];

    #[inline]
    fn deref(&self) -> &[Value] {
        match &self.0 {
            SlotStorage::Inline { len, slots } => &slots[..usize::from(*len)],
            SlotStorage::Heap(data, _) => data,
        }
    }
}

impl DerefMut for PropSlots {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        match &mut self.0 {
            SlotStorage::Inline { len, slots } => &mut slots[..usize::from(*len)],
            SlotStorage::Heap(data, _) => data,
        }
    }
}

impl Drop for PropSlots {
    fn drop(&mut self) {
        if let SlotStorage::Heap(data, ledger) = &mut self.0 {
            let bytes = data.capacity() * SLOT_BYTES;
            drop(std::mem::take(data));
            ledger.release_bytes(bytes);
        }
    }
}

#[derive(Debug)]
pub struct JsObject {
    pub shape: ShapeId,
    pub props: PropSlots,
    pub flags: ObjectFlags,
    pub data: ObjectData,
}

impl ArenaItem for JsObject {
    type Id = ObjId;
}

impl Trace for JsObject {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.shape.trace(tracer);
        self.props.trace(tracer);
        self.data.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Accessor {
    pub getter: Value,
    pub setter: Value,
}
impl Default for Accessor {
    fn default() -> Self {
        Self {
            getter: Value::UNDEFINED,
            setter: Value::UNDEFINED,
        }
    }
}
impl ArenaItem for Accessor {
    type Id = AccId;
}
impl Trace for Accessor {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.getter.trace(tracer);
        self.setter.trace(tracer);
    }
}

macro_rules! object_data {
    ($( $variant:ident($payload:ident) ),+ $(,)?) => {
        #[derive(Debug)]
        pub enum ObjectData {
            Ordinary,
            NativeFunction(NativeFunctionData),
            RegExp(RegExpData),
            $( $variant(TrackedBox<$payload>), )+
        }

        impl Trace for ObjectData {
            fn trace(&self, tracer: &Tracer<'_>) {
                match self {
                    Self::Ordinary => {},
                    Self::NativeFunction(data) => data.trace(tracer),
                    Self::RegExp(data) => data.trace(tracer),
                    $( Self::$variant(data) => data.trace(tracer), )+
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_slots_stay_inline_until_the_third_value() {
        assert_eq!(size_of::<PropSlots>(), 32);
        assert_eq!(size_of::<JsObject>(), 72);
        let ledger = HeapLedger::new(1024, 32).unwrap();
        let base = ledger.used_bytes();
        let mut slots = PropSlots::new();
        assert!(slots.is_empty());
        ledger.fail_after(Some(0));
        slots.push(&ledger, Value::EMPTY).unwrap();
        slots.push(&ledger, Value::int32(2)).unwrap();
        assert_eq!(&*slots, &[Value::EMPTY, Value::int32(2)]);
        assert_eq!(ledger.used_bytes(), base);
        assert_eq!(
            slots.push(&ledger, Value::int32(3)),
            Err(AllocError::Failpoint)
        );
        assert_eq!(&*slots, &[Value::EMPTY, Value::int32(2)]);
        assert_eq!(ledger.used_bytes(), base);
        slots.push(&ledger, Value::int32(3)).unwrap();
        slots.push(&ledger, Value::int32(4)).unwrap();
        assert_eq!(ledger.used_bytes(), base + 4 * SLOT_BYTES);
        ledger.fail_after(Some(0));
        assert_eq!(
            slots.push(&ledger, Value::int32(5)),
            Err(AllocError::Failpoint)
        );
        assert_eq!(slots.len(), 4);
        slots.push(&ledger, Value::int32(5)).unwrap();
        assert_eq!(ledger.peak_bytes(), base + (4 + 8) * SLOT_BYTES);
        assert_eq!(ledger.used_bytes(), base + 8 * SLOT_BYTES);
        ledger.fail_after(Some(0));
        slots.shrink_to(&ledger, 0);
        assert!(ledger.failpoint_armed());
        assert_eq!(ledger.used_bytes(), base + 5 * SLOT_BYTES);
        slots.truncate(2);
        slots.shrink_to(&ledger, 2);
        assert_eq!(&*slots, &[Value::EMPTY, Value::int32(2)]);
        assert_eq!(slots.heap_capacity(), 0);
        assert_eq!(ledger.used_bytes(), base);
        assert_eq!(slots.remove(0), Value::EMPTY);
        assert_eq!(&*slots, &[Value::int32(2)]);
        ledger.fail_after(None);
        slots.try_reserve_exact(&ledger, 2).unwrap();
        assert_eq!(ledger.used_bytes(), base + 3 * SLOT_BYTES);
        drop(slots);
        assert_eq!(ledger.used_bytes(), base);
    }
}

object_data!(
    Array(ArrayData),
    BytecodeFunction(BytecodeFunctionData),
    BoundFunction(BoundFunctionData),
    Arguments(ArgumentsData),
    MappedArguments(MappedArgumentsData),
    StringObject(StringObjectData),
    NumberObject(NumberObjectData),
    BooleanObject(BooleanObjectData),
    SymbolObject(SymbolObjectData),
    BigIntObject(BigIntObjectData),
    Error(ErrorData),
    CallSite(ErrorFrame),
    Date(DateData),
    ArrayBuffer(ArrayBufferData),
    SharedArrayBuffer(SharedArrayBufferData),
    TypedArray(TypedArrayData),
    DataView(DataViewData),
    Map(MapData),
    Set(SetData),
    CollectionIterator(CollectionIteratorData),
    WeakMap(WeakMapData),
    WeakSet(WeakSetData),
    WeakRef(WeakRefData),
    FinalizationRegistry(FinalizationRegistryData),
    Proxy(ProxyData),
    Promise(PromiseData),
    Generator(GeneratorData),
    AsyncFunction(AsyncFunctionData),
    AsyncGenerator(AsyncGeneratorData),
    Iterator(IteratorData),
    ForInIterator(ForInIteratorData),
    StringIterator(StringIteratorData),
    RegExpStringIterator(RegExpStringIteratorData),
    DisposableStack(DisposableStackData),
    AsyncDisposableStack(AsyncDisposableStackData),
    ModuleNamespace(ModuleNamespaceData),
    DomException(DomExceptionData),
    Performance(PerformanceData),
    HostObject(HostObjectData),
);
