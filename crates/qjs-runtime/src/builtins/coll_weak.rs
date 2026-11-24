use crate::atom::PropKey;
use crate::builtins::{COLL_WEAK_BASE, configure_ctor, define_data, install_method};
use crate::ops::{call, property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::{JsResult, ObjId, Value};

mod iterable;
mod ordered;
mod set_methods;
mod symbols;
mod weak;

pub(crate) use ordered::fast_map_set;

#[cfg(test)]
mod tests;

macro_rules! ids {
    ($($name:ident = $offset:expr),+ $(,)?) => {
        $(pub(crate) const $name: NativeFnId = NativeFnId(COLL_WEAK_BASE + $offset);)+
    };
}

ids! {
    MAP_CTOR = 0x00, SET_CTOR = 0x01, WEAK_MAP_CTOR = 0x02, WEAK_SET_CTOR = 0x03,
    WEAK_REF_CTOR = 0x04, REGISTRY_CTOR = 0x05, SYMBOL = 0x06,
    SPECIES = 0x10, MAP_GROUP_BY = 0x11, MAP_SET = 0x12, MAP_GET = 0x13,
    MAP_HAS = 0x14, MAP_DELETE = 0x15, MAP_CLEAR = 0x16, MAP_SIZE = 0x17,
    MAP_FOR_EACH = 0x18, MAP_KEYS = 0x19, MAP_VALUES = 0x1a, MAP_ENTRIES = 0x1b,
    MAP_NEXT = 0x1c, MAP_UPSERT = 0x1d, MAP_UPSERT_COMPUTED = 0x1e,
    SET_ADD = 0x20, SET_HAS = 0x21, SET_DELETE = 0x22, SET_CLEAR = 0x23,
    SET_SIZE = 0x24, SET_FOR_EACH = 0x25, SET_VALUES = 0x26, SET_ENTRIES = 0x27,
    SET_NEXT = 0x28, UNION = 0x29, INTERSECTION = 0x2a, DIFFERENCE = 0x2b,
    SYMMETRIC_DIFFERENCE = 0x2c, IS_SUBSET = 0x2d, IS_SUPERSET = 0x2e, IS_DISJOINT = 0x2f,
    WEAK_MAP_SET = 0x30, WEAK_MAP_GET = 0x31, WEAK_MAP_HAS = 0x32, WEAK_MAP_DELETE = 0x33,
    WEAK_MAP_UPSERT = 0x34, WEAK_MAP_UPSERT_COMPUTED = 0x35, WEAK_SET_ADD = 0x36,
    WEAK_SET_HAS = 0x37, WEAK_SET_DELETE = 0x38, WEAK_REF_DEREF = 0x39,
    REGISTER = 0x3a, UNREGISTER = 0x3b,
    SYMBOL_FOR = 0x40, SYMBOL_KEY_FOR = 0x41, SYMBOL_DESCRIPTION = 0x42,
    SYMBOL_TO_STRING = 0x43, SYMBOL_VALUE_OF = 0x44, SYMBOL_TO_PRIMITIVE = 0x45,
}

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    ctx.with_scope(ordered::install_map)?;
    ctx.with_scope(ordered::install_set)?;
    weak::install(ctx)?;
    ctx.with_scope(symbols::install)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        SPECIES => Ok(ctx.get(this)),
        MAP_CTOR | SET_CTOR => ordered::construct(ctx, args, id == SET_CTOR),
        WEAK_MAP_CTOR | WEAK_SET_CTOR | WEAK_REF_CTOR | REGISTRY_CTOR => {
            weak::construct(id, ctx, args)
        }
        MAP_GROUP_BY => ordered::group_by(ctx, args),
        MAP_NEXT | SET_NEXT => ordered::next(ctx, this, id == SET_NEXT),
        MAP_FOR_EACH | SET_FOR_EACH => ordered::for_each(ctx, this, args, id == SET_FOR_EACH),
        MAP_CLEAR | MAP_DELETE | MAP_ENTRIES | MAP_GET | MAP_HAS | MAP_KEYS | MAP_SET
        | MAP_SIZE | MAP_VALUES | MAP_UPSERT | MAP_UPSERT_COMPUTED => {
            ordered::method(id, ctx, this, args, false)
        }
        SET_ADD | SET_CLEAR | SET_DELETE | SET_ENTRIES | SET_HAS | SET_SIZE | SET_VALUES => {
            ordered::method(id, ctx, this, args, true)
        }
        UNION | INTERSECTION | DIFFERENCE | SYMMETRIC_DIFFERENCE | IS_SUBSET | IS_SUPERSET
        | IS_DISJOINT => set_methods::dispatch(id, ctx, this, args),
        WEAK_MAP_SET
        | WEAK_MAP_GET
        | WEAK_MAP_HAS
        | WEAK_MAP_DELETE
        | WEAK_MAP_UPSERT
        | WEAK_MAP_UPSERT_COMPUTED
        | WEAK_SET_ADD
        | WEAK_SET_HAS
        | WEAK_SET_DELETE
        | WEAK_REF_DEREF
        | REGISTER
        | UNREGISTER => weak::method(id, ctx, this, args),
        SYMBOL | SYMBOL_FOR | SYMBOL_KEY_FOR | SYMBOL_DESCRIPTION | SYMBOL_TO_STRING
        | SYMBOL_VALUE_OF | SYMBOL_TO_PRIMITIVE => symbols::dispatch(id, ctx, this, args),
        _ => Err(ctx.throw_type_error("unknown collection builtin")),
    }
}

fn class<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    ctor: ObjId,
    proto: ObjId,
    id: NativeFnId,
    name: &str,
    length: u32,
) -> JsResult<(Handle<'s>, Handle<'s>)> {
    let ctor = configure_ctor(ctx, ctor, id, name, length)?;
    let proto = ctx.root(Value::object(proto))?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let key = ctx.intern_str(name)?;
    define_data(ctx, global, key, ctor, methods())?;
    tag(ctx, proto, name)?;
    Ok((ctor, proto))
}

fn methods() -> PropFlags {
    PropFlags::WRITABLE | PropFlags::CONFIGURABLE
}

fn tag<'s>(ctx: &mut ExecCtx<'_, 's>, proto: Handle<'s>, name: &str) -> JsResult<()> {
    let tag = ctx.new_string(name)?;
    define_data(
        ctx,
        proto,
        wk(ctx, WellKnown::ToStringTag),
        tag,
        PropFlags::CONFIGURABLE,
    )
}

fn install_methods<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: Handle<'s>,
    methods: &[(&str, NativeFnId, u32)],
) -> JsResult<()> {
    let proto = ctx.get(proto);
    for &(name, id, length) in methods {
        ctx.with_scope(|ctx| {
            let proto = ctx.root(proto)?;
            let key = ctx.intern_str(name)?;
            install_method(ctx, proto, key, id, name, length)
        })?;
    }
    Ok(())
}

fn wk(ctx: &ExecCtx<'_, '_>, symbol: WellKnown) -> PropKey {
    let heap = ctx.heap();
    let id = heap.symbol_registry.well_known(symbol);
    heap.symbols.get(id).unwrap().key()
}

fn arg<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, index: usize) -> JsResult<Handle<'s>> {
    match args.get(index) {
        Some(arg) => Ok(arg),
        None => ctx.root(Value::UNDEFINED),
    }
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, key: PropKey) -> JsResult<Handle<'s>> {
    let value = property::get_property(ctx, object, key)?;
    ctx.root(value)
}

fn callable<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<()> {
    if ctx
        .get(value)
        .as_object()
        .is_some_and(|id| call::is_callable(ctx.heap(), id))
    {
        Ok(())
    } else {
        Err(ctx.throw_type_error("expected a function"))
    }
}

fn alias<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: Handle<'s>,
    key: PropKey,
    from: &str,
) -> JsResult<()> {
    let from = ctx.intern_str(from)?;
    let method = get(ctx, proto, from)?;
    define_data(ctx, proto, key, method, methods())
}
