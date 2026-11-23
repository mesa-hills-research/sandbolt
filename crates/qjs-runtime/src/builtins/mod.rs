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

use crate::atom::{Atom, PropKey};
use crate::object::{ObjectData, ObjectFlags};
use crate::ops::property::{PropertyDescriptor, define_own_property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::value::{JsResult, ObjId, Value};

pub const CORE_BASE: u16 = 0x1000;
pub const ARRAY_ITER_BASE: u16 = 0x1200;
pub const STRING_RE_BASE: u16 = 0x1400;
pub const NUM_DATE_BASE: u16 = 0x1600;
pub const COLL_WEAK_BASE: u16 = 0x1800;
pub const TYPED_ATOMICS_BASE: u16 = 0x1A00;
pub const PROMISE_JSON_EXTRAS_BASE: u16 = 0x1C00;
pub const VM_INTERNAL_BASE: u16 = 0x1E00;

/// Installs all ECMAScript built-in constructors, prototypes, methods, and global
/// properties onto `ctx.realm_id()`.
pub fn install_all(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    core::install(ctx)?;
    array_iter::install(ctx)?;
    string_re::install(ctx)?;
    num_date::install(ctx)?;
    coll_weak::install(ctx)?;
    typed_atomics::install(ctx)?;
    promise_json_extras::install(ctx)?;
    vm_internal::install(ctx)?;
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

/// Creates a native function with standard `name` and `length` metadata.
pub fn make_native_fn<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    name: &str,
    length: u32,
    is_constructor: bool,
) -> JsResult<Handle<'s>> {
    let func = ctx.new_native(id, is_constructor)?;
    set_fn_metadata(ctx, func, name, length)?;
    Ok(func)
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

/// Installs a non-enumerable writable+configurable method on `target`.
pub fn install_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    id: NativeFnId,
    name: &str,
    length: u32,
) -> JsResult<()> {
    let func = make_native_fn(ctx, id, name, length, false)?;
    define_data(
        ctx,
        target,
        key,
        func,
        PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
    )
}

/// Installs an accessor property (getter and optional setter) on `target`.
pub fn install_accessor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    getter: Option<(NativeFnId, &str)>,
    setter: Option<(NativeFnId, &str)>,
    configurable: bool,
) -> JsResult<()> {
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
