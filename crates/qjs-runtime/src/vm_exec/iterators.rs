use qjs_bytecode::{Instruction, Opcode, Operands};

use crate::atom::{Atom, KeyKind, PropKey};
use crate::builtins::promise_json_extras::{
    new_promise_capability, perform_promise_then, promise_resolve,
};
use crate::builtins::vm_internal::{ASYNC_CLOSE_REJECT, ASYNC_UNWRAP, SYNC_DISPOSER};
use crate::ledger::TrackedBox;
use crate::object::{IteratorData, IteratorKind, IteratorRecordData, ObjectData, ObjectFlags};
use crate::ops::{call, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::symbol::WellKnown;
use crate::value::*;
use crate::vm::CallFrame;

use super::{define_default, load, objects::get_method, replace, well_known};

fn state_key(index: u32) -> PropKey {
    PropKey::index(index).unwrap()
}

pub(super) fn iterator_op(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    inst: Instruction,
) -> JsResult<()> {
    use Opcode::*;
    let op = inst.opcode;
    let tail = ctx.runtime.vm.stack.len();
    let mut mark_done = None;
    let result = ctx.with_scope(|ctx| {
        let h = |n: usize| Handle::at(tail - n);
        let none = |n| (n, [Value::EMPTY; 3]);
        let one = |n, value| (n, [value, Value::EMPTY, Value::EMPTY]);
        let pair = |n, a, b| (n, [a, b, Value::EMPTY]);
        Ok(match op {
            ForInStart => one(1, for_in_start(ctx, h(1))?),
            ForInNext => {
                let [value, done] = for_in_next(ctx, h(1))?;
                pair(0, value, done)
            }
            ForOfStart | ForAwaitOfStart => {
                let (object, method) = get_iterator(ctx, h(1), op == ForAwaitOfStart)?;
                (
                    1,
                    [ctx.get(object), ctx.get(method), Value::catch_offset(0)],
                )
            }
            ForOfNext => {
                let Operands::Unsigned(skip) = inst.operands else {
                    unreachable!()
                };
                let pos = tail - skip as usize - 3;
                let iterator = Handle::at(pos);
                if ctx.get(iterator) == Value::UNDEFINED {
                    pair(0, Value::UNDEFINED, Value::bool(true))
                } else {
                    match iterator_next(ctx, iterator, Handle::at(pos + 1), Args::empty(), false) {
                        Ok((value, done)) => {
                            if done {
                                mark_done = Some(pos);
                            }
                            pair(0, value, Value::bool(done))
                        }
                        Err(error) => {
                            ctx.runtime.vm.stack[pos] = Value::UNDEFINED;
                            return Err(error);
                        }
                    }
                }
            }
            IteratorGetValueDone => {
                let (value, done) = result_value(ctx, h(1), true)?;
                if done
                    && tail >= frame.floor as usize + 4
                    && matches!(ctx.get(h(2)).decode(), ValueKind::CatchOffset(0))
                {
                    mark_done = Some(tail - 4);
                }
                pair(1, value, Value::bool(done))
            }
            IteratorClose => {
                ctx.runtime.vm.stack[tail - 1] = Value::UNDEFINED;
                if ctx.get(h(3)) != Value::UNDEFINED {
                    close_iterator(ctx, h(3))?;
                }
                none(3)
            }
            IteratorNext => {
                let args = Args::new(tail - 1, 1);
                let result = call::call(ctx, h(3), h(4), args)?;
                one(1, result)
            }
            IteratorCall => {
                let Operands::Unsigned(flags) = inst.operands else {
                    unreachable!()
                };
                let key = Atom::get(if flags & 1 != 0 { "throw" } else { "return" });
                if let Some(method) = get_method(ctx, h(4), key)? {
                    let args = if flags & 2 != 0 {
                        Args::empty()
                    } else {
                        Args::new(tail - 1, 1)
                    };
                    let result = call::call(ctx, method, h(4), args)?;
                    pair(1, result, Value::bool(false))
                } else {
                    one(0, Value::bool(true))
                }
            }
            UsingCheck => {
                let Operands::Unsigned(hint) = inst.operands else {
                    unreachable!()
                };
                let resource = ctx.get(h(1));
                if resource == Value::NULL || resource == Value::UNDEFINED {
                    one(0, Value::UNDEFINED)
                } else {
                    ctx.object_id(h(1))?;
                    let async_method = if hint != 0 {
                        get_method(ctx, h(1), well_known(ctx, WellKnown::AsyncDispose))?
                    } else {
                        None
                    };
                    let method = if let Some(method) = async_method {
                        method
                    } else {
                        let method = get_method(ctx, h(1), well_known(ctx, WellKnown::Dispose))?
                            .ok_or_else(|| ctx.throw_type_error("value is not disposable"))?;
                        if hint != 0 {
                            let actual = ctx.bind_function(method, h(1), Args::empty())?;
                            let wrapper = ctx.new_native(SYNC_DISPOSER, false)?;
                            ctx.bind_function(wrapper, actual, Args::empty())?
                        } else {
                            method
                        }
                    };
                    one(0, ctx.get(method))
                }
            }
            UsingDisposeInit => one(0, Value::UNINITIALIZED),
            UsingDispose | UsingDisposeAsync => {
                let Operands::Local(n) = inst.operands else {
                    unreachable!()
                };
                let pos = frame.base as usize + 2 + frame.arg_slots as usize + n as usize;
                let (resource, method) = (Handle::at(pos), Handle::at(pos + 1));
                if matches!(
                    ctx.get(resource),
                    Value::UNDEFINED | Value::NULL | Value::UNINITIALIZED
                ) {
                    if op == UsingDisposeAsync {
                        one(0, Value::UNDEFINED)
                    } else {
                        none(0)
                    }
                } else {
                    match call::call(ctx, method, resource, Args::empty()) {
                        Ok(value) if op == UsingDisposeAsync => one(0, value),
                        Ok(_) => none(0),
                        Err(Abrupt::Thrown) if op == UsingDispose => {
                            let error = ctx
                                .take_exception()
                                .map_or(Value::UNDEFINED, |(value, _)| value);
                            let error = ctx.root(error)?;
                            let value = suppress(ctx, error, h(1))?;
                            one(1, value)
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            UsingDisposeMerge => one(2, suppress(ctx, h(1), h(2))?),
            UsingDisposeEnd => {
                if ctx.get(h(1)) != Value::UNINITIALIZED {
                    return Err(ctx.throw(h(1)));
                }
                none(1)
            }
            _ => unreachable!(),
        })
    })?;
    if let Some(pos) = mark_done {
        ctx.runtime.vm.stack[pos] = Value::UNDEFINED;
    }
    let (drop, values) = result;
    let count = values
        .iter()
        .position(|v| *v == Value::EMPTY)
        .unwrap_or(values.len());
    replace(ctx, drop, &values[..count]);
    Ok(())
}

fn suppress<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    error: Handle<'s>,
    suppressed: Handle<'s>,
) -> JsResult<Value> {
    if ctx.get(suppressed) == Value::UNINITIALIZED {
        return Ok(ctx.get(error));
    }
    let constructor = ctx.root(Value::object(ctx.realm().suppressed_error_ctor))?;
    let args = ctx.args(&[error, suppressed])?;
    call::construct(ctx, constructor, args, constructor)
}

fn get_iterator<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterable: Handle<'s>,
    asynchronous: bool,
) -> JsResult<(Handle<'s>, Handle<'s>)> {
    let mut sync_fallback = false;
    let method = if asynchronous {
        match get_method(ctx, iterable, well_known(ctx, WellKnown::AsyncIterator))? {
            Some(method) => Some(method),
            None => {
                sync_fallback = true;
                get_method(ctx, iterable, well_known(ctx, WellKnown::Iterator))?
            }
        }
    } else {
        get_method(ctx, iterable, well_known(ctx, WellKnown::Iterator))?
    };
    let method = method.ok_or_else(|| ctx.throw_type_error("value is not iterable"))?;
    let object = call::call(ctx, method, iterable, Args::empty())?;
    let object = ctx.root(object)?;
    ctx.object_id(object)?;
    let next = property::get_property(ctx, object, Atom::get("next"))?;
    let next = ctx.root(next)?;
    if sync_fallback {
        return async_from_sync(ctx, object, next);
    }
    Ok((object, next))
}

fn async_from_sync<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
    next: Handle<'s>,
) -> JsResult<(Handle<'s>, Handle<'s>)> {
    let proto = ctx.realm().async_from_sync_iterator_proto;
    let record = IteratorRecordData {
        object: ctx.get(iterator),
        next: ctx.get(next),
    };
    let wrapper = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::Iterator(TrackedBox::new(
            ledger,
            IteratorData {
                kind: IteratorKind::Wrap(record),
            },
        )?))
    })?;
    let id = ctx.object_id(wrapper)?;
    property::private_define(ctx, id, super::objects::brand_key(), Value::bool(true))?;
    let next = property::get_property(ctx, wrapper, Atom::get("next"))?;
    let next = ctx.root(next)?;
    Ok((wrapper, next))
}

pub(crate) fn async_from_sync_call<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    wrapper: Handle<'s>,
    args: Args<'s>,
    kind: u16,
) -> JsResult<Value> {
    let constructor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
    let capability = new_promise_capability(ctx, constructor)?;
    let result = (|| {
        let id = ctx.object_id(wrapper)?;
        let record = match &ctx.heap().object(id).data {
            ObjectData::Iterator(data)
                if property::private_own(ctx, id, super::objects::brand_key())
                    == Some(Value::bool(true)) =>
            {
                let IteratorKind::Wrap(record) = data.kind else {
                    unreachable!()
                };
                record
            }
            _ => return Err(ctx.throw_type_error("expected an Async-from-Sync Iterator")),
        };
        let iterator = ctx.root(record.object)?;
        let method = if kind == 0 {
            Some(ctx.root(record.next)?)
        } else {
            get_method(
                ctx,
                iterator,
                Atom::get(if kind == 1 { "return" } else { "throw" }),
            )?
        };
        let Some(method) = method else {
            if kind == 2 {
                close_iterator(ctx, iterator)?;
                return Err(ctx.throw_type_error("iterator does not have a throw method"));
            }
            let value = args.get(0).map_or(Value::UNDEFINED, |arg| ctx.get(arg));
            let value = ctx.root(value)?;
            let result = iterator_result(ctx, value, true)?;
            let result = ctx.root(result)?;
            let args = ctx.args(&[result])?;
            let undefined = ctx.root(Value::UNDEFINED)?;
            call::call(ctx, capability.resolve, undefined, args)?;
            return Ok(());
        };
        let forwarded = if let Some(value) = args.get(0) {
            ctx.args(&[value])?
        } else {
            Args::empty()
        };
        let (value, done) = iterator_next(ctx, iterator, method, forwarded, true)?;
        let value = ctx.root(value)?;
        let close_on_reject = !done && kind != 1;
        let promise = match promise_resolve(ctx, constructor, value) {
            Err(Abrupt::Thrown) if close_on_reject => {
                let (error, _) = ctx.take_exception().unwrap();
                return close_original(ctx, iterator, error);
            }
            result => result?,
        };
        let done = ctx.root(Value::bool(done))?;
        let fulfilled = ctx.new_native(ASYNC_UNWRAP, false)?;
        let fulfilled = ctx.bind_function(fulfilled, done, Args::empty())?;
        let rejected = if close_on_reject {
            let rejected = ctx.new_native(ASYNC_CLOSE_REJECT, false)?;
            ctx.bind_function(rejected, iterator, Args::empty())?
        } else {
            ctx.root(Value::UNDEFINED)?
        };
        perform_promise_then(ctx, promise, fulfilled, rejected, Some(capability))
    })();
    if result == Err(Abrupt::Thrown) {
        let (error, _) = ctx.take_exception().unwrap();
        let error = ctx.root(error)?;
        let undefined = ctx.root(Value::UNDEFINED)?;
        let args = ctx.args(&[error])?;
        call::call(ctx, capability.reject, undefined, args)?;
        ctx.runtime.vm.thrown_frames.clear();
    } else {
        result?;
    }
    Ok(ctx.get(capability.promise))
}

pub(crate) fn close_original<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
    error: Value,
) -> JsResult<()> {
    let reason = ctx.root(error)?;
    if close_iterator(ctx, iterator) == Err(Abrupt::Interrupted) {
        return Err(Abrupt::Interrupted);
    }
    Err(ctx.throw(reason))
}

pub(crate) fn iterator_result<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    value: Handle<'s>,
    done: bool,
) -> JsResult<Value> {
    let object = ctx.new_object()?;
    let done = ctx.root(Value::bool(done))?;
    define_default(ctx, object, Atom::get("value"), value)?;
    define_default(ctx, object, Atom::get("done"), done)?;
    Ok(ctx.get(object))
}

fn iterator_next<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
    method: Handle<'s>,
    args: Args<'s>,
    read_done_value: bool,
) -> JsResult<(Value, bool)> {
    let result = call::call(ctx, method, iterator, args)?;
    let result = ctx.root(result)?;
    result_value(ctx, result, read_done_value)
}

fn result_value<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    result: Handle<'s>,
    read_done_value: bool,
) -> JsResult<(Value, bool)> {
    ctx.object_id(result)?;
    let done = property::get_property(ctx, result, Atom::get("done"))?;
    let done = convert::to_boolean_value(ctx.heap(), done);
    let value = if done && !read_done_value {
        Value::UNDEFINED
    } else {
        property::get_property(ctx, result, Atom::get("value"))?
    };
    Ok((value, done))
}

fn close_iterator<'s>(ctx: &mut ExecCtx<'_, 's>, iterator: Handle<'s>) -> JsResult<()> {
    if let Some(method) = get_method(ctx, iterator, Atom::get("return"))? {
        let result = call::call(ctx, method, iterator, Args::empty())?;
        if result.as_object().is_none() {
            return Err(ctx.throw_type_error("iterator return must return an object"));
        }
    }
    Ok(())
}

pub(super) fn close_exception_iterator(ctx: &mut ExecCtx<'_, '_>, index: usize) -> JsResult<()> {
    if ctx.runtime.vm.stack[index] == Value::UNDEFINED {
        return Ok(());
    }
    let original = ctx.pending_exception().unwrap_or(Value::UNDEFINED);
    let oom = ctx.is_oom();
    let shadow = std::mem::take(&mut ctx.runtime.vm.thrown_frames);
    let result = ctx.with_scope(|ctx| {
        let original = ctx.root(original)?;
        let result = close_iterator(ctx, Handle::at(index));
        if result == Err(Abrupt::Interrupted) {
            return result;
        }
        ctx.throw_value(ctx.get(original), oom);
        Ok(())
    });
    ctx.runtime.vm.thrown_frames = shadow;
    result
}

pub(super) fn append<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    array: Handle<'s>,
    mut index: u64,
    iterable: Handle<'s>,
) -> JsResult<u64> {
    let (iterator, method) = get_iterator(ctx, iterable, false)?;
    let (array, iterator, method) = (ctx.get(array), ctx.get(iterator), ctx.get(method));
    loop {
        ctx.check_interrupt()?;
        let done = ctx.with_scope(|ctx| {
            let array = ctx.root(array)?;
            let iterator = ctx.root(iterator)?;
            let method = ctx.root(method)?;
            let (value, done) = iterator_next(ctx, iterator, method, Args::empty(), false)?;
            if !done {
                let value = ctx.root(value)?;
                let key = load::index_key(ctx, index)?;
                define_default(ctx, array, key, value)?;
            }
            Ok(done)
        })?;
        if done {
            return Ok(index);
        }
        index += 1;
    }
}

fn for_in_start<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Value> {
    let proto = ctx.root(Value::object(ctx.realm().for_in_iterator_proto))?;
    let iterator = ctx.new_object_with_proto(Some(proto))?;
    let raw = ctx.get(value);
    let target = if raw == Value::NULL || raw == Value::UNDEFINED {
        ctx.root(Value::NULL)?
    } else {
        convert::to_object(ctx, value)?
    };
    let visited = ctx.new_object_with_proto(None)?;
    let index = ctx.root(Value::int32(0))?;
    let undefined = ctx.root(Value::UNDEFINED)?;
    for (i, value) in [target, visited, undefined, index].into_iter().enumerate() {
        define_default(ctx, iterator, state_key(i as u32), value)?;
    }
    Ok(ctx.get(iterator))
}

pub(crate) fn for_in_next<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
) -> JsResult<[Value; 2]> {
    let value = ctx.get(iterator);
    loop {
        let result = ctx.with_scope(|ctx| {
            let iterator = ctx.root(value)?;
            let target = property::get_property(ctx, iterator, state_key(0))?;
            if target == Value::NULL {
                return Ok(Some([Value::UNDEFINED, Value::bool(true)]));
            }
            let target = ctx.root(target)?;
            let visited = property::get_property(ctx, iterator, state_key(1))?;
            let visited = ctx.root(visited)?;
            let keys = property::get_property(ctx, iterator, state_key(2))?;
            let mut index = property::get_property(ctx, iterator, state_key(3))?
                .as_number()
                .unwrap() as u64;
            let keys = if keys == Value::UNDEFINED {
                let list = property::own_property_keys(ctx, target)?;
                let keys = ctx.new_array()?;
                for (i, &key) in list
                    .iter()
                    .filter(|k| !matches!(k.decode(), KeyKind::Symbol(_)))
                    .enumerate()
                {
                    let value = convert::key_value(ctx, key)?;
                    let n = load::index_key(ctx, i as u64)?;
                    define_default(ctx, keys, n, value)?;
                }
                property::set_property(ctx, iterator, state_key(2), keys)?;
                keys
            } else {
                ctx.root(keys)?
            };
            let len = property::get_property(ctx, keys, Atom::get("length"))?
                .as_number()
                .unwrap() as u64;
            if index < len {
                let current = load::index_key(ctx, index)?;
                let name = property::get_property(ctx, keys, current)?;
                let name = ctx.root(name)?;
                index += 1;
                let next = ctx.root(Value::number(index as f64))?;
                property::set_property(ctx, iterator, state_key(3), next)?;
                let key = convert::to_property_key(ctx, name)?;
                if !property::has_property(ctx, visited, key)?
                    && let Some(desc) = property::get_own_property_descriptor(ctx, target, key)?
                {
                    let marker = ctx.root(Value::UNDEFINED)?;
                    define_default(ctx, visited, key, marker)?;
                    if desc.enumerable == Some(true) {
                        return Ok(Some([ctx.get(name), Value::bool(false)]));
                    }
                }
            } else {
                let next =
                    property::get_prototype_of(ctx, target)?.map_or(Value::NULL, Value::object);
                let next = ctx.root(next)?;
                let zero = ctx.root(Value::int32(0))?;
                let undefined = ctx.root(Value::UNDEFINED)?;
                property::set_property(ctx, iterator, state_key(0), next)?;
                property::set_property(ctx, iterator, state_key(2), undefined)?;
                property::set_property(ctx, iterator, state_key(3), zero)?;
            }
            Ok(None)
        })?;
        if let Some(result) = result {
            return Ok(result);
        }
        ctx.check_interrupt()?;
    }
}
