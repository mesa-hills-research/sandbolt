use crate::atom::Atom;
use crate::heap::Heap;
use crate::object::{ObjectData, ObjectFlags};
use crate::root::{Args, ExecCtx, Handle, RootScope};
use crate::value::*;
use crate::vm::{CallFrame, MiniOp, exec};

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
    invoke(ctx, ctx.get(callee), ctx.get(this), args, None)
}

pub fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callee: Handle<'s>,
    args: Args<'s>,
    new_target: Handle<'s>,
) -> JsResult<Value> {
    let id = ctx.object_id(new_target)?;
    invoke(ctx, ctx.get(callee), ctx.get(new_target), args, Some(id))
}

pub(crate) fn function_realm(heap: &Heap, mut id: ObjId) -> Option<RealmId> {
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
    new_target: Option<ObjId>,
) -> JsResult<Value> {
    let depth = ctx.runtime.vm.frames.len();
    let base = ctx.runtime.vm.stack.len();
    let caller = ctx.realm;
    let reentry = usize::from(depth != 0 || ctx.runtime.vm.reentry_depth != 0);
    if depth == 0 && reentry == 0 {
        ctx.runtime.vm.thrown_frames.clear();
    }
    if !ctx
        .runtime
        .vm
        .check_budget(args.len().saturating_add(2), 1, reentry)
    {
        return Err(ctx.stack_overflow());
    }
    ctx.runtime.vm.reentry_depth += reentry;
    let result = (|| {
        ctx.reserve_stack(args.len() + 2)?;
        ctx.runtime.vm.stack.push_reserved(callee);
        ctx.runtime.vm.stack.push_reserved(this);
        for index in 0..args.len() {
            let value = ctx.get(args.get(index).unwrap());
            ctx.runtime.vm.stack.push_reserved(value);
        }
        enter(ctx, base, args.len(), new_target)?;
        drive(ctx, depth)?;
        Ok(ctx.runtime.vm.stack[base])
    })();
    while ctx.runtime.vm.frames.len() > depth {
        let frame = ctx.runtime.vm.frames.pop().unwrap();
        exec::close_frame(ctx, frame);
    }
    ctx.runtime.vm.stack.truncate(base);
    ctx.runtime.vm.reentry_depth -= reentry;
    ctx.realm = caller;
    result
}

pub(crate) fn enter(
    ctx: &mut ExecCtx<'_, '_>,
    base: usize,
    mut count: usize,
    mut new_target: Option<ObjId>,
) -> JsResult<()> {
    ctx.check_interrupt()?;
    let mut require_object = false;
    let mut callee = ctx.runtime.vm.stack[base]
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("expected a callable function"))?;
    loop {
        if let Some(target) = new_target {
            for id in [callee, target] {
                if !ctx
                    .heap()
                    .object(id)
                    .flags
                    .contains(ObjectFlags::IS_CONSTRUCTOR)
                {
                    return Err(ctx.throw_type_error("expected a constructor"));
                }
            }
        }
        let object = ctx.heap().object(callee);
        match &object.data {
            ObjectData::BoundFunction(bound) => {
                let (target, this, extra) = (bound.target, bound.this_value, bound.args.len());
                ctx.reserve_stack(extra)?;
                let ObjectData::BoundFunction(bound) = &ctx.runtime.heap.object(callee).data else {
                    unreachable!()
                };
                let stack = &mut ctx.runtime.vm.stack;
                for &arg in bound.args.iter() {
                    stack.push_reserved(arg);
                }
                stack[base + 2..].rotate_right(extra);
                if new_target.is_none() {
                    stack[base + 1] = this;
                }
                if new_target == Some(callee) {
                    new_target = Some(target);
                    stack[base + 1] = Value::object(target);
                }
                callee = target;
                stack[base] = Value::object(target);
                count += extra;
            }
            ObjectData::Proxy(proxy) => {
                if !proxy.callable && new_target.is_none() {
                    return Err(ctx.throw_type_error("expected a callable function"));
                }
                let (target, handler) = super::property::proxy_parts(ctx, callee)?;
                if let Some(trap) = proxy_call(ctx, base, count, target, handler, new_target)? {
                    require_object |= new_target.is_some();
                    new_target = None;
                    callee = trap;
                    count = ctx.runtime.vm.stack.len() - base - 2;
                } else {
                    callee = target;
                    ctx.runtime.vm.stack[base] = Value::object(target);
                }
            }
            ObjectData::NativeFunction(native) if new_target.is_none() => {
                let id = native.function.0;
                if let Some((new_count, target)) =
                    exec::transform_native_call(ctx, id, base, count)?
                {
                    callee = ctx.runtime.vm.stack[base]
                        .as_object()
                        .ok_or_else(|| ctx.throw_type_error("expected a callable function"))?;
                    count = new_count;
                    new_target = target;
                    ctx.check_interrupt()?;
                } else {
                    break;
                }
            }
            ObjectData::BytecodeFunction(_) | ObjectData::NativeFunction(_) => break,
            _ => return Err(ctx.throw_type_error("expected a callable function")),
        }
    }
    let (realm, code, preallocate) = match &ctx.heap().object(callee).data {
        ObjectData::NativeFunction(f) => {
            (f.realm, None, !(0x1000..=0x1fff).contains(&f.function.0))
        }
        ObjectData::BytecodeFunction(f) => {
            if f.is_class_constructor && new_target.is_none() {
                return Err(ctx.throw_type_error("class constructors must be invoked with 'new'"));
            }
            (f.realm, Some(f.code), !f.is_derived_constructor)
        }
        _ => unreachable!(),
    };
    let (arg_slots, locals, operands, capture_slots) = code
        .and_then(|id| ctx.heap().bytecode.get(id).unwrap().compiled.as_ref())
        .map_or((count, 0, 0, 0), |f| {
            let slots = count.max(f.arg_count as usize);
            (
                slots,
                f.var_count as usize,
                f.stack_size as usize,
                slots + f.var_count as usize,
            )
        });
    let storage = arg_slots - count + locals + capture_slots;
    if !ctx.runtime.vm.check_budget(storage + operands, 1, 0) {
        return Err(ctx.stack_overflow());
    }
    let capture_base = base + 2 + arg_slots + locals;
    let floor = capture_base + capture_slots;
    let this = if let Some(new_target) = new_target {
        if preallocate {
            ctx.with_scope(|ctx| {
                let object = ctx.ordinary_create_from_constructor(
                    new_target,
                    |realm| realm.object_proto,
                    ObjectFlags::EXTENSIBLE,
                    |_| Ok(ObjectData::Ordinary),
                )?;
                Ok(ctx.get(object))
            })?
        } else {
            Value::UNDEFINED
        }
    } else {
        ctx.runtime.vm.stack[base + 1]
    };
    ctx.root(this)?;
    ctx.reserve_stack(storage + operands)?;
    ctx.runtime.vm.stack.pop();
    for _ in 0..storage {
        ctx.runtime.vm.stack.push_reserved(Value::UNDEFINED);
    }
    let frame = CallFrame {
        callee,
        realm,
        code,
        new_target,
        pc: 0,
        base: base as u32,
        arg_count: count as u32,
        arg_slots: arg_slots as u32,
        capture_base: capture_base as u32,
        floor: floor as u32,
        last_pc: 0,
        return_call: false,
        require_object,
    };
    // The original call operands still contain new.target until the frame is live.
    ctx.root(this)?;
    ctx.allocate(|runtime| runtime.vm.frames.push(&runtime.heap.ledger, frame))?;
    ctx.runtime.vm.stack.pop();
    ctx.runtime.vm.stack[base + 1] = this;
    Ok(())
}

fn proxy_call(
    ctx: &mut ExecCtx<'_, '_>,
    base: usize,
    count: usize,
    target: ObjId,
    handler: ObjId,
    new_target: Option<ObjId>,
) -> JsResult<Option<ObjId>> {
    ctx.reserve_stack(5)?;
    let args = ctx.with_scope(|ctx| {
        let target = ctx.root(Value::object(target))?;
        let handler = ctx.root(Value::object(handler))?;
        let Some(trap) = super::property::lookup_proxy_trap(
            ctx,
            handler,
            Atom::get(if new_target.is_some() {
                "construct"
            } else {
                "apply"
            }),
        )?
        else {
            return Ok(None);
        };
        let array = exec::array_from_stack(ctx, base + 2, count)?;
        let last = new_target.map_or(ctx.runtime.vm.stack[base + 1], Value::object);
        let (arg1, arg2) = if new_target.is_some() {
            (ctx.get(array), last)
        } else {
            (last, ctx.get(array))
        };
        Ok(Some([
            ctx.get(trap),
            ctx.get(handler),
            ctx.get(target),
            arg1,
            arg2,
        ]))
    })?;
    if let Some(args) = args {
        ctx.runtime.vm.stack.truncate(base);
        for value in args {
            ctx.runtime.vm.stack.push_reserved(value);
        }
        Ok(args[0].as_object())
    } else {
        Ok(None)
    }
}

pub(crate) fn drive(ctx: &mut ExecCtx<'_, '_>, depth: usize) -> JsResult<()> {
    while ctx.runtime.vm.frames.len() > depth {
        let index = ctx.runtime.vm.frames.len() - 1;
        let frame = ctx.runtime.vm.frames[index];
        ctx.realm = frame.realm;
        let step = if frame.return_call {
            Ok(ctx.runtime.vm.stack.last().copied())
        } else if let Some(code) = frame.code {
            if ctx.heap().bytecode.get(code).unwrap().compiled.is_some() {
                exec::step(ctx, frame, index, code)
            } else {
                ctx.check_interrupt()
                    .and_then(|()| execute_mini(ctx, frame, index, code))
            }
        } else {
            native(ctx, frame).map(Some)
        }
        .and_then(|result| {
            if let Some(value) = result
                && value.as_object().is_none()
            {
                if frame.require_object {
                    return Err(ctx.throw_type_error("Proxy construct trap must return an object"));
                }
                if frame.new_target.is_some() {
                    let this = ctx.runtime.vm.stack[frame.base as usize + 1];
                    if this.as_object().is_some() {
                        return Ok(Some(this));
                    }
                }
            }
            Ok(result)
        });
        match step {
            Ok(Some(value)) => {
                ctx.runtime.vm.frames.pop();
                exec::close_frame(ctx, frame);
                ctx.runtime.vm.stack.truncate(frame.base as usize);
                ctx.runtime.vm.stack.push_reserved(value);
            }
            Ok(None) => {}
            Err(Abrupt::Interrupted) => {
                ctx.take_exception();
                return Err(Abrupt::Interrupted);
            }
            Err(Abrupt::Thrown) => {
                if ctx.runtime.vm.thrown_frames.is_empty() {
                    ctx.runtime.vm.thrown_frames = exec::capture_frames(ctx);
                }
                if !unwind(ctx, depth)? {
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
    let base = frame.base as usize;
    if (0x1000..=0x1fff).contains(&id.0) {
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
    RootScope::new(ctx.runtime, frame.realm).run(|ctx| {
        function(
            ctx,
            Handle::at(base + 1),
            Args::new(base + 2, frame.arg_count as usize),
        )
    })
}

fn execute_mini(
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
    let floor = frame.floor as usize;
    let value = match op {
        MiniOp::Push(value) => value,
        MiniOp::Constant(_) => {
            constant.ok_or_else(|| ctx.throw_type_error("invalid constant index"))?
        }
        MiniOp::This => ctx.runtime.vm.stack[base + 1],
        MiniOp::Arg(arg) => {
            if u32::from(arg) < frame.arg_count {
                ctx.runtime.vm.stack[base + 2 + arg as usize]
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
            enter(ctx, len - values, count as usize, None)?;
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

pub(crate) fn unwind(ctx: &mut ExecCtx<'_, '_>, depth: usize) -> JsResult<bool> {
    while ctx.runtime.vm.frames.len() > depth {
        let index = ctx.runtime.vm.frames.len() - 1;
        let frame = ctx.runtime.vm.frames[index];
        if let Some(code) = frame.code {
            let floor = frame.floor as usize;
            let compiled = ctx.heap().bytecode.get(code).unwrap().compiled.as_ref();
            let handler = compiled.and_then(|f| {
                f.handlers
                    .iter()
                    .copied()
                    .filter(|h| h.start_pc <= frame.last_pc && frame.last_pc < h.end_pc)
                    .min_by_key(|h| h.end_pc - h.start_pc)
            });
            let caught = if compiled.is_some() {
                exec::unwind_frame(ctx, frame, index, handler)?
            } else if let Some((offset, target)) = ctx.runtime.vm.stack[floor..]
                .iter()
                .enumerate()
                .rev()
                .find_map(|(offset, value)| match value.decode() {
                    ValueKind::CatchOffset(target) => Some((offset, target)),
                    _ => None,
                })
            {
                let (exception, _) = ctx.take_exception().unwrap_or((Value::UNDEFINED, false));
                ctx.runtime.vm.stack.truncate(floor + offset);
                ctx.runtime.vm.stack.push_reserved(exception);
                ctx.runtime.vm.frames[index].pc = target & !FINALLY;
                true
            } else {
                false
            };
            if caught {
                ctx.runtime.vm.frames[index].return_call = false;
                ctx.runtime.vm.thrown_frames.clear();
                return Ok(true);
            }
        }
        ctx.runtime.vm.frames.pop();
        exec::close_frame(ctx, frame);
        ctx.runtime.vm.stack.truncate(frame.base as usize);
    }
    Ok(false)
}
