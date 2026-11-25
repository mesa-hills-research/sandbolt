use super::iterable::IteratorRecord;
use super::ordered::{delete, entries, insert, lookup, new_collection, require_collection};
use super::*;
use crate::atom::Atom;
use crate::object::CollectionCursor;
use crate::ops::convert;

struct SetRecord<'s> {
    object: Handle<'s>,
    size: f64,
    has: Handle<'s>,
    keys: Handle<'s>,
}

impl<'s> SetRecord<'s> {
    fn get(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<Self> {
        ctx.object_id(object)?;
        let size = get(ctx, object, Atom::get("size"))?;
        let size = convert::to_number(ctx, size)?;
        if size.is_nan() {
            return Err(ctx.throw_type_error("set-like size is NaN"));
        }
        let size = size.trunc();
        if size < 0.0 {
            return Err(ctx.throw_range_error("set-like size is negative"));
        }
        let has = get(ctx, object, Atom::get("has"))?;
        callable(ctx, has)?;
        let keys = get(ctx, object, Atom::get("keys"))?;
        callable(ctx, keys)?;
        Ok(Self {
            object,
            size,
            has,
            keys,
        })
    }

    fn iterator(&self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<IteratorRecord<'s>> {
        IteratorRecord::from_method(ctx, self.object, self.keys)
    }
}

pub(super) fn dispatch<'s>(
    op: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let id = require_collection(ctx, this, true)?;
    let other = arg(ctx, args, 0)?;
    let other = SetRecord::get(ctx, other)?;
    let size = entries(ctx, id).len() as f64;
    match op {
        UNION | SYMMETRIC_DIFFERENCE => {
            let iterator = other.iterator(ctx)?;
            let result = new_collection(ctx, true)?;
            let result_id = ctx.object_id(result)?;
            copy(ctx, id, result_id)?;
            while iterator.with_scope(ctx, |ctx, iterator| {
                let Some(key) = iterator.step(ctx)? else {
                    return Ok(false);
                };
                let raw = ctx.get(key);
                if op == SYMMETRIC_DIFFERENCE && lookup(ctx, id, raw).is_some() {
                    delete(ctx, result_id, raw);
                } else {
                    let undefined = ctx.root(Value::UNDEFINED)?;
                    insert(ctx, result_id, key, undefined)?;
                }
                Ok(true)
            })? {}
            Ok(ctx.get(result))
        }
        INTERSECTION | DIFFERENCE => {
            let result = new_collection(ctx, true)?;
            let result_id = ctx.object_id(result)?;
            if op == DIFFERENCE {
                copy(ctx, id, result_id)?;
            }
            if size <= other.size {
                let source = if op == DIFFERENCE { result_id } else { id };
                visit_using_has(ctx, &other, source, |ctx, key, found| {
                    if found {
                        if op == INTERSECTION {
                            let undefined = ctx.root(Value::UNDEFINED)?;
                            insert(ctx, result_id, key, undefined)?;
                        } else {
                            delete(ctx, result_id, ctx.get(key));
                        }
                    }
                    Ok(true)
                })?;
            } else {
                let iterator = other.iterator(ctx)?;
                while iterator.with_scope(ctx, |ctx, iterator| {
                    let Some(key) = iterator.step(ctx)? else {
                        return Ok(false);
                    };
                    let raw = ctx.get(key);
                    if op == DIFFERENCE {
                        delete(ctx, result_id, raw);
                    } else if lookup(ctx, id, raw).is_some() {
                        let undefined = ctx.root(Value::UNDEFINED)?;
                        insert(ctx, result_id, key, undefined)?;
                    }
                    Ok(true)
                })? {}
            }
            Ok(ctx.get(result))
        }
        IS_SUBSET if size > other.size => Ok(Value::bool(false)),
        IS_SUPERSET if size < other.size => Ok(Value::bool(false)),
        IS_SUBSET | IS_DISJOINT if op == IS_SUBSET || size <= other.size => {
            let expected = op == IS_SUBSET;
            visit_using_has(ctx, &other, id, |_, _, found| Ok(found == expected)).map(Value::bool)
        }
        IS_SUPERSET | IS_DISJOINT => {
            let iterator = other.iterator(ctx)?;
            let expected = op == IS_SUPERSET;
            loop {
                let found = iterator.with_scope(ctx, |ctx, iterator| {
                    let Some(key) = iterator.step(ctx)? else {
                        return Ok(None);
                    };
                    let found = lookup(ctx, id, ctx.get(key)).is_some();
                    if found != expected {
                        iterator.close(ctx)?;
                    }
                    Ok(Some(found))
                })?;
                match found {
                    None => return Ok(Value::bool(true)),
                    Some(found) if found != expected => return Ok(Value::bool(false)),
                    Some(_) => {}
                }
            }
        }
        _ => unreachable!(),
    }
}

fn copy(ctx: &mut ExecCtx<'_, '_>, source: ObjId, result: ObjId) -> JsResult<()> {
    let mut cursor = CollectionCursor::default();
    while let Some((key, _)) = entries(ctx, source).next(&mut cursor) {
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let key = ctx.root(key)?;
            let undefined = ctx.root(Value::UNDEFINED)?;
            insert(ctx, result, key, undefined)
        })?;
    }
    Ok(())
}

fn visit_using_has<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    other: &SetRecord<'s>,
    source: ObjId,
    mut visit: impl for<'a> FnMut(&mut ExecCtx<'_, 'a>, Handle<'a>, bool) -> JsResult<bool>,
) -> JsResult<bool> {
    let object = ctx.get(other.object);
    let has = ctx.get(other.has);
    let mut cursor = CollectionCursor::default();
    while let Some((key, _)) = entries(ctx, source).next(&mut cursor) {
        if !ctx.with_scope(|ctx| {
            let key = ctx.root(key)?;
            let object = ctx.root(object)?;
            let has = ctx.root(has)?;
            let args = ctx.args(&[key])?;
            let found = call::call(ctx, has, object, args)?;
            let found = convert::to_boolean_value(ctx.heap(), found);
            visit(ctx, key, found)
        })? {
            return Ok(false);
        }
    }
    Ok(true)
}
