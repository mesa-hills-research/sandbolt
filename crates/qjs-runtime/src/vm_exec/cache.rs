use std::cell::Cell;

use qjs_bytecode::{Opcode, UnitAtom};

use crate::heap::Heap;
use crate::ledger::{AllocResult, HeapLedger, TrackedVec};
use crate::object::ObjectFlags;
use crate::shape::PropFlags;
use crate::value::*;
use crate::vm::{CompiledFunc, InlineCache};

pub(super) fn supports(op: Opcode) -> bool {
    use Opcode::*;
    matches!(
        op,
        GetField
            | GetField2
            | PutField
            | GetVar
            | GetVarUndef
            | PutVar
            | MakeVarRef
            | GetRefValue
            | PutRefValue
    )
}

pub(super) fn slots(
    ledger: &HeapLedger,
    count: usize,
) -> AllocResult<TrackedVec<Cell<Option<InlineCache>>>> {
    let mut entries = TrackedVec::new();
    if count != 0 {
        // A bounded direct-mapped table avoids a cache-sized field for each bytecode byte.
        let capacity = usize::next_power_of_two(count);
        entries.try_reserve_exact(ledger, capacity)?;
        for _ in 0..capacity {
            entries.push_reserved(Cell::new(None));
        }
    }
    Ok(entries)
}

#[derive(Clone, Copy)]
pub(super) enum Key {
    Field(UnitAtom),
    Global(UnitAtom),
    GlobalRef(UnitAtom),
    ResolvedRef(StrId),
}

#[inline]
pub(super) fn own_slot(
    heap: &Heap,
    function: &CompiledFunc,
    pc: u32,
    id: ObjId,
    writing: bool,
    (epoch, name): (usize, Key),
    (modules, realm): (&[ModId], RealmId),
) -> Option<InlineCache> {
    let object = heap.object(id);
    if object.flags.contains(ObjectFlags::IS_EXOTIC)
        || (writing && object.flags.contains(ObjectFlags::IS_PROTOTYPE))
    {
        return None;
    }
    let entries = &function.inline_caches;
    let cell = entries.get(((pc ^ (pc >> 4)) as usize) & entries.len().wrapping_sub(1))?;
    if let Some(entry) = cell.get()
        && entry.pc == pc
        && entry.epoch == epoch
        && entry.shape == object.shape
        && heap.shapes.get_weak(entry.shape).is_some()
        && (!writing || entry.flags.contains(PropFlags::WRITABLE))
        && match name {
            Key::ResolvedRef(str) => entry.key == Value::string(str),
            _ => true,
        }
    {
        return Some(entry);
    }
    fill(
        heap,
        function,
        cell,
        pc,
        id,
        writing,
        (epoch, name),
        (modules, realm),
    )
}

#[cold]
#[allow(clippy::too_many_arguments)]
fn fill(
    heap: &Heap,
    function: &CompiledFunc,
    cell: &Cell<Option<InlineCache>>,
    pc: u32,
    id: ObjId,
    writing: bool,
    (epoch, name): (usize, Key),
    (modules, realm): (&[ModId], RealmId),
) -> Option<InlineCache> {
    let (key, value) = match name {
        Key::Field(atom) => (super::compiled_key(heap, function, atom), Value::EMPTY),
        Key::Global(atom) | Key::GlobalRef(atom) => {
            let key = super::compiled_key(heap, function, atom);
            if super::bindings::lexical_in(heap, modules, realm, key).is_some() {
                return None;
            }
            let value = if matches!(name, Key::GlobalRef(_)) {
                let crate::atom::KeyKind::String(atom) = key.decode() else {
                    return None;
                };
                Value::string(heap.atoms.dynamic_string(atom)?)
            } else {
                Value::EMPTY
            };
            (key, value)
        }
        Key::ResolvedRef(str) => (heap.atoms.find(&heap.strings, str)?, Value::string(str)),
    };
    let object = heap.object(id);
    let (slot, flags) = heap.shapes.get_weak(object.shape)?.find(key)?;
    if flags.contains(PropFlags::ACCESSOR) || (writing && !flags.contains(PropFlags::WRITABLE)) {
        return None;
    }
    let entry = InlineCache {
        pc,
        slot: slot as u32,
        shape: object.shape,
        flags,
        key: value,
        epoch,
    };
    cell.set(Some(entry));
    Some(entry)
}
