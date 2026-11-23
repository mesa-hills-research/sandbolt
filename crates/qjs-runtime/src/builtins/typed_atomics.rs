use std::sync::Arc;

use crate::atom::{Atom, PropKey};
use crate::heap::Heap;
use crate::object::{ObjectData, ObjectFlags, SharedBufferBacking, TypedArrayData, TypedArrayKind};
use crate::ops::{call, convert, property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::{JsResult, ObjId, Value};

use super::{
    TYPED_ATOMICS_BASE, configure_ctor, define_data, install_accessor, install_method,
    make_native_fn,
};

mod atomics;
mod buffer;
mod elements;
mod encoding;
mod typed;
mod view;

#[cfg(test)]
mod tests;

const AB: u16 = 0;
const SAB: u16 = 1;
const ABSTRACT_TA: u16 = 2;
const DV: u16 = 3;
const SPECIES: u16 = 4;
const IS_VIEW: u16 = 5;
const TA_START: u16 = 16;
const BUFFER_START: u16 = 32;
const TYPED_START: u16 = 64;
const VIEW_START: u16 = 128;
const ATOMIC_START: u16 = 160;
const ENCODING_START: u16 = 192;

fn method_flags() -> PropFlags {
    PropFlags::WRITABLE | PropFlags::CONFIGURABLE
}

fn id(offset: u16) -> NativeFnId {
    NativeFnId(TYPED_ATOMICS_BASE + offset)
}

fn symbol(ctx: &ExecCtx<'_, '_>, name: WellKnown) -> PropKey {
    let sym = ctx.heap().symbol_registry.well_known(name);
    ctx.heap().symbols.get(sym).unwrap().key()
}

fn item<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, i: usize) -> JsResult<Handle<'s>> {
    match args.get(i) {
        Some(value) => Ok(value),
        None => ctx.root(Value::UNDEFINED),
    }
}

fn index_arg<'s>(ctx: &mut ExecCtx<'_, 's>, arg: Handle<'s>) -> JsResult<usize> {
    let n = convert::to_integer_or_infinity(ctx, arg)?;
    if !(0.0..=9_007_199_254_740_991.0).contains(&n) || n > usize::MAX as f64 {
        return Err(ctx.throw_range_error("Invalid index"));
    }
    Ok(n as usize)
}

fn relative(n: f64, len: usize) -> usize {
    if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        (n as usize).min(len)
    }
}

fn number(n: usize) -> Value {
    Value::number(n as f64)
}

fn key_at(ctx: &mut ExecCtx<'_, '_>, n: usize) -> JsResult<PropKey> {
    if let Ok(n) = u32::try_from(n)
        && let Some(key) = PropKey::index(n)
    {
        return Ok(key);
    }
    ctx.intern_str(&n.to_string())
}

fn named<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>, name: &str) -> JsResult<Value> {
    let key = ctx.intern_str(name)?;
    property::get_property(ctx, target, key)
}

fn put<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    name: &str,
    value: Value,
    flags: PropFlags,
) -> JsResult<()> {
    let value = ctx.root(value)?;
    let key = ctx.intern_str(name)?;
    define_data(ctx, target, key, value, flags)
}

fn methods<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    base: u16,
    table: &[(&str, u32)],
) -> JsResult<()> {
    let target = ctx.get(target);
    for (i, &(name, length)) in table.iter().enumerate() {
        ctx.with_scope(|ctx| {
            let target = ctx.root(target)?;
            let key = ctx.intern_str(name)?;
            install_method(ctx, target, key, id(base + i as u16), name, length)
        })?;
    }
    Ok(())
}

fn getters<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    base: u16,
    table: &[&str],
) -> JsResult<()> {
    let target = ctx.get(target);
    for (i, &name) in table.iter().enumerate() {
        ctx.with_scope(|ctx| {
            let target = ctx.root(target)?;
            let key = ctx.intern_str(name)?;
            install_accessor(
                ctx,
                target,
                key,
                Some((id(base + i as u16), &format!("get {name}"))),
                None,
                true,
            )
        })?;
    }
    Ok(())
}

fn tag<'s>(ctx: &mut ExecCtx<'_, 's>, proto: Handle<'s>, name: &str) -> JsResult<()> {
    let key = symbol(ctx, WellKnown::ToStringTag);
    let value = ctx.new_string(name)?;
    define_data(ctx, proto, key, value, PropFlags::CONFIGURABLE)
}

fn species<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>) -> JsResult<()> {
    let key = symbol(ctx, WellKnown::Species);
    install_accessor(
        ctx,
        target,
        key,
        Some((id(SPECIES), "get [Symbol.species]")),
        None,
        true,
    )
}

fn species_constructor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    source: Handle<'s>,
) -> JsResult<Option<Handle<'s>>> {
    let ctor = named(ctx, source, "constructor")?;
    if ctor == Value::UNDEFINED {
        return Ok(None);
    }
    if ctor.as_object().is_none() {
        return Err(ctx.throw_type_error("Invalid constructor"));
    }
    let ctor = ctx.root(ctor)?;
    let key = symbol(ctx, WellKnown::Species);
    let species = property::get_property(ctx, ctor, key)?;
    if species == Value::UNDEFINED || species == Value::NULL {
        return Ok(None);
    }
    let constructor = ctx.root(species)?;
    require_constructor(ctx, constructor)?;
    Ok(Some(constructor))
}

fn require_constructor<'s>(ctx: &mut ExecCtx<'_, 's>, ctor: Handle<'s>) -> JsResult<()> {
    if !ctx.get(ctor).as_object().is_some_and(|o| {
        ctx.heap()
            .object(o)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
    }) {
        return Err(ctx.throw_type_error("Expected a constructor"));
    }
    Ok(())
}

fn require_callable<'s>(ctx: &mut ExecCtx<'_, 's>, f: Handle<'s>) -> JsResult<()> {
    if !ctx
        .get(f)
        .as_object()
        .is_some_and(|o| call::is_callable(ctx.heap(), o))
    {
        return Err(ctx.throw_type_error("Expected a function"));
    }
    Ok(())
}

fn proto_from_target<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    default: impl Fn(&crate::realm::Realm) -> ObjId,
) -> JsResult<Handle<'s>> {
    let proto = property::get_property(ctx, target, PropKey::atom(Atom::PROTOTYPE))?;
    if proto.as_object().is_some() {
        return ctx.root(proto);
    }
    let mut target = ctx.object_id(target)?;
    let realm = loop {
        match &ctx.heap().object(target).data {
            ObjectData::NativeFunction(data) => break data.realm,
            ObjectData::BytecodeFunction(data) => break data.realm,
            ObjectData::BoundFunction(data) => target = data.target,
            ObjectData::Proxy(data) => {
                target = data
                    .target
                    .ok_or_else(|| ctx.throw_type_error("Proxy is revoked"))?
            }
            _ => break ctx.realm_id(),
        }
    };
    ctx.root(Value::object(default(ctx.heap().realm(realm))))
}

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    buffer::install(ctx)?;
    typed::install(ctx)?;
    view::install(ctx)?;
    atomics::install(ctx)
}

pub fn dispatch<'s>(
    native: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match native.0 - TYPED_ATOMICS_BASE {
        SPECIES => Ok(ctx.get(this)),
        IS_VIEW => Ok(Value::bool(
            args.get(0)
                .and_then(|h| ctx.get(h).as_object())
                .is_some_and(|o| {
                    matches!(
                        ctx.heap().object(o).data,
                        ObjectData::TypedArray(_) | ObjectData::DataView(_)
                    )
                }),
        )),
        ABSTRACT_TA => Err(ctx.throw_type_error("TypedArray is an abstract constructor")),
        offset @ (AB | SAB | DV | TA_START..=27) => {
            let target = ctx
                .new_target()
                .ok_or_else(|| ctx.throw_type_error("Constructor requires new"))?;
            let target = ctx.root(Value::object(target))?;
            construct_native(offset, ctx, target, args)
        }
        offset @ BUFFER_START..=63 => buffer::dispatch(offset - BUFFER_START, ctx, this, args),
        offset @ TYPED_START..=127 => typed::dispatch(offset - TYPED_START, ctx, this, args),
        offset @ VIEW_START..=159 => view::dispatch(offset - VIEW_START, ctx, this, args),
        offset @ ATOMIC_START..=191 => atomics::dispatch(offset - ATOMIC_START, ctx, args),
        offset @ ENCODING_START..=223 => {
            encoding::dispatch(offset - ENCODING_START, ctx, this, args)
        }
        _ => Err(ctx.throw_type_error("Unknown binary data builtin")),
    }
}

fn construct_native<'s>(
    offset: u16,
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match offset {
        AB | SAB => buffer::construct(ctx, target, args, offset == SAB),
        DV => view::construct(ctx, target, args),
        _ => typed::construct(
            ctx,
            target,
            args,
            TypedArrayKind::ALL[(offset - TA_START) as usize],
        ),
    }
}

pub fn detach_array_buffer(ctx: &mut ExecCtx<'_, '_>, obj: ObjId) -> JsResult<()> {
    buffer::detach(ctx, obj)
}

pub fn create_shared_array_buffer_from_arc<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    backing: Arc<SharedBufferBacking>,
) -> JsResult<Handle<'s>> {
    buffer::wrap_shared(ctx, backing)
}

pub fn typed_array_length(heap: &Heap, obj: ObjId) -> Option<usize> {
    typed::data(heap, obj).and_then(|view| typed::length(heap, view))
}

pub fn typed_array_get_index(
    ctx: &mut ExecCtx<'_, '_>,
    obj: ObjId,
    index: usize,
) -> JsResult<Value> {
    if let Some(view) = typed::data(ctx.heap(), obj) {
        typed::get(ctx, view, index)
    } else {
        Ok(Value::UNDEFINED)
    }
}

pub fn typed_array_set_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    obj: ObjId,
    index: Option<usize>,
    value: Handle<'s>,
) -> JsResult<()> {
    let Some(view) = typed::data(ctx.heap(), obj) else {
        return Err(ctx.throw_type_error("Expected a TypedArray"));
    };
    let bits = elements::encode(ctx, view.kind, value)?;
    if let Some(index) = index {
        typed::put_element(ctx, view, index, bits);
    }
    Ok(())
}
