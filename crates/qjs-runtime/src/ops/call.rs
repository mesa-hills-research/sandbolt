use qjs_bytecode::FunctionKind;

use crate::atom::Atom;
use crate::heap::Heap;
use crate::object::{ObjectData, ObjectFlags};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle, RootScope};
use crate::value::*;
use crate::vm::{CallFrame, CompiledFunc, MiniOp, VmState, exec};

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

fn lookup_function_realm(heap: &Heap, mut id: ObjId) -> Result<Option<RealmId>, ()> {
    loop {
        match &heap.object(id).data {
            ObjectData::NativeFunction(f) => return Ok(Some(f.realm)),
            ObjectData::BytecodeFunction(f) => return Ok(Some(f.realm)),
            ObjectData::BoundFunction(f) => id = f.target,
            ObjectData::Proxy(p) => id = p.target.ok_or(())?,
            _ => return Ok(None),
        }
    }
}

pub(crate) fn function_realm(heap: &Heap, id: ObjId) -> Option<RealmId> {
    lookup_function_realm(heap, id).ok().flatten()
}

pub(crate) fn get_function_realm(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<RealmId> {
    lookup_function_realm(ctx.heap(), id)
        .map(|realm| realm.unwrap_or(ctx.realm_id()))
        .map_err(|()| ctx.throw_type_error("Proxy is revoked"))
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
        exec::coroutines::abort_frame(ctx, frame);
    }
    ctx.runtime.vm.stack.truncate(base);
    ctx.runtime.vm.reentry_depth -= reentry;
    ctx.realm = caller;
    result
}

#[derive(Clone, Copy)]
struct FrameLayout {
    base: usize,
    count: usize,
    arg_slots: usize,
    capture_base: usize,
    floor: usize,
    operands: usize,
}

impl FrameLayout {
    #[inline]
    fn new(base: usize, count: usize, function: Option<&CompiledFunc>) -> Self {
        let (arg_slots, locals, operands, capture_slots) = function.map_or((count, 0, 0, 0), |f| {
            let slots = count.max(f.arg_count as usize);
            let captures = if f.needs_captures || f.eval.is_some() {
                slots + f.var_count as usize
            } else {
                0
            };
            (slots, f.var_count as usize, f.stack_size as usize, captures)
        });
        let capture_base = base + 2 + arg_slots + locals;
        Self {
            base,
            count,
            arg_slots,
            capture_base,
            floor: capture_base + capture_slots,
            operands,
        }
    }

    #[inline]
    fn storage(self) -> usize {
        self.floor - (self.base + 2 + self.count)
    }

    #[inline]
    fn needed(self) -> usize {
        self.storage() + self.operands
    }

    #[inline]
    fn frame(
        self,
        callee: ObjId,
        realm: RealmId,
        caller_realm: RealmId,
        code: Option<BcId>,
        new_target: Option<ObjId>,
        require_object: bool,
    ) -> CallFrame {
        CallFrame {
            callee,
            realm,
            caller_realm,
            code,
            new_target,
            pc: 0,
            base: self.base as u32,
            arg_count: self.count as u32,
            arg_slots: self.arg_slots as u32,
            capture_base: self.capture_base as u32,
            floor: self.floor as u32,
            last_pc: 0,
            fast_ip: u32::MAX,
            return_call: false,
            require_object,
            coroutine: None,
        }
    }
}

pub(crate) struct PlainCall {
    frame: CallFrame,
    builtin: Option<NativeFnId>,
    method: bool,
}

impl PlainCall {
    #[inline]
    pub(crate) fn enter(self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Option<BcId>> {
        let vm = &mut ctx.runtime.vm;
        let base = self.frame.base as usize;
        if self.method {
            vm.stack.swap(base, base + 1);
        } else {
            vm.stack.push_reserved(Value::UNDEFINED);
            vm.stack[base + 1..].rotate_right(1);
        }
        if vm.interrupt_handler.is_some() {
            ctx.check_interrupt()?;
        }
        let vm = &mut ctx.runtime.vm;
        while vm.stack.len() < self.frame.floor as usize {
            vm.stack.push_reserved(Value::UNDEFINED);
        }
        vm.frames.push_reserved(self.frame);
        ctx.realm = self.frame.realm;
        if let Some(id) = self.builtin {
            finish_builtin(ctx, id, self.frame)?;
        }
        Ok(self.frame.code)
    }
}

#[inline]
pub(crate) fn prepare_plain(
    heap: &Heap,
    vm: &VmState,
    caller: RealmId,
    count: usize,
    method: bool,
) -> Option<PlainCall> {
    prepare_plain_at(heap, vm, vm.stack.len(), caller, count, method)
}

#[inline]
pub(crate) fn prepare_plain_at(
    heap: &Heap,
    vm: &VmState,
    tail: usize,
    caller: RealmId,
    count: usize,
    method: bool,
) -> Option<PlainCall> {
    if vm.frames.len() == vm.frames.capacity() {
        return None;
    }
    let inserted = usize::from(!method);
    let base = tail - (count + 2 - inserted);
    let callee = vm.stack[base + usize::from(method)].as_object()?;
    let (realm, code, function, builtin) = match &heap.object(callee).data {
        ObjectData::BytecodeFunction(data) if !data.is_class_constructor => {
            let code = data.code;
            let function = heap.bytecode.get(code).unwrap().compiled.as_ref()?;
            if function.kind != FunctionKind::Normal || function.needs_arguments {
                return None;
            }
            (data.realm, Some(code), Some(function), None)
        }
        ObjectData::NativeFunction(data)
            if (0x1000..=0x1fff).contains(&data.function.0)
                && exec::NativeForward::from_id(data.function.0).is_none() =>
        {
            (data.realm, None, None, Some(data.function))
        }
        _ => return None,
    };
    let layout = FrameLayout::new(base, count, function);
    let needed = layout.needed() + inserted;
    // Always preserve the spare root slot required by ExecCtx::root at peak operand height.
    if vm.stack.capacity() - tail < needed + 1
        || (!method && tail + 1 >= u32::MAX as usize)
        || !vm.check_budget_at(tail, needed, 1, 0)
    {
        return None;
    }
    Some(PlainCall {
        frame: layout.frame(callee, realm, caller, code, None, false),
        builtin,
        method,
    })
}

pub(crate) fn enter(
    ctx: &mut ExecCtx<'_, '_>,
    base: usize,
    count: usize,
    new_target: Option<ObjId>,
) -> JsResult<()> {
    let realm = ctx.realm;
    let result = enter_call(ctx, base, count, new_target);
    ctx.realm = realm;
    result
}

fn enter_call(
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
                    return Err(not_a_constructor(ctx, id));
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
                let Some(forward) = exec::NativeForward::from_id(native.function.0) else {
                    break;
                };
                let realm = native.realm;
                if !ctx.runtime.vm.check_budget(0, 1, 0) {
                    return Err(ctx.stack_overflow());
                }
                // Keep the builtin on the VM stack without recursing into Rust.
                push_forwarding_frame(ctx, callee, realm, base)?;
                ctx.realm = realm;
                (count, new_target) = exec::transform_native_call(ctx, forward, base, count)?;
                callee = ctx.runtime.vm.stack[base]
                    .as_object()
                    .ok_or_else(|| ctx.throw_type_error("expected a callable function"))?;
                ctx.check_interrupt()?;
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
                let realm = f.realm;
                return Err(ctx.with_realm(realm, |ctx| {
                    ctx.throw_type_error("class constructors must be invoked with 'new'")
                }));
            }
            (f.realm, Some(f.code), !f.is_derived_constructor)
        }
        _ => unreachable!(),
    };
    let compiled = code.and_then(|id| ctx.heap().bytecode.get(id).unwrap().compiled.as_ref());
    let layout = FrameLayout::new(base, count, compiled);
    let needed = layout.needed();
    if !ctx.runtime.vm.check_budget(needed, 1, 0) {
        return Err(ctx.stack_overflow());
    }
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
    if ctx.runtime.vm.stack.capacity() - ctx.runtime.vm.stack.len() < needed + 1 {
        ctx.root(this)?;
        ctx.reserve_stack(needed)?;
        ctx.runtime.vm.stack.pop();
    }
    for _ in 0..layout.storage() {
        ctx.runtime.vm.stack.push_reserved(Value::UNDEFINED);
    }
    let frame = layout.frame(callee, realm, ctx.realm, code, new_target, require_object);
    // The original call operands still contain new.target until the frame is live.
    if ctx.runtime.vm.frames.len() < ctx.runtime.vm.frames.capacity() {
        ctx.runtime.vm.frames.push_reserved(frame);
    } else {
        ctx.root(this)?;
        ctx.allocate(|runtime| runtime.vm.frames.push(&runtime.heap.ledger, frame))?;
        ctx.runtime.vm.stack.pop();
    }
    ctx.runtime.vm.stack[base + 1] = this;
    let kind = code
        .and_then(|code| ctx.heap().bytecode.get(code).unwrap().compiled.as_ref())
        .map(|function| function.kind);
    if let Some(kind) = kind {
        exec::coroutines::enter(ctx, ctx.runtime.vm.frames.len() - 1, kind)?;
    }
    Ok(())
}

fn push_forwarding_frame(
    ctx: &mut ExecCtx<'_, '_>,
    callee: ObjId,
    realm: RealmId,
    base: usize,
) -> JsResult<()> {
    let frame = CallFrame {
        callee,
        realm,
        caller_realm: ctx.realm,
        code: None,
        new_target: None,
        pc: 0,
        base: base as u32,
        arg_count: 0,
        arg_slots: 0,
        capture_base: 0,
        floor: 0,
        last_pc: 0,
        fast_ip: u32::MAX,
        return_call: true,
        require_object: false,
        coroutine: None,
    };
    if ctx.runtime.vm.frames.len() < ctx.runtime.vm.frames.capacity() {
        ctx.runtime.vm.frames.push_reserved(frame);
        Ok(())
    } else {
        ctx.allocate(|runtime| runtime.vm.frames.push(&runtime.heap.ledger, frame))
    }
}

pub(crate) fn not_a_constructor(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> Abrupt {
    if let ObjectData::BytecodeFunction(f) = &ctx.heap().object(id).data
        && let Some(function) = &ctx.heap().bytecode.get(f.code).unwrap().compiled
        && !function.flags.name_is_inferred
        && let Some(name) = function.name
    {
        let name = super::property::key_text(ctx.heap(), name);
        ctx.throw_type_error(&format!("{name} is not a constructor"))
    } else {
        ctx.throw_type_error("not a constructor")
    }
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
                exec::run_frame(ctx, index, code)
            } else {
                ctx.check_interrupt()
                    .and_then(|()| execute_mini(ctx, frame, index, code))
            }
        } else {
            native(ctx, frame)
                .and_then(|value| exec::native_stack(ctx, value))
                .map(Some)
        }
        .and_then(|mut result| {
            if let Some(value) = result
                && value.as_object().is_none()
            {
                if frame.require_object {
                    return Err(ctx.throw_type_error("Proxy construct trap must return an object"));
                }
                if frame.new_target.is_some() {
                    let this = ctx.runtime.vm.stack[frame.base as usize + 1];
                    if this.as_object().is_some() {
                        result = Some(this);
                    }
                }
            }
            result
                .map(|value| exec::coroutines::finish(ctx, frame, value))
                .transpose()
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
                if let Err(Abrupt::Interrupted) = exec::capture_throw(ctx) {
                    ctx.take_exception();
                    return Err(Abrupt::Interrupted);
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
    if (0x1000..=0x1fff).contains(&id.0) {
        return builtin(ctx, id, frame);
    }
    let base = frame.base as usize;
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

fn builtin(ctx: &mut ExecCtx<'_, '_>, id: NativeFnId, frame: CallFrame) -> JsResult<Value> {
    let base = frame.base as usize;
    RootScope::new(ctx.runtime, frame.realm).run(|ctx| {
        crate::builtins::dispatch(
            id,
            ctx,
            Handle::at(base + 1),
            Args::new(base + 2, frame.arg_count as usize),
        )
        .unwrap()
    })
}

#[inline(never)]
fn finish_builtin(ctx: &mut ExecCtx<'_, '_>, id: NativeFnId, frame: CallFrame) -> JsResult<()> {
    let value = builtin(ctx, id, frame)?;
    let value = exec::native_stack(ctx, value)?;
    ctx.realm = frame.caller_realm;
    let vm = &mut ctx.runtime.vm;
    vm.frames.pop();
    vm.stack.truncate(frame.base as usize);
    vm.stack.push_reserved(value);
    Ok(())
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
        let handled = exec::coroutines::reject_if_async(ctx, frame)?;
        ctx.runtime.vm.frames.pop();
        exec::coroutines::abort_frame(ctx, frame);
        ctx.runtime.vm.stack.truncate(frame.base as usize);
        if let Some(result) = handled {
            ctx.runtime.vm.stack.push_reserved(result);
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

    #[test]
    fn builtin_calls_resume_the_caller_after_reentry_and_throw() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let js = r#"
            function run() {
                const value = { valueOf() { return Math.max(1, 4); } };
                const error = { valueOf() { throw 'coercion'; } };
                let log = '';
                try { Math.min(value, error); } catch (e) { log += e; }
                return log + ':' + Math.min(value, 9);
            }
            run();
        "#;
        assert_eq!(
            engine.eval_to_string(js, EvalOptions::default()).unwrap(),
            "coercion:4"
        );
    }
}
