use crate::atom::{Atom, PropKey};
use crate::ledger::TrackedVec;
use crate::object::{HostObjectData, ObjectData, ObjectFlags};
use crate::ops::call::{call, is_callable};
use crate::ops::convert::to_object;
use crate::ops::property::{get_property, get_property_with_receiver};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::{JsResult, Value, ValueKind};

mod extras;
mod json;
mod promise;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub(crate) use promise::{
    PromiseCapability, fulfill_promise, new_promise_capability, perform_promise_then,
    promise_resolve, reject_promise,
};

const BASE: u16 = super::PROMISE_JSON_EXTRAS_BASE;
const DATA: PropFlags = PropFlags::DEFAULT_DATA;

fn mutable() -> PropFlags {
    PropFlags::WRITABLE | PropFlags::CONFIGURABLE
}
const LENGTH: PropKey = PropKey::atom(Atom::LENGTH);

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    ctx.with_scope(promise::install)?;
    ctx.with_scope(json::install)?;
    ctx.with_scope(extras::install)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id.0 - BASE {
        0x00..=0x7f => promise::dispatch(id, ctx, this, args),
        0x80..=0x9f => json::dispatch(id, ctx, args),
        _ => extras::dispatch(id, ctx, this, args),
    }
}

fn arg<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, index: usize) -> JsResult<Handle<'s>> {
    match args.get(index) {
        Some(value) => Ok(value),
        None => ctx.root(Value::UNDEFINED),
    }
}

fn global<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
    ctx.root(Value::object(ctx.realm().global_object))
}

fn data<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    value: Handle<'s>,
    flags: PropFlags,
) -> JsResult<()> {
    let key = ctx.intern_str(name)?;
    super::define_data(ctx, object, key, value, flags)
}

fn define_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    id: NativeFnId,
    length: u32,
) -> JsResult<()> {
    let key = ctx.intern_str(name)?;
    super::install_method(ctx, object, key, id, name, length)
}

fn well_known(ctx: &ExecCtx<'_, '_>, which: WellKnown) -> PropKey {
    let id = ctx.heap().symbol_registry.well_known(which);
    ctx.heap().symbols.get(id).unwrap().key()
}

fn tag<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>, name: &str) -> JsResult<()> {
    let key = well_known(ctx, WellKnown::ToStringTag);
    let name = ctx.new_string(name)?;
    super::define_data(ctx, target, key, name, PropFlags::CONFIGURABLE)
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>, key: PropKey) -> JsResult<Handle<'s>> {
    let object = to_object(ctx, target)?;
    let value = get_property_with_receiver(ctx, object, key, target)?;
    ctx.root(value)
}

fn get_named<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    name: &str,
) -> JsResult<Handle<'s>> {
    let key = ctx.intern_str(name)?;
    get(ctx, target, key)
}

fn callable(ctx: &ExecCtx<'_, '_>, value: Value) -> bool {
    value
        .as_object()
        .is_some_and(|id| is_callable(ctx.heap(), id))
}

fn call_with<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callback: Handle<'s>,
    receiver: Handle<'s>,
    args: &[Handle<'s>],
) -> JsResult<Value> {
    let args = ctx.args(args)?;
    call(ctx, callback, receiver, args)
}

fn host<'s>(ctx: &mut ExecCtx<'_, 's>, data: HostObjectData) -> JsResult<Handle<'s>> {
    ctx.make_object(None, ObjectFlags::default(), |ledger| {
        Ok(ObjectData::HostObject(crate::ledger::TrackedBox::new(
            ledger, data,
        )?))
    })
}

fn closure<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    receiver: Handle<'s>,
    length: u32,
) -> JsResult<Handle<'s>> {
    let function = ctx.new_native(id, false)?;
    let bound = ctx.bind_function(function, receiver, Args::empty())?;
    super::set_fn_metadata(ctx, bound, "", length)?;
    Ok(bound)
}

fn index_key(ctx: &mut ExecCtx<'_, '_>, index: u64) -> JsResult<PropKey> {
    if let Ok(index) = u32::try_from(index)
        && let Some(key) = PropKey::index(index)
    {
        return Ok(key);
    }
    let mut bytes = [0; 20];
    let len = sandbolt_dtoa::u64toa(&mut bytes, index);
    ctx.intern_str(std::str::from_utf8(&bytes[..len]).unwrap())
}

fn is_array(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<bool> {
    let Some(mut id) = value.as_object() else {
        return Ok(false);
    };
    loop {
        match &ctx.heap().object(id).data {
            ObjectData::Array(_) => return Ok(true),
            ObjectData::Proxy(proxy) => {
                id = proxy
                    .target
                    .ok_or_else(|| ctx.throw_type_error("proxy is revoked"))?;
            }
            _ => return Ok(false),
        }
    }
}

fn units(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<TrackedVec<u16>> {
    let ValueKind::String(id) = value.decode() else {
        unreachable!()
    };
    ctx.allocate(|runtime| {
        let heap = &runtime.heap;
        let mut out = TrackedVec::new();
        out.try_reserve_exact(&heap.ledger, heap.string(id).len() as usize)?;
        for unit in heap.string_units(id) {
            out.push_reserved(unit);
        }
        Ok(out)
    })
}

fn get_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    index: u64,
) -> JsResult<Handle<'s>> {
    let key = index_key(ctx, index)?;
    let value = get_property(ctx, object, key)?;
    ctx.root(value)
}
