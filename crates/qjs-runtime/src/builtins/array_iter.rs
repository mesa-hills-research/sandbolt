use crate::atom::{Atom, PropKey};
use crate::builtins::ARRAY_ITER_BASE;
use crate::ledger::TrackedVec;
use crate::object::{ArrayIteratorKind, ObjectData, ObjectFlags};
use crate::ops::call::{call, is_callable};
use crate::ops::convert::{to_boolean, to_length};
use crate::ops::property::{PropertyDescriptor, define_own_property, get_property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::symbol::WellKnown;
use crate::value::{Abrupt, JsResult, Value, ValueKind};

mod array;
mod iterator;
mod resource;

pub(crate) use iterator::new_array_iterator;

use crate::builtins::promise_json_extras::{perform_promise_then, promise_resolve, reject_promise};

fn await_value<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    value: Handle<'s>,
    fulfilled: NativeFnId,
    rejected: NativeFnId,
) -> JsResult<()> {
    let ctor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
    let promise = promise_resolve(ctx, ctor, value)?;
    let on_fulfilled = ctx.new_native(fulfilled, false)?;
    let on_fulfilled = ctx.bind_function(on_fulfilled, state, Args::empty())?;
    let on_rejected = ctx.new_native(rejected, false)?;
    let on_rejected = ctx.bind_function(on_rejected, state, Args::empty())?;
    perform_promise_then(ctx, promise, on_fulfilled, on_rejected, None)
}

fn reject_thrown<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    thrown: Abrupt,
) -> JsResult<()> {
    if thrown == Abrupt::Interrupted {
        return Err(thrown);
    }
    let (error, _) = ctx.take_exception().expect("throw without exception");
    let error = ctx.root(error)?;
    reject_promise(ctx, promise, error)
}

const LENGTH: PropKey = PropKey::atom(Atom::LENGTH);
const MAX_LENGTH: u64 = (1 << 53) - 1;

fn key(ctx: &mut ExecCtx<'_, '_>, name: &str) -> JsResult<PropKey> {
    ctx.intern_str(name)
}

fn symbol(ctx: &ExecCtx<'_, '_>, which: WellKnown) -> PropKey {
    let heap = ctx.heap();
    heap.symbols
        .get(heap.symbol_registry.well_known(which))
        .unwrap()
        .key()
}

fn named<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, name: &str) -> JsResult<Value> {
    let key = key(ctx, name)?;
    get_property(ctx, object, key)
}

fn index(ctx: &mut ExecCtx<'_, '_>, i: u64) -> JsResult<PropKey> {
    if let Ok(i) = u32::try_from(i)
        && let Some(key) = PropKey::index(i)
    {
        return Ok(key);
    }
    ctx.intern_str(&i.to_string())
}

fn get_index<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, i: u64) -> JsResult<Value> {
    let key = index(ctx, i)?;
    get_property(ctx, object, key)
}

fn create<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
) -> JsResult<()> {
    if !define_own_property(ctx, object, key, PropertyDescriptor::data(value))? {
        return Err(ctx.throw_type_error("Cannot create property"));
    }
    Ok(())
}

fn create_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    i: u64,
    value: Value,
) -> JsResult<()> {
    let value = ctx.root(value)?;
    let key = index(ctx, i)?;
    create(ctx, object, key, value)
}

fn append_string(
    ctx: &mut ExecCtx<'_, '_>,
    text: &mut TrackedVec<u16>,
    string: Value,
) -> JsResult<()> {
    let ValueKind::String(id) = string.decode() else {
        unreachable!()
    };
    let len = ctx.heap().string(id).len() as usize;
    if text.capacity() - text.len() < len {
        ctx.allocate(|runtime| text.try_reserve(&runtime.heap.ledger, len))?;
    }
    for unit in ctx.heap().string_units(id) {
        text.push_reserved(unit);
    }
    Ok(())
}

fn get_length<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<u64> {
    let value = get_property(ctx, object, LENGTH)?;
    let value = ctx.root(value)?;
    to_length(ctx, value)
}

fn arg<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, i: usize) -> JsResult<Handle<'s>> {
    match args.get(i) {
        Some(handle) => Ok(handle),
        None => ctx.root(Value::UNDEFINED),
    }
}

fn callable(ctx: &ExecCtx<'_, '_>, value: Value) -> bool {
    value
        .as_object()
        .is_some_and(|id| is_callable(ctx.heap(), id))
}

fn constructor(ctx: &ExecCtx<'_, '_>, value: Value) -> bool {
    value.as_object().is_some_and(|id| {
        ctx.heap()
            .object(id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
    })
}

fn get_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<Option<Handle<'s>>> {
    let method = get_property(ctx, object, key)?;
    if matches!(method, Value::NULL | Value::UNDEFINED) {
        Ok(None)
    } else if callable(ctx, method) {
        Ok(Some(ctx.root(method)?))
    } else {
        Err(ctx.throw_type_error("Property is not a method"))
    }
}

#[derive(Clone, Copy, Debug)]
struct IterRecord<'s> {
    object: Handle<'s>,
    next: Handle<'s>,
}

impl<'s> IterRecord<'s> {
    fn direct(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<Self> {
        ctx.object_id(object)?;
        let next = named(ctx, object, "next")?;
        Ok(Self {
            object,
            next: ctx.root(next)?,
        })
    }

    fn from_method(
        ctx: &mut ExecCtx<'_, 's>,
        receiver: Handle<'s>,
        method: Handle<'s>,
    ) -> JsResult<Self> {
        let object = call(ctx, method, receiver, Args::empty())?;
        let object = ctx.root(object)?;
        Self::direct(ctx, object)
    }

    fn step(ctx: &mut ExecCtx<'_, 's>, record: Self) -> JsResult<Option<Value>> {
        Self::step_mode(ctx, record, true)
    }

    fn step_mode(
        ctx: &mut ExecCtx<'_, 's>,
        record: Self,
        read_value: bool,
    ) -> JsResult<Option<Value>> {
        let object = ctx.get(record.object);
        let next = ctx.get(record.next);
        ctx.with_scope(|ctx| {
            let object = ctx.root(object)?;
            let next = ctx.root(next)?;
            let result = call(ctx, next, object, Args::empty())?;
            let result = ctx.root(result)?;
            ctx.object_id(result)?;
            let done = named(ctx, result, "done")?;
            let done = ctx.root(done)?;
            if to_boolean(ctx, done) {
                Ok(None)
            } else if read_value {
                named(ctx, result, "value").map(Some)
            } else {
                Ok(Some(Value::UNDEFINED))
            }
        })
    }

    fn close(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<()> {
        let key = key(ctx, "return")?;
        if let Some(method) = get_method(ctx, object, key)? {
            let result = call(ctx, method, object, Args::empty())?;
            if result.as_object().is_none() {
                return Err(ctx.throw_type_error("Iterator return did not return an object"));
            }
        }
        Ok(())
    }
}

fn on_error<'s, T>(
    ctx: &mut ExecCtx<'_, 's>,
    result: JsResult<T>,
    close: impl FnOnce(&mut ExecCtx<'_, 's>) -> JsResult<()>,
) -> JsResult<T> {
    if !matches!(result, Err(Abrupt::Thrown)) {
        return result;
    }
    let (value, oom) = ctx.take_exception().expect("throw without exception");
    let value = ctx.root(value)?;
    match close(ctx) {
        Err(Abrupt::Interrupted) => Err(Abrupt::Interrupted),
        _ => Err(ctx.throw_value(ctx.get(value), oom)),
    }
}

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    iterator::install(ctx)?;
    array::install(ctx)?;
    resource::install(ctx)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id.0 - ARRAY_ITER_BASE {
        n @ 0..=127 => array::dispatch(n, ctx, this, args),
        n @ 128..=255 => iterator::dispatch(n - 128, ctx, this, args),
        n @ 256..=383 => resource::dispatch(n - 256, ctx, this, args),
        _ => Err(ctx.throw_type_error("Unknown array or iterator builtin")),
    }
}

#[cfg(test)]
mod tests;
