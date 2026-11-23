use crate::atom::{Atom, PropKey};
use crate::heap::Heap;
use crate::object::{ObjectData, ObjectFlags};
use crate::root::{Args, ExecCtx, Handle, RootScope};
use crate::value::*;
use crate::vm::{CallFrame, MiniOp};

const FINALLY: u32 = 1 << 31;

pub fn is_callable(heap: &Heap, id: ObjId) -> bool {
    match &heap.object(id).data {
        ObjectData::BytecodeFunction(_)
        | ObjectData::NativeFunction(_)
        | ObjectData::BoundFunction(_) => true,
        ObjectData::Proxy(data) => data.callable,
        _ => false,
    }
}

pub fn call<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callee: Handle<'s>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    invoke(ctx, ctx.get(callee), ctx.get(this), args, false)
}

pub fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callee: Handle<'s>,
    args: Args<'s>,
    new_target: Handle<'s>,
) -> JsResult<Value> {
    let mut callee_id = ctx.object_id(callee)?;
    let mut new_target_id = ctx.object_id(new_target)?;
    for id in [callee_id, new_target_id] {
        if !ctx
            .heap()
            .object(id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
        {
            return Err(ctx.throw_type_error("expected a constructor"));
        }
    }
    let actual_callee = ctx.get(callee);
    while let ObjectData::BoundFunction(bound) = &ctx.heap().object(callee_id).data {
        if new_target_id == callee_id {
            new_target_id = bound.target;
        }
        callee_id = bound.target;
    }
    let new_target = ctx.root(Value::object(new_target_id))?;
    let proto = super::property::get_property(ctx, new_target, PropKey::atom(Atom::PROTOTYPE))?;
    let realm = function_realm(ctx.heap(), new_target_id).unwrap_or(ctx.realm_id());
    let proto = proto
        .as_object()
        .unwrap_or(ctx.heap().realm(realm).object_proto);
    let rooted_proto = ctx.root(Value::object(proto))?;
    let object = ctx.new_object_with_proto(Some(rooted_proto))?;
    let result = invoke(ctx, actual_callee, ctx.get(object), args, true)?;
    Ok(if result.as_object().is_some() {
        result
    } else {
        ctx.get(object)
    })
}

fn function_realm(heap: &Heap, mut id: ObjId) -> Option<RealmId> {
    loop {
        match &heap.object(id).data {
            ObjectData::NativeFunction(f) => return Some(f.realm),
            ObjectData::BytecodeFunction(f) => return Some(f.realm),
            ObjectData::BoundFunction(f) => id = f.target,
            ObjectData::Proxy(p) => id = p.target?,
            _ => return None,
        }
    }
}

fn invoke<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callee: Value,
    this: Value,
    args: Args<'s>,
    constructing: bool,
) -> JsResult<Value> {
    let depth = ctx.runtime.vm.frames.len();
    let base = ctx.runtime.vm.stack.len();
    let caller = ctx.realm;
    let reentry = usize::from(depth != 0);
    if !ctx
        .runtime
        .vm
        .check_budget(args.len().saturating_add(2), 1, reentry)
    {
        return Err(ctx.stack_overflow());
    }
    ctx.runtime.vm.reentry_depth += reentry;
    let result = (|| {
        ctx.check_interrupt()?;
        ctx.reserve_stack(args.len() + 2)?;
        ctx.runtime.vm.stack.push_reserved(callee);
        ctx.runtime.vm.stack.push_reserved(this);
        for index in 0..args.len() {
            let value = ctx.get(args.get(index).unwrap());
            ctx.runtime.vm.stack.push_reserved(value);
        }
        enter(ctx, base, args.len(), constructing)?;
        drive(ctx, depth)?;
        Ok(ctx.runtime.vm.stack[base])
    })();
    ctx.runtime.vm.stack.truncate(base);
    ctx.runtime.vm.frames.truncate(depth);
    ctx.runtime.vm.reentry_depth -= reentry;
    ctx.realm = caller;
    result
}

fn enter(
    ctx: &mut ExecCtx<'_, '_>,
    base: usize,
    mut count: usize,
    constructing: bool,
) -> JsResult<()> {
    let mut callee = base_value(ctx, base)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("expected a callable function"))?;
    loop {
        let object = ctx.heap().object(callee);
        match &object.data {
            ObjectData::BoundFunction(bound) => {
                let (target, this, extra) = (bound.target, bound.this_value, bound.args.len());
                ctx.reserve_stack(extra)?;
                let heap = &ctx.runtime.heap;
                let ObjectData::BoundFunction(bound) = &heap.object(callee).data else {
                    unreachable!()
                };
                let stack = &mut ctx.runtime.vm.stack;
                for &arg in bound.args.iter() {
                    stack.push_reserved(arg);
                }
                stack[base + 2..].rotate_right(extra);
                if !constructing {
                    stack[base + 1] = this;
                }
                callee = target;
                count += extra;
            }
            ObjectData::Proxy(proxy) => {
                callee = proxy
                    .target
                    .ok_or_else(|| ctx.throw_type_error("proxy is revoked"))?;
            }
            ObjectData::BytecodeFunction(_) | ObjectData::NativeFunction(_) => break,
            _ => return Err(ctx.throw_type_error("expected a callable function")),
        }
    }
    let (realm, code) = match &ctx.heap().object(callee).data {
        ObjectData::NativeFunction(f) => (f.realm, None),
        ObjectData::BytecodeFunction(f) => (f.realm, Some(f.code)),
        _ => unreachable!(),
    };
    if !ctx.runtime.vm.check_budget(0, 1, 0) {
        return Err(ctx.stack_overflow());
    }
    let frame = CallFrame {
        callee,
        realm,
        code,
        pc: 0,
        base: base as u32,
        arg_count: count as u32,
    };
    ctx.allocate(|runtime| runtime.vm.frames.push(&runtime.heap.ledger, frame))
}

fn base_value(ctx: &ExecCtx<'_, '_>, index: usize) -> Value {
    ctx.runtime.vm.stack[index]
}

fn drive(ctx: &mut ExecCtx<'_, '_>, depth: usize) -> JsResult<()> {
    while ctx.runtime.vm.frames.len() > depth {
        let frame_index = ctx.runtime.vm.frames.len() - 1;
        let frame = ctx.runtime.vm.frames[frame_index];
        ctx.realm = frame.realm;
        let step = ctx.check_interrupt().and_then(|()| {
            if let Some(code) = frame.code {
                execute(ctx, frame, frame_index, code)
            } else {
                native(ctx, frame).map(Some)
            }
        });
        match step {
            Ok(Some(value)) => {
                ctx.runtime.vm.frames.pop();
                ctx.runtime.vm.stack.truncate(frame.base as usize);
                ctx.runtime.vm.stack.push_reserved(value);
            }
            Ok(None) => {}
            Err(Abrupt::Interrupted) => {
                ctx.take_exception();
                return Err(Abrupt::Interrupted);
            }
            Err(Abrupt::Thrown) => {
                if !unwind(ctx, depth) {
                    return Err(Abrupt::Thrown);
                }
            }
        }
    }
    Ok(())
}

fn native(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame) -> JsResult<Value> {
    let ObjectData::NativeFunction(data) = &ctx.heap().object(frame.callee).data else {
        unreachable!()
    };
    let id = data.function;
    if (0x1000..=0x1FFF).contains(&id.0) {
        let base = frame.base as usize;
        return RootScope::new(ctx.runtime, frame.realm).run(|ctx| {
            crate::builtins::dispatch(
                id,
                ctx,
                Handle::at(base + 1),
                Args::new(base + 2, frame.arg_count as usize),
            )
            .unwrap()
        });
    }
    let function = super::convert::builtin_native(id).or_else(|| {
        (id.0 < 0xfff0)
            .then(|| ctx.runtime.native_dispatch.get(id.0 as usize).copied())
            .flatten()
    });
    let Some(function) = function else {
        return Err(ctx.throw_type_error("native function is not installed"));
    };
    let base = frame.base as usize;
    RootScope::new(ctx.runtime, frame.realm).run(|ctx| {
        function(
            ctx,
            Handle::at(base + 1),
            Args::new(base + 2, frame.arg_count as usize),
        )
    })
}

fn execute(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    index: usize,
    code: BcId,
) -> JsResult<Option<Value>> {
    let func = ctx.heap().bytecode.get(code).unwrap();
    let Some(op) = func.code.get(frame.pc as usize).copied() else {
        return Err(ctx.throw_type_error("invalid bytecode offset"));
    };
    let constant = match op {
        MiniOp::Constant(i) => func.constants.get(i as usize).copied(),
        _ => None,
    };
    ctx.runtime.vm.frames[index].pc += 1;
    let base = frame.base as usize;
    let floor = base + 2 + frame.arg_count as usize;
    let value = match op {
        MiniOp::Push(value) => value,
        MiniOp::Constant(_) => {
            constant.ok_or_else(|| ctx.throw_type_error("invalid constant index"))?
        }
        MiniOp::This => base_value(ctx, base + 1),
        MiniOp::Arg(arg) => {
            if u32::from(arg) < frame.arg_count {
                base_value(ctx, base + 2 + arg as usize)
            } else {
                Value::UNDEFINED
            }
        }
        MiniOp::Capture(i) => {
            let ObjectData::BytecodeFunction(f) = &ctx.heap().object(frame.callee).data else {
                unreachable!()
            };
            f.closure
                .get(i as usize)
                .copied()
                .ok_or_else(|| ctx.throw_type_error("invalid closure index"))?
        }
        MiniOp::Callee => Value::object(frame.callee),
        MiniOp::Pop => {
            pop(ctx, floor)?;
            return Ok(None);
        }
        MiniOp::Dup => ctx
            .runtime
            .vm
            .stack
            .get(floor..)
            .and_then(|slice| slice.last())
            .copied()
            .ok_or_else(|| ctx.throw_type_error("bytecode stack underflow"))?,
        MiniOp::Call(count) => {
            let values = count as usize + 2;
            let len = ctx.runtime.vm.stack.len();
            if len - floor < values {
                return Err(ctx.throw_type_error("bytecode stack underflow"));
            }
            enter(ctx, len - values, count as usize, false)?;
            return Ok(None);
        }
        MiniOp::Return => return pop(ctx, floor).map(Some),
        MiniOp::Jump(target) => {
            ctx.runtime.vm.frames[index].pc = target;
            return Ok(None);
        }
        MiniOp::Try { target, finally } => {
            if target >= FINALLY {
                return Err(ctx.throw_type_error("invalid handler offset"));
            }
            Value::catch_offset(target | if finally { FINALLY } else { 0 })
        }
        MiniOp::EndTry => {
            if !matches!(pop(ctx, floor)?.decode(), ValueKind::CatchOffset(_)) {
                return Err(ctx.throw_type_error("invalid handler stack"));
            }
            return Ok(None);
        }
        MiniOp::Throw | MiniOp::ResumeThrow => {
            let value = pop(ctx, floor)?;
            return Err(ctx.throw_value(value, false));
        }
    };
    ctx.root(value)?;
    Ok(None)
}

fn pop(ctx: &mut ExecCtx<'_, '_>, floor: usize) -> JsResult<Value> {
    if ctx.runtime.vm.stack.len() <= floor {
        return Err(ctx.throw_type_error("bytecode stack underflow"));
    }
    Ok(ctx.runtime.vm.stack.pop().unwrap())
}

fn unwind(ctx: &mut ExecCtx<'_, '_>, depth: usize) -> bool {
    while ctx.runtime.vm.frames.len() > depth {
        let i = ctx.runtime.vm.frames.len() - 1;
        let frame = ctx.runtime.vm.frames[i];
        let floor = frame.base as usize + 2 + frame.arg_count as usize;
        if frame.code.is_some()
            && let Some((offset, target)) = ctx.runtime.vm.stack[floor..]
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, v)| match v.decode() {
                    ValueKind::CatchOffset(target) => Some((i, target)),
                    _ => None,
                })
        {
            let (exception, _) = ctx.take_exception().unwrap_or((Value::UNDEFINED, false));
            ctx.runtime.vm.stack.truncate(floor + offset);
            ctx.runtime.vm.stack.push_reserved(exception);
            ctx.runtime.vm.frames[i].pc = target & !FINALLY;
            return true;
        }
        ctx.runtime.vm.frames.pop();
        ctx.runtime.vm.stack.truncate(frame.base as usize);
    }
    false
}
