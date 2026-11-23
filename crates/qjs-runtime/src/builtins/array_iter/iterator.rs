use super::*;
use crate::builtins::promise_json_extras::{fulfill_promise, new_promise_capability};
use crate::builtins::typed_atomics::{typed_array_get_index, typed_array_length};
use crate::builtins::{configure_ctor, define_data, install_accessor, install_method};
use crate::ledger::TrackedBox;
use crate::object::{
    ARRAY_NEXT_OFFSET as ARRAY_NEXT, HelperData, HelperOp, HelperState, IteratorData, IteratorKind,
    IteratorRecordData,
};
use crate::ops::convert::{to_number, to_object};
use crate::ops::property::{get_own_property_descriptor, get_prototype_of, set_property};
use crate::shape::PropFlags;
use crate::value::{ObjId, ValueKind};

mod extensions;
mod zip;

const CTOR: u16 = 0;
const FROM: u16 = 1;
const CONCAT: u16 = 2;
const ZIP: u16 = 3;
const ZIP_KEYED: u16 = 4;
const ITER: u16 = 5;
const DISPOSE: u16 = 6;
const GET_TAG: u16 = 7;
const SET_TAG: u16 = 8;
const GET_CTOR: u16 = 9;
const SET_CTOR: u16 = 10;
const MAP: u16 = 11;
const FILTER: u16 = 12;
const TAKE: u16 = 13;
const DROP: u16 = 14;
const FLAT_MAP: u16 = 15;
const REDUCE: u16 = 16;
const TO_ARRAY: u16 = 17;
const FOR_EACH: u16 = 18;
const SOME: u16 = 19;
const EVERY: u16 = 20;
const FIND: u16 = 21;
const WRAP_NEXT: u16 = 23;
const WRAP_RETURN: u16 = 24;
const HELPER_NEXT: u16 = 25;
const HELPER_RETURN: u16 = 26;
const CHUNKS: u16 = 27;
const WINDOWS: u16 = 28;
const INCLUDES: u16 = 29;
const JOIN: u16 = 30;
const ASYNC_DISPOSE: u16 = 31;
const DISCARD_RESULT: u16 = 32;

const fn id(n: u16) -> NativeFnId {
    NativeFnId(ARRAY_ITER_BASE + 128 + n)
}

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = configure_ctor(ctx, ctx.realm().iterator_ctor, id(CTOR), "Iterator", 0)?;
    let proto = ctx.root(Value::object(ctx.realm().iterator_proto))?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let k = key(ctx, "Iterator")?;
    define_data(
        ctx,
        global,
        k,
        ctor,
        PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
    )?;
    for (name, op, len) in [
        ("from", FROM, 1),
        ("concat", CONCAT, 0),
        ("zip", ZIP, 1),
        ("zipKeyed", ZIP_KEYED, 1),
    ] {
        let key = key(ctx, name)?;
        install_method(ctx, ctor, key, id(op), name, len)?;
    }
    for (name, op, len) in [
        ("map", MAP, 1),
        ("filter", FILTER, 1),
        ("take", TAKE, 1),
        ("drop", DROP, 1),
        ("flatMap", FLAT_MAP, 1),
        ("reduce", REDUCE, 1),
        ("toArray", TO_ARRAY, 0),
        ("forEach", FOR_EACH, 1),
        ("some", SOME, 1),
        ("every", EVERY, 1),
        ("find", FIND, 1),
        ("chunks", CHUNKS, 1),
        ("windows", WINDOWS, 1),
        ("includes", INCLUDES, 1),
        ("join", JOIN, 1),
    ] {
        let key = key(ctx, name)?;
        install_method(ctx, proto, key, id(op), name, len)?;
    }
    install_method(
        ctx,
        proto,
        symbol(ctx, WellKnown::Iterator),
        id(ITER),
        "[Symbol.iterator]",
        0,
    )?;
    install_method(
        ctx,
        proto,
        symbol(ctx, WellKnown::Dispose),
        id(DISPOSE),
        "[Symbol.dispose]",
        0,
    )?;
    let async_proto = ctx.root(Value::object(ctx.realm().async_iterator_proto))?;
    install_method(
        ctx,
        async_proto,
        symbol(ctx, WellKnown::AsyncDispose),
        id(ASYNC_DISPOSE),
        "[Symbol.asyncDispose]",
        0,
    )?;
    let k = key(ctx, "constructor")?;
    install_accessor(
        ctx,
        proto,
        k,
        Some((id(GET_CTOR), "get constructor")),
        Some((id(SET_CTOR), "set constructor")),
        true,
    )?;
    install_accessor(
        ctx,
        proto,
        symbol(ctx, WellKnown::ToStringTag),
        Some((id(GET_TAG), "get [Symbol.toStringTag]")),
        Some((id(SET_TAG), "set [Symbol.toStringTag]")),
        true,
    )?;
    for (proto, next, ret, tag) in [
        (
            ctx.realm().array_iterator_proto,
            ARRAY_NEXT,
            None,
            Some("Array Iterator"),
        ),
        (
            ctx.realm().iterator_wrap_proto,
            WRAP_NEXT,
            Some(WRAP_RETURN),
            None,
        ),
        (
            ctx.realm().iterator_helper_proto,
            HELPER_NEXT,
            Some(HELPER_RETURN),
            Some("Iterator Helper"),
        ),
    ] {
        let proto = ctx.root(Value::object(proto))?;
        let k = key(ctx, "next")?;
        install_method(ctx, proto, k, id(next), "next", 0)?;
        if let Some(ret) = ret {
            let k = key(ctx, "return")?;
            install_method(ctx, proto, k, id(ret), "return", 0)?;
        }
        if let Some(tag) = tag {
            let tag = ctx.new_string(tag)?;
            define_data(
                ctx,
                proto,
                symbol(ctx, WellKnown::ToStringTag),
                tag,
                PropFlags::CONFIGURABLE,
            )?;
        }
    }
    Ok(())
}

pub(super) fn dispatch<'s>(
    n: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match n {
        CTOR => {
            let ctor = ctx.realm().iterator_ctor;
            let Some(target) = ctx.new_target().filter(|&target| target != ctor) else {
                return Err(ctx.throw_type_error("Iterator is an abstract constructor"));
            };
            let object = ctx.ordinary_create_from_constructor(
                target,
                |realm| realm.iterator_proto,
                ObjectFlags::EXTENSIBLE,
                |_| Ok(ObjectData::Ordinary),
            )?;
            Ok(ctx.get(object))
        }
        ITER => Ok(ctx.get(this)),
        DISPOSE => {
            let object = to_object(ctx, this)?;
            let k = key(ctx, "return")?;
            if let Some(method) = array::get_method_v(ctx, object, this, k)? {
                call(ctx, method, this, Args::empty())?;
            }
            Ok(Value::UNDEFINED)
        }
        ASYNC_DISPOSE => async_dispose(ctx, this),
        DISCARD_RESULT => Ok(Value::UNDEFINED),
        GET_CTOR => Ok(Value::object(ctx.realm().iterator_ctor)),
        GET_TAG => {
            let tag = ctx.new_string("Iterator")?;
            Ok(ctx.get(tag))
        }
        SET_CTOR | SET_TAG => {
            let receiver = ctx.object_id(this)?;
            if receiver == ctx.realm().iterator_proto {
                return Err(ctx.throw_type_error("Cannot replace an Iterator prototype accessor"));
            }
            let k = if n == SET_CTOR {
                key(ctx, "constructor")?
            } else {
                symbol(ctx, WellKnown::ToStringTag)
            };
            let value = arg(ctx, args, 0)?;
            if get_own_property_descriptor(ctx, this, k)?.is_some() {
                if !set_property(ctx, this, k, value)? {
                    return Err(ctx.throw_type_error("Cannot set Iterator property"));
                }
            } else {
                create(ctx, this, k, value)?;
            }
            Ok(Value::UNDEFINED)
        }
        FROM => from(ctx, args),
        CONCAT => concat(ctx, args),
        ZIP | ZIP_KEYED => zip::create_zip(ctx, args, n == ZIP_KEYED),
        ARRAY_NEXT => array_next(ctx, this),
        WRAP_NEXT | WRAP_RETURN => wrap(ctx, this, n == WRAP_RETURN),
        MAP | FILTER | FLAT_MAP | TAKE | DROP => make_helper(ctx, this, args, n),
        CHUNKS | WINDOWS => extensions::chunk_helper(ctx, this, args, n == WINDOWS),
        INCLUDES => extensions::includes(ctx, this, args),
        JOIN => extensions::join(ctx, this, args),
        HELPER_NEXT | HELPER_RETURN => helper_next(ctx, this, n == HELPER_RETURN),
        REDUCE | TO_ARRAY | FOR_EACH | SOME | EVERY | FIND => consume(ctx, this, args, n),
        _ => Err(ctx.throw_type_error("Unknown iterator builtin")),
    }
}

fn async_dispose<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    let ctor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
    let capability = new_promise_capability(ctx, ctor)?;
    let status = (|| {
        let object = to_object(ctx, this)?;
        let k = key(ctx, "return")?;
        let undefined = ctx.root(Value::UNDEFINED)?;
        let Some(method) = array::get_method_v(ctx, object, this, k)? else {
            return fulfill_promise(ctx, capability.promise, undefined);
        };
        let value = call(ctx, method, this, Args::empty())?;
        let value = ctx.root(value)?;
        let promise = promise_resolve(ctx, ctor, value)?;
        let discard = ctx.new_native(id(DISCARD_RESULT), false)?;
        perform_promise_then(ctx, promise, discard, undefined, Some(capability))
    })();
    if let Err(error) = status {
        reject_thrown(ctx, capability.promise, error)?;
    }
    Ok(ctx.get(capability.promise))
}

pub(crate) fn new_array_iterator<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    kind: ArrayIteratorKind,
) -> JsResult<Handle<'s>> {
    let target = ctx.get(target);
    new_iterator(ctx, ctx.realm().array_iterator_proto, || {
        IteratorKind::Array {
            target,
            kind,
            index: 0,
        }
    })
}

fn new_iterator<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: ObjId,
    data: impl Fn() -> IteratorKind,
) -> JsResult<Handle<'s>> {
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::Iterator(TrackedBox::new(
            ledger,
            IteratorData { kind: data() },
        )?))
    })
}

fn data<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> Option<&'a IteratorKind> {
    match &ctx.heap().object(id).data {
        ObjectData::Iterator(data) => Some(&data.kind),
        _ => None,
    }
}

fn data_mut<'a>(ctx: &'a mut ExecCtx<'_, '_>, id: ObjId) -> &'a mut IteratorKind {
    let ObjectData::Iterator(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        unreachable!()
    };
    &mut data.kind
}

fn helper<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> &'a HelperData {
    let Some(IteratorKind::Helper(data)) = data(ctx, id) else {
        unreachable!()
    };
    data
}

fn helper_mut<'a>(ctx: &'a mut ExecCtx<'_, '_>, id: ObjId) -> &'a mut HelperData {
    let IteratorKind::Helper(data) = data_mut(ctx, id) else {
        unreachable!()
    };
    data
}

pub(super) fn result(ctx: &mut ExecCtx<'_, '_>, value: Option<Value>) -> JsResult<Value> {
    let item = ctx.root(value.unwrap_or(Value::UNDEFINED))?;
    let result = ctx.new_object()?;
    let done = ctx.root(Value::bool(value.is_none()))?;
    let k = key(ctx, "value")?;
    create(ctx, result, k, item)?;
    let k = key(ctx, "done")?;
    create(ctx, result, k, done)?;
    Ok(ctx.get(result))
}

fn array_next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    let id = ctx.object_id(this)?;
    let Some(&IteratorKind::Array {
        target,
        kind,
        index,
    }) = data(ctx, id)
    else {
        return Err(ctx.throw_type_error("Expected an Array Iterator"));
    };
    if target == Value::UNDEFINED {
        return result(ctx, None);
    }
    let target_id = target.as_object().unwrap();
    let target = ctx.root(target)?;
    let typed = matches!(ctx.heap().object(target_id).data, ObjectData::TypedArray(_));
    let len = if typed {
        typed_array_length(ctx.heap(), target_id)
            .ok_or_else(|| ctx.throw_type_error("Typed array is out of bounds"))? as u64
    } else {
        get_length(ctx, target)?
    };
    let IteratorKind::Array {
        target: live_target,
        index: live_index,
        ..
    } = data_mut(ctx, id)
    else {
        unreachable!()
    };
    if index >= len {
        *live_target = Value::UNDEFINED;
        return result(ctx, None);
    }
    *live_index = index + 1;
    let read = |ctx: &mut ExecCtx<'_, 's>| {
        if typed {
            typed_array_get_index(ctx, target_id, index as usize)
        } else {
            get_index(ctx, target, index)
        }
    };
    let key = Value::number(index as f64);
    let item = match kind {
        ArrayIteratorKind::Key => key,
        ArrayIteratorKind::Value => read(ctx)?,
        ArrayIteratorKind::Entry => {
            let value = read(ctx)?;
            let value = ctx.root(value)?;
            let pair = ctx.new_array()?;
            create_index(ctx, pair, 0, key)?;
            create_index(ctx, pair, 1, ctx.get(value))?;
            ctx.get(pair)
        }
    };
    result(ctx, Some(item))
}

impl IteratorRecordData {
    fn rooted<'s>(self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<IterRecord<'s>> {
        Ok(IterRecord {
            object: ctx.root(self.object)?,
            next: ctx.root(self.next)?,
        })
    }
}

impl<'s> IterRecord<'s> {
    fn raw(self, ctx: &ExecCtx<'_, 's>) -> IteratorRecordData {
        IteratorRecordData {
            object: ctx.get(self.object),
            next: ctx.get(self.next),
        }
    }
}

fn flattenable<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    item: Handle<'s>,
    allow_string: bool,
) -> JsResult<IterRecord<'s>> {
    let value = ctx.get(item);
    let object = if value.as_object().is_some() {
        item
    } else if allow_string && matches!(value.decode(), ValueKind::String(_)) {
        to_object(ctx, item)?
    } else {
        return Err(ctx.throw_type_error("Expected an iterable object"));
    };
    if let Some(method) = array::get_method_v(ctx, object, item, symbol(ctx, WellKnown::Iterator))?
    {
        IterRecord::from_method(ctx, item, method)
    } else {
        IterRecord::direct(ctx, item)
    }
}

fn from<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let item = arg(ctx, args, 0)?;
    let record = flattenable(ctx, item, true)?;
    let iterator_proto = ctx.realm().iterator_proto;
    let start = ctx.get(record.object);
    let current = ctx.mut_root(start)?;
    loop {
        let candidate = ctx.get(current.handle());
        let proto = ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let candidate = ctx.root(candidate)?;
            get_prototype_of(ctx, candidate)
        })?;
        let Some(proto) = proto else {
            break;
        };
        if proto == iterator_proto {
            return Ok(start);
        }
        ctx.set(&current, Value::object(proto));
    }
    let record = record.raw(ctx);
    let wrapped = new_iterator(ctx, ctx.realm().iterator_wrap_proto, || {
        IteratorKind::Wrap(record)
    })?;
    Ok(ctx.get(wrapped))
}

fn wrap<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, close: bool) -> JsResult<Value> {
    let id = ctx.object_id(this)?;
    let Some(&IteratorKind::Wrap(record)) = data(ctx, id) else {
        return Err(ctx.throw_type_error("Expected a wrapped iterator"));
    };
    let record = record.rooted(ctx)?;
    if close {
        let k = key(ctx, "return")?;
        if let Some(method) = get_method(ctx, record.object, k)? {
            return call(ctx, method, record.object, Args::empty());
        }
        result(ctx, None)
    } else {
        call(ctx, record.next, record.object, Args::empty())
    }
}

fn make_helper<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    n: u16,
) -> JsResult<Value> {
    ctx.object_id(this)?;
    let value = arg(ctx, args, 0)?;
    let op = match n {
        MAP => HelperOp::Map,
        FILTER => HelperOp::Filter,
        FLAT_MAP => HelperOp::FlatMap,
        TAKE => HelperOp::Take,
        DROP => HelperOp::Drop,
        _ => unreachable!(),
    };
    let limit = if matches!(op, HelperOp::Drop | HelperOp::Take) {
        let number = to_number(ctx, value).and_then(|number| {
            if number.is_nan()
                || number.trunc() < 0.0
                || (number.is_finite() && number > MAX_LENGTH as f64)
            {
                Err(ctx.throw_range_error("Invalid iterator limit"))
            } else {
                Ok(number.trunc())
            }
        });
        on_error(ctx, number, |ctx| IterRecord::close(ctx, this))?
    } else {
        check_callback(ctx, this, value)?;
        0.0
    };
    let record = IterRecord::direct(ctx, this)?.raw(ctx);
    let callback = ctx.get(value);
    let helper = new_iterator(ctx, ctx.realm().iterator_helper_proto, || {
        let mut data = HelperData::new(op);
        data.source = Some(record);
        data.limit = limit;
        if !matches!(op, HelperOp::Take | HelperOp::Drop) {
            data.callback = callback;
        }
        IteratorKind::Helper(data)
    })?;
    Ok(ctx.get(helper))
}

fn check_callback<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    callback: Handle<'s>,
) -> JsResult<()> {
    if !callable(ctx, ctx.get(callback)) {
        let err = Err(ctx.throw_type_error("Callback is not callable"));
        return on_error(ctx, err, |ctx| IterRecord::close(ctx, this));
    }
    Ok(())
}

fn concat<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let result = new_iterator(ctx, ctx.realm().iterator_helper_proto, || {
        IteratorKind::Helper(HelperData::new(HelperOp::Concat))
    })?;
    let id = ctx.object_id(result)?;
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::Iterator(data) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        let IteratorKind::Helper(data) = &mut data.kind else {
            unreachable!()
        };
        data.sequence.try_reserve_exact(&heap.ledger, args.len())
    })?;
    for i in 0..args.len() {
        let object = ctx.get(args.get(i).unwrap());
        let record = ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let item = ctx.root(object)?;
            ctx.object_id(item)?;
            let Some(method) = get_method(ctx, item, symbol(ctx, WellKnown::Iterator))? else {
                return Err(ctx.throw_type_error("Object is not iterable"));
            };
            Ok(IteratorRecordData {
                object,
                next: ctx.get(method),
            })
        })?;
        helper_mut(ctx, id).sequence.push_reserved(record);
    }
    Ok(ctx.get(result))
}

fn close_record(ctx: &mut ExecCtx<'_, '_>, data: IteratorRecordData) -> JsResult<()> {
    ctx.with_scope(|ctx| {
        let object = ctx.root(data.object)?;
        IterRecord::close(ctx, object)
    })
}

fn close_helper(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<()> {
    if matches!(helper(ctx, id).op, HelperOp::Zip { .. }) {
        return zip::close_all(ctx, id);
    }
    let (inner, source) = (helper(ctx, id).inner, helper(ctx, id).source);
    let closed = match inner {
        Some(record) => close_record(ctx, record),
        None => Ok(()),
    };
    match source {
        Some(record) if closed.is_ok() => close_record(ctx, record),
        Some(record) => on_error(ctx, closed, |ctx| close_record(ctx, record)),
        None => closed,
    }
}

fn helper_next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, close: bool) -> JsResult<Value> {
    let id = ctx.object_id(this)?;
    let (op, state) = match data(ctx, id) {
        Some(IteratorKind::Complete) => return result(ctx, None),
        Some(IteratorKind::Helper(data)) => (data.op, data.state),
        _ => return Err(ctx.throw_type_error("Expected an Iterator Helper")),
    };
    match state {
        HelperState::Completed => return result(ctx, None),
        HelperState::Executing => {
            return Err(ctx.throw_type_error("Iterator helper is already running"));
        }
        _ => (),
    }
    helper_mut(ctx, id).state = if close && state == HelperState::SuspendedStart {
        // Close callbacks can reenter a helper that has yet to run.
        HelperState::Completed
    } else {
        HelperState::Executing
    };
    let item = if close {
        close_helper(ctx, id).map(|()| None)
    } else {
        match op {
            HelperOp::Zip { keyed, mode } => zip::step(ctx, id, keyed, mode),
            HelperOp::Concat => concat_step(ctx, id),
            HelperOp::Chunks { size } => extensions::chunk_step(ctx, id, size, false, true),
            HelperOp::Windows {
                size,
                allow_partial,
            } => extensions::chunk_step(ctx, id, size, true, allow_partial),
            _ => simple_step(ctx, id, op),
        }
    };
    if matches!(item, Ok(Some(_))) {
        helper_mut(ctx, id).state = HelperState::SuspendedYield;
    } else {
        *data_mut(ctx, id) = IteratorKind::Complete;
    }
    result(ctx, item?)
}

fn concat_step(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<Option<Value>> {
    loop {
        ctx.check_interrupt()?;
        if let Some(source) = helper(ctx, id).source {
            let item = ctx.with_scope(|ctx| {
                let record = source.rooted(ctx)?;
                IterRecord::step(ctx, record)
            })?;
            if item.is_some() {
                return Ok(item);
            }
            helper_mut(ctx, id).source = None;
        }
        let position = helper(ctx, id).position as usize;
        let Some(&iterable) = helper(ctx, id).sequence.get(position) else {
            return Ok(None);
        };
        let record = ctx.with_scope(|ctx| {
            let data = iterable.rooted(ctx)?;
            let record = IterRecord::from_method(ctx, data.object, data.next)?;
            Ok(record.raw(ctx))
        })?;
        let helper = helper_mut(ctx, id);
        helper.position += 1;
        helper.source = Some(record);
    }
}

fn simple_step(ctx: &mut ExecCtx<'_, '_>, id: ObjId, op: HelperOp) -> JsResult<Option<Value>> {
    let source = helper(ctx, id).source.unwrap();
    loop {
        ctx.check_interrupt()?;
        if let Some(inner) = helper(ctx, id).inner {
            let item = ctx.with_scope(|ctx| {
                let record = inner.rooted(ctx)?;
                IterRecord::step(ctx, record)
            });
            match item {
                Ok(Some(value)) => return Ok(Some(value)),
                Ok(None) => helper_mut(ctx, id).inner = None,
                Err(_) => return on_error(ctx, item, |ctx| close_record(ctx, source)),
            }
        }
        let limit = helper(ctx, id).limit;
        if matches!(op, HelperOp::Take) && limit == 0.0 {
            close_record(ctx, source)?;
            return Ok(None);
        }
        if matches!(op, HelperOp::Take | HelperOp::Drop) && limit > 0.0 {
            helper_mut(ctx, id).limit -= 1.0;
        }
        enum Action {
            Yield(Value),
            Done,
            Skip,
            Inner(IteratorRecordData),
        }
        let action = ctx.with_scope(|ctx| {
            let source = source.rooted(ctx)?;
            let Some(value) = IterRecord::step(ctx, source)? else {
                return Ok(Action::Done);
            };
            match op {
                HelperOp::Take => return Ok(Action::Yield(value)),
                HelperOp::Drop if limit > 0.0 => return Ok(Action::Skip),
                HelperOp::Drop => return Ok(Action::Yield(value)),
                _ => {}
            }
            let status = (|| {
                let item = ctx.root(value)?;
                let h = helper_mut(ctx, id);
                let index = h.position;
                h.position += 1;
                let callback = h.callback;
                let mapped = invoke_callback(ctx, callback, item, index, None)?;
                match op {
                    HelperOp::Map => Ok(Action::Yield(mapped)),
                    HelperOp::Filter => {
                        let mapped = ctx.root(mapped)?;
                        Ok(if to_boolean(ctx, mapped) {
                            Action::Yield(value)
                        } else {
                            Action::Skip
                        })
                    }
                    HelperOp::FlatMap => {
                        let mapped = ctx.root(mapped)?;
                        let record = flattenable(ctx, mapped, false)?.raw(ctx);
                        Ok(Action::Inner(record))
                    }
                    _ => unreachable!(),
                }
            })();
            on_error(ctx, status, |ctx| IterRecord::close(ctx, source.object))
        })?;
        match action {
            Action::Yield(value) => return Ok(Some(value)),
            Action::Done => return Ok(None),
            Action::Skip => {}
            Action::Inner(record) => helper_mut(ctx, id).inner = Some(record),
        }
    }
}

fn invoke_callback<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callback: Value,
    item: Handle<'s>,
    index: u64,
    acc: Option<Value>,
) -> JsResult<Value> {
    let cb = ctx.root(callback)?;
    let index = ctx.root(Value::number(index as f64))?;
    let this = ctx.root(Value::UNDEFINED)?;
    let args = if let Some(acc) = acc {
        let acc = ctx.root(acc)?;
        ctx.args(&[acc, item, index])?
    } else {
        ctx.args(&[item, index])?
    };
    call(ctx, cb, this, args)
}

fn consume<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    n: u16,
) -> JsResult<Value> {
    ctx.object_id(this)?;
    let callback = if n == TO_ARRAY {
        Value::UNDEFINED
    } else {
        let callback = arg(ctx, args, 0)?;
        check_callback(ctx, this, callback)?;
        ctx.get(callback)
    };
    let record = IterRecord::direct(ctx, this)?;
    let initial = args.get(1).map(|arg| ctx.get(arg));
    let accumulator = ctx.mut_root(initial.unwrap_or(Value::UNDEFINED))?;
    let mut started = initial.is_some();
    let output = if n == TO_ARRAY {
        let out = ctx.new_array()?;
        ctx.get(out)
    } else {
        Value::UNDEFINED
    };
    for i in 0.. {
        ctx.check_interrupt()?;
        let Some(value) = IterRecord::step(ctx, record)? else {
            break;
        };
        if n == REDUCE && !started {
            ctx.set(&accumulator, value);
            started = true;
            continue;
        }
        let acc = ctx.get(accumulator.handle());
        let outcome = ctx.with_scope(|ctx| {
            let item = ctx.root(value)?;
            if n == TO_ARRAY {
                let out = ctx.root(output)?;
                create_index(ctx, out, i, value)?;
                return Ok((Value::UNDEFINED, false));
            }
            let mapped = invoke_callback(ctx, callback, item, i, (n == REDUCE).then_some(acc))?;
            let mapped = ctx.root(mapped)?;
            Ok((ctx.get(mapped), to_boolean(ctx, mapped)))
        });
        let (mapped, matched) = on_error(ctx, outcome, |ctx| IterRecord::close(ctx, this))?;
        match n {
            REDUCE => ctx.set(&accumulator, mapped),
            FIND | SOME if matched => {
                let value = ctx.root(value)?;
                IterRecord::close(ctx, this)?;
                return Ok(if n == SOME {
                    Value::bool(true)
                } else {
                    ctx.get(value)
                });
            }
            EVERY if !matched => {
                IterRecord::close(ctx, this)?;
                return Ok(Value::bool(false));
            }
            _ => {}
        }
    }
    match n {
        REDUCE if !started => {
            Err(ctx.throw_type_error("Reduce of empty iterator with no initial value"))
        }
        REDUCE => Ok(ctx.get(accumulator.handle())),
        SOME => Ok(Value::bool(false)),
        EVERY => Ok(Value::bool(true)),
        TO_ARRAY => Ok(output),
        _ => Ok(Value::UNDEFINED),
    }
}
