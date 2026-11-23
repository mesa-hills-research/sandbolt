use qjs_bytecode::{FunctionKind, Opcode};

use crate::atom::Atom;
use crate::builtins::promise_json_extras::{
    new_promise_capability, perform_promise_then, promise_resolve,
};
use crate::builtins::vm_internal::{AWAIT_FULFILLED, AWAIT_REJECTED};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{
    AsyncFunctionData, AsyncGeneratorData, GeneratorData, ObjectData, ObjectFlags,
};
use crate::ops::{call, property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::value::*;
use crate::vm::{CallFrame, CoroRequest, CoroStatus, CoroutineState, VarRefState};

use super::{bindings, compiled, iterators};

const NEXT: u16 = 0;
const RETURN: u16 = 1;
const THROW: u16 = 2;

pub(crate) fn enter(ctx: &mut ExecCtx<'_, '_>, index: usize, kind: FunctionKind) -> JsResult<()> {
    if kind == FunctionKind::Normal {
        return Ok(());
    }
    let frame = ctx.runtime.vm.frames[index];
    let id = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        heap.coroutines.insert(
            &heap.ledger,
            CoroutineState {
                realm: frame.realm,
                frames: TrackedVec::new(),
                stack: TrackedVec::new(),
                open_vars: TrackedVec::new(),
                owner: None,
                kind,
                status: CoroStatus::Running,
                initial_call: true,
                capability: [Value::UNDEFINED; 3],
                requests: TrackedVec::new(),
            },
        )
    })?;
    ctx.runtime.vm.frames[index].coroutine = Some(id);
    ctx.with_realm(frame.realm, |ctx| {
        let prototype = match kind {
            FunctionKind::Async => ctx.realm().object_proto,
            FunctionKind::Generator | FunctionKind::AsyncGenerator => {
                let constructor = ctx.root(Value::object(frame.callee))?;
                let proto = property::get_property(ctx, constructor, Atom::get("prototype"))?;
                proto
                    .as_object()
                    .unwrap_or(if kind == FunctionKind::Generator {
                        ctx.realm().generator_proto
                    } else {
                        ctx.realm().async_generator_proto
                    })
            }
            FunctionKind::Normal => unreachable!(),
        };
        ctx.root(Value::object(prototype))?;
        let owner = ctx.make_object(Some(prototype), ObjectFlags::EXTENSIBLE, |ledger| {
            Ok(match kind {
                FunctionKind::Generator => ObjectData::Generator(TrackedBox::new(
                    ledger,
                    GeneratorData { state: Some(id) },
                )?),
                FunctionKind::Async => ObjectData::AsyncFunction(TrackedBox::new(
                    ledger,
                    AsyncFunctionData { state: Some(id) },
                )?),
                FunctionKind::AsyncGenerator => ObjectData::AsyncGenerator(TrackedBox::new(
                    ledger,
                    AsyncGeneratorData { state: Some(id) },
                )?),
                FunctionKind::Normal => unreachable!(),
            })
        })?;
        let owner = ctx.object_id(owner)?;
        state_mut(ctx, id).owner = Some(owner);
        if kind == FunctionKind::Async {
            let capability = capability(ctx)?;
            state_mut(ctx, id).capability = capability;
        }
        Ok(())
    })
}

fn state<'a>(ctx: &'a ExecCtx<'_, '_>, id: CoroId) -> &'a CoroutineState {
    ctx.heap().coroutines.get(id).unwrap()
}
fn state_mut<'a>(ctx: &'a mut ExecCtx<'_, '_>, id: CoroId) -> &'a mut CoroutineState {
    ctx.runtime.heap.coroutines.get_mut(id).unwrap()
}

fn scoped_frame(
    ctx: &mut ExecCtx<'_, '_>,
    id: CoroId,
    frame: CallFrame,
    status: CoroStatus,
) -> JsResult<()> {
    let start = frame.base as usize;
    let len = ctx.runtime.vm.stack.len() - start;
    ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let state = heap.coroutines.get_mut(id).unwrap();
        state.stack.try_reserve_exact(&heap.ledger, len)?;
        state.frames.try_reserve_exact(&heap.ledger, 1)?;
        Ok(())
    })?;
    let rt = &mut ctx.runtime;
    let state = rt.heap.coroutines.get_mut(id).unwrap();
    for &value in &rt.vm.stack[start..] {
        state.stack.push_reserved(value);
    }
    let mut saved = frame;
    saved.base = 0;
    saved.floor -= frame.base;
    saved.capture_base -= frame.base;
    state.frames.push_reserved(saved);
    if status != CoroStatus::Start {
        state.stack[len - 1] = Value::UNDEFINED;
    }
    for (i, value) in state.stack[saved.capture_base as usize..saved.floor as usize]
        .iter()
        .copied()
        .enumerate()
    {
        if let ValueKind::VarRef(reference) = value.decode() {
            rt.heap.var_refs.get_mut(reference).unwrap().state = VarRefState::OpenCoro {
                coro: id,
                local: i as u32 + 2,
            };
        }
    }
    state.status = status;
    Ok(())
}

pub(super) fn suspend(ctx: &mut ExecCtx<'_, '_>, index: usize, opcode: Opcode) -> JsResult<Value> {
    let frame = ctx.runtime.vm.frames[index];
    let id = frame
        .coroutine
        .ok_or_else(|| ctx.throw_type_error("function cannot suspend"))?;
    let kind = state(ctx, id).kind;
    let raw = *ctx.runtime.vm.stack.last().unwrap();
    let status = match opcode {
        Opcode::InitialYield => CoroStatus::Start,
        Opcode::Yield => CoroStatus::Yield,
        Opcode::YieldStar | Opcode::AsyncYieldStar => CoroStatus::YieldStar,
        Opcode::Await => {
            await_value(ctx, id, raw)?;
            CoroStatus::Await
        }
        _ => unreachable!(),
    };
    let value = match (kind, status) {
        (_, CoroStatus::Start) => Value::object(state(ctx, id).owner.unwrap()),
        (FunctionKind::Async, CoroStatus::Await) => async_result(ctx, id),
        (_, CoroStatus::Await) => Value::UNDEFINED,
        _ => raw,
    };
    scoped_frame(ctx, id, frame, status)?;
    Ok(value)
}

fn capability(ctx: &mut ExecCtx<'_, '_>) -> JsResult<[Value; 3]> {
    let constructor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
    let c = new_promise_capability(ctx, constructor)?;
    Ok([ctx.get(c.promise), ctx.get(c.resolve), ctx.get(c.reject)])
}

fn settle(
    ctx: &mut ExecCtx<'_, '_>,
    capability: [Value; 3],
    value: Value,
    reject: bool,
) -> JsResult<()> {
    ctx.with_scope(|ctx| {
        let value = ctx.root(value)?;
        let resolver = ctx.root(capability[if reject { 2 } else { 1 }])?;
        let this = ctx.root(Value::UNDEFINED)?;
        let args = ctx.args(&[value])?;
        call::call(ctx, resolver, this, args)?;
        Ok(())
    })
}

fn async_result(ctx: &ExecCtx<'_, '_>, id: CoroId) -> Value {
    let c = state(ctx, id);
    if c.initial_call {
        c.capability[0]
    } else {
        Value::UNDEFINED
    }
}

fn await_value(ctx: &mut ExecCtx<'_, '_>, id: CoroId, value: Value) -> JsResult<()> {
    let owner = state(ctx, id).owner.unwrap();
    ctx.with_scope(|ctx| {
        let value = ctx.root(value)?;
        let constructor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
        let promise = promise_resolve(ctx, constructor, value)?;
        let owner = ctx.root(Value::object(owner))?;
        let yes = bind(ctx, AWAIT_FULFILLED, owner)?;
        let no = bind(ctx, AWAIT_REJECTED, owner)?;
        perform_promise_then(ctx, promise, yes, no, None)
    })
}

fn bind<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    native: NativeFnId,
    owner: Handle<'s>,
) -> JsResult<Handle<'s>> {
    let function = ctx.new_native(native, false)?;
    ctx.bind_function(function, owner, Args::empty())
}

pub(crate) fn finish(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame, value: Value) -> JsResult<Value> {
    let Some(id) = frame.coroutine else {
        bindings::close_frame(ctx, frame);
        return Ok(value);
    };
    if state(ctx, id).status != CoroStatus::Running {
        return Ok(value);
    }
    if state(ctx, id).kind != FunctionKind::Async {
        state_mut(ctx, id).status = CoroStatus::Done;
        bindings::close_frame(ctx, frame);
        return Ok(value);
    }
    let result = async_result(ctx, id);
    let capability = state(ctx, id).capability;
    settle(ctx, capability, value, false)?;
    state_mut(ctx, id).status = CoroStatus::Done;
    bindings::close_frame(ctx, frame);
    Ok(result)
}

pub(crate) fn reject_if_async(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
) -> JsResult<Option<Value>> {
    let Some(id) = frame.coroutine else {
        return Ok(None);
    };
    if state(ctx, id).kind != FunctionKind::Async || state(ctx, id).status != CoroStatus::Running {
        return Ok(None);
    }
    let result = async_result(ctx, id);
    let capability = state(ctx, id).capability;
    let exception = ctx
        .take_exception()
        .map_or(Value::UNDEFINED, |(value, _)| value);
    state_mut(ctx, id).status = CoroStatus::Done;
    settle(ctx, capability, exception, true)?;
    ctx.runtime.vm.thrown_frames.clear();
    Ok(Some(result))
}

pub(crate) fn abort_frame(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame) {
    if let Some(id) = frame.coroutine {
        state_mut(ctx, id).status = CoroStatus::Done;
        let state = state_mut(ctx, id);
        state.stack.clear();
        state.frames.clear();
    }
    bindings::close_frame(ctx, frame);
}

fn abandon(ctx: &mut ExecCtx<'_, '_>, id: CoroId) {
    let (base, floor) = state(ctx, id).frames.first().map_or((0, 0), |frame| {
        (frame.capture_base as usize, frame.floor as usize)
    });
    let heap = &mut ctx.runtime.heap;
    let c = heap.coroutines.get_mut(id).unwrap();
    for (i, value) in c.stack[base..floor].iter().copied().enumerate() {
        if let ValueKind::VarRef(reference) = value.decode() {
            heap.var_refs.get_mut(reference).unwrap().state = VarRefState::Closed(c.stack[2 + i]);
        }
    }
    c.status = CoroStatus::Done;
    c.stack.clear();
    c.frames.clear();
}

fn resume(ctx: &mut ExecCtx<'_, '_>, id: CoroId, mode: u16, input: Value) -> JsResult<Value> {
    ctx.check_interrupt()?;
    let status = state(ctx, id).status;
    let start = ctx.runtime.vm.stack.len();
    let depth = ctx.runtime.vm.frames.len();
    let realm = ctx.realm;
    let mut frame = state(ctx, id).frames[0];
    let value_count = state(ctx, id).stack.len();
    let operands = compiled(ctx, frame.code.unwrap()).stack_size as usize;
    if !ctx.runtime.vm.check_budget(value_count + 1, 1, 1) {
        return Err(ctx.stack_overflow());
    }
    ctx.reserve_stack(value_count + operands + 1)?;
    frame.base = start as u32;
    frame.floor += frame.base;
    frame.capture_base += frame.base;
    ctx.allocate(|rt| rt.vm.frames.push(&rt.heap.ledger, frame))?;
    ctx.runtime.vm.reentry_depth += 1;
    let rt = &mut ctx.runtime;
    let c = rt.heap.coroutines.get_mut(id).unwrap();
    // Restore the live frame before switching its cells back to stack indices.
    for &value in c.stack.iter() {
        rt.vm.stack.push_reserved(value);
    }
    for (i, value) in rt.vm.stack[frame.capture_base as usize..frame.floor as usize]
        .iter()
        .copied()
        .enumerate()
    {
        if let ValueKind::VarRef(reference) = value.decode() {
            rt.heap.var_refs.get_mut(reference).unwrap().state =
                VarRefState::OpenStack((start + 2 + i) as u32);
        }
    }
    c.initial_call = false;
    c.status = CoroStatus::Running;
    c.frames.clear();
    c.stack.clear();
    ctx.realm = frame.realm;
    let throw = mode == THROW && (status == CoroStatus::Yield || status == CoroStatus::Await);
    let result = (|| {
        if throw {
            ctx.throw_value(input, false);
            ctx.runtime.vm.thrown_frames = super::capture_frames(ctx);
            if !call::unwind(ctx, depth)? {
                return Err(Abrupt::Thrown);
            }
        } else if status != CoroStatus::Start {
            let top = ctx.runtime.vm.stack.last_mut().unwrap();
            *top = input;
            if status != CoroStatus::Await {
                ctx.runtime
                    .vm
                    .stack
                    .push_reserved(Value::int32(i32::from(mode)));
            }
        }
        call::drive(ctx, depth)?;
        Ok(ctx.runtime.vm.stack[start])
    })();
    while ctx.runtime.vm.frames.len() > depth {
        let frame = ctx.runtime.vm.frames.pop().unwrap();
        abort_frame(ctx, frame);
    }
    ctx.runtime.vm.reentry_depth -= 1;
    ctx.runtime.vm.stack.truncate(start);
    ctx.realm = realm;
    result
}

fn owner_id<'s>(ctx: &ExecCtx<'_, 's>, owner: Handle<'s>, kind: FunctionKind) -> Option<CoroId> {
    let id = ctx.get(owner).as_object()?;
    match (kind, &ctx.heap().object(id).data) {
        (FunctionKind::Generator, ObjectData::Generator(g)) => g.state,
        (FunctionKind::AsyncGenerator, ObjectData::AsyncGenerator(g)) => g.state,
        (FunctionKind::Async, ObjectData::AsyncFunction(g)) => g.state,
        _ => None,
    }
}

pub(crate) fn resume_generator<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    generator: Handle<'s>,
    args: Args<'s>,
    kind: u16,
) -> JsResult<Value> {
    let value = args.get(0).map_or(Value::UNDEFINED, |arg| ctx.get(arg));
    if kind >= 3 {
        return enqueue(ctx, generator, kind - 3, value);
    }
    let id = owner_id(ctx, generator, FunctionKind::Generator)
        .ok_or_else(|| ctx.throw_type_error("expected a generator"))?;
    let status = state(ctx, id).status;
    if status == CoroStatus::Running {
        return Err(ctx.throw_type_error("generator is already running"));
    }
    let value = if status == CoroStatus::Done || status == CoroStatus::Start && kind != NEXT {
        if status != CoroStatus::Done {
            abandon(ctx, id);
        }
        match kind {
            THROW => return Err(ctx.throw_value(value, false)),
            RETURN => value,
            _ => Value::UNDEFINED,
        }
    } else {
        resume(ctx, id, kind, value)?
    };
    if state(ctx, id).status == CoroStatus::YieldStar {
        return Ok(value);
    }
    let value = ctx.root(value)?;
    iterators::iterator_result(ctx, value, state(ctx, id).status == CoroStatus::Done)
}

fn enqueue<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    generator: Handle<'s>,
    mode: u16,
    value: Value,
) -> JsResult<Value> {
    let capability = capability(ctx)?;
    let Some(id) = owner_id(ctx, generator, FunctionKind::AsyncGenerator) else {
        ctx.throw_type_error("expected an async generator");
        let exception = ctx.take_exception().unwrap().0;
        settle(ctx, capability, exception, true)?;
        return Ok(capability[0]);
    };
    let request = CoroRequest {
        mode,
        value,
        capability,
    };
    ctx.allocate(|rt| {
        rt.heap
            .coroutines
            .get_mut(id)
            .unwrap()
            .requests
            .push(&rt.heap.ledger, request)
    })?;
    drain(ctx, id)?;
    Ok(capability[0])
}

fn drain(ctx: &mut ExecCtx<'_, '_>, id: CoroId) -> JsResult<()> {
    while let Some(request) = state(ctx, id).requests.first().copied() {
        ctx.check_interrupt()?;
        match state(ctx, id).status {
            CoroStatus::Running | CoroStatus::Await | CoroStatus::AwaitReturn => return Ok(()),
            CoroStatus::Start if request.mode != NEXT => {
                abandon(ctx, id);
            }
            _ => {}
        }
        if state(ctx, id).status == CoroStatus::Done {
            match request.mode {
                NEXT => complete_request(ctx, id, Value::UNDEFINED, true, false)?,
                THROW => complete_request(ctx, id, request.value, true, true)?,
                RETURN => {
                    await_value(ctx, id, request.value)?;
                    state_mut(ctx, id).status = CoroStatus::AwaitReturn;
                    return Ok(());
                }
                _ => unreachable!(),
            }
        } else {
            let result = resume(ctx, id, request.mode, request.value);
            after_resume(ctx, id, result)?;
        }
    }
    Ok(())
}

fn after_resume(ctx: &mut ExecCtx<'_, '_>, id: CoroId, result: JsResult<Value>) -> JsResult<()> {
    match result {
        Ok(value) => match state(ctx, id).status {
            CoroStatus::Done => complete_request(ctx, id, value, true, false),
            CoroStatus::Yield | CoroStatus::YieldStar => {
                complete_request(ctx, id, value, false, false)
            }
            CoroStatus::Await => Ok(()),
            _ => unreachable!(),
        },
        Err(Abrupt::Thrown) => {
            let exception = ctx
                .take_exception()
                .map_or(Value::UNDEFINED, |(value, _)| value);
            ctx.runtime.vm.thrown_frames.clear();
            complete_request(ctx, id, exception, true, true)
        }
        Err(error) => Err(error),
    }
}

fn complete_request(
    ctx: &mut ExecCtx<'_, '_>,
    id: CoroId,
    value: Value,
    done: bool,
    reject: bool,
) -> JsResult<()> {
    let capability = state(ctx, id).requests[0].capability;
    // Keep the queue entry pinned across promise resolution.
    ctx.with_scope(|ctx| {
        let value = if reject {
            value
        } else {
            let value = ctx.root(value)?;
            iterators::iterator_result(ctx, value, done)?
        };
        settle(ctx, capability, value, reject)
    })?;
    state_mut(ctx, id).requests.remove(0);
    Ok(())
}

pub(crate) fn resume_await<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    owner: Handle<'s>,
    args: Args<'s>,
    reject: bool,
) -> JsResult<Value> {
    let value = args.get(0).map_or(Value::UNDEFINED, |h| ctx.get(h));
    let kind = if owner_id(ctx, owner, FunctionKind::Async).is_some() {
        FunctionKind::Async
    } else {
        FunctionKind::AsyncGenerator
    };
    let id = owner_id(ctx, owner, kind)
        .ok_or_else(|| ctx.throw_type_error("invalid await continuation"))?;
    if kind == FunctionKind::Async {
        resume(ctx, id, if reject { THROW } else { NEXT }, value)?;
    } else {
        if state(ctx, id).status == CoroStatus::AwaitReturn {
            state_mut(ctx, id).status = CoroStatus::Done;
            complete_request(ctx, id, value, true, reject)?;
        } else {
            let result = resume(ctx, id, if reject { THROW } else { NEXT }, value);
            after_resume(ctx, id, result)?;
        }
        drain(ctx, id)?;
    }
    Ok(Value::UNDEFINED)
}
