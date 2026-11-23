mod bindings;
pub(crate) mod coroutines;
mod iterators;
mod load;
pub(crate) mod modules;
pub(crate) mod numeric;
mod objects;

#[cfg(test)]
mod tests;

use qjs_bytecode::{ExceptionHandler, Instruction, Opcode, Operands};
use qjs_syntax::SourceText;

use crate::atom::{Atom, AtomTable, KeyKind, PropKey};
use crate::object::{ErrorKind, ObjectData, ObjectFlags};
use crate::ops::{call, compare, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::value::*;
use crate::vm::{CallFrame, CompiledFunc};

pub(crate) use bindings::{read_ref, write_ref};
pub(crate) use iterators::{async_from_sync_call, close_original, for_in_next, iterator_result};

pub(crate) use coroutines::resume_generator;

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

    pub fn capture_error_stack(&mut self, error: Handle<'s>) -> JsResult<()> {
        let id = self.object_id(error)?;
        if !matches!(&self.heap().object(id).data, ObjectData::Error(data) if data.stack == Value::UNDEFINED)
        {
            return Ok(());
        }
        use std::fmt::Write;
        let frames = capture_frames(self);
        let mut text = String::new();
        for frame in frames {
            if frame.function.is_empty() {
                writeln!(
                    text,
                    "    at {}:{}:{}",
                    frame.file, frame.line, frame.column
                )
                .unwrap();
            } else {
                writeln!(
                    text,
                    "    at {} ({}:{}:{})",
                    frame.function, frame.file, frame.line, frame.column
                )
                .unwrap();
            }
        }
        let text = self.new_string(&text)?;
        let text = self.get(text);
        let ObjectData::Error(data) = &mut self.runtime.heap.objects.get_mut(id).unwrap().data
        else {
            unreachable!()
        };
        data.stack = text;
        Ok(())
    }
}

pub(crate) fn capture_frames(ctx: &ExecCtx<'_, '_>) -> Vec<crate::Frame> {
    let heap = ctx.heap();
    let mut frames = Vec::new();
    for frame in ctx.runtime.vm.frames.iter().rev() {
        let Some(code) = frame.code else {
            continue;
        };
        let Some(f) = &heap.bytecode.get(code).unwrap().compiled else {
            continue;
        };
        let end = f.pc_map.partition_point(|p| p.pc <= frame.last_pc);
        let byte = f.pc_map[..end]
            .last()
            .map_or(f.source_start, |p| p.byte_offset);
        let line = f
            .line_starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1);
        let column = byte.saturating_sub(f.line_starts[line]) + 1;
        frames.push(crate::Frame {
            file: String::from_utf8_lossy(&f.filename).into_owned(),
            function: f.name.map_or_else(String::new, |key| match key.decode() {
                KeyKind::Index(i) => i.to_string(),
                KeyKind::String(atom) => AtomTable::builtin(atom)
                    .map(str::to_owned)
                    .or_else(|| {
                        heap.atoms
                            .dynamic_string(atom)
                            .map(|id| utf16_lossy(heap, id))
                    })
                    .unwrap_or_default(),
                KeyKind::Symbol(_) => String::new(),
            }),
            line: line as u32 + 1,
            column,
        });
        if f.flags.backtrace_barrier {
            break;
        }
    }
    frames
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
    let index = atom
        .raw()
        .wrapping_sub(qjs_bytecode::BuiltinAtom::SymbolToPrimitive as u32) as usize;
    if let Some(symbol) = crate::symbol::WellKnown::ALL.get(index) {
        well_known(ctx, *symbol)
    } else {
        compiled(ctx, code).key(atom)
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

pub(crate) fn step(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    index: usize,
    code: BcId,
) -> JsResult<Option<Value>> {
    use Opcode::*;
    let function = compiled(ctx, code);
    if let Ok(i) = function
        .hoisted
        .binary_search_by_key(&frame.pc, |&(pc, _)| pc)
    {
        let next = function.hoisted[i].1;
        ctx.runtime.vm.frames[index].pc = next;
        return Ok(None);
    }
    let strict = function.strict;
    let inst = Instruction::decode(&function.code, frame.pc as usize)
        .map_err(|_| ctx.throw_type_error("invalid bytecode offset"))?;
    let op = inst.opcode;
    let next = frame.pc + u32::from(op.info().size);
    ctx.runtime.vm.frames[index].last_pc = frame.pc;
    ctx.runtime.vm.frames[index].pc = next;
    let this = frame.base as usize + 1;
    let arg_start = this + 1;
    let local_start = arg_start + frame.arg_slots as usize;
    let tail = ctx.runtime.vm.stack.len();
    let handle = |n: usize| Handle::at(tail - n);
    let value = |ctx: &ExecCtx<'_, '_>, n: usize| ctx.runtime.vm.stack[tail - n];
    match op {
        Invalid => unreachable!("verified opcode"),
        Nop => {}
        PushI32 | PushI8 | PushI16 | Push0 | Push1 | Push2 | Push3 | Push4 | Push5 | Push6
        | Push7 | PushMinus1 | PushBigintI32 => {
            let Operands::Signed(n) = inst.operands else {
                unreachable!()
            };
            let n = if op == PushBigintI32 {
                Value::short_bigint(i64::from(n)).unwrap()
            } else {
                Value::int32(n)
            };
            replace(ctx, 0, &[n]);
        }
        Undefined | Null | PushTrue | PushFalse => replace(
            ctx,
            0,
            &[match op {
                Undefined => Value::UNDEFINED,
                Null => Value::NULL,
                PushTrue => Value::bool(true),
                _ => Value::bool(false),
            }],
        ),
        PushEmptyString | PushAtomValue | PrivateSymbol | Object | PushThis | SpecialObject
        | Rest | ArrayFrom | PushConst | PushConst8 | Fclosure | Fclosure8 => {
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
                    PushConst | PushConst8 | Fclosure | Fclosure8 => {
                        let Operands::Constant(c) = inst.operands else {
                            unreachable!()
                        };
                        let function = ctx.heap().bytecode.get(code).unwrap();
                        let constant = function.constants[c as usize];
                        if matches!(op, Fclosure | Fclosure8) {
                            let ValueKind::Bytecode(child) = constant.decode() else {
                                unreachable!()
                            };
                            return bindings::closure(ctx, frame, child);
                        } else if let Some((_, flags)) = function
                            .compiled
                            .as_ref()
                            .unwrap()
                            .regex_flags
                            .iter()
                            .find(|&&(i, _)| i == c)
                            .copied()
                        {
                            return objects::regexp_literal(ctx, constant, flags);
                        } else {
                            return Ok(constant);
                        }
                    }
                    _ => unreachable!(),
                };
                Ok(ctx.get(result))
            })?;
            replace(ctx, consumed, &[result]);
        }
        Drop => replace(ctx, 1, &[]),
        Nip => replace(ctx, 2, &[value(ctx, 1)]),
        Nip1 => replace(ctx, 3, &[value(ctx, 2), value(ctx, 1)]),
        Dup => replace(ctx, 0, &[value(ctx, 1)]),
        Dup1 => replace(ctx, 2, &[value(ctx, 2), value(ctx, 2), value(ctx, 1)]),
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
        Swap => ctx.runtime.vm.stack.swap(tail - 2, tail - 1),
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
                    target = Some(ctx.object_id(Handle::at(start + 1))?);
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
        Return | ReturnAsync => return Ok(Some(value(ctx, 1))),
        ReturnUndef => return Ok(Some(Value::UNDEFINED)),
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
                return Err(ctx.throw_type_error(
                    "derived class constructor must return an object or undefined",
                ));
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
        GetLoc0Loc1 => replace(
            ctx,
            0,
            &[
                ctx.runtime.vm.stack[local_start],
                ctx.runtime.vm.stack[local_start + 1],
            ],
        ),
        GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 | GetLocCheck | PutLoc
        | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 | PutLocCheck | PutLocCheckInit
        | SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 | SetLocUninitialized
        | CloseLoc => {
            let Operands::Local(n) = inst.operands else {
                unreachable!()
            };
            let slot = local_start + n as usize;
            let current = ctx.runtime.vm.stack[slot];
            match op {
                GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 | GetLocCheck => {
                    if op == GetLocCheck {
                        bindings::check_initialized(ctx, current)?;
                    }
                    replace(ctx, 0, &[current]);
                }
                CloseLoc => bindings::close_local(ctx, frame, slot),
                SetLocUninitialized => ctx.runtime.vm.stack[slot] = Value::UNINITIALIZED,
                _ => {
                    if op == PutLocCheck {
                        bindings::check_initialized(ctx, current)?;
                    }
                    if op == PutLocCheckInit && current != Value::UNINITIALIZED {
                        return Err(
                            ctx.throw_error(ErrorKind::Reference, "binding already initialized")
                        );
                    }
                    ctx.runtime.vm.stack[slot] = value(ctx, 1);
                    if !matches!(op, SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3) {
                        replace(ctx, 1, &[]);
                    }
                }
            }
        }
        GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3 | PutArg | PutArg0 | PutArg1 | PutArg2
        | PutArg3 | SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3 => {
            let Operands::Argument(n) = inst.operands else {
                unreachable!()
            };
            let slot = arg_start + n as usize;
            if matches!(op, GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3) {
                replace(ctx, 0, &[ctx.runtime.vm.stack[slot]]);
            } else {
                ctx.runtime.vm.stack[slot] = value(ctx, 1);
                if matches!(op, PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3) {
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
            let result = ctx.with_scope(|ctx| {
                let (a, b) = (Handle::at(tail - 2), Handle::at(tail - 1));
                Ok(match op {
                    Eq | Neq => compare::abstract_eq(ctx, a, b)? ^ (op == Neq),
                    StrictEq | StrictNeq => compare::strict_eq(ctx, a, b) ^ (op == StrictNeq),
                    Lt => compare::abstract_relational(ctx, a, b, true)? == Some(true),
                    Gt => compare::abstract_relational(ctx, b, a, false)? == Some(true),
                    Lte => compare::abstract_relational(ctx, b, a, false)? == Some(false),
                    Gte => compare::abstract_relational(ctx, a, b, true)? == Some(false),
                    _ => unreachable!(),
                })
            })?;
            replace(ctx, 2, &[Value::bool(result)]);
        }
        Typeof | Lnot | TypeofIsFunction | TypeofIsUndefined | IsUndefinedOrNull | IsUndefined
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
                Lnot => Value::bool(!convert::to_boolean_value(ctx.heap(), raw)),
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
            let (count, spread) = match inst.operands {
                Operands::Eval { arguments, .. } => (arguments as usize, false),
                Operands::Unsigned(_) => (1, true),
                _ => unreachable!(),
            };
            let start = tail - count - 1;
            if spread {
                ctx.reserve_stack(1)?;
                replace(ctx, 0, &[Value::UNDEFINED]);
                ctx.runtime.vm.stack.swap(tail - 1, tail);
                spread_call(
                    ctx,
                    start,
                    Handle::at(start),
                    Handle::at(start + 1),
                    Handle::at(start + 2),
                    0,
                )?;
            } else {
                ctx.reserve_stack(1)?;
                replace(ctx, 0, &[Value::UNDEFINED]);
                ctx.runtime.vm.stack[start + 1..].rotate_right(1);
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
    let new_target = (mode == 1).then(|| ctx.object_id(this)).transpose()?;
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

pub(crate) fn transform_native_call(
    ctx: &mut ExecCtx<'_, '_>,
    id: u16,
    base: usize,
    count: usize,
) -> JsResult<Option<(usize, Option<ObjId>)>> {
    let core = crate::builtins::CORE_BASE;
    let arg = |ctx: &ExecCtx<'_, '_>, i| {
        if i < count {
            ctx.runtime.vm.stack[base + 2 + i]
        } else {
            Value::UNDEFINED
        }
    };
    if id == core + 0x43 {
        let this = arg(ctx, 0);
        ctx.runtime.vm.stack[base] = ctx.runtime.vm.stack[base + 1];
        ctx.runtime.vm.stack[base + 1] = this;
        if count > 0 {
            ctx.runtime.vm.stack.remove(base + 2);
        }
        return Ok(Some((count.saturating_sub(1), None)));
    }
    let (callee, this, array, new_target, nullish) = match id.wrapping_sub(core) {
        0x41 => (
            ctx.runtime.vm.stack[base + 1],
            arg(ctx, 0),
            arg(ctx, 1),
            None,
            true,
        ),
        0x80 => (arg(ctx, 0), arg(ctx, 1), arg(ctx, 2), None, false),
        0x81 => {
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
        _ => return Ok(None),
    };
    // Array getters may collect, so retain the argument list while it is expanded.
    let array = ctx.root(array)?;
    let count = expand_array_like(ctx, base, callee, this, array, nullish)?;
    Ok(Some((count, new_target)))
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
