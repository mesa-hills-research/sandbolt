use std::cell::Cell;

use qjs_bytecode::{BuiltinAtom, UnitAtom};

use crate::arena::Arena;
use crate::atom::Atom;
use crate::heap::Heap;
use crate::object::{JsObject, ObjectData, ObjectFlags};
use crate::shape::{PropFlags, Shape};
use crate::symbol::{SymbolData, SymbolRegistry};
use crate::value::*;
use crate::vm::{CompiledFunc, InlineCache, PrimCacheTag};

#[inline]
pub(super) fn get_field(
    heap: &Heap,
    function: &CompiledFunc,
    cell: &Cell<InlineCache>,
    atom: UnitAtom,
    receiver: Value,
    realm: RealmId,
) -> Option<Value> {
    match cell.get() {
        InlineCache::Own { shape, slot, .. } => {
            let id = receiver.as_object()?;
            let object = heap.object(id);
            if object.shape == shape {
                let value = object.props[slot as usize];
                if !value.is_auto_native() {
                    return Some(value);
                }
                return None;
            }
        }
        InlineCache::Proto1 {
            shape,
            holder,
            holder_shape,
            slot,
        } => {
            let id = receiver.as_object()?;
            let object = heap.object(id);
            if object.shape == shape {
                // The live object traces its shape, which traces its prototype `holder`.
                let proto = heap.object(holder);
                if proto.shape == holder_shape {
                    let value = proto.props[slot as usize];
                    if !value.is_auto_native() {
                        return Some(value);
                    }
                    return None;
                }
            }
        }
        InlineCache::Primitive {
            tag,
            realm: cached_realm,
            holder,
            holder_shape,
            slot,
        } => {
            if realm == cached_realm && tag_matches(tag, receiver) {
                // The active realm traces its intrinsic prototype `holder`.
                let proto = heap.object(holder);
                if proto.shape == holder_shape {
                    let value = proto.props[slot as usize];
                    if !value.is_auto_native() {
                        return Some(value);
                    }
                    return None;
                }
            }
        }
        InlineCache::ArrayLength => {
            if let Some(id) = receiver.as_object()
                && let ObjectData::Array(array) = &heap.object(id).data
            {
                return Some(length_value(array.length));
            }
        }
        InlineCache::StringLength => {
            if let ValueKind::String(id) = receiver.decode() {
                return Some(Value::int32(heap.string(id).len() as i32));
            }
        }
        _ => {}
    }
    fill_get_field(heap, function, cell, atom, receiver, realm)
}

#[inline]
pub(super) fn length_value(len: u32) -> Value {
    if len <= i32::MAX as u32 {
        Value::int32(len as i32)
    } else {
        Value::number(f64::from(len))
    }
}

#[inline]
fn is_plain_for_key(heap: &Heap, object: &JsObject, key: crate::atom::PropKey) -> bool {
    if matches!(object.data, ObjectData::MappedArguments(_)) {
        return false;
    }
    if !object.flags.contains(ObjectFlags::IS_EXOTIC) {
        return true;
    }
    matches!(
        object.data,
        ObjectData::Array(_) | ObjectData::StringObject(_)
    ) && heap.array_index(key).is_none()
        && key != crate::atom::PropKey::atom(Atom::LENGTH)
}

#[inline]
fn tag_matches(tag: PrimCacheTag, value: Value) -> bool {
    match tag {
        PrimCacheTag::String => matches!(value.decode(), ValueKind::String(_)),
        PrimCacheTag::Number => value.is_number(),
    }
}

#[cold]
fn fill_get_field(
    heap: &Heap,
    function: &CompiledFunc,
    cell: &Cell<InlineCache>,
    atom: UnitAtom,
    receiver: Value,
    realm: RealmId,
) -> Option<Value> {
    if atom.raw() == BuiltinAtom::Length as u32 {
        if let Some(id) = receiver.as_object()
            && let ObjectData::Array(array) = &heap.object(id).data
        {
            cell.set(InlineCache::ArrayLength);
            return Some(length_value(array.length));
        }
        if let ValueKind::String(id) = receiver.decode() {
            cell.set(InlineCache::StringLength);
            return Some(Value::int32(heap.string(id).len() as i32));
        }
    }
    if let Some(id) = receiver.as_object() {
        let object = heap.object(id);
        let key = super::compiled_key(heap, function, atom);
        if !is_plain_for_key(heap, object, key) {
            return None;
        }
        let shape = heap.shape(object.shape);
        if shape.is_mutable() {
            return None;
        }
        if let Some((slot, flags)) = shape.find(key) {
            if flags.contains(PropFlags::ACCESSOR) {
                return None;
            }
            let value = object.props[slot];
            if value.is_auto_native() {
                return None;
            }
            cell.set(InlineCache::Own {
                shape: object.shape,
                slot: slot as u32,
                flags,
            });
            return Some(value);
        }
        let holder = shape.proto?;
        let proto = heap.object(holder);
        if !is_plain_for_key(heap, proto, key) {
            return None;
        }
        let proto_shape = heap.shape(proto.shape);
        if proto_shape.is_mutable() {
            return None;
        }
        let (slot, flags) = proto_shape.find(key)?;
        if flags.contains(PropFlags::ACCESSOR) {
            return None;
        }
        let value = proto.props[slot];
        if value.is_auto_native() {
            return None;
        }
        cell.set(InlineCache::Proto1 {
            shape: object.shape,
            holder,
            holder_shape: proto.shape,
            slot: slot as u32,
        });
        return Some(value);
    }

    let (tag, holder) = match receiver.decode() {
        ValueKind::String(_) => {
            let key = super::compiled_key(heap, function, atom);
            if heap.array_index(key).is_some() || key == crate::atom::PropKey::atom(Atom::LENGTH) {
                return None;
            }
            (PrimCacheTag::String, heap.realm(realm).string_proto)
        }
        ValueKind::Int32(_) | ValueKind::Float64(_) => {
            (PrimCacheTag::Number, heap.realm(realm).number_proto)
        }
        _ => return None,
    };
    let key = super::compiled_key(heap, function, atom);
    let proto = heap.object(holder);
    if !is_plain_for_key(heap, proto, key) {
        return None;
    }
    let proto_shape = heap.shape(proto.shape);
    if proto_shape.is_mutable() {
        return None;
    }
    let (slot, flags) = proto_shape.find(key)?;
    if flags.contains(PropFlags::ACCESSOR) {
        return None;
    }
    let value = proto.props[slot];
    if value.is_auto_native() {
        return None;
    }
    cell.set(InlineCache::Primitive {
        tag,
        realm,
        holder,
        holder_shape: proto.shape,
        slot: slot as u32,
    });
    Some(value)
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn put_field(
    objects: &mut Arena<JsObject>,
    shapes: &Arena<Shape>,
    symbols: &Arena<SymbolData>,
    symbol_registry: &SymbolRegistry,
    function: &CompiledFunc,
    cell: &Cell<InlineCache>,
    atom: UnitAtom,
    receiver: Value,
    value: Value,
) -> bool {
    let Some(id) = receiver.as_object() else {
        return false;
    };
    match cell.get() {
        InlineCache::Own { shape, slot, flags } => {
            let object = objects.get_mut(id).unwrap();
            if object.shape == shape && flags.contains(PropFlags::WRITABLE) {
                object.props[slot as usize] = value;
                return true;
            }
        }
        InlineCache::ArrayLength => {
            if let ValueKind::Int32(n) = value.decode()
                && n >= 0
            {
                let object = objects.get_mut(id).unwrap();
                if object.flags.contains(ObjectFlags::FAST_ARRAY)
                    && !object.flags.contains(ObjectFlags::IS_PROTOTYPE)
                    && let ObjectData::Array(array) = &mut object.data
                    && array.length_writable
                    && (n as u32) <= array.length
                {
                    let n = n as u32;
                    if (n as usize) < array.elements.len() {
                        array.elements.truncate(n as usize);
                    }
                    array.length = n;
                    return true;
                }
            }
            return false;
        }
        _ => {}
    }
    fill_put_field(
        objects,
        shapes,
        symbols,
        symbol_registry,
        function,
        cell,
        atom,
        id,
        value,
    )
}

#[cold]
#[allow(clippy::too_many_arguments)]
fn fill_put_field(
    objects: &mut Arena<JsObject>,
    shapes: &Arena<Shape>,
    symbols: &Arena<SymbolData>,
    symbol_registry: &SymbolRegistry,
    function: &CompiledFunc,
    cell: &Cell<InlineCache>,
    atom: UnitAtom,
    id: ObjId,
    value: Value,
) -> bool {
    if atom.raw() == BuiltinAtom::Length as u32
        && matches!(objects.get(id).unwrap().data, ObjectData::Array(_))
    {
        if let ValueKind::Int32(n) = value.decode()
            && n >= 0
        {
            let object = objects.get_mut(id).unwrap();
            if object.flags.contains(ObjectFlags::FAST_ARRAY)
                && !object.flags.contains(ObjectFlags::IS_PROTOTYPE)
                && let ObjectData::Array(array) = &mut object.data
                && array.length_writable
                && (n as u32) <= array.length
            {
                let n = n as u32;
                if (n as usize) < array.elements.len() {
                    array.elements.truncate(n as usize);
                }
                array.length = n;
                cell.set(InlineCache::ArrayLength);
                return true;
            }
        }
        return false;
    }
    let object = objects.get(id).unwrap();
    if object.flags.contains(ObjectFlags::IS_EXOTIC)
        || object.flags.contains(ObjectFlags::IS_PROTOTYPE)
        || matches!(object.data, ObjectData::MappedArguments(_))
    {
        return false;
    }
    let shape_id = object.shape;
    let shape = shapes.get(shape_id).unwrap();
    if shape.is_mutable() {
        return false;
    }
    let key = super::compiled_key_parts(symbols, symbol_registry, function, atom);
    let Some((slot, flags)) = shape.find(key) else {
        return false;
    };
    if flags.contains(PropFlags::ACCESSOR) || !flags.contains(PropFlags::WRITABLE) {
        return false;
    }
    cell.set(InlineCache::Own {
        shape: shape_id,
        slot: slot as u32,
        flags,
    });
    objects.get_mut(id).unwrap().props[slot] = value;
    true
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn global_slot(
    heap: &Heap,
    function: &CompiledFunc,
    cell: &Cell<InlineCache>,
    atom: UnitAtom,
    object: &JsObject,
    writing: bool,
    want_ref_key: bool,
    epoch: usize,
    modules: &[ModId],
    realm: RealmId,
) -> Option<(usize, Value)> {
    if let InlineCache::Global {
        realm: cached_realm,
        shape,
        slot,
        flags,
        epoch: cached_epoch,
        key,
    } = cell.get()
        && cached_realm == realm
        && cached_epoch == epoch
        && shape == object.shape
        && (!writing || flags.contains(PropFlags::WRITABLE))
        && (!want_ref_key || key != Value::EMPTY)
        && (writing || want_ref_key || !object.props[slot as usize].is_auto_native())
    {
        return Some((slot as usize, key));
    }
    fill_global(
        heap,
        function,
        cell,
        atom,
        object,
        writing,
        want_ref_key,
        epoch,
        modules,
        realm,
    )
}

#[cold]
#[allow(clippy::too_many_arguments)]
fn fill_global(
    heap: &Heap,
    function: &CompiledFunc,
    cell: &Cell<InlineCache>,
    atom: UnitAtom,
    object: &JsObject,
    writing: bool,
    want_ref_key: bool,
    epoch: usize,
    modules: &[ModId],
    realm: RealmId,
) -> Option<(usize, Value)> {
    if object.flags.contains(ObjectFlags::IS_EXOTIC)
        || (writing && object.flags.contains(ObjectFlags::IS_PROTOTYPE))
    {
        return None;
    }
    let key = super::compiled_key(heap, function, atom);
    if super::bindings::lexical_in(heap, modules, realm, key).is_some() {
        return None;
    }
    let ref_value = if want_ref_key {
        let crate::atom::KeyKind::String(atom) = key.decode() else {
            return None;
        };
        Value::string(heap.atoms.dynamic_string(atom)?)
    } else {
        Value::EMPTY
    };
    let shape = heap.shape(object.shape);
    if shape.is_mutable() {
        return None;
    }
    let (slot, flags) = shape.find(key)?;
    if flags.contains(PropFlags::ACCESSOR)
        || (writing && !flags.contains(PropFlags::WRITABLE))
        || (!writing && !want_ref_key && object.props[slot].is_auto_native())
    {
        return None;
    }
    cell.set(InlineCache::Global {
        realm,
        shape: object.shape,
        slot: slot as u32,
        flags,
        epoch,
        key: ref_value,
    });
    Some((slot, ref_value))
}

#[inline]
pub(super) fn resolved_ref_slot(
    heap: &Heap,
    cell: &Cell<InlineCache>,
    object: &JsObject,
    writing: bool,
    str_id: StrId,
    realm: RealmId,
) -> Option<usize> {
    let target = Value::string(str_id);
    if let InlineCache::ResolvedRef {
        realm: cached_realm,
        shape,
        slot,
        flags,
        key,
    } = cell.get()
        && cached_realm == realm
        && shape == object.shape
        && key == target
        && (!writing || flags.contains(PropFlags::WRITABLE))
        && (writing || !object.props[slot as usize].is_auto_native())
    {
        return Some(slot as usize);
    }
    fill_resolved_ref(heap, cell, object, writing, str_id, realm)
}

#[cold]
fn fill_resolved_ref(
    heap: &Heap,
    cell: &Cell<InlineCache>,
    object: &JsObject,
    writing: bool,
    str_id: StrId,
    realm: RealmId,
) -> Option<usize> {
    if object.flags.contains(ObjectFlags::IS_EXOTIC)
        || (writing && object.flags.contains(ObjectFlags::IS_PROTOTYPE))
    {
        return None;
    }
    let key = heap.atoms.find(&heap.strings, str_id)?;
    let shape = heap.shape(object.shape);
    if shape.is_mutable() {
        return None;
    }
    let (slot, flags) = shape.find(key)?;
    if flags.contains(PropFlags::ACCESSOR)
        || (writing && !flags.contains(PropFlags::WRITABLE))
        || (!writing && object.props[slot].is_auto_native())
    {
        return None;
    }
    cell.set(InlineCache::ResolvedRef {
        realm,
        shape: object.shape,
        slot: slot as u32,
        flags,
        key: Value::string(str_id),
    });
    Some(slot)
}

#[cfg(test)]
mod tests {
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

    #[test]
    fn property_caches_follow_layout_changes_across_collection() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let first = "function read(o) { return o.x; } function write(o, v) { o.x = v; }\
                     (() => { const a = { x: 1, y: 2 }; write(a, 3); return read(a); })();";
        assert_eq!(
            engine
                .eval_to_string(first, EvalOptions::default())
                .unwrap(),
            "3"
        );
        engine.collect_garbage();
        let second = "const b = { y: 4, x: 5 }; const before = read(b);\
                      write(b, 6); [before, read(b), b.y].join('|');";
        assert_eq!(
            engine
                .eval_to_string(second, EvalOptions::default())
                .unwrap(),
            "5|6|4"
        );
    }

    #[test]
    fn prototype_and_primitive_caches_invalidate_on_shadow_and_mutation() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let script = r#"
            function callFoo(o) { return o.foo; }
            const p = { foo: 10 };
            const a = Object.create(p);
            const r1 = callFoo(a);
            a.foo = 20;
            const r2 = callFoo(a);
            const b = Object.create(p);
            const r3 = callFoo(b);
            p.foo = 30;
            const r4 = callFoo(b);
            delete p.foo;
            const r5 = String(callFoo(b));
            function strLen(s) { return s.length; }
            function slice1(s) { return s.slice(1); }
            const r6 = strLen("hello") + ":" + slice1("hello");
            String.prototype.slice = function() { return "patched"; };
            const r7 = slice1("hello");
            [r1, r2, r3, r4, r5, r6, r7].join("|");
        "#;
        assert_eq!(
            engine
                .eval_to_string(script, EvalOptions::default())
                .unwrap(),
            "10|20|10|30|undefined|5:ello|patched"
        );
    }
}
