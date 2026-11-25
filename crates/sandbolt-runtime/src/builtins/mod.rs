//! Builtin ECMAScript object installers and native function dispatchers.
//!
//! Each builtin family owns a disjoint 512-ID block in `NativeFnId(u16)`
//! (`0x1000..=0x1FFF`) so families develop and merge without conflicts:
//! - `0x1000..=0x11FF`: `core` (Object, Function, Error/*Error, Reflect, Proxy)
//! - `0x1200..=0x13FF`: `array_iter` (Array, Iterator + helpers + zip, Array.fromAsync, DisposableStack, AsyncDisposableStack)
//! - `0x1400..=0x15FF`: `string_re` (String, RegExp, URI + escape/unescape)
//! - `0x1600..=0x17FF`: `num_date` (Number, Boolean, Math, BigInt, Date, global numeric fns)
//! - `0x1800..=0x19FF`: `coll_weak` (Map, Set, WeakMap, WeakSet, WeakRef, FinalizationRegistry, Symbol)
//! - `0x1A00..=0x1BFF`: `typed_atomics` (ArrayBuffer, SharedArrayBuffer, TypedArray + Float16Array, DataView, Atomics)
//! - `0x1C00..=0x1DFF`: `promise_json_extras` (Promise, JSON, DOMException, performance, atob, btoa, queueMicrotask)
//! - `0x1E00..=0x1FFF`: `vm_internal` (eval, Function ctor, Generator/Async/AsyncGenerator/AsyncFromSyncIterator)

pub mod array_iter;
pub mod coll_weak;
pub mod core;
pub mod num_date;
pub mod promise_json_extras;
pub mod string_re;
pub mod typed_atomics;
pub mod vm_internal;

use crate::atom::{Atom, AtomTable, KeyKind, PropKey};
use crate::object::{JsObject, NativeFunctionData, ObjectData, ObjectFlags, PropSlots};
use crate::ops::property::{
    PropertyDescriptor, define_auto_accessor, define_auto_native, define_own_property,
};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::string::JsString;
use crate::symbol::SymbolDescription;
use crate::value::{JsResult, ObjId, Value};

pub const CORE_BASE: u16 = 0x1000;
pub const ARRAY_ITER_BASE: u16 = 0x1200;
pub const STRING_RE_BASE: u16 = 0x1400;
pub const NUM_DATE_BASE: u16 = 0x1600;
pub const COLL_WEAK_BASE: u16 = 0x1800;
pub const TYPED_ATOMICS_BASE: u16 = 0x1A00;
pub const PROMISE_JSON_EXTRAS_BASE: u16 = 0x1C00;
pub const VM_INTERNAL_BASE: u16 = 0x1E00;

const INSERTION_SORT_LIMIT: usize = 6;

fn stable_insertion_sort<T: Copy>(
    values: &mut [T],
    mut compare: impl FnMut(T, T) -> JsResult<std::cmp::Ordering>,
) -> JsResult<()> {
    for i in 1..values.len() {
        let mut j = i;
        while j > 0 && compare(values[j - 1], values[j])?.is_gt() {
            values.swap(j - 1, j);
            j -= 1;
        }
    }
    Ok(())
}

/// Installs all ECMAScript built-in constructors, prototypes, methods, and global
/// properties onto `ctx.realm_id()`.
pub fn install_all(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    ctx.runtime.heap.building_shapes = true;
    let result = (|| {
        core::install(ctx)?;
        array_iter::install(ctx)?;
        string_re::install(ctx)?;
        num_date::install(ctx)?;
        coll_weak::install(ctx)?;
        typed_atomics::install(ctx)?;
        promise_json_extras::install(ctx)?;
        vm_internal::install(ctx)?;
        Ok(())
    })();
    ctx.runtime.heap.finish_bootstrap_shapes();
    result?;
    ctx.allocate(|runtime| runtime.heap.ensure_bootstrap_lookups())?;
    let realm = ctx.realm_id();
    ctx.runtime
        .heap
        .realms
        .get_mut(realm)
        .unwrap()
        .std_array_prototype_intact = true;
    Ok(())
}

/// Dispatches a reserved builtin `NativeFnId` (`0x1000..=0x1FFF`).
/// Returns `None` if `id` is outside the builtin ranges.
pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> Option<JsResult<Value>> {
    match id.0 {
        0x1000..=0x11FF => Some(core::dispatch(id, ctx, this, args)),
        0x1200..=0x13FF => Some(array_iter::dispatch(id, ctx, this, args)),
        0x1400..=0x15FF => Some(string_re::dispatch(id, ctx, this, args)),
        0x1600..=0x17FF => Some(num_date::dispatch(id, ctx, this, args)),
        0x1800..=0x19FF => Some(coll_weak::dispatch(id, ctx, this, args)),
        0x1A00..=0x1BFF => Some(typed_atomics::dispatch(id, ctx, this, args)),
        0x1C00..=0x1DFF => Some(promise_json_extras::dispatch(id, ctx, this, args)),
        0x1E00..=0x1FFF => Some(vm_internal::dispatch(id, ctx, this, args)),
        _ => None,
    }
}

/// Sets `length` and `name` properties on a native function object per ES2025
/// (`writable: false, enumerable: false, configurable: true`).
pub fn set_fn_metadata<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    func: Handle<'s>,
    name: &str,
    length: u32,
) -> JsResult<()> {
    let len_val = ctx.root(Value::int32(length as i32))?;
    let name_val = ctx.new_string(name)?;
    let len_desc = PropertyDescriptor {
        value: Some(len_val),
        writable: Some(false),
        enumerable: Some(false),
        configurable: Some(true),
        ..PropertyDescriptor::default()
    };
    define_own_property(ctx, func, PropKey::atom(Atom::LENGTH), len_desc)?;
    let name_desc = PropertyDescriptor {
        value: Some(name_val),
        writable: Some(false),
        enumerable: Some(false),
        configurable: Some(true),
        ..PropertyDescriptor::default()
    };
    define_own_property(ctx, func, PropKey::atom(Atom::NAME), name_desc)?;
    Ok(())
}

fn alloc_fn_name(
    heap: &mut crate::heap::Heap,
    name: &str,
    share_atom: bool,
) -> crate::ledger::AllocResult<crate::value::StrId> {
    if share_atom
        && let Some(key) = heap.atoms.find_str(&heap.strings, name)
        && let KeyKind::String(atom) = key.decode()
        && let Some(existing) = heap.atoms.dynamic_string(atom)
    {
        return Ok(existing);
    }
    let name_str = JsString::from_str(&heap.ledger, name)?;
    let name_id = heap.strings.insert(&heap.ledger, name_str)?;
    if share_atom && AtomTable::builtin_key(name).is_none() {
        let key = heap.atoms.insert(&heap.ledger, &heap.strings, name_id)?;
        if let KeyKind::String(atom) = key.decode()
            && let Some(existing) = heap.atoms.dynamic_string(atom)
        {
            return Ok(existing);
        }
    }
    Ok(name_id)
}

/// Creates a native function with standard `name` and `length` metadata.
pub fn make_native_fn<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    name: &str,
    length: u32,
    is_constructor: bool,
) -> JsResult<Handle<'s>> {
    let realm = ctx.realm_id();
    let flags = if is_constructor {
        ObjectFlags::EXTENSIBLE | ObjectFlags::IS_CONSTRUCTOR
    } else {
        ObjectFlags::EXTENSIBLE
    };
    let obj = ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let name_id = alloc_fn_name(heap, name, is_constructor)?;
        let shape = heap.realm(realm).native_fn_shape;
        let props = PropSlots::two(Value::int32(length as i32), Value::string(name_id));
        heap.objects.insert(
            &heap.ledger,
            JsObject {
                shape,
                props,
                flags,
                data: ObjectData::NativeFunction(NativeFunctionData {
                    function: id,
                    realm,
                }),
            },
        )
    })?;
    ctx.root(Value::object(obj))
}

/// Updates an already-bootstrapped constructor object's `NativeFnId`, `name`, and `length`.
pub fn configure_ctor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    ctor_id: ObjId,
    fn_id: NativeFnId,
    name: &str,
    length: u32,
) -> JsResult<Handle<'s>> {
    if let ObjectData::NativeFunction(data) =
        &mut ctx.runtime.heap.objects.get_mut(ctor_id).unwrap().data
    {
        data.function = fn_id;
    }
    ctx.runtime.heap.objects.get_mut(ctor_id).unwrap().flags |= ObjectFlags::IS_CONSTRUCTOR;
    let ctor = ctx.root(Value::object(ctor_id))?;
    let realm = ctx.realm_id();
    let shape = ctx.heap().object(ctor_id).shape;
    let ctor_shape = ctx.heap().realm(realm).ctor_shape;
    let ctor_proto_shape = ctx.heap().realm(realm).ctor_proto_shape;
    if shape == ctor_shape {
        ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let name_id = alloc_fn_name(heap, name, true)?;
            let obj = heap.objects.get_mut(ctor_id).unwrap();
            obj.props[1] = Value::int32(length as i32);
            obj.props[2] = Value::string(name_id);
            Ok(())
        })?;
        return Ok(ctor);
    }
    if ctor_proto_shape == Some(shape) {
        ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let name_id = alloc_fn_name(heap, name, true)?;
            let obj = heap.objects.get_mut(ctor_id).unwrap();
            obj.props.try_reserve_exact(&heap.ledger, 2)?;
            obj.props.push_reserved(Value::int32(length as i32));
            obj.props.push_reserved(Value::string(name_id));
            obj.shape = ctor_shape;
            Ok(())
        })?;
        return Ok(ctor);
    }
    set_fn_metadata(ctx, ctor, name, length)?;
    Ok(ctor)
}

/// Defines a data property on `target` with explicit `PropFlags`.
pub fn define_data<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
    flags: PropFlags,
) -> JsResult<()> {
    let desc = PropertyDescriptor {
        value: Some(value),
        writable: Some(flags.contains(PropFlags::WRITABLE)),
        enumerable: Some(flags.contains(PropFlags::ENUMERABLE)),
        configurable: Some(flags.contains(PropFlags::CONFIGURABLE)),
        ..PropertyDescriptor::default()
    };
    define_own_property(ctx, target, key, desc)?;
    Ok(())
}

fn name_matches_key(heap: &crate::heap::Heap, key: PropKey, name: &str) -> bool {
    match key.decode() {
        KeyKind::String(atom) => {
            if let Some(text) = AtomTable::builtin(atom) {
                text == name
            } else if let Some(str_id) = heap.atoms.dynamic_string(atom) {
                let mut units = heap.string_units(str_id);
                name.encode_utf16().all(|u| units.next() == Some(u)) && units.next().is_none()
            } else {
                false
            }
        }
        KeyKind::Symbol(token) => {
            let Some(sym_id) = heap.symbol_registry.from_token(token) else {
                return false;
            };
            match heap.symbols.get(sym_id).unwrap().description {
                SymbolDescription::None => name.is_empty(),
                SymbolDescription::WellKnown(wk) => {
                    name.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                        == Some(wk.description())
                }
                SymbolDescription::String(str_id) => {
                    let Some(inner) = name.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                    else {
                        return false;
                    };
                    let mut units = heap.string_units(str_id);
                    inner.encode_utf16().all(|u| units.next() == Some(u)) && units.next().is_none()
                }
            }
        }
        KeyKind::Index(_) => false,
    }
}

/// Installs a method on `target` with explicit `PropFlags`, using lazy `AutoNative`
/// storage when `target` is a normal object and the method name matches `key`.
pub(crate) fn install_method_with_flags<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    id: NativeFnId,
    name: &str,
    length: u32,
    flags: PropFlags,
) -> JsResult<()> {
    let target_id = ctx.object_id(target)?;
    let can_auto = {
        let heap = ctx.heap();
        let object = heap.object(target_id);
        !matches!(
            object.data,
            ObjectData::Proxy(_) | ObjectData::TypedArray(_) | ObjectData::ModuleNamespace(_)
        ) && key != PropKey::atom(Atom::LENGTH)
            && heap.array_index(key).is_none()
            && u8::try_from(length).is_ok()
            && name_matches_key(heap, key, name)
    };
    if can_auto && let Some(auto) = Value::auto_native(id.0, length as u8, ctx.realm_id().index()) {
        return define_auto_native(ctx, target_id, key, flags, auto);
    }
    let func = make_native_fn(ctx, id, name, length, false)?;
    define_data(ctx, target, key, func, flags)
}

/// Installs a non-enumerable writable+configurable method on `target`.
pub fn install_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    id: NativeFnId,
    name: &str,
    length: u32,
) -> JsResult<()> {
    install_method_with_flags(
        ctx,
        target,
        key,
        id,
        name,
        length,
        PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
    )
}

/// Installs an accessor property (getter and optional setter) on `target`,
/// using lazy `AutoNative` storage when `target` is a fresh property on a normal
/// object and the getter/setter names match `"get "` / `"set "` + `key`.
pub fn install_accessor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    getter: Option<(NativeFnId, &str)>,
    setter: Option<(NativeFnId, &str)>,
    configurable: bool,
) -> JsResult<()> {
    let target_id = ctx.object_id(target)?;
    let realm_index = ctx.realm_id().index();
    let (can_auto, get_auto, set_auto) = {
        let heap = ctx.heap();
        let object = heap.object(target_id);
        let can_auto = !matches!(
            object.data,
            ObjectData::Proxy(_) | ObjectData::TypedArray(_) | ObjectData::ModuleNamespace(_)
        ) && key != PropKey::atom(Atom::LENGTH)
            && heap.array_index(key).is_none()
            && heap.shape(object.shape).find(key).is_none();
        if can_auto {
            let get_auto = getter.and_then(|(id, name)| {
                name.strip_prefix("get ")
                    .is_some_and(|rest| name_matches_key(heap, key, rest))
                    .then(|| Value::auto_native(id.0, 0, realm_index))
                    .flatten()
            });
            let set_auto = setter.and_then(|(id, name)| {
                name.strip_prefix("set ")
                    .is_some_and(|rest| name_matches_key(heap, key, rest))
                    .then(|| Value::auto_native(id.0, 1, realm_index))
                    .flatten()
            });
            (true, get_auto, set_auto)
        } else {
            (false, None, None)
        }
    };
    if can_auto && (get_auto.is_some() || set_auto.is_some()) {
        let get_val = match (getter, get_auto) {
            (None, _) => Value::UNDEFINED,
            (_, Some(auto)) => auto,
            (Some((id, name)), None) => {
                let func = make_native_fn(ctx, id, name, 0, false)?;
                ctx.get(func)
            }
        };
        let get_root = ctx.root(get_val)?;
        let set_val = match (setter, set_auto) {
            (None, _) => Value::UNDEFINED,
            (_, Some(auto)) => auto,
            (Some((id, name)), None) => {
                let func = make_native_fn(ctx, id, name, 1, false)?;
                ctx.get(func)
            }
        };
        let mut flags = PropFlags::ACCESSOR;
        flags.set(PropFlags::CONFIGURABLE, configurable);
        return define_auto_accessor(ctx, target_id, key, flags, ctx.get(get_root), set_val);
    }
    let get = match getter {
        Some((id, name)) => Some(make_native_fn(ctx, id, name, 0, false)?),
        None => None,
    };
    let set = match setter {
        Some((id, name)) => Some(make_native_fn(ctx, id, name, 1, false)?),
        None => None,
    };
    let desc = PropertyDescriptor {
        get,
        set,
        enumerable: Some(false),
        configurable: Some(configurable),
        ..PropertyDescriptor::default()
    };
    define_own_property(ctx, target, key, desc)?;
    Ok(())
}
