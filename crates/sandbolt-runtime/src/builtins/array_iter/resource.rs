use super::*;
use crate::builtins::promise_json_extras::fulfill_promise;
use crate::builtins::{
    configure_ctor, define_data, install_accessor, install_method, make_native_fn,
};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{
    AsyncDisposableStackData, DisposableResource, DisposableStackData, ErrorData, ErrorKind,
};
use crate::ops::promise::new_promise;
use crate::shape::PropFlags;
use crate::value::ObjId;

const CTOR: u16 = 0;
const DISPOSED: u16 = 1;
const USE: u16 = 2;
const ADOPT: u16 = 3;
const DEFER: u16 = 4;
const MOVE: u16 = 5;
const DISPOSE: u16 = 6;
const ASYNC: u16 = 16;
const CONTINUE: u16 = 32;
const REJECTED: u16 = 33;

const fn id(n: u16) -> NativeFnId {
    NativeFnId(ARRAY_ITER_BASE + 256 + n)
}

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    for (async_, ctor, proto, name) in [
        (
            false,
            ctx.realm().disposable_stack_ctor,
            ctx.realm().disposable_stack_proto,
            "DisposableStack",
        ),
        (
            true,
            ctx.realm().async_disposable_stack_ctor,
            ctx.realm().async_disposable_stack_proto,
            "AsyncDisposableStack",
        ),
    ] {
        ctx.with_scope(|ctx| {
            let offset = if async_ { ASYNC } else { 0 };
            let attrs = PropFlags::WRITABLE | PropFlags::CONFIGURABLE;
            let ctor = configure_ctor(ctx, ctor, id(offset + CTOR), name, 0)?;
            let proto = ctx.root(Value::object(proto))?;
            let global = ctx.root(Value::object(ctx.realm().global_object))?;
            let k = key(ctx, name)?;
            define_data(ctx, global, k, ctor, attrs)?;
            let k = key(ctx, "disposed")?;
            install_accessor(
                ctx,
                proto,
                k,
                Some((id(offset + DISPOSED), "get disposed")),
                None,
                true,
            )?;
            for (op, name, length) in [
                (USE, "use", 1),
                (ADOPT, "adopt", 2),
                (DEFER, "defer", 1),
                (MOVE, "move", 0),
            ] {
                let k = key(ctx, name)?;
                install_method(ctx, proto, k, id(offset + op), name, length)?;
            }
            let (well_known, dispose) = if async_ {
                (WellKnown::AsyncDispose, "disposeAsync")
            } else {
                (WellKnown::Dispose, "dispose")
            };
            let method = make_native_fn(ctx, id(offset + DISPOSE), dispose, 0, false)?;
            let k = key(ctx, dispose)?;
            define_data(ctx, proto, k, method, attrs)?;
            define_data(ctx, proto, symbol(ctx, well_known), method, attrs)?;
            let tag = ctx.new_string(name)?;
            define_data(
                ctx,
                proto,
                symbol(ctx, WellKnown::ToStringTag),
                tag,
                PropFlags::CONFIGURABLE,
            )
        })?;
    }
    Ok(())
}

pub(super) fn dispatch<'s>(
    n: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    if matches!(n, CONTINUE | REJECTED) {
        let id = ctx.object_id(this)?;
        let promise = ctx.root(async_state(ctx, id).promise.unwrap())?;
        let status = (|| {
            if n == REJECTED {
                let reason = arg(ctx, args, 0)?;
                let error = record_error(ctx, async_state(ctx, id).error, ctx.get(reason))?;
                async_state_mut(ctx, id).error = Some(error);
            }
            dispose_async_step(ctx, this)
        })();
        if let Err(thrown) = status {
            reject_thrown(ctx, promise, thrown)?;
        }
        return Ok(Value::UNDEFINED);
    }
    let async_ = n >= ASYNC;
    let op = n % ASYNC;
    if op == CTOR {
        let target = ctx
            .new_target()
            .ok_or_else(|| ctx.throw_type_error("Stack constructor requires new"))?;
        let result = ctx.ordinary_create_from_constructor(
            target,
            |realm| {
                if async_ {
                    realm.async_disposable_stack_proto
                } else {
                    realm.disposable_stack_proto
                }
            },
            ObjectFlags::EXTENSIBLE,
            |heap| new_data(&heap.ledger, async_),
        )?;
        return Ok(ctx.get(result));
    }
    if op == DISPOSE && async_ {
        return dispose_async(ctx, this);
    }
    let (stack, disposed) = brand(ctx, this, async_)?;
    if op == DISPOSED {
        return Ok(Value::bool(disposed));
    }
    if op == DISPOSE {
        return if disposed {
            Ok(Value::UNDEFINED)
        } else {
            dispose(ctx, stack)
        };
    }
    if disposed {
        return Err(ctx.throw_error(ErrorKind::Reference, "Stack is disposed"));
    }
    match op {
        MOVE => {
            let result = new_stack(ctx, async_)?;
            let id = ctx.object_id(result)?;
            transfer(ctx, stack, id);
            Ok(ctx.get(result))
        }
        USE | ADOPT | DEFER => add(ctx, this, stack, args, op, async_),
        _ => Err(ctx.throw_type_error("Unknown disposal builtin")),
    }
}

fn new_data(
    ledger: &crate::ledger::HeapLedger,
    async_: bool,
) -> crate::ledger::AllocResult<ObjectData> {
    if async_ {
        Ok(ObjectData::AsyncDisposableStack(TrackedBox::new(
            ledger,
            AsyncDisposableStackData::default(),
        )?))
    } else {
        Ok(ObjectData::DisposableStack(TrackedBox::new(
            ledger,
            DisposableStackData::default(),
        )?))
    }
}

fn new_stack<'s>(ctx: &mut ExecCtx<'_, 's>, async_: bool) -> JsResult<Handle<'s>> {
    let proto = if async_ {
        ctx.realm().async_disposable_stack_proto
    } else {
        ctx.realm().disposable_stack_proto
    };
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        new_data(ledger, async_)
    })
}

fn brand<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, async_: bool) -> JsResult<(ObjId, bool)> {
    let stack = ctx.object_id(this)?;
    match &ctx.heap().object(stack).data {
        ObjectData::DisposableStack(data) if !async_ => Ok((stack, data.disposed)),
        ObjectData::AsyncDisposableStack(data) if async_ => Ok((stack, data.disposed)),
        _ => Err(ctx.throw_type_error("Expected a disposable stack")),
    }
}

fn fields(data: &mut ObjectData) -> (&mut TrackedVec<DisposableResource>, &mut bool) {
    match data {
        ObjectData::DisposableStack(data) => {
            let data = &mut **data;
            (&mut data.resources, &mut data.disposed)
        }
        ObjectData::AsyncDisposableStack(data) => {
            let data = &mut **data;
            (&mut data.resources, &mut data.disposed)
        }
        _ => unreachable!(),
    }
}

fn transfer(ctx: &mut ExecCtx<'_, '_>, source: ObjId, target: ObjId) {
    let objects = &mut ctx.runtime.heap.objects;
    let (resources, disposed) = fields(&mut objects.get_mut(source).unwrap().data);
    *disposed = true;
    let resources = std::mem::take(resources);
    *fields(&mut objects.get_mut(target).unwrap().data).0 = resources;
}

fn add<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    stack: ObjId,
    args: Args<'s>,
    op: u16,
    async_: bool,
) -> JsResult<Value> {
    let first = arg(ctx, args, 0)?;
    let mut await_result = async_;
    let (value, method, adopt) = match op {
        USE if matches!(ctx.get(first), Value::UNDEFINED | Value::NULL) => {
            if !async_ {
                return Ok(ctx.get(first));
            }
            (ctx.get(first), Value::UNDEFINED, false)
        }
        USE => {
            ctx.object_id(first)?;
            let async_method = if async_ {
                get_method(ctx, first, symbol(ctx, WellKnown::AsyncDispose))?
            } else {
                None
            };
            let method = if let Some(method) = async_method {
                method
            } else {
                await_result = false;
                let method = get_method(ctx, first, symbol(ctx, WellKnown::Dispose))?;
                method.ok_or_else(|| ctx.throw_type_error("Resource has no disposal method"))?
            };
            (ctx.get(first), ctx.get(method), false)
        }
        ADOPT => {
            let method = arg(ctx, args, 1)?;
            (ctx.get(first), ctx.get(method), true)
        }
        DEFER => (Value::UNDEFINED, ctx.get(first), false),
        _ => unreachable!(),
    };
    if (op != USE || method != Value::UNDEFINED) && !callable(ctx, method) {
        return Err(ctx.throw_type_error("Disposal method is not callable"));
    }
    // A disposer getter can move or dispose the stack while its record is built.
    if !brand(ctx, this, async_)?.1 {
        let item = DisposableResource {
            value,
            method,
            adopt,
            await_result,
        };
        ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let (entries, _) = fields(&mut heap.objects.get_mut(stack).unwrap().data);
            entries.push(&heap.ledger, item)
        })?;
    }
    Ok(value)
}

fn invoke(ctx: &mut ExecCtx<'_, '_>, resource: DisposableResource) -> JsResult<Value> {
    let value = ctx.root(resource.value)?;
    let method = ctx.root(resource.method)?;
    if resource.adopt {
        let this = ctx.root(Value::UNDEFINED)?;
        let args = ctx.args(&[value])?;
        call(ctx, method, this, args)
    } else {
        call(ctx, method, value, Args::empty())
    }
}

fn record_error(
    ctx: &mut ExecCtx<'_, '_>,
    previous: Option<Value>,
    current: Value,
) -> JsResult<Value> {
    let Some(previous) = previous else {
        return Ok(current);
    };
    let current = ctx.root(current)?;
    let previous = ctx.root(previous)?;
    let error = ctx.make_object(
        Some(ctx.realm().suppressed_error_proto),
        ObjectFlags::EXTENSIBLE,
        |ledger| {
            Ok(ObjectData::Error(TrackedBox::new(
                ledger,
                ErrorData {
                    kind: ErrorKind::Suppressed,
                    message: Value::UNDEFINED,
                    stack: Value::UNDEFINED,
                    frames: crate::ledger::TrackedVec::new(),
                },
            )?))
        },
    )?;
    let flags = PropFlags::WRITABLE | PropFlags::CONFIGURABLE;
    let k = key(ctx, "error")?;
    define_data(ctx, error, k, current, flags)?;
    let k = key(ctx, "suppressed")?;
    define_data(ctx, error, k, previous, flags)?;
    Ok(ctx.get(error))
}

fn dispose(ctx: &mut ExecCtx<'_, '_>, stack: ObjId) -> JsResult<Value> {
    *fields(&mut ctx.runtime.heap.objects.get_mut(stack).unwrap().data).1 = true;
    let error = ctx.mut_root(Value::UNDEFINED)?;
    let mut has_error = false;
    loop {
        let previous = has_error.then_some(ctx.get(error.handle()));
        let Some(resource) = fields(&mut ctx.runtime.heap.objects.get_mut(stack).unwrap().data)
            .0
            .last()
            .copied()
        else {
            break;
        };
        let outcome = ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let result = invoke(ctx, resource);
            fields(&mut ctx.runtime.heap.objects.get_mut(stack).unwrap().data)
                .0
                .pop();
            match result {
                Ok(_) => Ok(None),
                Err(Abrupt::Interrupted) => Err(Abrupt::Interrupted),
                Err(Abrupt::Thrown) => {
                    let (current, _) = ctx.take_exception().unwrap();
                    record_error(ctx, previous, current).map(Some)
                }
            }
        })?;
        if let Some(outcome) = outcome {
            has_error = true;
            ctx.set(&error, outcome);
        }
    }
    *fields(&mut ctx.runtime.heap.objects.get_mut(stack).unwrap().data).0 = TrackedVec::new();
    if has_error {
        Err(ctx.throw(error.handle()))
    } else {
        Ok(Value::UNDEFINED)
    }
}

fn async_state<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> &'a AsyncDisposableStackData {
    let ObjectData::AsyncDisposableStack(data) = &ctx.heap().object(id).data else {
        unreachable!()
    };
    data
}

fn async_state_mut<'a>(
    ctx: &'a mut ExecCtx<'_, '_>,
    id: ObjId,
) -> &'a mut AsyncDisposableStackData {
    let ObjectData::AsyncDisposableStack(data) =
        &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
    else {
        unreachable!()
    };
    data
}

fn dispose_async<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    let promise = new_promise(ctx)?;
    let status = (|| {
        let (stack, disposed) = brand(ctx, this, true)?;
        if disposed {
            let undefined = ctx.root(Value::UNDEFINED)?;
            return fulfill_promise(ctx, promise, undefined);
        }
        let state = new_stack(ctx, true)?;
        let id = ctx.object_id(state)?;
        transfer(ctx, stack, id);
        async_state_mut(ctx, id).promise = Some(ctx.get(promise));
        dispose_async_step(ctx, state)
    })();
    if let Err(thrown) = status {
        reject_thrown(ctx, promise, thrown)?;
    }
    Ok(ctx.get(promise))
}

fn dispose_async_step<'s>(ctx: &mut ExecCtx<'_, 's>, state: Handle<'s>) -> JsResult<()> {
    let state_raw = ctx.get(state);
    let state_id = ctx.object_id(state)?;
    loop {
        ctx.check_interrupt()?;
        let Some(resource) = async_state(ctx, state_id).resources.last().copied() else {
            break;
        };
        if resource.method == Value::UNDEFINED {
            let state = async_state_mut(ctx, state_id);
            state.resources.pop();
            state.needs_await = true;
            continue;
        }
        let await_result = ctx.with_scope(|ctx| {
            let result = invoke(ctx, resource);
            async_state_mut(ctx, state_id).resources.pop();
            let value = match result {
                Ok(value) if resource.await_result => value,
                Ok(_) => Value::UNDEFINED,
                Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
                Err(Abrupt::Thrown) => {
                    let (current, _) = ctx.take_exception().unwrap();
                    let error = record_error(ctx, async_state(ctx, state_id).error, current)?;
                    async_state_mut(ctx, state_id).error = Some(error);
                    if resource.await_result {
                        return Ok(false);
                    }
                    Value::UNDEFINED
                }
            };
            let value = ctx.root(value)?;
            let state = ctx.root(state_raw)?;
            async_state_mut(ctx, state_id).has_awaited = true;
            let status = await_value(ctx, state, value, id(CONTINUE), id(REJECTED));
            match status {
                Ok(()) => Ok(true),
                Err(Abrupt::Interrupted) => Err(Abrupt::Interrupted),
                Err(Abrupt::Thrown) => {
                    let (current, _) = ctx.take_exception().unwrap();
                    let error = record_error(ctx, async_state(ctx, state_id).error, current)?;
                    async_state_mut(ctx, state_id).error = Some(error);
                    Ok(false)
                }
            }
        })?;
        if await_result {
            return Ok(());
        }
    }
    let data = async_state(ctx, state_id);
    if data.needs_await && !data.has_awaited {
        async_state_mut(ctx, state_id).has_awaited = true;
        let undefined = ctx.root(Value::UNDEFINED)?;
        return await_value(ctx, state, undefined, id(CONTINUE), id(REJECTED));
    }
    let (promise, error) = (data.promise.unwrap(), data.error);
    let promise = ctx.root(promise)?;
    let value = ctx.root(error.unwrap_or(Value::UNDEFINED))?;
    if error.is_some() {
        reject_promise(ctx, promise, value)
    } else {
        fulfill_promise(ctx, promise, value)
    }
}
