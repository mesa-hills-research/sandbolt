use super::{callable, get, wk};
use crate::atom::{Atom, PropKey};
use crate::builtins::define_data;
use crate::ops::{call, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::{Abrupt, JsResult, Value};

#[derive(Clone, Copy)]
pub(super) struct IteratorRecord<'s> {
    iterator: Handle<'s>,
    next: Handle<'s>,
}

impl<'s> IteratorRecord<'s> {
    pub(super) fn from_iterable(ctx: &mut ExecCtx<'_, 's>, iterable: Handle<'s>) -> JsResult<Self> {
        let object = convert::to_object(ctx, iterable)?;
        let key = wk(ctx, WellKnown::Iterator);
        let method = property::get_property_with_receiver(ctx, object, key, iterable)?;
        let method = ctx.root(method)?;
        Self::from_method(ctx, iterable, method)
    }

    pub(super) fn from_method(
        ctx: &mut ExecCtx<'_, 's>,
        receiver: Handle<'s>,
        method: Handle<'s>,
    ) -> JsResult<Self> {
        let iterator = call::call(ctx, method, receiver, Args::empty())?;
        let iterator = ctx.root(iterator)?;
        let next = get(ctx, iterator, Atom::get("next"))?;
        Ok(Self { iterator, next })
    }

    pub(super) fn with_scope<T>(
        self,
        ctx: &mut ExecCtx<'_, 's>,
        visit: impl for<'a> FnOnce(&mut ExecCtx<'_, 'a>, IteratorRecord<'a>) -> JsResult<T>,
    ) -> JsResult<T> {
        let iterator = ctx.get(self.iterator);
        let next = ctx.get(self.next);
        ctx.with_scope(|ctx| {
            let iterator = ctx.root(iterator)?;
            let next = ctx.root(next)?;
            visit(ctx, IteratorRecord { iterator, next })
        })
    }

    pub(super) fn step(self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Option<Handle<'s>>> {
        let result = call::call(ctx, self.next, self.iterator, Args::empty())?;
        let result = ctx.root(result)?;
        let done = get(ctx, result, Atom::get("done"))?;
        if convert::to_boolean(ctx, done) {
            Ok(None)
        } else {
            get(ctx, result, Atom::get("value")).map(Some)
        }
    }

    pub(super) fn close(self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<()> {
        let method = get(ctx, self.iterator, Atom::get("return"))?;
        if nullish(ctx.get(method)) {
            return Ok(());
        }
        let result = call::call(ctx, method, self.iterator, Args::empty())?;
        if result.as_object().is_none() {
            return Err(ctx.throw_type_error("iterator return must return an object"));
        }
        Ok(())
    }

    pub(super) fn close_on_throw<T>(
        self,
        ctx: &mut ExecCtx<'_, 's>,
        result: JsResult<T>,
    ) -> JsResult<T> {
        if matches!(result, Err(Abrupt::Thrown)) {
            let exception = ctx.pending_exception().unwrap();
            let oom = ctx.is_oom();
            if let Ok(exception_root) = ctx.root(exception) {
                ctx.take_exception();
                if self.close(ctx) == Err(Abrupt::Interrupted) {
                    return Err(Abrupt::Interrupted);
                }
                return Err(ctx.throw_value(ctx.get(exception_root), oom));
            }
            return Err(ctx.throw_value(exception, oom));
        }
        result
    }
}

pub(super) fn populate<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    collection: Handle<'s>,
    items: Handle<'s>,
    is_set: bool,
) -> JsResult<()> {
    if nullish(ctx.get(items)) {
        return Ok(());
    }
    let adder = get(
        ctx,
        collection,
        Atom::get(if is_set { "add" } else { "set" }),
    )?;
    callable(ctx, adder)?;
    let iter = IteratorRecord::from_iterable(ctx, items)?;
    let collection = ctx.get(collection);
    let adder = ctx.get(adder);
    while iter.with_scope(ctx, |ctx, iter| {
        let Some(item) = iter.step(ctx)? else {
            return Ok(false);
        };
        let consume = (|| {
            let adder = ctx.root(adder)?;
            let collection = ctx.root(collection)?;
            let args = if is_set {
                ctx.args(&[item])?
            } else {
                let key = get(ctx, item, PropKey::index(0).unwrap())?;
                let value = get(ctx, item, PropKey::index(1).unwrap())?;
                ctx.args(&[key, value])?
            };
            call::call(ctx, adder, collection, args)
        })();
        iter.close_on_throw(ctx, consume)?;
        Ok(true)
    })? {}
    Ok(())
}

pub(super) fn nullish(value: Value) -> bool {
    matches!(value, Value::NULL | Value::UNDEFINED)
}

pub(super) fn array_push<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    array: Handle<'s>,
    index: u32,
    value: Handle<'s>,
) -> JsResult<()> {
    let key = match PropKey::index(index) {
        Some(key) => key,
        None => {
            let index = ctx.root(Value::number(index.into()))?;
            convert::to_property_key(ctx, index)?
        }
    };
    define_data(ctx, array, key, value, PropFlags::DEFAULT_DATA)
}

pub(super) fn iter_result(ctx: &mut ExecCtx<'_, '_>, value: Value, done: bool) -> JsResult<Value> {
    let value = ctx.root(value)?;
    let done = ctx.root(Value::bool(done))?;
    let result = ctx.new_object()?;
    define_data(
        ctx,
        result,
        Atom::get("value"),
        value,
        PropFlags::DEFAULT_DATA,
    )?;
    define_data(
        ctx,
        result,
        Atom::get("done"),
        done,
        PropFlags::DEFAULT_DATA,
    )?;
    Ok(ctx.get(result))
}
