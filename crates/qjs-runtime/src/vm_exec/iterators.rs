use qjs_bytecode::{Instruction, Opcode, Operands};

use crate::atom::{Atom, INLINE_INDEX_LIMIT, KeyKind, PropKey};
use crate::builtins::promise_json_extras::{
    new_promise_capability, perform_promise_then, promise_resolve,
};
use crate::builtins::vm_internal::{ASYNC_CLOSE_REJECT, ASYNC_UNWRAP, SYNC_DISPOSER};
use crate::heap::Heap;
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{
    ARRAY_ITERATOR_NEXT, ArrayIteratorKind, DenseForIn, ForInIteratorData, IteratorData,
    IteratorKind, IteratorRecordData, ObjectData, ObjectFlags,
};
use crate::ops::{call, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::*;
use crate::vm::CallFrame;

use super::{define_default, load, objects::get_method, replace, well_known};

pub(super) fn iterator_op(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    inst: Instruction,
) -> JsResult<()> {
    use Opcode::*;
    let op = inst.opcode;
    let tail = ctx.runtime.vm.stack.len();
    if op == ForOfNext && ctx.runtime.vm.interrupt_handler.is_none() {
        let Operands::Unsigned(skip) = inst.operands else {
            unreachable!()
        };
        let pos = tail - skip as usize - 3;
        let iterator = ctx.runtime.vm.stack[pos];
        let method = ctx.runtime.vm.stack[pos + 1];
        if let Some((value, done)) = dense_array_next(ctx, iterator, method) {
            if done {
                ctx.runtime.vm.stack[pos] = Value::UNDEFINED;
            }
            replace(ctx, 0, &[value, Value::bool(done)]);
            return Ok(());
        }
    }
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

fn dense_array_next(
    ctx: &mut ExecCtx<'_, '_>,
    iterator: Value,
    method: Value,
) -> Option<(Value, bool)> {
    if iterator == Value::UNDEFINED {
        return Some((Value::UNDEFINED, true));
    }
    let id = iterator.as_object()?;
    let heap = ctx.heap();
    let ObjectData::Iterator(data) = &heap.object(id).data else {
        return None;
    };
    let IteratorKind::Array {
        kind: ArrayIteratorKind::Value,
        target,
        index,
    } = data.kind
    else {
        return None;
    };
    let ObjectData::NativeFunction(next) = &heap.object(method.as_object()?).data else {
        return None;
    };
    if next.function != ARRAY_ITERATOR_NEXT {
        return None;
    }
    let (value, done) = if target == Value::UNDEFINED {
        return Some((Value::UNDEFINED, true));
    } else {
        let object = heap.object(target.as_object().unwrap());
        let ObjectData::Array(array) = &object.data else {
            return None;
        };
        if !object.flags.contains(ObjectFlags::FAST_ARRAY) {
            return None;
        }
        if index >= u64::from(array.length) {
            (Value::UNDEFINED, true)
        } else {
            // An own dense element cannot invoke an inherited getter.
            let value = *array.elements.get(index as usize)?;
            if value == Value::EMPTY {
                return None;
            }
            (value, false)
        }
    };
    let ObjectData::Iterator(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        unreachable!()
    };
    let IteratorKind::Array { target, index, .. } = &mut data.kind else {
        unreachable!()
    };
    if done {
        *target = Value::UNDEFINED;
    } else {
        *index += 1;
    }
    Some((value, done))
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
    let raw = ctx.get(value);
    let target = if matches!(raw, Value::NULL | Value::UNDEFINED) {
        None
    } else {
        let target = convert::to_object(ctx, value)?;
        ctx.get(target).as_object()
    };
    let proto = ctx.realm().for_in_iterator_proto;
    let iterator = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        let mut state = ForInIteratorData::default();
        state.target = target;
        Ok(ObjectData::ForInIterator(TrackedBox::new(ledger, state)?))
    })?;
    Ok(ctx.get(iterator))
}

fn for_in_data(heap: &Heap, iterator: ObjId) -> &ForInIteratorData {
    let ObjectData::ForInIterator(data) = &heap.object(iterator).data else {
        unreachable!()
    };
    data
}

fn for_in_data_mut(heap: &mut Heap, iterator: ObjId) -> &mut ForInIteratorData {
    let ObjectData::ForInIterator(data) = &mut heap.objects.get_mut(iterator).unwrap().data else {
        unreachable!()
    };
    data
}

fn dense_for_in(heap: &Heap, target: ObjId) -> Option<DenseForIn> {
    let object = heap.object(target);
    let ObjectData::Array(array) = &object.data else {
        return None;
    };
    if object.flags.contains(ObjectFlags::FAST_ARRAY)
        && array.elements.len() == array.length as usize
        && array.length <= INLINE_INDEX_LIMIT
        && heap.shape(object.shape).props.is_empty()
        && !array.elements.contains(&Value::EMPTY)
    {
        Some(DenseForIn {
            length: array.length,
            active: true,
            absent: TrackedVec::new(),
        })
    } else {
        None
    }
}

fn might_enumerate(heap: &Heap, mut target: Option<ObjId>) -> bool {
    while let Some(id) = target {
        let object = heap.object(id);
        match &object.data {
            ObjectData::Ordinary => (),
            ObjectData::Array(array) if array.elements.is_empty() => (),
            _ => return true,
        }
        let shape = heap.shape(object.shape);
        if shape
            .props
            .iter()
            .any(|prop| prop.flags.contains(PropFlags::ENUMERABLE))
        {
            return true;
        }
        target = shape.proto;
    }
    false
}

pub(crate) fn for_in_next<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
) -> JsResult<[Value; 2]> {
    let iterator = ctx.object_id(iterator)?;
    if !matches!(
        ctx.heap().object(iterator).data,
        ObjectData::ForInIterator(_)
    ) {
        return Err(ctx.throw_type_error("expected a for-in iterator"));
    }
    loop {
        let data = for_in_data(ctx.heap(), iterator);
        let Some(target) = data.target else {
            return Ok([Value::UNDEFINED, Value::bool(true)]);
        };
        let index = data.index;
        if let Some(dense) = data.dense.as_ref().filter(|dense| dense.active) {
            if index < dense.length as usize {
                let key = PropKey::index(index as u32).unwrap();
                for_in_data_mut(&mut ctx.runtime.heap, iterator).index += 1;
                let target_object = ctx.heap().object(target);
                let (exists, enumerable) = if target_object.flags.contains(ObjectFlags::FAST_ARRAY)
                {
                    let ObjectData::Array(array) = &target_object.data else {
                        unreachable!()
                    };
                    let present = array
                        .elements
                        .get(index)
                        .is_some_and(|&v| v != Value::EMPTY);
                    (present, present)
                } else {
                    ctx.with_scope(|ctx| {
                        let target = ctx.root(Value::object(target))?;
                        let desc = property::get_own_property_descriptor(ctx, target, key)?;
                        Ok((
                            desc.is_some(),
                            desc.is_some_and(|desc| desc.enumerable == Some(true)),
                        ))
                    })?
                };
                if enumerable {
                    let name = convert::key_value(ctx, key)?;
                    return Ok([ctx.get(name), Value::bool(false)]);
                }
                if !exists {
                    ctx.allocate(|rt| {
                        let heap = &mut rt.heap;
                        let ledger = heap.ledger.clone();
                        for_in_data_mut(heap, iterator)
                            .dense
                            .as_mut()
                            .unwrap()
                            .absent
                            .push(&ledger, index as u32)
                    })?;
                }
            } else {
                let heap = ctx.heap();
                let target = heap.shape(heap.object(target).shape).proto;
                let next = target.filter(|_| might_enumerate(heap, target));
                let data = for_in_data_mut(&mut ctx.runtime.heap, iterator);
                data.dense.as_mut().unwrap().active = false;
                data.target = next;
                data.index = 0;
            }
        } else {
            if data.keys.is_none() {
                if data.dense.is_none()
                    && let Some(dense) = dense_for_in(ctx.heap(), target)
                {
                    for_in_data_mut(&mut ctx.runtime.heap, iterator).dense = Some(dense);
                    continue;
                }
                ctx.with_scope(|ctx| {
                    let target = ctx.root(Value::object(target))?;
                    let mut keys = property::own_property_keys(ctx, target)?;
                    keys.retain(|key| !matches!(key.decode(), KeyKind::Symbol(_)));
                    for_in_data_mut(&mut ctx.runtime.heap, iterator).keys = Some(keys);
                    Ok(())
                })?;
            }
            if let Some(&key) = for_in_data(ctx.heap(), iterator)
                .keys
                .as_ref()
                .unwrap()
                .get(index)
            {
                let data = for_in_data_mut(&mut ctx.runtime.heap, iterator);
                data.index += 1;
                if !data.has_seen(key) {
                    let enumerable = ctx.with_scope(|ctx| {
                        let target = ctx.root(Value::object(target))?;
                        let Some(desc) = property::get_own_property_descriptor(ctx, target, key)?
                        else {
                            return Ok(false);
                        };
                        ctx.allocate(|rt| {
                            let heap = &mut rt.heap;
                            let ledger = heap.ledger.clone();
                            for_in_data_mut(heap, iterator).visit(&ledger, key)
                        })?;
                        Ok(desc.enumerable == Some(true))
                    })?;
                    if enumerable {
                        let name = convert::key_value(ctx, key)?;
                        return Ok([ctx.get(name), Value::bool(false)]);
                    }
                }
            } else {
                ctx.with_scope(|ctx| {
                    let target = ctx.root(Value::object(target))?;
                    let proto = property::get_prototype_of(ctx, target)?;
                    let data = for_in_data_mut(&mut ctx.runtime.heap, iterator);
                    data.target = proto;
                    data.index = 0;
                    data.keys = None;
                    Ok(())
                })?;
            }
        }
        ctx.check_interrupt()?;
    }
}

#[cfg(test)]
mod tests {
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

    fn run(script: &str) -> String {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        engine
            .eval_to_string(script, EvalOptions::default())
            .unwrap()
    }

    #[test]
    fn for_of_keeps_captured_next_and_reads_inherited_holes() {
        assert_eq!(
            run(r#"
            const proto = Object.getPrototypeOf([][Symbol.iterator]());
            const next = proto.next;
            let seen = '';
            for (const value of [1, 2]) {
                seen += value;
                proto.next = function () { return { done: true }; };
            }
            for (const value of [3]) seen += value;
            proto.next = next;
            const parent = Object.create(Array.prototype, { 1: { get() { return 8; } } });
            const holes = [0, , 2];
            Object.setPrototypeOf(holes, parent);
            for (const value of holes) seen += value;
            const a = [4, 5];
            for (const value of a) { seen += value; if (value === 4) a.push(6); }
            seen;
        "#),
            "12082456"
        );
    }

    #[test]
    fn for_in_checks_deletions_shadowing_and_proxy_descriptors() {
        assert_eq!(
            run(r#"
            let dense = [0, 1, 2];
            const own = [];
            for (const key in dense) {
                own.push(key);
                if (key === '0') { delete dense[1]; dense[3] = 3; }
            }
            const parent = { hidden: 0, inherited: 1 };
            const sparse = [, 1];
            Object.setPrototypeOf(sparse, parent);
            Object.defineProperty(sparse, 'hidden', { value: 2 });
            const keys = [];
            for (const key in sparse) keys.push(key);
            const calls = [];
            const proxy = new Proxy({ a: 1, b: 2 }, {
                ownKeys() { calls.push('keys'); return ['b', 'a']; },
                getOwnPropertyDescriptor(target, key) {
                    calls.push(key);
                    return Object.getOwnPropertyDescriptor(target, key);
                },
                getPrototypeOf() { calls.push('proto'); return null; },
            });
            const order = [];
            for (const key in proxy) order.push(key);
            [own.join(''), keys.join(','), order.join(''), calls.join(',')].join('|');
        "#),
            "02|1,inherited|ba|keys,b,a,proto"
        );
    }
}
