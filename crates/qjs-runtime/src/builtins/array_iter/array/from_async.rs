use super::*;
use crate::builtins::promise_json_extras::new_promise_capability;
use crate::object::{
    FromAsyncData, FromAsyncPhase, IteratorData, IteratorKind, IteratorRecordData,
};

pub(super) fn start<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    constructor: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let promise_ctor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
    let capability = new_promise_capability(ctx, promise_ctor)?;
    let promise = ctx.get(capability.promise);
    let resolve = ctx.get(capability.resolve);
    let status = (|| {
        let source = arg(ctx, args, 0)?;
        let mapper = arg(ctx, args, 1)?;
        let this_arg = arg(ctx, args, 2)?;
        let mapper = ctx.get(mapper);
        let this_arg = ctx.get(this_arg);
        if mapper != Value::UNDEFINED && !callable(ctx, mapper) {
            return Err(ctx.throw_type_error("Array.fromAsync mapper is not callable"));
        }
        let source_object = to_object(ctx, source)?;
        let async_method = get_method_v(
            ctx,
            source_object,
            source,
            symbol(ctx, WellKnown::AsyncIterator),
        )?;
        let sync = async_method.is_none();
        let method = if async_method.is_some() {
            async_method
        } else {
            get_method_v(ctx, source_object, source, symbol(ctx, WellKnown::Iterator))?
        };
        let (iterator, length) = if let Some(method) = method {
            let record = IterRecord::from_method(ctx, source, method)?;
            (
                Some(IteratorRecordData {
                    object: ctx.get(record.object),
                    next: ctx.get(record.next),
                }),
                None,
            )
        } else {
            (None, Some(get_length(ctx, source_object)?))
        };
        let output = construct_result(ctx, constructor, length)?;
        let result = ctx.get(output);
        let source = ctx.get(source_object);
        let state = ctx.make_object(None, ObjectFlags::default(), |ledger| {
            Ok(ObjectData::Iterator(TrackedBox::new(
                ledger,
                IteratorData {
                    kind: IteratorKind::FromAsync(FromAsyncData {
                        promise,
                        resolve,
                        source,
                        result,
                        mapper,
                        this_arg,
                        iterator,
                        sync,
                        length: length.unwrap_or(0),
                        index: 0,
                        phase: FromAsyncPhase::Next,
                        saved: Value::UNDEFINED,
                    }),
                },
            )?))
        })?;
        advance(ctx, state, Value::UNDEFINED, false)
    })();
    if let Err(error) = status {
        reject_thrown(ctx, capability.promise, error)?;
    }
    Ok(promise)
}

fn state_data<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> &'a FromAsyncData {
    let ObjectData::Iterator(data) = &ctx.heap().object(id).data else {
        unreachable!()
    };
    let IteratorKind::FromAsync(state) = &data.kind else {
        unreachable!()
    };
    state
}

fn state_mut<'a>(ctx: &'a mut ExecCtx<'_, '_>, id: ObjId) -> &'a mut FromAsyncData {
    let ObjectData::Iterator(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        unreachable!()
    };
    let IteratorKind::FromAsync(state) = &mut data.kind else {
        unreachable!()
    };
    state
}

pub(super) fn resume<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    args: Args<'s>,
    rejected: bool,
) -> JsResult<Value> {
    let input = arg(ctx, args, 0)?;
    advance(ctx, state, ctx.get(input), rejected)?;
    Ok(Value::UNDEFINED)
}

fn advance<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    input: Value,
    rejected: bool,
) -> JsResult<()> {
    let state_id = ctx.object_id(state)?;
    let phase = state_data(ctx, state_id).phase;
    let status = if matches!(phase, FromAsyncPhase::Close) {
        return complete_close(ctx, state_id);
    } else if rejected {
        Err(ctx.throw_value(input, false))
    } else {
        (|| {
            ctx.check_interrupt()?;
            match phase {
                FromAsyncPhase::Next => next(ctx, state, state_id),
                FromAsyncPhase::AsyncResult => {
                    let input = ctx.root(input)?;
                    let (done, value) = unpack(ctx, input, false)?;
                    if done {
                        finish(ctx, state_id)
                    } else {
                        mapped(ctx, state, state_id, value)
                    }
                }
                FromAsyncPhase::SyncValue { done } => {
                    state_mut(ctx, state_id).saved = input;
                    suspend(
                        ctx,
                        state,
                        state_id,
                        Value::UNDEFINED,
                        FromAsyncPhase::SyncResult { done },
                    )
                }
                FromAsyncPhase::SyncResult { done } => {
                    let value = state_data(ctx, state_id).saved;
                    if done {
                        finish(ctx, state_id)
                    } else {
                        mapped(ctx, state, state_id, value)
                    }
                }
                FromAsyncPhase::MapValue => mapped(ctx, state, state_id, input),
                FromAsyncPhase::Mapped => {
                    write_item(ctx, state_id, input)?;
                    next(ctx, state, state_id)
                }
                FromAsyncPhase::Close => unreachable!(),
            }
        })()
    };
    match status {
        Err(Abrupt::Thrown) => fail(ctx, state, state_id),
        _ => status,
    }
}

fn next<'s>(ctx: &mut ExecCtx<'_, 's>, state: Handle<'s>, id: ObjId) -> JsResult<()> {
    let data = state_data(ctx, id);
    let (index, length, source, iterator, sync) = (
        data.index,
        data.length,
        data.source,
        data.iterator,
        data.sync,
    );
    if let Some(iterator) = iterator {
        if index == MAX_LENGTH {
            return Err(ctx.throw_type_error("Array exceeds maximum length"));
        }
        let obj = ctx.root(iterator.object)?;
        let next = ctx.root(iterator.next)?;
        let value = call(ctx, next, obj, Args::empty())?;
        if sync {
            let value = ctx.root(value)?;
            let (done, value) = unpack(ctx, value, true)?;
            suspend(ctx, state, id, value, FromAsyncPhase::SyncValue { done })
        } else {
            suspend(ctx, state, id, value, FromAsyncPhase::AsyncResult)
        }
    } else if index == length {
        finish(ctx, id)
    } else {
        let source = ctx.root(source)?;
        let value = get_index(ctx, source, index)?;
        suspend(ctx, state, id, value, FromAsyncPhase::MapValue)
    }
}

fn unpack<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    result: Handle<'s>,
    read_when_done: bool,
) -> JsResult<(bool, Value)> {
    ctx.object_id(result)?;
    let done = named(ctx, result, "done")?;
    let done = ctx.root(done)?;
    let done = to_boolean(ctx, done);
    let value = if !done || read_when_done {
        named(ctx, result, "value")?
    } else {
        Value::UNDEFINED
    };
    Ok((done, value))
}

fn mapped<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    id: ObjId,
    value: Value,
) -> JsResult<()> {
    let data = state_data(ctx, id);
    let (mapper, index, this) = (data.mapper, data.index, data.this_arg);
    if mapper == Value::UNDEFINED {
        write_item(ctx, id, value)?;
        return next(ctx, state, id);
    }
    let value = ctx.root(value)?;
    let mapper = ctx.root(mapper)?;
    let this = ctx.root(this)?;
    let index = ctx.root(Value::number(index as f64))?;
    let args = ctx.args(&[value, index])?;
    let mapped = call(ctx, mapper, this, args)?;
    suspend(ctx, state, id, mapped, FromAsyncPhase::Mapped)
}

fn write_item(ctx: &mut ExecCtx<'_, '_>, id: ObjId, value: Value) -> JsResult<()> {
    let value = ctx.root(value)?;
    let data = state_data(ctx, id);
    let (result, index) = (data.result, data.index);
    let result = ctx.root(result)?;
    create_index(ctx, result, index, ctx.get(value))?;
    state_mut(ctx, id).index += 1;
    Ok(())
}

fn suspend<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    id_: ObjId,
    value: Value,
    phase: FromAsyncPhase,
) -> JsResult<()> {
    let value = ctx.root(value)?;
    state_mut(ctx, id_).phase = phase;
    await_value(ctx, state, value, id(ASYNC_CONTINUE), id(ASYNC_REJECTED))
}

fn finish(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<()> {
    let data = state_mut(ctx, id);
    data.iterator = None;
    let (result, index, resolve) = (data.result, data.index, data.resolve);
    let result = ctx.root(result)?;
    set_len(ctx, result, index)?;
    let resolve = ctx.root(resolve)?;
    let this = ctx.root(Value::UNDEFINED)?;
    let args = ctx.args(&[result])?;
    call(ctx, resolve, this, args)?;
    Ok(())
}

fn fail<'s>(ctx: &mut ExecCtx<'_, 's>, state: Handle<'s>, id: ObjId) -> JsResult<()> {
    let (error, _) = ctx.take_exception().expect("throw without exception");
    let data = state_mut(ctx, id);
    data.saved = error;
    let (record, sync) = (data.iterator, data.sync);
    if let Some(iterator) = record {
        let closing = (|| {
            let iterator = ctx.root(iterator.object)?;
            let key = key(ctx, "return")?;
            let Some(method) = get_method(ctx, iterator, key)? else {
                return Ok(false);
            };
            let result = call(ctx, method, iterator, Args::empty())?;
            let value = if sync {
                let result = ctx.root(result)?;
                unpack(ctx, result, true)?.1
            } else {
                result
            };
            suspend(ctx, state, id, value, FromAsyncPhase::Close)?;
            Ok(true)
        })();
        match closing {
            Ok(true) => return Ok(()),
            Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
            Err(Abrupt::Thrown) => {
                ctx.take_exception();
            }
            Ok(false) => {}
        }
    }
    complete_close(ctx, id)
}

fn complete_close(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<()> {
    let data = state_data(ctx, id);
    let (promise, error) = (data.promise, data.saved);
    let promise = ctx.root(promise)?;
    let error = ctx.root(error)?;
    reject_promise(ctx, promise, error)
}
