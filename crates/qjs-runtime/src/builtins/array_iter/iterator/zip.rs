use super::*;
use crate::object::{ZipInput, ZipMode};
use crate::ops::convert::key_value;
use crate::ops::property::own_property_keys;

pub(super) fn create_zip<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
    keyed: bool,
) -> JsResult<Value> {
    let items = arg(ctx, args, 0)?;
    ctx.object_id(items)?;
    let options = arg(ctx, args, 1)?;
    let mode = if ctx.get(options) == Value::UNDEFINED {
        ZipMode::Shortest
    } else {
        ctx.object_id(options)?;
        let mode = named(ctx, options, "mode")?;
        match mode.decode() {
            ValueKind::Undefined => ZipMode::Shortest,
            ValueKind::String(text) => {
                let units = || ctx.heap().string_units(text);
                if units().eq("shortest".encode_utf16()) {
                    ZipMode::Shortest
                } else if units().eq("longest".encode_utf16()) {
                    ZipMode::Longest
                } else if units().eq("strict".encode_utf16()) {
                    ZipMode::Strict
                } else {
                    return Err(ctx.throw_type_error("Invalid iterator zip mode"));
                }
            }
            _ => return Err(ctx.throw_type_error("Invalid iterator zip mode")),
        }
    };
    let padding = if mode == ZipMode::Longest {
        named(ctx, options, "padding")?
    } else {
        Value::UNDEFINED
    };
    let padding = if padding != Value::UNDEFINED {
        let padding = ctx.root(padding)?;
        ctx.object_id(padding)?;
        Some(padding)
    } else {
        None
    };
    let out = new_iterator(ctx, ctx.realm().iterator_helper_proto, || {
        IteratorKind::Helper(HelperData::new(HelperOp::Zip { keyed, mode }))
    })?;
    let id = ctx.object_id(out)?;
    let status = if keyed {
        collect_keyed(ctx, id, items, padding)
    } else {
        collect_iterables(ctx, id, items).and_then(|()| add_padding(ctx, id, padding))
    };
    on_error(ctx, status, |ctx| {
        let closed = close_all(ctx, id);
        if let Some(outer) = helper(ctx, id).source {
            if closed.is_ok() {
                close_record(ctx, outer)
            } else {
                on_error(ctx, closed, |ctx| close_record(ctx, outer))
            }
        } else {
            closed
        }
    })?;
    Ok(ctx.get(out))
}

fn append<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: ObjId,
    key: Option<PropKey>,
    record: IterRecord<'s>,
) -> JsResult<()> {
    let item = ZipInput {
        key,
        record: Some(record.raw(ctx)),
        pad: Value::UNDEFINED,
    };
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::Iterator(data) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        let IteratorKind::Helper(data) = &mut data.kind else {
            unreachable!()
        };
        data.zip.push(&heap.ledger, item)
    })
}

fn iterable<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<IterRecord<'s>> {
    let Some(method) = get_method(ctx, value, symbol(ctx, WellKnown::Iterator))? else {
        return Err(ctx.throw_type_error("Object is not iterable"));
    };
    IterRecord::from_method(ctx, value, method)
}

fn collect_iterables<'s>(ctx: &mut ExecCtx<'_, 's>, id: ObjId, items: Handle<'s>) -> JsResult<()> {
    let outer = iterable(ctx, items)?;
    helper_mut(ctx, id).source = Some(outer.raw(ctx));
    loop {
        ctx.check_interrupt()?;
        let item = IterRecord::step(ctx, outer);
        match item {
            Ok(Some(value)) => ctx.with_scope(|ctx| {
                let value = ctx.root(value)?;
                let record = flattenable(ctx, value, false)?;
                append(ctx, id, None, record)
            })?,
            done => {
                helper_mut(ctx, id).source = None;
                return done.map(|_| ());
            }
        }
    }
}

fn collect_keyed<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: ObjId,
    items: Handle<'s>,
    padding: Option<Handle<'s>>,
) -> JsResult<()> {
    let keys = own_property_keys(ctx, items)?;
    let held = ctx.new_array()?;
    let roots = ctx.get(held);
    // Getters can delete keys before their turns. Hold the original key list.
    for (i, &key) in keys.iter().enumerate() {
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let held = ctx.root(roots)?;
            let key = key_value(ctx, key)?;
            create_index(ctx, held, i as u64, ctx.get(key))
        })?;
    }
    let inputs = ctx.get(items);
    for &key in keys.iter() {
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let items = ctx.root(inputs)?;
            if get_own_property_descriptor(ctx, items, key)?
                .is_some_and(|d| d.enumerable == Some(true))
            {
                let value = get_property(ctx, items, key)?;
                if value != Value::UNDEFINED {
                    let value = ctx.root(value)?;
                    let record = flattenable(ctx, value, false)?;
                    append(ctx, id, Some(key), record)?;
                }
            }
            Ok(())
        })?;
    }
    if let Some(padding) = padding {
        let padding = ctx.get(padding);
        for i in 0..helper(ctx, id).zip.len() {
            let key = helper(ctx, id).zip[i].key.unwrap();
            let pad = ctx.with_scope(|ctx| {
                ctx.check_interrupt()?;
                let padding = ctx.root(padding)?;
                get_property(ctx, padding, key)
            })?;
            helper_mut(ctx, id).zip[i].pad = pad;
        }
    }
    Ok(())
}

fn add_padding<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: ObjId,
    padding: Option<Handle<'s>>,
) -> JsResult<()> {
    let Some(padding) = padding else {
        return Ok(());
    };
    let padding = iterable(ctx, padding)?;
    let count = helper(ctx, id).zip.len();
    for i in 0..count {
        ctx.check_interrupt()?;
        let Some(value) = IterRecord::step(ctx, padding)? else {
            return Ok(());
        };
        helper_mut(ctx, id).zip[i].pad = value;
    }
    IterRecord::close(ctx, padding.object)
}

pub(super) fn close_all(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<()> {
    let mut status = Ok(());
    for i in (0..helper(ctx, id).zip.len()).rev() {
        let record = helper(ctx, id).zip[i].record;
        if let Some(record) = record {
            status = if status.is_ok() {
                close_record(ctx, record)
            } else {
                on_error(ctx, status, |ctx| close_record(ctx, record))
            };
            helper_mut(ctx, id).zip[i].record = None;
            if status == Err(Abrupt::Interrupted) {
                break;
            }
        }
    }
    status
}

pub(super) fn step(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    keyed: bool,
    mode: ZipMode,
) -> JsResult<Option<Value>> {
    let count = helper(ctx, id).zip.len();
    if count == 0 {
        return Ok(None);
    }
    let out = if keyed {
        ctx.new_object_with_proto(None)?
    } else {
        ctx.new_array()?
    };
    let output = ctx.get(out);
    let mut yielded = false;
    let mut strict_done = false;
    for i in 0..count {
        ctx.check_interrupt()?;
        let input = helper(ctx, id).zip[i];
        let mut item = input.pad;
        if let Some(record) = input.record {
            let value = ctx.with_scope(|ctx| {
                let record = record.rooted(ctx)?;
                IterRecord::step_mode(ctx, record, !strict_done)
            });
            match value {
                Err(_) => {
                    helper_mut(ctx, id).zip[i].record = None;
                    return on_error(ctx, value, |ctx| close_all(ctx, id));
                }
                Ok(None) => {
                    helper_mut(ctx, id).zip[i].record = None;
                    if mode == ZipMode::Shortest {
                        close_all(ctx, id)?;
                        return Ok(None);
                    }
                    if mode == ZipMode::Strict {
                        if i != 0 && !strict_done {
                            return mismatch(ctx, id);
                        }
                        strict_done = true;
                    }
                }
                Ok(Some(value)) => {
                    if strict_done {
                        return mismatch(ctx, id);
                    }
                    item = value;
                    yielded = true;
                }
            }
        }
        if !strict_done {
            let status = ctx.with_scope(|ctx| {
                let out = ctx.root(output)?;
                if let Some(key) = input.key {
                    let value = ctx.root(item)?;
                    create(ctx, out, key, value)
                } else {
                    create_index(ctx, out, i as u64, item)
                }
            });
            on_error(ctx, status, |ctx| close_all(ctx, id))?;
        }
    }
    Ok(yielded.then_some(output))
}

fn mismatch(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<Option<Value>> {
    let error = Err(ctx.throw_type_error("Zipped iterators have different lengths"));
    on_error(ctx, error, |ctx| close_all(ctx, id))
}
