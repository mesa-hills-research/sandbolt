mod bindings;
mod cache;
pub(crate) mod coroutines;
pub(crate) mod eval;
mod iterators;
mod load;
pub(crate) mod modules;
pub(crate) mod numeric;
mod objects;
pub(crate) mod stack;

#[cfg(test)]
mod tests;

use qjs_bytecode::{ExceptionHandler, Instruction, Opcode, Operands};
use qjs_syntax::SourceText;

use crate::atom::{Atom, PropKey};
use crate::object::{ErrorKind, ObjectData, ObjectFlags};
use crate::ops::{call, compare, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::value::*;
use crate::vm::{CallFrame, CompiledFunc, VarRefState};

pub(crate) use bindings::{read_ref, write_ref};
pub(crate) use iterators::{async_from_sync_call, close_original, for_in_next, iterator_result};

pub(crate) use coroutines::resume_generator;
pub(crate) use objects::brand_key;

pub(crate) use stack::{capture_frames, capture_throw, native_stack, saved_frames};

impl<'s> ExecCtx<'_, 's> {
    pub fn function_source(&mut self, function: Handle<'s>) -> JsResult<Option<Handle<'s>>> {
        let id = self.object_id(function)?;
        let source = match &self.heap().object(id).data {
            ObjectData::BytecodeFunction(data) => self
                .heap()
                .bytecode
                .get(data.code)
                .unwrap()
                .compiled
                .as_ref()
                .and_then(|f| f.source),
            _ => None,
        };
        source.map(|id| self.root(Value::string(id))).transpose()
    }
}

pub(crate) fn utf16_lossy(heap: &crate::heap::Heap, id: StrId) -> String {
    char::decode_utf16(heap.string_units(id))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

pub(crate) fn array_from_stack<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    start: usize,
    count: usize,
) -> JsResult<Handle<'s>> {
    let array = ctx.new_array()?;
    let id = ctx.object_id(array)?;
    ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let ObjectData::Array(data) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        data.elements
            .extend_from_slice(&heap.ledger, &rt.vm.stack[start..start + count])?;
        data.length = count as u32;
        Ok(())
    })?;
    Ok(array)
}

pub(super) fn well_known(ctx: &ExecCtx<'_, '_>, symbol: crate::symbol::WellKnown) -> PropKey {
    let id = ctx.heap().symbol_registry.well_known(symbol);
    ctx.heap().symbols.get(id).unwrap().key()
}

fn key(ctx: &ExecCtx<'_, '_>, code: BcId, atom: qjs_bytecode::UnitAtom) -> PropKey {
    compiled_key(ctx.heap(), compiled(ctx, code), atom)
}

fn compiled_key(
    heap: &crate::heap::Heap,
    function: &CompiledFunc,
    atom: qjs_bytecode::UnitAtom,
) -> PropKey {
    let index = atom
        .raw()
        .wrapping_sub(qjs_bytecode::BuiltinAtom::SymbolToPrimitive as u32) as usize;
    if let Some(&symbol) = crate::symbol::WellKnown::ALL.get(index) {
        let id = heap.symbol_registry.well_known(symbol);
        heap.symbols.get(id).unwrap().key()
    } else {
        function.key(atom)
    }
}

fn compiled<'a>(ctx: &'a ExecCtx<'_, '_>, code: BcId) -> &'a CompiledFunc {
    ctx.heap()
        .bytecode
        .get(code)
        .unwrap()
        .compiled
        .as_ref()
        .unwrap()
}

fn replace(ctx: &mut ExecCtx<'_, '_>, drop: usize, values: &[Value]) {
    let stack = &mut ctx.runtime.vm.stack;
    stack.truncate(stack.len() - drop);
    for &value in values {
        stack.push_reserved(value);
    }
}

fn branch(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame, index: usize, target: u32) -> JsResult<()> {
    if target <= frame.pc {
        ctx.check_interrupt()?;
    }
    ctx.runtime.vm.frames[index].pc = target;
    Ok(())
}

pub(crate) fn run_frame(
    ctx: &mut ExecCtx<'_, '_>,
    mut index: usize,
    mut code: BcId,
) -> JsResult<Option<Value>> {
    use Opcode::*;
    let entry_index = index;
    'frames: loop {
        let frame = ctx.runtime.vm.frames[index];
        let arg_base = frame.base as usize + 2;
        let var_base = arg_base + frame.arg_slots as usize;
        let mut pc = frame.pc;
        loop {
            // A slow instruction can collect or reenter the VM. Take these borrows again after it.
            let heap = &mut ctx.runtime.heap;
            let vm = &mut ctx.runtime.vm;
            let bytecode = heap.bytecode.get(code).unwrap();
            let function = bytecode.compiled.as_ref().unwrap();
            let bytes = &function.code;
            let (at, op) = loop {
                if !function.hoisted.is_empty()
                    && let Ok(i) = function.hoisted.binary_search_by_key(&pc, |&(at, _)| at)
                {
                    pc = function.hoisted[i].1;
                    continue;
                }
                let at = pc;
                let pos = at as usize + 1;
                let byte = bytes[at as usize];
                let op = Opcode::ALL[byte as usize];
                pc += u32::from(op.info().size);
                let u16 = || u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
                let u32 = || {
                    u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                };
                let stack = &mut vm.stack;
                let tail = stack.len();
                match op {
                    Nop => {}
                    PushI32 | PushI16 | PushI8 | PushMinus1 | Push0 | Push1 | Push2 | Push3
                    | Push4 | Push5 | Push6 | Push7 => {
                        stack.push_reserved(Value::int32(match op {
                            PushI32 => u32() as i32,
                            PushI16 => i32::from(u16() as i16),
                            PushI8 => i32::from(bytes[pos] as i8),
                            _ => i32::from(byte) - Push0 as i32,
                        }))
                    }
                    PushBigintI32 => {
                        stack.push_reserved(Value::short_bigint(i64::from(u32() as i32)).unwrap());
                    }
                    Undefined => stack.push_reserved(Value::UNDEFINED),
                    Null => stack.push_reserved(Value::NULL),
                    PushFalse | PushTrue => stack.push_reserved(Value::bool(op == PushTrue)),
                    PushThis => {
                        let raw = stack[arg_base - 1];
                        let value = if function.strict || raw.as_object().is_some() {
                            raw
                        } else if raw == Value::NULL || raw == Value::UNDEFINED {
                            Value::object(heap.realm(frame.realm).global_object)
                        } else {
                            break (at, op);
                        };
                        stack.push_reserved(value);
                    }
                    SpecialObject if matches!(bytes[pos], 2 | 3) && function.eval.is_none() => {
                        let value = if bytes[pos] == 2 {
                            Value::object(frame.callee)
                        } else {
                            frame.new_target.map_or(Value::UNDEFINED, Value::object)
                        };
                        stack.push_reserved(value);
                    }
                    PushConst | PushConst8 => {
                        let i = if op == PushConst8 {
                            bytes[pos].into()
                        } else {
                            u32()
                        };
                        if !function.regex_constants.is_empty()
                            && function.regex_constants.binary_search(&i).is_ok()
                        {
                            break (at, op);
                        }
                        stack.push_reserved(bytecode.constants[i as usize]);
                    }
                    Drop => {
                        stack.pop();
                    }
                    Dup => stack.push_reserved(stack[tail - 1]),
                    Dup1 => {
                        let last = stack[tail - 1];
                        stack[tail - 1] = stack[tail - 2];
                        stack.push_reserved(last);
                    }
                    Swap => stack.swap(tail - 2, tail - 1),
                    Nip => {
                        let last = stack.pop().unwrap();
                        stack[tail - 2] = last;
                    }
                    GetLoc0Loc1 => {
                        stack.push_reserved(stack[var_base]);
                        stack.push_reserved(stack[var_base + 1]);
                    }
                    GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 | PutLoc | PutLoc8
                    | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 | SetLoc | SetLoc8 | SetLoc0
                    | SetLoc1 | SetLoc2 | SetLoc3 => {
                        let slot = var_base
                            + match op {
                                GetLoc | PutLoc | SetLoc => usize::from(u16()),
                                GetLoc8 | PutLoc8 | SetLoc8 => usize::from(bytes[pos]),
                                _ => usize::from((byte - GetLoc0 as u8) % 4),
                            };
                        match op {
                            GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => {
                                stack.push_reserved(stack[slot]);
                            }
                            PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => {
                                stack[slot] = stack.pop().unwrap();
                            }
                            _ => stack[slot] = stack[tail - 1],
                        }
                    }
                    GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3 | PutArg | PutArg0 | PutArg1
                    | PutArg2 | PutArg3 | SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3 => {
                        let slot = arg_base
                            + match op {
                                GetArg | PutArg | SetArg => usize::from(u16()),
                                _ => usize::from((byte - GetArg0 as u8) % 4),
                            };
                        match op {
                            GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3 => {
                                stack.push_reserved(stack[slot]);
                            }
                            PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3 => {
                                stack[slot] = stack.pop().unwrap();
                            }
                            _ => stack[slot] = stack[tail - 1],
                        }
                    }
                    GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3 | PutVarRef
                    | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3 | SetVarRef
                    | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3 => {
                        let i = match op {
                            GetVarRef | PutVarRef | SetVarRef => usize::from(u16()),
                            _ => usize::from((byte - GetVarRef0 as u8) % 4),
                        };
                        let ObjectData::BytecodeFunction(data) = &heap.object(frame.callee).data
                        else {
                            unreachable!()
                        };
                        let Some(ValueKind::VarRef(id)) = data.closure.get(i).map(|v| v.decode())
                        else {
                            break (at, op);
                        };
                        if matches!(
                            op,
                            GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3
                        ) {
                            let value = match heap.var_refs.get(id).unwrap().state {
                                VarRefState::OpenStack(i) => stack[i as usize],
                                VarRefState::OpenCoro { coro, local } => {
                                    heap.coroutines.get(coro).unwrap().stack[local as usize]
                                }
                                VarRefState::Closed(value) => value,
                            };
                            stack.push_reserved(value);
                        } else {
                            let value = stack[tail - 1];
                            let entry = heap.var_refs.get_mut(id).unwrap();
                            match entry.state {
                                VarRefState::OpenStack(i) => stack[i as usize] = value,
                                VarRefState::OpenCoro { coro, local } => {
                                    heap.coroutines.get_mut(coro).unwrap().stack[local as usize] =
                                        value;
                                }
                                VarRefState::Closed(_) => entry.state = VarRefState::Closed(value),
                            }
                            if matches!(
                                op,
                                PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3
                            ) {
                                stack.pop();
                            }
                        }
                    }
                    Add | Sub | Mul | Div | Mod | Shl | Sar | Shr | And | Or | Xor | Pow
                    | AddLoc => {
                        let lhs = if op == AddLoc {
                            var_base + usize::from(bytes[pos])
                        } else {
                            tail - 2
                        };
                        let binop = if op == AddLoc { Add } else { op };
                        let Some(result) = fast_binary(
                            binop,
                            stack[lhs],
                            stack[tail - 1],
                            vm.interrupt_handler.is_some(),
                        ) else {
                            break (at, op);
                        };
                        stack[lhs] = result;
                        stack.pop();
                    }
                    Neg | Plus | Not | Inc | Dec | PostInc | PostDec | IncLoc | DecLoc => {
                        let slot = if matches!(op, IncLoc | DecLoc) {
                            var_base + usize::from(bytes[pos])
                        } else {
                            tail - 1
                        };
                        let Some((before, after)) = fast_unary(op, stack[slot]) else {
                            break (at, op);
                        };
                        if matches!(op, PostInc | PostDec) {
                            stack[slot] = before;
                            stack.push_reserved(after);
                        } else {
                            stack[slot] = after;
                        }
                    }
                    Eq | Neq | StrictEq | StrictNeq | Lt | Lte | Gt | Gte => {
                        let (Some(a), Some(b)) =
                            (stack[tail - 2].as_number(), stack[tail - 1].as_number())
                        else {
                            break (at, op);
                        };
                        stack[tail - 2] = Value::bool(match op {
                            Eq | StrictEq => a == b,
                            Neq | StrictNeq => a != b,
                            Lt => a < b,
                            Lte => a <= b,
                            Gt => a > b,
                            Gte => a >= b,
                            _ => unreachable!(),
                        });
                        stack.pop();
                    }
                    Lnot => {
                        stack[tail - 1] =
                            Value::bool(!convert::to_boolean_value(heap, stack[tail - 1]));
                    }
                    GetField | GetField2 | PutField => {
                        let writing = op == PutField;
                        let object_slot = tail - 1 - usize::from(writing);
                        let Some(id) = stack[object_slot].as_object() else {
                            break (at, op);
                        };
                        let name = cache::Key::Field(qjs_bytecode::UnitAtom(u32()));
                        let Some(entry) = cache::own_slot(
                            heap,
                            function,
                            at,
                            id,
                            writing,
                            (0, name),
                            (&[], frame.realm),
                        ) else {
                            break (at, op);
                        };
                        let slot = entry.slot as usize;
                        match op {
                            PutField => {
                                heap.objects.get_mut(id).unwrap().props[slot] = stack[tail - 1];
                                stack.truncate(tail - 2);
                            }
                            GetField2 => stack.push_reserved(heap.object(id).props[slot]),
                            _ => stack[object_slot] = heap.object(id).props[slot],
                        }
                    }
                    GetVar | GetVarUndef | PutVar | MakeVarRef => {
                        if function.eval.is_some() {
                            break (at, op);
                        }
                        let id = heap.realm(frame.realm).global_object;
                        let atom = qjs_bytecode::UnitAtom(u32());
                        let name = if op == MakeVarRef {
                            cache::Key::GlobalRef(atom)
                        } else {
                            cache::Key::Global(atom)
                        };
                        let Some(entry) = cache::own_slot(
                            heap,
                            function,
                            at,
                            id,
                            op == PutVar,
                            (vm.global_lexical_epoch, name),
                            (&ctx.runtime.modules, frame.realm),
                        ) else {
                            break (at, op);
                        };
                        let slot = entry.slot as usize;
                        match op {
                            PutVar => {
                                heap.objects.get_mut(id).unwrap().props[slot] = stack.pop().unwrap()
                            }
                            MakeVarRef => {
                                stack.push_reserved(Value::object(id));
                                stack.push_reserved(entry.key);
                            }
                            _ => stack.push_reserved(heap.object(id).props[slot]),
                        }
                    }
                    GetRefValue | PutRefValue => {
                        let writing = op == PutRefValue;
                        let source = tail - 2 - usize::from(writing);
                        let global = heap.realm(frame.realm).global_object;
                        if stack[source] != Value::object(global) {
                            break (at, op);
                        }
                        let ValueKind::String(str) = stack[source + 1].decode() else {
                            break (at, op);
                        };
                        let Some(entry) = cache::own_slot(
                            heap,
                            function,
                            at,
                            global,
                            writing,
                            (0, cache::Key::ResolvedRef(str)),
                            (&[], frame.realm),
                        ) else {
                            break (at, op);
                        };
                        let slot = entry.slot as usize;
                        if writing {
                            heap.objects.get_mut(global).unwrap().props[slot] = stack[tail - 1];
                            stack.truncate(source);
                        } else {
                            stack.push_reserved(heap.object(global).props[slot]);
                        }
                    }
                    GetArrayEl | GetArrayEl2 | PutArrayEl => {
                        let writing = op == PutArrayEl;
                        let object_slot = tail - 2 - usize::from(writing);
                        let Some(id) = stack[object_slot].as_object() else {
                            break (at, op);
                        };
                        let ValueKind::Int32(index) = stack[object_slot + 1].decode() else {
                            break (at, op);
                        };
                        let Ok(index) = usize::try_from(index) else {
                            break (at, op);
                        };
                        let object = heap.object(id);
                        if writing {
                            let value = stack[tail - 1];
                            match &object.data {
                                ObjectData::Array(array)
                                    if object.flags.contains(
                                        ObjectFlags::FAST_ARRAY | ObjectFlags::EXTENSIBLE,
                                    ) && !object.flags.contains(ObjectFlags::IS_PROTOTYPE)
                                        && array
                                            .elements
                                            .get(index)
                                            .is_some_and(|&v| v != Value::EMPTY) =>
                                {
                                    let ObjectData::Array(array) =
                                        &mut heap.objects.get_mut(id).unwrap().data
                                    else {
                                        unreachable!()
                                    };
                                    array.elements[index] = value;
                                }
                                ObjectData::TypedArray(view) => {
                                    let view = **view;
                                    if crate::builtins::typed_atomics::fast_typed_array_set(
                                        &mut heap.objects,
                                        view,
                                        index,
                                        value,
                                    )
                                    .is_none()
                                    {
                                        break (at, op);
                                    }
                                }
                                _ => break (at, op),
                            }
                            stack.truncate(object_slot);
                        } else {
                            let value = match &object.data {
                                ObjectData::Array(array)
                                    if object.flags.contains(ObjectFlags::FAST_ARRAY) =>
                                {
                                    array
                                        .elements
                                        .get(index)
                                        .copied()
                                        .filter(|&v| v != Value::EMPTY)
                                }
                                ObjectData::TypedArray(view) => {
                                    crate::builtins::typed_atomics::fast_typed_array_get(
                                        heap, **view, index,
                                    )
                                }
                                _ => None,
                            };
                            let Some(value) = value else { break (at, op) };
                            if op == GetArrayEl2 {
                                stack[tail - 1] = value;
                            } else {
                                stack[object_slot] = value;
                                stack.pop();
                            }
                        }
                    }
                    Goto | Goto8 | Goto16 | IfFalse | IfTrue | IfFalse8 | IfTrue8 => {
                        let conditional = !matches!(op, Goto | Goto8 | Goto16);
                        if !conditional
                            || matches!(op, IfTrue | IfTrue8)
                                == convert::to_boolean_value(heap, stack[tail - 1])
                        {
                            let offset = match op {
                                Goto8 | IfFalse8 | IfTrue8 => i32::from(bytes[pos] as i8),
                                Goto16 => i32::from(u16() as i16),
                                _ => u32() as i32,
                            };
                            let target = (at + 1).wrapping_add_signed(offset);
                            if target <= at && vm.interrupt_handler.is_some() {
                                break (at, op);
                            }
                            pc = target;
                        }
                        if conditional {
                            stack.pop();
                        }
                    }
                    Call | Call0 | Call1 | Call2 | Call3 | CallMethod => {
                        let count = match op {
                            Call | CallMethod => usize::from(u16()),
                            _ => usize::from(byte - Call0 as u8),
                        };
                        let Some(plain) =
                            call::prepare_plain(heap, vm, frame.realm, count, op == CallMethod)
                        else {
                            break (at, op);
                        };
                        vm.frames[index].last_pc = at;
                        vm.frames[index].pc = pc;
                        (code, ctx.realm) = plain.enter(ctx)?;
                        index += 1;
                        continue 'frames;
                    }
                    Return | ReturnUndef | ReturnAsync => {
                        let value = if op == ReturnUndef {
                            Value::UNDEFINED
                        } else {
                            stack[tail - 1]
                        };
                        if index == entry_index {
                            vm.frames[index].last_pc = at;
                            vm.frames[index].pc = pc;
                            return Ok(Some(value));
                        }
                        bindings::close_frame_slots(&mut heap.var_refs, stack, frame);
                        vm.frames.pop();
                        stack.truncate(frame.base as usize);
                        stack.push_reserved(value);
                        index -= 1;
                        let caller = &vm.frames[index];
                        code = caller.code.unwrap();
                        ctx.realm = caller.realm;
                        continue 'frames;
                    }
                    _ => break (at, op),
                }
            };
            let strict = function.strict;
            let inst = Instruction::decode(bytes, at as usize).expect("verified instruction");
            debug_assert_eq!(inst.opcode, op);
            let frame = &mut vm.frames[index];
            frame.last_pc = at;
            frame.pc = pc;
            let mut original = *frame;
            original.pc = at;
            let result = slow_op(ctx, original, index, code, inst, strict, pc)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(result);
            }
            pc = ctx.runtime.vm.frames[index].pc;
        }
    }
}

#[inline]
fn fast_binary(op: Opcode, lhs: Value, rhs: Value, has_interrupt: bool) -> Option<Value> {
    use Opcode::*;
    match (lhs.decode(), rhs.decode()) {
        (ValueKind::Int32(a), ValueKind::Int32(b)) => {
            let integer = match op {
                Add => a.checked_add(b),
                Sub => a.checked_sub(b),
                Mul if !(a == 0 && b < 0) && !(b == 0 && a < 0) => a.checked_mul(b),
                And => Some(a & b),
                Or => Some(a | b),
                Xor => Some(a ^ b),
                Shl => Some(a.wrapping_shl(b as u32)),
                Sar => Some(a.wrapping_shr(b as u32)),
                _ => None,
            };
            Some(integer.map_or_else(
                || numeric::number_binary(op, a.into(), b.into()),
                Value::int32,
            ))
        }
        (ValueKind::ShortBigInt(a), ValueKind::ShortBigInt(b)) => {
            numeric::short_binary(op, a, b, has_interrupt)
        }
        _ => Some(numeric::number_binary(
            op,
            lhs.as_number()?,
            rhs.as_number()?,
        )),
    }
}

#[inline]
fn fast_unary(op: Opcode, value: Value) -> Option<(Value, Value)> {
    use Opcode::*;
    let op = match op {
        IncLoc => Inc,
        DecLoc => Dec,
        _ => op,
    };
    match value.decode() {
        ValueKind::Int32(n) => {
            let int = match op {
                Inc | PostInc => n.checked_add(1),
                Dec | PostDec => n.checked_sub(1),
                Not => Some(!n),
                Plus => Some(n),
                _ => None,
            };
            Some(int.map_or_else(
                || numeric::number_unary(op, n.into()),
                |after| (value, Value::int32(after)),
            ))
        }
        ValueKind::Float64(n) => Some(numeric::number_unary(op, n)),
        ValueKind::ShortBigInt(n) => Some((value, numeric::short_unary(op, n)?)),
        _ => None,
    }
}

#[cold]
fn slow_op(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    index: usize,
    code: BcId,
    inst: Instruction,
    strict: bool,
    next: u32,
) -> JsResult<Option<Value>> {
    use Opcode::*;
    let op = inst.opcode;
    let this = frame.base as usize + 1;
    let arg_start = this + 1;
    let local_start = arg_start + frame.arg_slots as usize;
    let tail = ctx.runtime.vm.stack.len();
    let handle = |n: usize| Handle::at(tail - n);
    let value = |ctx: &ExecCtx<'_, '_>, n: usize| ctx.runtime.vm.stack[tail - n];
    match op {
        Invalid => unreachable!("verified opcode"),
        PushConst | PushConst8 => {
            let Operands::Constant(c) = inst.operands else {
                unreachable!()
            };
            let bc = ctx.heap().bytecode.get(code).unwrap();
            let constant = bc.constants[c as usize];
            let is_regex = !bc.compiled.as_ref().unwrap().regex_constants.is_empty()
                && bc
                    .compiled
                    .as_ref()
                    .unwrap()
                    .regex_constants
                    .binary_search(&c)
                    .is_ok();
            let result = if is_regex {
                ctx.with_scope(|ctx| objects::clone_regexp(ctx, constant))?
            } else {
                constant
            };
            replace(ctx, 0, &[result]);
        }
        PushEmptyString | PushAtomValue | PrivateSymbol | Object | PushThis | SpecialObject
        | Rest | ArrayFrom | Fclosure | Fclosure8 => {
            let mut consumed = 0;
            let result = ctx.with_scope(|ctx| {
                let result = match op {
                    PushEmptyString => ctx.new_string("")?,
                    PushAtomValue | PrivateSymbol => {
                        let Operands::Atom(atom) = inst.operands else {
                            unreachable!()
                        };
                        let key = key(ctx, code, atom);
                        let name = convert::key_value(ctx, key)?;
                        if op == PrivateSymbol {
                            ctx.new_symbol(Some(name), true)?
                        } else {
                            name
                        }
                    }
                    Object => ctx.new_object()?,
                    PushThis => {
                        let raw = ctx.runtime.vm.stack[this];
                        if strict {
                            return Ok(raw);
                        }
                        if raw == Value::NULL || raw == Value::UNDEFINED {
                            return Ok(Value::object(ctx.realm().global_object));
                        }
                        convert::to_object(ctx, Handle::at(this))?
                    }
                    SpecialObject => {
                        let Operands::Unsigned(selector) = inst.operands else {
                            unreachable!()
                        };
                        return bindings::special_object(ctx, frame, code, selector);
                    }
                    ArrayFrom | Rest => {
                        let (start, count) = match inst.operands {
                            Operands::Call(count) => {
                                consumed = count as usize;
                                (tail - consumed, consumed)
                            }
                            Operands::Unsigned(offset) => (
                                arg_start + offset.min(frame.arg_count) as usize,
                                frame.arg_count.saturating_sub(offset) as usize,
                            ),
                            _ => unreachable!(),
                        };
                        array_from_stack(ctx, start, count)?
                    }
                    Fclosure | Fclosure8 => {
                        let Operands::Constant(c) = inst.operands else {
                            unreachable!()
                        };
                        let function = ctx.heap().bytecode.get(code).unwrap();
                        let constant = function.constants[c as usize];
                        let ValueKind::Bytecode(child) = constant.decode() else {
                            unreachable!()
                        };
                        return bindings::closure(ctx, frame, child);
                    }
                    _ => unreachable!(),
                };
                Ok(ctx.get(result))
            })?;
            replace(ctx, consumed, &[result]);
        }
        Nip1 => replace(ctx, 3, &[value(ctx, 2), value(ctx, 1)]),
        Dup2 => replace(ctx, 0, &[value(ctx, 2), value(ctx, 1)]),
        Dup3 => replace(ctx, 0, &[value(ctx, 3), value(ctx, 2), value(ctx, 1)]),
        Insert2 => replace(ctx, 2, &[value(ctx, 1), value(ctx, 2), value(ctx, 1)]),
        Insert3 => replace(
            ctx,
            3,
            &[value(ctx, 1), value(ctx, 3), value(ctx, 2), value(ctx, 1)],
        ),
        Insert4 => replace(
            ctx,
            4,
            &[
                value(ctx, 1),
                value(ctx, 4),
                value(ctx, 3),
                value(ctx, 2),
                value(ctx, 1),
            ],
        ),
        Perm3 => ctx.runtime.vm.stack[tail - 3..tail - 1].rotate_right(1),
        Perm4 => ctx.runtime.vm.stack[tail - 4..tail - 1].rotate_right(1),
        Perm5 => ctx.runtime.vm.stack[tail - 5..tail - 1].rotate_right(1),
        Swap2 => ctx.runtime.vm.stack[tail - 4..].rotate_left(2),
        Rot3l => ctx.runtime.vm.stack[tail - 3..].rotate_left(1),
        Rot4l => ctx.runtime.vm.stack[tail - 4..].rotate_left(1),
        Rot5l => ctx.runtime.vm.stack[tail - 5..].rotate_left(1),
        Rot3r => ctx.runtime.vm.stack[tail - 3..].rotate_right(1),
        Call | Call0 | Call1 | Call2 | Call3 | CallMethod | CallConstructor | TailCall
        | TailCallMethod => {
            let Operands::Call(count) = inst.operands else {
                unreachable!()
            };
            let count = count as usize;
            let mut target = None;
            let start = match op {
                CallMethod | TailCallMethod => {
                    let start = tail - count - 2;
                    ctx.runtime.vm.stack.swap(start, start + 1);
                    start
                }
                CallConstructor => {
                    let start = tail - count - 2;
                    target = Some(
                        ctx.runtime.vm.stack[start + 1]
                            .as_object()
                            .ok_or_else(|| ctx.throw_type_error("not a function"))?,
                    );
                    start
                }
                _ => {
                    let start = tail - count - 1;
                    ctx.reserve_stack(1)?;
                    ctx.runtime.vm.stack.push_reserved(Value::UNDEFINED);
                    ctx.runtime.vm.stack[start + 1..].rotate_right(1);
                    start
                }
            };
            call::enter(ctx, start, count, target)?;
            if matches!(op, TailCall | TailCallMethod) {
                ctx.runtime.vm.frames[index].return_call = true;
            }
        }
        Apply => {
            let Operands::Unsigned(mode) = inst.operands else {
                unreachable!()
            };
            spread_call(ctx, tail - 3, handle(3), handle(2), handle(1), mode)?;
        }
        Throw => return Err(ctx.throw_value(value(ctx, 1), false)),
        ThrowError => {
            let Operands::AtomU8(atom, kind) = inst.operands else {
                unreachable!()
            };
            let key = key(ctx, code, atom);
            let name = ctx.with_scope(|ctx| {
                let h = convert::key_value(ctx, key)?;
                let ValueKind::String(id) = ctx.get(h).decode() else {
                    unreachable!()
                };
                Ok(utf16_lossy(ctx.heap(), id))
            })?;
            return Err(match kind {
                0 => ctx.throw_type_error(&format!("'{name}' is read-only")),
                1 => ctx.throw_error(ErrorKind::Syntax, &format!("redeclaration of '{name}'")),
                2 => ctx.throw_error(
                    ErrorKind::Reference,
                    &format!("'{name}' is not initialized"),
                ),
                3 => ctx.throw_error(ErrorKind::Reference, "unsupported reference to 'super'"),
                4 => ctx.throw_type_error("iterator does not have a throw method"),
                _ => ctx.throw_error(ErrorKind::Internal, "invalid throw error kind"),
            });
        }
        CheckCtor => {
            if frame.new_target.is_none() {
                return Err(ctx.throw_type_error("class constructors must be invoked with 'new'"));
            }
        }
        CheckCtorReturn => {
            let raw = value(ctx, 1);
            if raw != Value::UNDEFINED && raw.as_object().is_none() {
                return Err(ctx.with_realm(frame.caller_realm, |ctx| {
                    ctx.throw_type_error(
                        "derived class constructor must return an object or undefined",
                    )
                }));
            }
            replace(ctx, 0, &[Value::bool(raw == Value::UNDEFINED)]);
        }
        InitCtor => {
            let Some(target) = frame.new_target else {
                return Err(ctx.throw_type_error("class constructors must be invoked with 'new'"));
            };
            let proto = ctx.with_scope(|ctx| {
                let callee = ctx.root(Value::object(frame.callee))?;
                property::get_prototype_of(ctx, callee)
                    .map(|p| p.map_or(Value::NULL, Value::object))
            })?;
            ctx.reserve_stack(usize::try_from(frame.arg_count).unwrap() + 2)?;
            replace(ctx, 0, &[proto, Value::object(target)]);
            for pos in arg_start..arg_start + frame.arg_count as usize {
                let arg = ctx.runtime.vm.stack[pos];
                replace(ctx, 0, &[arg]);
            }
            call::enter(ctx, tail, frame.arg_count as usize, Some(target))?;
        }
        Goto | Goto8 | Goto16 | IfFalse | IfFalse8 | IfTrue | IfTrue8 | Catch | Gosub => {
            let target = inst.branch_target(frame.pc).unwrap() as u32;
            match op {
                Catch => replace(ctx, 0, &[Value::catch_offset(target + 1)]),
                Gosub => {
                    replace(ctx, 0, &[Value::int32(next as i32)]);
                    branch(ctx, frame, index, target)?;
                }
                IfFalse | IfFalse8 | IfTrue | IfTrue8 => {
                    let condition = convert::to_boolean_value(ctx.heap(), value(ctx, 1));
                    replace(ctx, 1, &[]);
                    if matches!(op, IfTrue | IfTrue8) == condition {
                        branch(ctx, frame, index, target)?;
                    }
                }
                _ => branch(ctx, frame, index, target)?,
            }
        }
        Ret => {
            let ValueKind::Int32(pc) = value(ctx, 1).decode() else {
                return Err(ctx.throw_type_error("invalid finally return address"));
            };
            replace(ctx, 1, &[]);
            branch(ctx, frame, index, pc as u32)?;
        }
        NipCatch => {
            let start = frame.floor as usize;
            let offset = ctx.runtime.vm.stack[start..tail - 1]
                .iter()
                .rposition(|v| matches!(v.decode(), ValueKind::CatchOffset(_)));
            let Some(offset) = offset else {
                return Err(ctx.throw_type_error("missing exception marker"));
            };
            replace(ctx, tail - start - offset, &[value(ctx, 1)]);
        }
        GetLocCheck | PutLocCheck | PutLocCheckInit | SetLocUninitialized | CloseLoc => {
            let Operands::Local(n) = inst.operands else {
                unreachable!()
            };
            let slot = local_start + n as usize;
            let current = ctx.runtime.vm.stack[slot];
            match op {
                GetLocCheck => {
                    if current == Value::UNINITIALIZED {
                        ctx.with_realm(frame.caller_realm, |ctx| {
                            bindings::check_initialized(ctx, current)
                        })?;
                    }
                    replace(ctx, 0, &[current]);
                }
                CloseLoc => bindings::close_local(ctx, frame, slot),
                SetLocUninitialized => ctx.runtime.vm.stack[slot] = Value::UNINITIALIZED,
                _ => {
                    if op == PutLocCheck && current == Value::UNINITIALIZED {
                        ctx.with_realm(frame.caller_realm, |ctx| {
                            bindings::check_initialized(ctx, current)
                        })?;
                    }
                    if op == PutLocCheckInit && current != Value::UNINITIALIZED {
                        return Err(
                            ctx.throw_error(ErrorKind::Reference, "binding already initialized")
                        );
                    }
                    ctx.runtime.vm.stack[slot] = value(ctx, 1);
                    replace(ctx, 1, &[]);
                }
            }
        }
        GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3 | GetVarRefCheck
        | PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3 | PutVarRefCheck
        | PutVarRefCheckInit | SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3 => {
            bindings::var_op(ctx, frame, inst)?;
        }
        GetVar | GetVarUndef | PutVar | PutVarInit | DefineVar | CheckDefineVar | DefineFunc
        | DeleteVar | MakeVarRef | MakeLocRef | MakeArgRef | MakeVarRefRef | GetRefValue
        | PutRefValue | WithGetVar | WithPutVar | WithDeleteVar | WithMakeRef | WithGetRef
        | WithGetRefUndef => {
            bindings::binding_op(ctx, frame, code, index, inst)?;
        }
        Neg | Plus | Dec | Inc | PostDec | PostInc | Not | DecLoc | IncLoc | AddLoc => {
            let local = match inst.operands {
                Operands::Local(i) => Some(local_start + i as usize),
                _ => None,
            };
            let slot = local.unwrap_or(tail - 1);
            let (before, after) = ctx.with_scope(|ctx| {
                if op == AddLoc {
                    return numeric::binary(ctx, Add, Handle::at(slot), Handle::at(tail - 1))
                        .map(|v| (Value::UNDEFINED, v));
                }
                let op = match op {
                    IncLoc => Inc,
                    DecLoc => Dec,
                    other => other,
                };
                numeric::unary(ctx, op, Handle::at(slot))
            })?;
            if let Some(slot) = local {
                ctx.runtime.vm.stack[slot] = after;
                if op == AddLoc {
                    replace(ctx, 1, &[]);
                }
            } else if matches!(op, PostDec | PostInc) {
                replace(ctx, 1, &[before, after]);
            } else {
                replace(ctx, 1, &[after]);
            }
        }
        Mul | Div | Mod | Add | Sub | Shl | Sar | Shr | And | Xor | Or | Pow => {
            let result = ctx.with_scope(|ctx| {
                numeric::binary(ctx, op, Handle::at(tail - 2), Handle::at(tail - 1))
            })?;
            replace(ctx, 2, &[result]);
        }
        Eq | Neq | StrictEq | StrictNeq | Lt | Lte | Gt | Gte => {
            let result = if matches!(op, StrictEq | StrictNeq) {
                compare::strict_eq(ctx, Handle::at(tail - 2), Handle::at(tail - 1))
                    ^ (op == StrictNeq)
            } else {
                ctx.with_scope(|ctx| {
                    let (a, b) = (Handle::at(tail - 2), Handle::at(tail - 1));
                    Ok(match op {
                        Eq | Neq => compare::abstract_eq(ctx, a, b)? ^ (op == Neq),
                        Lt => compare::abstract_relational(ctx, a, b, true)? == Some(true),
                        Gt => compare::abstract_relational(ctx, b, a, false)? == Some(true),
                        Lte => compare::abstract_relational(ctx, b, a, false)? == Some(false),
                        Gte => compare::abstract_relational(ctx, a, b, true)? == Some(false),
                        _ => unreachable!(),
                    })
                })?
            };
            replace(ctx, 2, &[Value::bool(result)]);
        }
        Typeof | TypeofIsFunction | TypeofIsUndefined | IsUndefinedOrNull | IsUndefined
        | IsNull => {
            let raw = value(ctx, 1);
            let result = match op {
                Typeof => {
                    let name = convert::typeof_str(ctx, handle(1));
                    ctx.with_scope(|ctx| {
                        let name = ctx.new_string(name)?;
                        Ok(ctx.get(name))
                    })?
                }
                TypeofIsFunction => Value::bool(convert::typeof_str(ctx, handle(1)) == "function"),
                TypeofIsUndefined => {
                    Value::bool(convert::typeof_str(ctx, handle(1)) == "undefined")
                }
                IsUndefinedOrNull => Value::bool(raw == Value::UNDEFINED || raw == Value::NULL),
                IsUndefined => Value::bool(raw == Value::UNDEFINED),
                IsNull => Value::bool(raw == Value::NULL),
                _ => unreachable!(),
            };
            replace(ctx, 1, &[result]);
        }
        GetField | GetField2 | PutField | GetLength | GetArrayEl | GetArrayEl2 | PutArrayEl
        | DefineField | DefineArrayEl | GetSuper | GetSuperValue | PutSuperValue
        | GetPrivateField | PutPrivateField | DefinePrivateField | CheckBrand | AddBrand
        | PrivateIn | ToObject | ToPropkey | ToPropkey2 | CheckObject | In | Instanceof
        | Delete | SetName | SetNameComputed | SetProto | SetHomeObject | DefineMethod
        | DefineMethodComputed | DefineClass | DefineClassComputed | CopyDataProperties
        | Append | Regexp => {
            objects::object_op(ctx, frame, code, inst)?;
        }
        ForInStart | ForInNext | ForOfStart | ForAwaitOfStart | ForOfNext | IteratorClose
        | IteratorGetValueDone | IteratorNext | IteratorCall | UsingCheck | UsingDisposeInit
        | UsingDispose | UsingDisposeAsync | UsingDisposeMerge | UsingDisposeEnd => {
            iterators::iterator_op(ctx, frame, inst)?;
        }
        Eval | ApplyEval => {
            let (start, count, scope) = match inst.operands {
                Operands::Eval { arguments, scope } => {
                    let start = tail - arguments as usize - 1;
                    ctx.reserve_stack(1)?;
                    replace(ctx, 0, &[Value::UNDEFINED]);
                    ctx.runtime.vm.stack[start + 1..].rotate_right(1);
                    (start, arguments as usize, scope)
                }
                Operands::Unsigned(scope) => {
                    let start = tail - 2;
                    let callee = ctx.runtime.vm.stack[start];
                    let count = expand_array_like(
                        ctx,
                        start,
                        callee,
                        Value::UNDEFINED,
                        Handle::at(start + 1),
                        false,
                    )?;
                    (start, count, scope as u16)
                }
                _ => unreachable!(),
            };
            if eval::is_intrinsic(ctx, ctx.runtime.vm.stack[start]) {
                let input = if count == 0 {
                    Value::UNDEFINED
                } else {
                    ctx.runtime.vm.stack[start + 2]
                };
                let result = ctx.with_scope(|ctx| eval::execute(ctx, frame, scope, input))?;
                replace(ctx, count + 2, &[result]);
            } else {
                call::enter(ctx, start, count, None)?;
            }
        }
        InitialYield | Yield | YieldStar | AsyncYieldStar | Await => {
            return coroutines::suspend(ctx, index, op).map(Some);
        }
        Import => {
            let resolution = ctx.with_scope(|ctx| {
                modules::dynamic_import(ctx, code, Handle::at(tail - 2), Handle::at(tail - 1))
            })?;
            replace(ctx, 2, &[resolution]);
        }
        _ => unreachable!("instruction handled by the dispatch loop"),
    }
    Ok(None)
}

fn spread_call<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    base: usize,
    callee: Handle<'s>,
    this: Handle<'s>,
    array: Handle<'s>,
    mode: u32,
) -> JsResult<()> {
    let this_value = ctx.get(this);
    let new_target = (mode == 1)
        .then(|| {
            this_value
                .as_object()
                .ok_or_else(|| ctx.throw_type_error("not a function"))
        })
        .transpose()?;
    if mode == 1
        && !ctx
            .get(callee)
            .as_object()
            .is_some_and(|id| call::is_callable(ctx.heap(), id))
    {
        return Err(ctx.throw_type_error("not a function"));
    }
    let count = expand_array_like(ctx, base, ctx.get(callee), this_value, array, mode != 1)?;
    call::enter(ctx, base, count, new_target)
}

fn expand_array_like<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    base: usize,
    callee: Value,
    this: Value,
    arg_array: Handle<'s>,
    allow_nullish: bool,
) -> JsResult<usize> {
    let raw = ctx.get(arg_array);
    let valid = callee
        .as_object()
        .is_some_and(|id| call::is_callable(ctx.heap(), id));
    if !valid {
        return Err(ctx.throw_type_error("expected a callable function"));
    }
    let count = if allow_nullish && (raw == Value::NULL || raw == Value::UNDEFINED) {
        0
    } else {
        ctx.object_id(arg_array)?;
        ctx.with_scope(|ctx| {
            let arg_array = ctx.root(raw)?;
            let n = property::get_property(ctx, arg_array, Atom::get("length"))?;
            let n = ctx.root(n)?;
            convert::to_length(ctx, n)
        })?
    };
    let count = usize::try_from(count).map_err(|_| ctx.stack_overflow())?;
    ctx.reserve_stack(count.saturating_add(2))?;
    let start = ctx.runtime.vm.stack.len();
    replace(ctx, 0, &[callee, this]);
    for n in 0..count {
        let value = ctx.with_scope(|ctx| {
            let arg_array = ctx.root(raw)?;
            let key = load::index_key(ctx, n as u64)?;
            property::get_property(ctx, arg_array, key)
        })?;
        replace(ctx, 0, &[value]);
    }
    ctx.runtime
        .vm
        .stack
        .copy_within(start..start + count + 2, base);
    ctx.runtime.vm.stack.truncate(base + count + 2);
    Ok(count)
}

#[derive(Clone, Copy)]
pub(crate) enum NativeForward {
    Call,
    Apply,
    ReflectApply,
    ReflectConstruct,
}

impl NativeForward {
    pub(crate) fn from_id(id: u16) -> Option<Self> {
        Some(match id.wrapping_sub(crate::builtins::CORE_BASE) {
            0x43 => Self::Call,
            0x41 => Self::Apply,
            0x80 => Self::ReflectApply,
            0x81 => Self::ReflectConstruct,
            _ => return None,
        })
    }
}

pub(crate) fn transform_native_call(
    ctx: &mut ExecCtx<'_, '_>,
    forward: NativeForward,
    base: usize,
    count: usize,
) -> JsResult<(usize, Option<ObjId>)> {
    let arg = |ctx: &ExecCtx<'_, '_>, i| {
        if i < count {
            ctx.runtime.vm.stack[base + 2 + i]
        } else {
            Value::UNDEFINED
        }
    };
    if matches!(forward, NativeForward::Call) {
        let this = arg(ctx, 0);
        ctx.runtime.vm.stack[base] = ctx.runtime.vm.stack[base + 1];
        ctx.runtime.vm.stack[base + 1] = this;
        if count > 0 {
            ctx.runtime.vm.stack.remove(base + 2);
        }
        return Ok((count.saturating_sub(1), None));
    }
    let (callee, this, array, new_target, nullish) = match forward {
        NativeForward::Apply => (
            ctx.runtime.vm.stack[base + 1],
            arg(ctx, 0),
            arg(ctx, 1),
            None,
            true,
        ),
        NativeForward::ReflectApply => (arg(ctx, 0), arg(ctx, 1), arg(ctx, 2), None, false),
        NativeForward::ReflectConstruct => {
            let callee = arg(ctx, 0);
            let target = if count > 2 { arg(ctx, 2) } else { callee };
            for value in [callee, target] {
                if !value.as_object().is_some_and(|id| {
                    ctx.heap()
                        .object(id)
                        .flags
                        .contains(ObjectFlags::IS_CONSTRUCTOR)
                }) {
                    return Err(ctx.throw_type_error("expected a constructor"));
                }
            }
            (callee, target, arg(ctx, 1), target.as_object(), false)
        }
        NativeForward::Call => unreachable!(),
    };
    // Array getters may collect, so retain the argument list while it is expanded.
    let array = ctx.root(array)?;
    let count = expand_array_like(ctx, base, callee, this, array, nullish)?;
    Ok((count, new_target))
}

pub(crate) fn unwind_frame(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    index: usize,
    handler: Option<ExceptionHandler>,
) -> JsResult<bool> {
    let floor = frame.floor as usize;
    while let Some(offset) = ctx.runtime.vm.stack[floor..]
        .iter()
        .rposition(|v| matches!(v.decode(), ValueKind::CatchOffset(_)))
    {
        let pos = floor + offset;
        let ValueKind::CatchOffset(target) = ctx.runtime.vm.stack[pos].decode() else {
            unreachable!()
        };
        ctx.runtime.vm.stack.truncate(pos);
        if target == 0 {
            if pos >= floor + 2 {
                iterators::close_exception_iterator(ctx, pos - 2)?;
            }
            ctx.runtime
                .vm
                .stack
                .truncate(pos.saturating_sub(2).max(floor));
            continue;
        }
        let value = ctx
            .take_exception()
            .map_or(Value::UNDEFINED, |(value, _)| value);
        replace(ctx, 0, &[value]);
        ctx.runtime.vm.frames[index].pc = target - 1;
        return Ok(true);
    }
    if let Some(handler) = handler {
        let value = ctx
            .take_exception()
            .map_or(Value::UNDEFINED, |(value, _)| value);
        ctx.runtime
            .vm
            .stack
            .truncate(floor + handler.stack_depth as usize);
        replace(ctx, 0, &[value]);
        ctx.runtime.vm.frames[index].pc = handler.handler_pc;
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn execute_image(
    ctx: &mut ExecCtx<'_, '_>,
    image: &qjs_bytecode::ProgramImage,
    source: Option<&SourceText>,
) -> JsResult<Value> {
    if !image.modules.is_empty() {
        return modules::evaluate_image(ctx, image, source);
    }
    let entry = load::load(ctx, image, source)?;
    let this = ctx.root(Value::object(ctx.realm().global_object))?;
    call::call(ctx, entry, this, Args::empty())
}

fn define_default<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
) -> JsResult<()> {
    define_flags(ctx, object, key, value, PropFlags::DEFAULT_DATA)
}

fn define_flags<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
    flags: PropFlags,
) -> JsResult<()> {
    let desc = property::PropertyDescriptor {
        value: Some(value),
        writable: Some(flags.contains(PropFlags::WRITABLE)),
        enumerable: Some(flags.contains(PropFlags::ENUMERABLE)),
        configurable: Some(flags.contains(PropFlags::CONFIGURABLE)),
        ..Default::default()
    };
    if property::define_own_property(ctx, object, key, desc)? {
        Ok(())
    } else {
        Err(ctx.throw_type_error("cannot define property"))
    }
}
