use qjs_bytecode::{ClosureKind, Instruction, Opcode, Operands};

use crate::atom::{Atom, PropKey};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{ArgumentsData, ErrorKind, MappedArgumentsData, ObjectData, ObjectFlags};
use crate::ops::{convert, property};
use crate::root::{ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::*;
use crate::vm::{CallFrame, ModuleRecord, VarRef, VarRefState};

use super::{branch, compiled, define_default, define_flags, key, load, replace, well_known};

pub(crate) fn read_ref(ctx: &ExecCtx<'_, '_>, id: VarRefId) -> Value {
    match ctx.heap().var_refs.get(id).unwrap().state {
        VarRefState::OpenStack(index) => ctx.runtime.vm.stack[index as usize],
        VarRefState::OpenCoro { coro, local } => {
            ctx.heap().coroutines.get(coro).unwrap().stack[local as usize]
        }
        VarRefState::Closed(value) => value,
    }
}

pub(crate) fn write_ref(ctx: &mut ExecCtx<'_, '_>, id: VarRefId, value: Value) {
    let heap = &mut ctx.runtime.heap;
    let reference = heap.var_refs.get_mut(id).unwrap();
    match reference.state {
        VarRefState::OpenStack(index) => ctx.runtime.vm.stack[index as usize] = value,
        VarRefState::OpenCoro { coro, local } => {
            heap.coroutines.get_mut(coro).unwrap().stack[local as usize] = value
        }
        VarRefState::Closed(_) => reference.state = VarRefState::Closed(value),
    }
}

fn capture_slot(frame: CallFrame, slot: usize) -> usize {
    frame.capture_base as usize + slot - (frame.base as usize + 2)
}

pub(super) fn local_ref(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    slot: usize,
) -> JsResult<Value> {
    let cell = capture_slot(frame, slot);
    let cached = ctx.runtime.vm.stack[cell];
    if cached != Value::UNDEFINED {
        return Ok(cached);
    }
    let state = VarRefState::OpenStack(slot as u32);
    let id = ctx.allocate(|rt| rt.heap.var_refs.insert(&rt.heap.ledger, VarRef { state }))?;
    let value = Value::var_ref(id);
    ctx.runtime.vm.stack[cell] = value;
    Ok(value)
}

pub(super) fn close_local(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame, slot: usize) {
    let cell = capture_slot(frame, slot);
    if let ValueKind::VarRef(reference) = ctx.runtime.vm.stack[cell].decode() {
        let value = ctx.runtime.vm.stack[slot];
        ctx.runtime.heap.var_refs.get_mut(reference).unwrap().state = VarRefState::Closed(value);
        ctx.runtime.vm.stack[cell] = Value::UNDEFINED;
    }
}

pub(crate) fn close_frame(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame) {
    for slot in frame.capture_base as usize..frame.floor as usize {
        let local = slot - frame.capture_base as usize + frame.base as usize + 2;
        close_local(ctx, frame, local);
    }
}

pub(super) fn captured(ctx: &ExecCtx<'_, '_>, frame: CallFrame, index: u16) -> Value {
    let ObjectData::BytecodeFunction(data) = &ctx.heap().object(frame.callee).data else {
        unreachable!()
    };
    data.closure
        .get(index as usize)
        .copied()
        .unwrap_or(Value::UNDEFINED)
}

pub(super) fn closure(ctx: &mut ExecCtx<'_, '_>, frame: CallFrame, child: BcId) -> JsResult<Value> {
    if frame
        .code
        .is_some_and(|parent| compiled(ctx, parent).class_field_context)
    {
        let f = ctx
            .runtime
            .heap
            .bytecode
            .get_mut(child)
            .unwrap()
            .compiled
            .as_mut()
            .unwrap();
        if !f.flags.has_prototype && !f.flags.need_home_object && !f.flags.new_target_allowed {
            f.class_field_context = true;
        }
    }
    let mut captures = TrackedVec::new();
    let len = compiled(ctx, child).closures.len();
    ctx.allocate(|rt| captures.try_reserve_exact(&rt.heap.ledger, len))?;
    for index in 0..len {
        let def = &compiled(ctx, child).closures[index];
        let n = def.index;
        let value = match def.kind {
            ClosureKind::Local => local_ref(
                ctx,
                frame,
                frame.base as usize + 2 + frame.arg_slots as usize + n as usize,
            )?,
            ClosureKind::Argument => local_ref(ctx, frame, frame.base as usize + 2 + n as usize)?,
            ClosureKind::Ref | ClosureKind::GlobalRef => captured(ctx, frame, n),
            ClosureKind::Global
            | ClosureKind::GlobalDeclaration
            | ClosureKind::ModuleDeclaration
            | ClosureKind::ModuleImport => lexical(ctx, key(ctx, child, def.name))
                .map_or(Value::UNDEFINED, |(id, _)| Value::var_ref(id)),
        };
        captures.push_reserved(value);
    }
    let closure = load::make_function(ctx, child, &captures)?;
    Ok(ctx.get(closure))
}

pub(super) fn check_initialized(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
    if value == Value::UNINITIALIZED {
        Err(ctx.throw_error(ErrorKind::Reference, "binding is not initialized"))
    } else {
        Ok(())
    }
}

pub(super) fn var_op(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    inst: Instruction,
) -> JsResult<()> {
    use Opcode::*;
    let Operands::VarRef(index) = inst.operands else {
        unreachable!()
    };
    let ValueKind::VarRef(id) = captured(ctx, frame, index).decode() else {
        return Err(ctx.throw_error(ErrorKind::Reference, "missing closure binding"));
    };
    let current = read_ref(ctx, id);
    match inst.opcode {
        GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3 | GetVarRefCheck => {
            if inst.opcode == GetVarRefCheck {
                check_initialized(ctx, current)?;
            }
            replace(ctx, 0, &[current]);
        }
        op => {
            if op == PutVarRefCheck {
                check_initialized(ctx, current)?;
            }
            if op == PutVarRefCheckInit && current != Value::UNINITIALIZED {
                return Err(ctx.throw_error(ErrorKind::Reference, "binding already initialized"));
            }
            let value = *ctx.runtime.vm.stack.last().unwrap();
            write_ref(ctx, id, value);
            if !matches!(
                op,
                SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3
            ) {
                replace(ctx, 1, &[]);
            }
        }
    }
    Ok(())
}

fn global_scope(ctx: &ExecCtx<'_, '_>) -> Option<ModId> {
    ctx.runtime.modules.iter().copied().find(|&id| {
        let record = ctx.heap().modules.get(id).unwrap();
        record.global_scope && record.realm == Some(ctx.realm_id())
    })
}

fn lexical(ctx: &ExecCtx<'_, '_>, key: PropKey) -> Option<(VarRefId, bool)> {
    let scope = ctx.heap().modules.get(global_scope(ctx)?).unwrap();
    scope
        .lexical
        .iter()
        .find_map(|&(k, value)| (k == key).then_some(value))
}

fn define_lexical(ctx: &mut ExecCtx<'_, '_>, key: PropKey, writable: bool) -> JsResult<()> {
    let scope = if let Some(id) = global_scope(ctx) {
        id
    } else {
        let realm = ctx.realm_id();
        ctx.allocate(|rt| {
            rt.modules.try_reserve(&rt.heap.ledger, 1)?;
            let id = rt.heap.modules.insert(
                &rt.heap.ledger,
                ModuleRecord {
                    realm: Some(realm),
                    global_scope: true,
                    ..Default::default()
                },
            )?;
            rt.modules.push_reserved(id);
            Ok(id)
        })?
    };
    ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let scope = heap.modules.get_mut(scope).unwrap();
        scope.lexical.try_reserve(&heap.ledger, 1)?;
        let id = heap.var_refs.insert(
            &heap.ledger,
            VarRef {
                state: VarRefState::Closed(Value::UNINITIALIZED),
            },
        )?;
        scope.lexical.push_reserved((key, (id, writable)));
        Ok(())
    })
}

fn scoped_ref(ctx: &mut ExecCtx<'_, '_>, code: BcId, key: PropKey) -> JsResult<Option<Value>> {
    if let Some(binding) = super::eval::lookup(ctx, code, key)? {
        return Ok(Some(binding));
    }
    Ok(lexical(ctx, key).map(|(id, _)| Value::var_ref(id)))
}

fn global_ref(ctx: &mut ExecCtx<'_, '_>, code: BcId, key: PropKey) -> JsResult<Value> {
    match scoped_ref(ctx, code, key)? {
        Some(binding) => Ok(binding),
        None => global_object_ref(ctx, key),
    }
}

fn global_object_ref(ctx: &mut ExecCtx<'_, '_>, key: PropKey) -> JsResult<Value> {
    let global = Value::object(ctx.realm().global_object);
    let h = ctx.root(global)?;
    Ok(if property::has_property(ctx, h, key)? {
        global
    } else {
        Value::UNDEFINED
    })
}

fn global_data_slot(
    ctx: &ExecCtx<'_, '_>,
    key: PropKey,
) -> Option<(ObjId, usize, Value, PropFlags)> {
    // The realm global is ordinary, so its own data properties can use their slots directly.
    let id = ctx.realm().global_object;
    let object = ctx.heap().object(id);
    let (index, flags) = ctx.heap().shape(object.shape).find(key)?;
    let value = object.props[index];
    (!matches!(value.decode(), ValueKind::Accessor(_))).then_some((id, index, value, flags))
}

fn read_binding<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    binding: Handle<'s>,
    key: PropKey,
    missing_ok: bool,
) -> JsResult<Value> {
    match ctx.get(binding).decode() {
        ValueKind::VarRef(id) => {
            let value = read_ref(ctx, id);
            check_initialized(ctx, value)?;
            Ok(value)
        }
        ValueKind::Undefined if missing_ok => Ok(Value::UNDEFINED),
        ValueKind::Undefined => {
            Err(ctx.throw_error(ErrorKind::Reference, "binding is not defined"))
        }
        _ => property::get_property(ctx, binding, key),
    }
}

fn write_binding<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    binding: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
    strict: bool,
    initialize: bool,
) -> JsResult<()> {
    let object = match ctx.get(binding).decode() {
        ValueKind::VarRef(id) => {
            if !initialize {
                check_initialized(ctx, read_ref(ctx, id))?;
                if let Some(kind) = super::eval::immutable_kind(ctx, id) {
                    if kind == qjs_bytecode::VarKind::FunctionName && !strict {
                        return Ok(());
                    }
                    return Err(ctx.throw_type_error("assignment to constant binding"));
                }
                if lexical(ctx, key) == Some((id, false)) {
                    return Err(ctx.throw_type_error("assignment to constant binding"));
                }
            }
            write_ref(ctx, id, ctx.get(value));
            return Ok(());
        }
        ValueKind::Undefined if strict => {
            return Err(ctx.throw_error(ErrorKind::Reference, "binding is not defined"));
        }
        ValueKind::Undefined => ctx.root(Value::object(ctx.realm().global_object))?,
        _ => binding,
    };
    if strict && !property::has_property(ctx, object, key)? {
        return Err(ctx.throw_error(ErrorKind::Reference, "binding is not defined"));
    }
    if !property::set_property(ctx, object, key, value)? && strict {
        return Err(ctx.throw_type_error("cannot assign property"));
    }
    Ok(())
}

pub(super) fn binding_op(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    code: BcId,
    index: usize,
    inst: Instruction,
) -> JsResult<()> {
    use Opcode::*;
    let op = inst.opcode;
    let strict;
    let fast_global;
    let atom = {
        let cf = compiled(ctx, code);
        strict = cf.strict;
        fast_global = cf.eval.is_none() && ctx.runtime.modules.is_empty();
        match inst.operands {
            Operands::Atom(atom)
            | Operands::AtomU8(atom, _)
            | Operands::AtomU16(atom, _)
            | Operands::AtomJumpU8(atom, _, _) => Some(key(ctx, code, atom)),
            _ => None,
        }
    };
    let tail = ctx.runtime.vm.stack.len();
    if fast_global {
        match op {
            GetVar | GetVarUndef => {
                if let Some((_, _, value, _)) = global_data_slot(ctx, atom.unwrap()) {
                    ctx.runtime.vm.stack.push_reserved(value);
                    return Ok(());
                }
            }
            PutVar | PutVarInit => {
                if let Some((id, slot_index, _, flags)) = global_data_slot(ctx, atom.unwrap())
                    && flags.contains(PropFlags::WRITABLE)
                {
                    let value = ctx.runtime.vm.stack[tail - 1];
                    ctx.runtime.heap.objects.get_mut(id).unwrap().props[slot_index] = value;
                    property::invalidate_array_prototypes(ctx, id);
                    ctx.runtime.vm.stack.truncate(tail - 1);
                    return Ok(());
                }
            }
            MakeVarRef => {
                let k = atom.unwrap();
                if let Some((id, _, _, _)) = global_data_slot(ctx, k)
                    && let crate::atom::KeyKind::String(a) = k.decode()
                    && let Some(str_id) = ctx.heap().atoms.dynamic_string(a)
                {
                    ctx.runtime.vm.stack.push_reserved(Value::object(id));
                    ctx.runtime.vm.stack.push_reserved(Value::string(str_id));
                    return Ok(());
                }
            }
            GetRefValue => {
                let global_id = ctx.realm().global_object;
                if ctx.runtime.vm.stack[tail - 2] == Value::object(global_id)
                    && let ValueKind::String(str_id) = ctx.runtime.vm.stack[tail - 1].decode()
                    && let Some(k) = ctx.heap().atoms.find(&ctx.heap().strings, str_id)
                    && let Some((_, _, value, _)) = global_data_slot(ctx, k)
                {
                    ctx.runtime.vm.stack.push_reserved(value);
                    return Ok(());
                }
            }
            PutRefValue => {
                let global_id = ctx.realm().global_object;
                if ctx.runtime.vm.stack[tail - 3] == Value::object(global_id)
                    && let ValueKind::String(str_id) = ctx.runtime.vm.stack[tail - 2].decode()
                    && let Some(k) = ctx.heap().atoms.find(&ctx.heap().strings, str_id)
                    && let Some((id, slot_index, _, flags)) = global_data_slot(ctx, k)
                    && flags.contains(PropFlags::WRITABLE)
                {
                    let value = ctx.runtime.vm.stack[tail - 1];
                    ctx.runtime.heap.objects.get_mut(id).unwrap().props[slot_index] = value;
                    property::invalidate_array_prototypes(ctx, id);
                    ctx.runtime.vm.stack.truncate(tail - 3);
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    let arg = frame.base as usize + 2;
    let result = ctx.with_scope(|ctx| {
        let h = |n: usize| Handle::at(tail - n);
        let result = match op {
            GetVar | GetVarUndef => {
                let key = atom.unwrap();
                let binding = scoped_ref(ctx, code, key)?;
                if binding.is_none()
                    && let Some((_, _, value, _)) = global_data_slot(ctx, key)
                {
                    return Ok((0, [value, Value::EMPTY], None));
                }
                let binding = match binding {
                    Some(binding) => binding,
                    None => global_object_ref(ctx, key)?,
                };
                let binding = ctx.root(binding)?;
                (
                    0,
                    [
                        read_binding(ctx, binding, key, op == GetVarUndef)?,
                        Value::EMPTY,
                    ],
                    None,
                )
            }
            PutVar | PutVarInit => {
                let key = atom.unwrap();
                let binding = scoped_ref(ctx, code, key)?;
                if binding.is_none()
                    && let Some((id, index, _, flags)) = global_data_slot(ctx, key)
                    && flags.contains(PropFlags::WRITABLE)
                {
                    let value = ctx.get(h(1));
                    ctx.runtime.heap.objects.get_mut(id).unwrap().props[index] = value;
                    property::invalidate_array_prototypes(ctx, id);
                    return Ok((1, [Value::EMPTY; 2], None));
                }
                let binding = match binding {
                    Some(binding) => binding,
                    None => global_object_ref(ctx, key)?,
                };
                let binding = ctx.root(binding)?;
                write_binding(ctx, binding, key, h(1), strict, op == PutVarInit)?;
                (1, [Value::EMPTY; 2], None)
            }
            CheckDefineVar | DefineVar | DefineFunc | DeleteVar => {
                let key = atom.unwrap();
                if op == DeleteVar {
                    let binding = global_ref(ctx, code, key)?;
                    let deleted = match binding.decode() {
                        ValueKind::VarRef(_) => false,
                        ValueKind::Undefined => true,
                        _ => {
                            let object = ctx.root(binding)?;
                            property::delete_property(ctx, object, key)?
                        }
                    };
                    return Ok((0, [Value::bool(deleted), Value::EMPTY], None));
                }
                if super::eval::declaration(ctx, code, key, op, (op == DefineFunc).then(|| h(1)))? {
                    return Ok((usize::from(op == DefineFunc), [Value::EMPTY; 2], None));
                }
                let global = ctx.root(Value::object(ctx.realm().global_object))?;
                let mut flags = match inst.operands {
                    Operands::AtomU8(_, flags) => flags,
                    _ => 0,
                };
                if compiled(ctx, code).eval.is_some() {
                    flags |= 1;
                }
                let lexical_binding = flags & 0x80 != 0;
                let existing = property::get_own_property_descriptor(ctx, global, key)?;
                match op {
                    CheckDefineVar => {
                        if lexical(ctx, key).is_some()
                            || (lexical_binding
                                && existing.is_some_and(|d| d.configurable == Some(false)))
                        {
                            return Err(ctx.throw_error(
                                ErrorKind::Syntax,
                                "redeclaration of global binding",
                            ));
                        }
                        if !lexical_binding
                            && existing.is_none()
                            && !property::is_extensible(ctx, global)?
                        {
                            return Err(ctx.throw_type_error("cannot define global binding"));
                        }
                        if flags & 0x40 != 0
                            && existing.is_some_and(|d| {
                                d.configurable == Some(false)
                                    && (d.writable != Some(true) || d.enumerable != Some(true))
                            })
                        {
                            return Err(ctx.throw_type_error("cannot define global function"));
                        }
                    }
                    DefineVar if lexical_binding => {
                        if lexical(ctx, key).is_none() {
                            define_lexical(ctx, key, flags & 2 != 0)?;
                        }
                    }
                    DefineVar | DefineFunc => {
                        let value = if op == DefineFunc {
                            h(1)
                        } else {
                            ctx.root(Value::UNDEFINED)?
                        };
                        if op == DefineFunc
                            && existing.is_some_and(|d| d.configurable == Some(false))
                        {
                            if !property::define_own_property(
                                ctx,
                                global,
                                key,
                                property::PropertyDescriptor {
                                    value: Some(value),
                                    ..Default::default()
                                },
                            )? {
                                return Err(ctx.throw_type_error("cannot define global function"));
                            }
                        } else if op == DefineFunc || existing.is_none() {
                            let mut attrs = PropFlags::WRITABLE | PropFlags::ENUMERABLE;
                            if flags & 1 != 0 {
                                attrs |= PropFlags::CONFIGURABLE;
                            }
                            define_flags(ctx, global, key, value, attrs)?;
                        }
                    }
                    _ => unreachable!(),
                }
                (usize::from(op == DefineFunc), [Value::EMPTY; 2], None)
            }
            MakeVarRef | MakeLocRef | MakeArgRef | MakeVarRefRef => {
                let key = atom.unwrap();
                let value = match inst.operands {
                    Operands::Atom(_) => global_ref(ctx, code, key)?,
                    Operands::AtomU16(_, n) => match op {
                        MakeLocRef => {
                            local_ref(ctx, frame, arg + frame.arg_slots as usize + n as usize)?
                        }
                        MakeArgRef => local_ref(ctx, frame, arg + n as usize)?,
                        MakeVarRefRef => captured(ctx, frame, n),
                        _ => unreachable!(),
                    },
                    _ => unreachable!(),
                };
                let name = convert::key_value(ctx, key)?;
                (0, [value, ctx.get(name)], None)
            }
            GetRefValue | PutRefValue => {
                let offset = usize::from(op == PutRefValue);
                let key = convert::to_property_key(ctx, h(1 + offset))?;
                if offset == 1 {
                    write_binding(ctx, h(3), key, h(1), strict, false)?;
                    (3, [Value::EMPTY; 2], None)
                } else {
                    (
                        0,
                        [read_binding(ctx, h(2), key, false)?, Value::EMPTY],
                        None,
                    )
                }
            }
            WithGetVar | WithPutVar | WithDeleteVar | WithMakeRef | WithGetRef
            | WithGetRefUndef => {
                let Operands::AtomJumpU8(_, _, is_with) = inst.operands else {
                    unreachable!()
                };
                let key = atom.unwrap();
                let object = h(1);
                let mut found = property::has_property(ctx, object, key)?;
                if found && is_with != 0 {
                    let excluded = property::get_property(
                        ctx,
                        object,
                        well_known(ctx, WellKnown::Unscopables),
                    )?;
                    if excluded.as_object().is_some() {
                        let excluded = ctx.root(excluded)?;
                        let skip = property::get_property(ctx, excluded, key)?;
                        found = !convert::to_boolean_value(ctx.heap(), skip);
                    }
                }
                if !found {
                    (1, [Value::EMPTY; 2], None)
                } else {
                    let target = inst.branch_target(frame.pc).map(|i| i as u32);
                    match op {
                        WithGetVar | WithGetRef | WithGetRefUndef => {
                            let exists =
                                op == WithGetRefUndef || property::has_property(ctx, object, key)?;
                            let result = if exists {
                                property::get_property(ctx, object, key)?
                            } else if strict {
                                return Err(
                                    ctx.throw_error(ErrorKind::Reference, "binding is not defined")
                                );
                            } else {
                                Value::UNDEFINED
                            };
                            if op == WithGetRef {
                                (0, [result, Value::EMPTY], target)
                            } else if op == WithGetRefUndef {
                                (1, [Value::UNDEFINED, result], target)
                            } else {
                                (1, [result, Value::EMPTY], target)
                            }
                        }
                        WithPutVar => {
                            let exists = property::has_property(ctx, object, key)?;
                            if strict && !exists {
                                return Err(
                                    ctx.throw_error(ErrorKind::Reference, "binding is not defined")
                                );
                            }
                            if !property::set_property(ctx, object, key, h(2))? && strict {
                                return Err(ctx.throw_type_error("cannot assign property"));
                            }
                            (2, [Value::EMPTY; 2], target)
                        }
                        WithDeleteVar => (
                            1,
                            [
                                Value::bool(property::delete_property(ctx, object, key)?),
                                Value::EMPTY,
                            ],
                            target,
                        ),
                        WithMakeRef => {
                            let key = convert::key_value(ctx, key)?;
                            (0, [ctx.get(key), Value::EMPTY], target)
                        }
                        _ => unreachable!(),
                    }
                }
            }
            _ => unreachable!(),
        };
        Ok(result)
    })?;
    let (consumed, produced, target) = result;
    let len = produced
        .iter()
        .position(|v| *v == Value::EMPTY)
        .unwrap_or(produced.len());
    replace(ctx, consumed, &produced[..len]);
    if let Some(target) = target {
        branch(ctx, frame, index, target)?;
    }
    Ok(())
}

pub(super) fn special_object(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    code: BcId,
    selector: u32,
) -> JsResult<Value> {
    if (2..=4).contains(&selector)
        && let Some(frame) = compiled(ctx, code)
            .eval
            .as_ref()
            .and_then(|env| env.special)
    {
        return Ok(frame[selector as usize - 2]);
    }
    Ok(match selector {
        0 | 1 => return arguments(ctx, frame, code, selector == 1),
        2 => Value::object(frame.callee),
        3 => frame.new_target.map_or(Value::UNDEFINED, Value::object),
        4 => {
            let ObjectData::BytecodeFunction(data) = &ctx.heap().object(frame.callee).data else {
                unreachable!()
            };
            data.home_object.map_or(Value::UNDEFINED, Value::object)
        }
        5 => {
            let object = ctx.new_object_with_proto(None)?;
            ctx.get(object)
        }
        6 => return super::modules::import_meta(ctx, code),
        7 => {
            let object = ctx.new_object_with_proto(None)?;
            ctx.get(object)
        }
        _ => return Err(ctx.throw_error(ErrorKind::Internal, "invalid special object")),
    })
}

fn arguments(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    code: BcId,
    mapped: bool,
) -> JsResult<Value> {
    let count = frame.arg_count as usize;
    let start = frame.base as usize + 2;
    let mut parameters = TrackedVec::new();
    if mapped {
        let n = count.min(compiled(ctx, code).arg_count as usize);
        ctx.allocate(|rt| parameters.try_reserve_exact(&rt.heap.ledger, n))?;
        for i in 0..n {
            let vars = &compiled(ctx, code).vars;
            let repeated = vars.get(i).is_some_and(|var| {
                vars[i + 1..compiled(ctx, code).arg_count as usize]
                    .iter()
                    .any(|later| later.name == var.name)
            });
            let reference = if repeated {
                None
            } else {
                let ValueKind::VarRef(reference) = local_ref(ctx, frame, start + i)?.decode()
                else {
                    unreachable!()
                };
                Some(reference)
            };
            parameters.push_reserved(reference);
        }
    }
    let proto = ctx.realm().object_proto;
    let object = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(if mapped {
            ObjectData::MappedArguments(TrackedBox::new(
                ledger,
                MappedArgumentsData {
                    parameters: TrackedVec::from_slice(ledger, &parameters)?,
                },
            )?)
        } else {
            ObjectData::Arguments(TrackedBox::new(
                ledger,
                ArgumentsData {
                    elements: TrackedVec::new(),
                },
            )?)
        })
    })?;
    for i in 0..count {
        let key = load::index_key(ctx, i as u64)?;
        define_default(ctx, object, key, Handle::at(start + i))?;
    }
    let length = ctx.root(Value::number(count as f64))?;
    let flags = PropFlags::WRITABLE | PropFlags::CONFIGURABLE;
    define_flags(ctx, object, Atom::get("length"), length, flags)?;
    if mapped {
        let callee = ctx.root(Value::object(frame.callee))?;
        define_flags(ctx, object, Atom::get("callee"), callee, flags)?;
    } else {
        let poison = ctx.root(Value::object(ctx.realm().throw_type_error))?;
        property::define_own_property(
            ctx,
            object,
            Atom::get("callee"),
            property::PropertyDescriptor {
                get: Some(poison),
                set: Some(poison),
                ..Default::default()
            },
        )?;
    }
    let array = ctx.root(Value::object(ctx.realm().array_proto))?;
    let iterator_key = well_known(ctx, WellKnown::Iterator);
    let iterator = property::get_property(ctx, array, iterator_key)?;
    let iterator = ctx.root(iterator)?;
    define_flags(ctx, object, iterator_key, iterator, flags)?;
    Ok(ctx.get(object))
}
