use crate::atom::{KeyKind, PropKey};
use crate::builtins::{CORE_BASE, define_data};
use crate::ops::{call, convert, property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::{JsResult, Value};

mod errors;
mod function;
mod object;
mod proxy;
mod reflect;
#[cfg(test)]
mod tests;

fn method_flags() -> PropFlags {
    PropFlags::WRITABLE | PropFlags::CONFIGURABLE
}
const ZERO: PropKey = PropKey::index(0).unwrap();
const ONE: PropKey = PropKey::index(1).unwrap();

const fn id(offset: u16) -> NativeFnId {
    NativeFnId(CORE_BASE + offset)
}

fn symbol_key(ctx: &ExecCtx<'_, '_>, symbol: WellKnown) -> PropKey {
    let heap = ctx.heap();
    let symbol = heap.symbol_registry.well_known(symbol);
    heap.symbols.get(symbol).unwrap().key()
}

fn methods<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    table: &[(u16, &str, u32)],
) -> JsResult<()> {
    for &(offset, name, length) in table {
        let key = ctx.intern_str(name)?;
        super::install_method(ctx, target, key, id(offset), name, length)?;
    }
    Ok(())
}

fn define_global<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str, value: Handle<'s>) -> JsResult<()> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let key = ctx.intern_str(name)?;
    define_data(ctx, global, key, value, method_flags())
}

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    ctx.with_scope(object::install)?;
    ctx.with_scope(function::install)?;
    ctx.with_scope(errors::install)?;
    ctx.with_scope(reflect::install)?;
    ctx.with_scope(proxy::install)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id.0 - CORE_BASE {
        op @ 0x00..=0x3f => object::dispatch(op, ctx, this, args),
        op @ 0x40..=0x5f => function::dispatch(op, ctx, this, args),
        op @ 0x60..=0x7f => errors::dispatch(op, ctx, this, args),
        op @ 0x80..=0x9f => reflect::dispatch(op, ctx, args),
        op @ 0xa0..=0xbf => proxy::dispatch(op, ctx, this, args),
        _ => Err(ctx.throw_type_error("unknown core builtin")),
    }
}

fn arg<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, i: usize) -> JsResult<Handle<'s>> {
    match args.get(i) {
        Some(arg) => Ok(arg),
        None => ctx.root(Value::UNDEFINED),
    }
}

fn require_callable(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
    if value
        .as_object()
        .is_some_and(|id| call::is_callable(ctx.heap(), id))
    {
        Ok(())
    } else {
        Err(ctx.throw_type_error("expected a callable function"))
    }
}

fn require_coercible(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
    if matches!(value, Value::NULL | Value::UNDEFINED) {
        Err(ctx.throw_type_error("cannot convert null or undefined to object"))
    } else {
        Ok(())
    }
}

fn prototype_arg<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Option<Handle<'s>>> {
    if ctx.get(value) == Value::NULL {
        return Ok(None);
    }
    ctx.object_id(value)?;
    Ok(Some(value))
}

fn index_key(ctx: &mut ExecCtx<'_, '_>, i: u64) -> JsResult<PropKey> {
    if let Some(key) = u32::try_from(i).ok().and_then(PropKey::index) {
        return Ok(key);
    }
    let number = ctx.root(Value::number(i as f64))?;
    convert::to_property_key(ctx, number)
}

fn put<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
) -> JsResult<()> {
    define_or_throw(ctx, object, key, property::PropertyDescriptor::data(value))
}

fn define_or_throw<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    desc: property::PropertyDescriptor<Handle<'s>>,
) -> JsResult<()> {
    if !property::define_own_property(ctx, object, key, desc)? {
        return Err(ctx.throw_type_error("cannot define property"));
    }
    Ok(())
}

fn set_or_throw<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
) -> JsResult<()> {
    if !property::set_property(ctx, object, key, value)? {
        return Err(ctx.throw_type_error("cannot set property"));
    }
    Ok(())
}

// The snapshot survives getters or traps deleting keys from the original object.
fn root_keys(ctx: &mut ExecCtx<'_, '_>, keys: &[PropKey]) -> JsResult<()> {
    let dynamic = keys
        .iter()
        .filter(|&&key| ctx.heap().dynamic_key_value(key).is_some())
        .count();
    ctx.reserve_stack(dynamic)?;
    for &key in keys {
        if let Some(value) = ctx.heap().dynamic_key_value(key) {
            ctx.runtime.vm.stack.push_reserved(value);
        }
    }
    Ok(())
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, key: PropKey) -> JsResult<Handle<'s>> {
    let value = property::get_property(ctx, object, key)?;
    ctx.root(value)
}

fn push_array<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    array: Handle<'s>,
    index: u64,
    value: Handle<'s>,
) -> JsResult<()> {
    let key = index_key(ctx, index)?;
    put(ctx, array, key, value)
}

fn concat<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    prefix: &str,
    middle: Handle<'s>,
    suffix: &str,
) -> JsResult<Value> {
    let prefix = ctx.new_string(prefix)?;
    let text = ctx.concat_strings(prefix, middle)?;
    let suffix = ctx.new_string(suffix)?;
    let text = ctx.concat_strings(text, suffix)?;
    Ok(ctx.get(text))
}

fn for_each_iterator<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterable: Handle<'s>,
    mut f: impl for<'a> FnMut(&mut ExecCtx<'_, 'a>, Handle<'a>, u64) -> JsResult<()>,
) -> JsResult<()> {
    let boxed = convert::to_object(ctx, iterable)?;
    let key = symbol_key(ctx, WellKnown::Iterator);
    let method = property::get_property_with_receiver(ctx, boxed, key, iterable)?;
    let method = ctx.root(method)?;
    let result = call::call(ctx, method, iterable, Args::empty())?;
    let iterator = ctx.root(result)?;
    ctx.object_id(iterator)?;
    let next = get(ctx, iterator, crate::atom::Atom::get("next"))?;
    let (iterator, next) = (ctx.get(iterator), ctx.get(next));
    for index in 0.. {
        let done = ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let iterator = ctx.root(iterator)?;
            let next = ctx.root(next)?;
            let result = call::call(ctx, next, iterator, Args::empty())?;
            let result = ctx.root(result)?;
            ctx.object_id(result)?;
            let done = get(ctx, result, crate::atom::Atom::get("done"))?;
            if convert::to_boolean(ctx, done) {
                return Ok(true);
            }
            let value = get(ctx, result, crate::atom::Atom::get("value"))?;
            if let Err(error) = f(ctx, value, index) {
                return Err(close_iterator_on_throw(ctx, iterator, error));
            }
            Ok(false)
        })?;
        if done {
            break;
        }
    }
    Ok(())
}

fn close_iterator_on_throw<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
    error: crate::value::Abrupt,
) -> crate::value::Abrupt {
    use crate::value::Abrupt;
    if error == Abrupt::Interrupted {
        return error;
    }
    let Some((exception, oom)) = ctx.take_exception() else {
        return error;
    };
    let close = (|| {
        ctx.root(exception)?;
        let method = get(ctx, iterator, crate::atom::Atom::get("return"))?;
        if !matches!(ctx.get(method), Value::NULL | Value::UNDEFINED) {
            call::call(ctx, method, iterator, Args::empty())?;
        }
        Ok(())
    })();
    if close == Err(Abrupt::Interrupted) {
        return Abrupt::Interrupted;
    }
    ctx.throw_value(exception, oom)
}
