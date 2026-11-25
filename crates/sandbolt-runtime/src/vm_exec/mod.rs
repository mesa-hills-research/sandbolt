mod bindings;
mod cache;
pub(crate) mod coroutines;
pub(crate) mod eval;
mod fast;
mod iterators;
mod load;
pub(crate) mod modules;
pub(crate) mod numeric;
mod objects;
pub(crate) mod stack;

#[cfg(test)]
mod tests;

use std::time::{SystemTime, UNIX_EPOCH};

use sandbolt_bytecode::{ExceptionHandler, FunctionKind, Instruction, Opcode, Operands};
use sandbolt_syntax::SourceText;

use crate::atom::{Atom, PropKey};
use crate::builtins::{
    ARRAY_ITER_BASE, NUM_DATE_BASE, STRING_RE_BASE, coll_weak, core as core_builtins, num_date,
    string_re,
};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{ErrorKind, JsObject, ObjectData, ObjectFlags};
use crate::ops::{call, compare, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::string::{INLINE_LATIN1_CAPACITY, JsString};
use crate::value::*;
use crate::vm::{CallFrame, CompiledFunc, VarRefState};

pub(crate) use bindings::{read_ref, write_ref};
pub(crate) use fast::FastProgram;
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

fn key(ctx: &ExecCtx<'_, '_>, code: BcId, atom: sandbolt_bytecode::UnitAtom) -> PropKey {
    compiled_key(ctx.heap(), compiled(ctx, code), atom)
}

fn compiled_key(
    heap: &crate::heap::Heap,
    function: &CompiledFunc,
    atom: sandbolt_bytecode::UnitAtom,
) -> PropKey {
    compiled_key_parts(&heap.symbols, &heap.symbol_registry, function, atom)
}

fn compiled_key_parts(
    symbols: &crate::arena::Arena<crate::symbol::SymbolData>,
    symbol_registry: &crate::symbol::SymbolRegistry,
    function: &CompiledFunc,
    atom: sandbolt_bytecode::UnitAtom,
) -> PropKey {
    let index = atom
        .raw()
        .wrapping_sub(sandbolt_bytecode::BuiltinAtom::SymbolToPrimitive as u32)
        as usize;
    if let Some(&symbol) = crate::symbol::WellKnown::ALL.get(index) {
        let id = symbol_registry.well_known(symbol);
        symbols.get(id).unwrap().key()
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

const OP_SIZE: [u8; 256] = {
    let mut sizes = [0; 256];
    let mut i = 0;
    while i < Opcode::ALL.len() {
        sizes[i] = Opcode::ALL[i].info().size;
        i += 1;
    }
    sizes
};

const FN_ARRAY_POP: u16 = ARRAY_ITER_BASE + 36;
const FN_ARRAY_PUSH: u16 = ARRAY_ITER_BASE + 37;
const FN_STRING_CTOR: u16 = STRING_RE_BASE;
const FN_STRING_CONCAT: u16 = STRING_RE_BASE + 8;
const FN_STRING_SLICE: u16 = STRING_RE_BASE + 24;
const FN_REGEXP_EXEC: u16 = string_re::REGEXP_EXEC.0;
const FN_MATH_MAX: u16 = NUM_DATE_BASE + 0x30 + 24;
const FN_MATH_MIN: u16 = NUM_DATE_BASE + 0x30 + 25;
const FN_DATE_NOW: u16 = NUM_DATE_BASE + 0x101;
const FN_NUM_TO_EXPONENTIAL: u16 = num_date::NUM_TO_EXPONENTIAL.0;
const FN_NUM_TO_FIXED: u16 = num_date::NUM_TO_FIXED.0;
const FN_NUM_TO_PRECISION: u16 = num_date::NUM_TO_PRECISION.0;
const FN_NUM_TO_STRING: u16 = num_date::NUM_TO_STRING.0;
const FN_MAP_SET: u16 = coll_weak::MAP_SET.0;
const FN_MAP_GET: u16 = coll_weak::MAP_GET.0;
const FN_MAP_HAS: u16 = coll_weak::MAP_HAS.0;
const FN_MAP_DELETE: u16 = coll_weak::MAP_DELETE.0;
const FN_SET_ADD: u16 = coll_weak::SET_ADD.0;
const FN_SET_HAS: u16 = coll_weak::SET_HAS.0;
const FN_SET_DELETE: u16 = coll_weak::SET_DELETE.0;
const FN_WEAK_MAP_SET: u16 = coll_weak::WEAK_MAP_SET.0;
const FN_WEAK_MAP_GET: u16 = coll_weak::WEAK_MAP_GET.0;
const FN_WEAK_MAP_HAS: u16 = coll_weak::WEAK_MAP_HAS.0;
const FN_WEAK_MAP_DELETE: u16 = coll_weak::WEAK_MAP_DELETE.0;
const FN_OBJECT_CTOR: u16 = core_builtins::OBJECT_CTOR.0;

enum FastBreak {
    Slow(u32),
    Interrupted(u32),
    ReturnTop {
        value: Value,
        at: u32,
        next_pc: u32,
    },
    ReturnToCaller {
        caller_code: BcId,
        caller_realm: RealmId,
        caller_ip: u32,
    },
    EnterJs {
        callee_code: BcId,
        callee_realm: RealmId,
        callee_limit: u32,
        base: u32,
        count: u16,
        method: bool,
        args_ready: bool,
    },
    TryPlainCall {
        count: usize,
        method: bool,
        call_pc: u32,
        next_pc: u32,
        next_ip: usize,
    },
    AppendArray {
        id: ObjId,
        value: Value,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    DictPut {
        id: ObjId,
        shape_id: ShapeId,
        key: PropKey,
        value: Value,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    PutFieldTransitionGrow {
        id: ObjId,
        to_shape: ShapeId,
        value: Value,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    ConcatStrings {
        a: StrId,
        b: StrId,
        dest: usize,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    NewObject {
        realm: RealmId,
        dest: usize,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    NewNullProtoObject {
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    NewArrayEmpty {
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    ForOfStartArray {
        arr_id: ObjId,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    CloneRegExp {
        constant: Value,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    IntToStringDest {
        value: i32,
        dest: usize,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    FloatToStringDest {
        value: f64,
        dest: usize,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    FormatNumber {
        base: u32,
        value: f64,
        radix: u8,
        digits: u8,
        options: sandbolt_dtoa::DtoaOptions,
        at: u32,
        next_pc: u32,
        next_ip: u32,
    },
    BigIntBinary {
        op: Opcode,
        lhs: Value,
        rhs: Value,
        dest: usize,
        truncate_to: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    BigIntMulAddLoc {
        dst_slot: usize,
        a: Value,
        b: Value,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    MapSet {
        base: usize,
        id: ObjId,
        key: Value,
        value: Value,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    WeakMapSet {
        base: usize,
        id: ObjId,
        key: Value,
        value: Value,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    RegExpExec {
        base: usize,
        re_id: ObjId,
        input_id: StrId,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    ForInIndex {
        value: i32,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    StringSlice {
        base: usize,
        start: usize,
        end: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
    StringConcat {
        base: usize,
        argc: usize,
        at: u32,
        next_pc: u32,
        next_ip: usize,
    },
}

#[inline]
fn is_bigint(value: Value) -> bool {
    matches!(
        value.decode(),
        ValueKind::ShortBigInt(_) | ValueKind::BigInt(_)
    )
}

#[inline]
fn parse_ascii_array_index(heap: &crate::heap::Heap, id: StrId) -> Option<usize> {
    let bytes = heap.string(id).as_latin1()?;
    if bytes.is_empty() || bytes.len() > 9 {
        return None;
    }
    let first = bytes[0];
    if first == b'0' {
        return (bytes.len() == 1).then_some(0);
    }
    if !(b'1'..=b'9').contains(&first) {
        return None;
    }
    let mut val = usize::from(first - b'0');
    for &b in &bytes[1..] {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val * 10 + usize::from(b - b'0');
    }
    Some(val)
}

#[inline]
fn fast_array_index(heap: &crate::heap::Heap, value: Value) -> Option<usize> {
    match value.decode() {
        ValueKind::Int32(n) => usize::try_from(n).ok(),
        ValueKind::String(id) => parse_ascii_array_index(heap, id),
        _ => None,
    }
}

#[inline]
fn fast_array_get_value(heap: &crate::heap::Heap, id: ObjId, element: usize) -> Option<Value> {
    let object = heap.object(id);
    match &object.data {
        ObjectData::Array(array) if object.flags.contains(ObjectFlags::FAST_ARRAY) => array
            .elements
            .get(element)
            .copied()
            .filter(|&v| v != Value::EMPTY),
        ObjectData::TypedArray(view) => {
            crate::builtins::typed_atomics::fast_typed_array_get(heap, **view, element)
        }
        _ => None,
    }
}

#[inline]
fn fast_int_to_string_id(ctx: &mut ExecCtx<'_, '_>, value: i32) -> JsResult<StrId> {
    let mut buf = [0u8; 16];
    let len = sandbolt_dtoa::i32toa(&mut buf, value);
    if len == 1 && buf[0].is_ascii() {
        let slot = usize::from(buf[0]);
        if let Some(id) = ctx.heap().atoms.ascii_string(slot) {
            return Ok(id);
        }
        let id = ctx.allocate(|rt| {
            let s = JsString::from_latin1(&rt.heap.ledger, &buf[..1])?;
            rt.heap.strings.insert(&rt.heap.ledger, s)
        })?;
        ctx.runtime.heap.atoms.cache_ascii_string(slot, id);
        Ok(id)
    } else {
        let mut data = [0u8; INLINE_LATIN1_CAPACITY];
        data[..len].copy_from_slice(&buf[..len]);
        ctx.allocate(|rt| {
            rt.heap.strings.insert(
                &rt.heap.ledger,
                JsString::InlineLatin1 {
                    len: len as u8,
                    is_ascii: true,
                    data,
                },
            )
        })
    }
}

enum EmptyStringAdd {
    Inline(Value),
    Int(i32),
    Float(f64),
    None,
}

#[inline]
fn check_empty_string_add(heap: &crate::heap::Heap, lhs: Value, rhs: Value) -> EmptyStringAdd {
    let num = match (lhs.decode(), rhs.decode()) {
        (ValueKind::Int32(n), ValueKind::String(s))
        | (ValueKind::String(s), ValueKind::Int32(n))
            if heap.string(s).is_empty() =>
        {
            if (0..=9).contains(&n)
                && let Some(id) = heap.atoms.ascii_string((b'0' + n as u8) as usize)
            {
                return EmptyStringAdd::Inline(Value::string(id));
            }
            return EmptyStringAdd::Int(n);
        }
        (ValueKind::Float64(f), ValueKind::String(s))
        | (ValueKind::String(s), ValueKind::Float64(f))
            if heap.string(s).is_empty() =>
        {
            f
        }
        _ => return EmptyStringAdd::None,
    };
    EmptyStringAdd::Float(num)
}

#[inline]
fn fast_inc_value(value: Value) -> Option<Value> {
    match value.decode() {
        ValueKind::Int32(n) => Some(
            n.checked_add(1)
                .map_or_else(|| Value::number(f64::from(n) + 1.0), Value::int32),
        ),
        ValueKind::Float64(n) => Some(Value::number(n + 1.0)),
        _ => None,
    }
}

#[inline]
fn fast_dec_value(value: Value) -> Option<Value> {
    match value.decode() {
        ValueKind::Int32(n) => Some(
            n.checked_sub(1)
                .map_or_else(|| Value::number(f64::from(n) - 1.0), Value::int32),
        ),
        ValueKind::Float64(n) => Some(Value::number(n - 1.0)),
        _ => None,
    }
}

#[inline]
fn fast_lt(a: Value, b: Value) -> Option<bool> {
    match (a.decode(), b.decode()) {
        (ValueKind::Int32(x), ValueKind::Int32(y)) => Some(x < y),
        _ => Some(a.as_number()? < b.as_number()?),
    }
}

#[inline]
fn fast_lte(a: Value, b: Value) -> Option<bool> {
    match (a.decode(), b.decode()) {
        (ValueKind::Int32(x), ValueKind::Int32(y)) => Some(x <= y),
        _ => Some(a.as_number()? <= b.as_number()?),
    }
}

#[inline]
fn fast_date_now() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        Err(error) => -(error.duration().as_millis() as f64),
    }
}

#[inline]
fn fast_math_minmax(a: Value, b: Value, max: bool) -> Option<Value> {
    match (a.decode(), b.decode()) {
        (ValueKind::Int32(x), ValueKind::Int32(y)) => {
            Some(Value::int32(if max { x.max(y) } else { x.min(y) }))
        }
        _ => {
            let x = a.as_number()?;
            let y = b.as_number()?;
            if x.is_nan() || y.is_nan() {
                return Some(Value::number(f64::NAN));
            }
            let take_y = if max {
                y.total_cmp(&x).is_gt()
            } else {
                y.total_cmp(&x).is_lt()
            };
            Some(Value::number(if take_y { y } else { x }))
        }
    }
}

#[inline]
fn ensure_fast_program(ctx: &mut ExecCtx<'_, '_>, code: BcId) -> bool {
    let heap = &mut ctx.runtime.heap;
    let function = heap.bytecode.get(code).unwrap().compiled.as_ref().unwrap();
    if function.fast.is_some() {
        return true;
    }
    if !function.hoisted.is_empty() || heap.ledger.failpoint_armed() {
        return false;
    }
    let compiled = FastProgram::compile(&heap.ledger, function)
        .and_then(|prog| TrackedBox::new(&heap.ledger, prog));
    if let Ok(prog) = compiled {
        heap.bytecode
            .get_mut(code)
            .unwrap()
            .compiled
            .as_mut()
            .unwrap()
            .fast = Some(prog);
        true
    } else {
        false
    }
}

pub(crate) fn run_frame(
    ctx: &mut ExecCtx<'_, '_>,
    mut index: usize,
    mut code: BcId,
) -> JsResult<Option<Value>> {
    use fast::FastOp;

    let entry_index = index;
    let mut sp = ctx.runtime.vm.stack.len();
    let mut cached_ip: Option<usize> = None;

    'frames: loop {
        let frame = ctx.runtime.vm.frames[index];
        let arg_base = frame.base as usize + 2;
        let var_base = arg_base + frame.arg_slots as usize;
        let mut pc = frame.pc;

        if !ensure_fast_program(ctx, code) {
            ctx.runtime.vm.stack.truncate(sp);
            return run_raw_frame(ctx, index, code, entry_index);
        }

        let frame_limit = {
            let f = ctx
                .runtime
                .heap
                .bytecode
                .get(code)
                .unwrap()
                .compiled
                .as_ref()
                .unwrap();
            frame.floor as usize + usize::from(f.stack_size)
        };

        'resume: loop {
            if ctx.runtime.vm.stack.capacity() < frame_limit + 1 {
                ctx.runtime.vm.stack.truncate(sp);
                let extra = (frame_limit + 1).saturating_sub(sp);
                ctx.reserve_stack(extra)?;
            }
            while ctx.runtime.vm.stack.len() < frame_limit {
                ctx.runtime.vm.stack.push_reserved(Value::UNDEFINED);
            }

            let start_ip = if let Some(ip) = cached_ip.take() {
                Ok(ip)
            } else {
                let fast = ctx
                    .runtime
                    .heap
                    .bytecode
                    .get(code)
                    .unwrap()
                    .compiled
                    .as_ref()
                    .unwrap()
                    .fast
                    .as_deref()
                    .unwrap();
                fast.orig_pcs.binary_search(&pc)
            };

            let Ok(mut ip) = start_ip else {
                // Resumed mid-fusion after a slow-path bailout; step raw bytecode until a boundary.
                ctx.runtime.vm.stack.truncate(sp);
                let result = run_raw_step(ctx, index, code, pc)?;
                if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                    return Ok(result);
                }
                pc = ctx.runtime.vm.frames[index].pc;
                sp = ctx.runtime.vm.stack.len();
                continue 'resume;
            };

            let brk = {
                let heap = &mut ctx.runtime.heap;
                let vm = &mut ctx.runtime.vm;
                let modules = &ctx.runtime.modules;
                let bytecode = heap.bytecode.get(code).unwrap();
                let function = bytecode.compiled.as_ref().unwrap();
                let fast = function.fast.as_deref().unwrap();
                let ops = &fast.ops[..];
                let orig_pcs = &fast.orig_pcs[..];
                let ic_atoms = &fast.ic_atoms[..];
                let caches = &fast.caches[..];
                let constants = &bytecode.constants[..];
                let stack_cap = vm.stack.capacity();
                let stack_bytes_limit = vm.limits.stack_bytes;
                let reentry_bytes = vm.reentry_depth.saturating_mul(crate::vm::REENTRY_COST);
                let stack: &mut [Value] = &mut vm.stack[..];
                let global_object = heap.realm(frame.realm).global_object;

                loop {
                    let cur_ip = ip;
                    let op = ops[cur_ip];
                    ip = cur_ip + 1;

                    match op {
                        FastOp::Nop => {}
                        FastOp::PushI32(n) => {
                            stack[sp] = Value::int32(n);
                            sp += 1;
                        }
                        FastOp::PushBigintI32(n) => {
                            stack[sp] = Value::short_bigint(i64::from(n)).unwrap();
                            sp += 1;
                        }
                        FastOp::Undefined => {
                            stack[sp] = Value::UNDEFINED;
                            sp += 1;
                        }
                        FastOp::Null => {
                            stack[sp] = Value::NULL;
                            sp += 1;
                        }
                        FastOp::PushFalse => {
                            stack[sp] = Value::bool(false);
                            sp += 1;
                        }
                        FastOp::PushTrue => {
                            stack[sp] = Value::bool(true);
                            sp += 1;
                        }
                        FastOp::PushEmptyString => {
                            if let Some(id) = heap.atoms.ascii_string(128) {
                                stack[sp] = Value::string(id);
                                sp += 1;
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::PushThis => {
                            let raw = stack[arg_base - 1];
                            let value = if function.strict || raw.as_object().is_some() {
                                raw
                            } else if raw == Value::NULL || raw == Value::UNDEFINED {
                                Value::object(global_object)
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::SpecialObject(sel) => {
                            stack[sp] = if sel == 2 {
                                Value::object(frame.callee)
                            } else {
                                frame.new_target.map_or(Value::UNDEFINED, Value::object)
                            };
                            sp += 1;
                        }
                        FastOp::PushConst(idx) => {
                            stack[sp] = constants[idx as usize];
                            sp += 1;
                        }
                        FastOp::PushRegExpConst(idx) => {
                            break FastBreak::CloneRegExp {
                                constant: constants[idx as usize],
                                at: orig_pcs[cur_ip],
                                next_pc: orig_pcs[cur_ip + 1],
                                next_ip: ip,
                            };
                        }
                        FastOp::PushConstRepeat { index: idx, count } => {
                            let value = constants[idx as usize];
                            let n = usize::from(count);
                            stack[sp..sp + n].fill(value);
                            sp += n;
                        }
                        FastOp::PushConstSeq { start, count } => {
                            let s = start as usize;
                            let n = usize::from(count);
                            stack[sp..sp + n].copy_from_slice(&constants[s..s + n]);
                            sp += n;
                        }
                        FastOp::Drop => {
                            sp -= 1;
                        }
                        FastOp::Dup => {
                            stack[sp] = stack[sp - 1];
                            sp += 1;
                        }
                        FastOp::Dup1 => {
                            let last = stack[sp - 1];
                            stack[sp - 1] = stack[sp - 2];
                            stack[sp] = last;
                            sp += 1;
                        }
                        FastOp::Swap => {
                            stack.swap(sp - 2, sp - 1);
                        }
                        FastOp::Nip => {
                            let last = stack[sp - 1];
                            sp -= 1;
                            stack[sp - 1] = last;
                        }
                        FastOp::Insert2 => {
                            let a = stack[sp - 2];
                            let b = stack[sp - 1];
                            stack[sp - 2] = b;
                            stack[sp - 1] = a;
                            stack[sp] = b;
                            sp += 1;
                        }
                        FastOp::Insert3 => {
                            let a = stack[sp - 3];
                            let b = stack[sp - 2];
                            let c = stack[sp - 1];
                            stack[sp - 3] = c;
                            stack[sp - 2] = a;
                            stack[sp - 1] = b;
                            stack[sp] = c;
                            sp += 1;
                        }
                        FastOp::Perm3 => {
                            stack[sp - 3..sp - 1].rotate_right(1);
                        }
                        FastOp::Perm4 => {
                            stack[sp - 4..sp - 1].rotate_right(1);
                        }
                        FastOp::Rot3l => {
                            let a = stack[sp - 3];
                            stack[sp - 3] = stack[sp - 2];
                            stack[sp - 2] = stack[sp - 1];
                            stack[sp - 1] = a;
                        }
                        FastOp::Rot3r => {
                            let c = stack[sp - 1];
                            stack[sp - 1] = stack[sp - 2];
                            stack[sp - 2] = stack[sp - 3];
                            stack[sp - 3] = c;
                        }
                        FastOp::GetLoc(n) => {
                            stack[sp] = stack[var_base + usize::from(n)];
                            sp += 1;
                        }
                        FastOp::PutLoc(n) => {
                            sp -= 1;
                            stack[var_base + usize::from(n)] = stack[sp];
                        }
                        FastOp::SetLoc(n) => {
                            stack[var_base + usize::from(n)] = stack[sp - 1];
                        }
                        FastOp::GetLocPair(a, b) => {
                            stack[sp] = stack[var_base + usize::from(a)];
                            stack[sp + 1] = stack[var_base + usize::from(b)];
                            sp += 2;
                        }
                        FastOp::GetLocPutLoc(src, dst) => {
                            stack[var_base + usize::from(dst)] = stack[var_base + usize::from(src)];
                        }
                        FastOp::GetArgPutLoc(src, dst) => {
                            stack[var_base + usize::from(dst)] = stack[arg_base + usize::from(src)];
                        }
                        FastOp::PushI16PutLoc(dst, imm) => {
                            stack[var_base + usize::from(dst)] = Value::int32(i32::from(imm));
                        }
                        FastOp::GetArg(n) => {
                            stack[sp] = stack[arg_base + usize::from(n)];
                            sp += 1;
                        }
                        FastOp::PutArg(n) => {
                            sp -= 1;
                            stack[arg_base + usize::from(n)] = stack[sp];
                        }
                        FastOp::SetArg(n) => {
                            stack[arg_base + usize::from(n)] = stack[sp - 1];
                        }
                        FastOp::GetVarRef(n) => {
                            let ObjectData::BytecodeFunction(data) =
                                &heap.object(frame.callee).data
                            else {
                                unreachable!()
                            };
                            let Some(ValueKind::VarRef(id)) =
                                data.closure.get(usize::from(n)).map(|v| v.decode())
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let value = match heap.var_refs.get(id).unwrap().state {
                                VarRefState::OpenStack(i) => stack[i as usize],
                                VarRefState::OpenCoro { coro, local } => {
                                    heap.coroutines.get(coro).unwrap().stack[local as usize]
                                }
                                VarRefState::Closed(value) => value,
                            };
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::GetLocCheck(n) => {
                            let value = stack[var_base + usize::from(n)];
                            if value == Value::UNINITIALIZED {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::PutLocCheck(n) => {
                            let slot = var_base + usize::from(n);
                            if stack[slot] == Value::UNINITIALIZED {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            sp -= 1;
                            stack[slot] = stack[sp];
                        }
                        FastOp::PutLocCheckInit(n) => {
                            let slot = var_base + usize::from(n);
                            if stack[slot] != Value::UNINITIALIZED {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            sp -= 1;
                            stack[slot] = stack[sp];
                        }
                        FastOp::SetLocUninitialized(n) => {
                            stack[var_base + usize::from(n)] = Value::UNINITIALIZED;
                        }
                        FastOp::GetVarRefCheck(n) => {
                            let ObjectData::BytecodeFunction(data) =
                                &heap.object(frame.callee).data
                            else {
                                unreachable!()
                            };
                            let Some(ValueKind::VarRef(id)) =
                                data.closure.get(usize::from(n)).map(|v| v.decode())
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let value = match heap.var_refs.get(id).unwrap().state {
                                VarRefState::OpenStack(i) => stack[i as usize],
                                VarRefState::OpenCoro { coro, local } => {
                                    heap.coroutines.get(coro).unwrap().stack[local as usize]
                                }
                                VarRefState::Closed(value) => value,
                            };
                            if value == Value::UNINITIALIZED {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::PutVarRef(n)
                        | FastOp::SetVarRef(n)
                        | FastOp::PutVarRefCheck(n)
                        | FastOp::PutVarRefCheckInit(n) => {
                            let ObjectData::BytecodeFunction(data) =
                                &heap.object(frame.callee).data
                            else {
                                unreachable!()
                            };
                            let Some(ValueKind::VarRef(id)) =
                                data.closure.get(usize::from(n)).map(|v| v.decode())
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let value = stack[sp - 1];
                            if matches!(
                                op,
                                FastOp::PutVarRefCheck(_) | FastOp::PutVarRefCheckInit(_)
                            ) {
                                let current = match heap.var_refs.get(id).unwrap().state {
                                    VarRefState::OpenStack(i) => stack[i as usize],
                                    VarRefState::OpenCoro { coro, local } => {
                                        heap.coroutines.get(coro).unwrap().stack[local as usize]
                                    }
                                    VarRefState::Closed(value) => value,
                                };
                                let uninitialized = current == Value::UNINITIALIZED;
                                if uninitialized == matches!(op, FastOp::PutVarRefCheck(_)) {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                }
                            }
                            let entry = heap.var_refs.get_mut(id).unwrap();
                            match entry.state {
                                VarRefState::OpenStack(i) => stack[i as usize] = value,
                                VarRefState::OpenCoro { coro, local } => {
                                    heap.coroutines.get_mut(coro).unwrap().stack[local as usize] =
                                        value;
                                }
                                VarRefState::Closed(_) => entry.state = VarRefState::Closed(value),
                            }
                            if !matches!(op, FastOp::SetVarRef(_)) {
                                sp -= 1;
                            }
                        }
                        FastOp::IncLoc(n) => {
                            let slot = var_base + usize::from(n);
                            let Some(after) = fast_inc_value(stack[slot]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[slot] = after;
                        }
                        FastOp::DecLoc(n) => {
                            let slot = var_base + usize::from(n);
                            let Some(after) = fast_dec_value(stack[slot]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[slot] = after;
                        }
                        FastOp::IncArg(n) => {
                            let slot = arg_base + usize::from(n);
                            let Some(after) = fast_inc_value(stack[slot]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[slot] = after;
                        }
                        FastOp::DecArg(n) => {
                            let slot = arg_base + usize::from(n);
                            let Some(after) = fast_dec_value(stack[slot]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[slot] = after;
                        }
                        FastOp::IncVarRef(n) | FastOp::DecVarRef(n) => {
                            let ObjectData::BytecodeFunction(data) =
                                &heap.object(frame.callee).data
                            else {
                                unreachable!()
                            };
                            let Some(ValueKind::VarRef(id)) =
                                data.closure.get(usize::from(n)).map(|v| v.decode())
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let inc = matches!(op, FastOp::IncVarRef(_));
                            let entry = heap.var_refs.get_mut(id).unwrap();
                            match entry.state {
                                VarRefState::OpenStack(i) => {
                                    let cur = stack[i as usize];
                                    let next = if inc {
                                        fast_inc_value(cur)
                                    } else {
                                        fast_dec_value(cur)
                                    };
                                    let Some(after) = next else {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    };
                                    stack[i as usize] = after;
                                }
                                VarRefState::OpenCoro { coro, local } => {
                                    let coro_stack =
                                        &mut heap.coroutines.get_mut(coro).unwrap().stack;
                                    let cur = coro_stack[local as usize];
                                    let next = if inc {
                                        fast_inc_value(cur)
                                    } else {
                                        fast_dec_value(cur)
                                    };
                                    let Some(after) = next else {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    };
                                    coro_stack[local as usize] = after;
                                }
                                VarRefState::Closed(cur) => {
                                    let next = if inc {
                                        fast_inc_value(cur)
                                    } else {
                                        fast_dec_value(cur)
                                    };
                                    let Some(after) = next else {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    };
                                    entry.state = VarRefState::Closed(after);
                                }
                            }
                        }
                        FastOp::AddLoc(n) => {
                            let lhs_slot = var_base + usize::from(n);
                            let lhs = stack[lhs_slot];
                            let rhs = stack[sp - 1];
                            if let Some(result) =
                                fast_binary(Opcode::Add, lhs, rhs, vm.interrupt_handler.is_some())
                            {
                                stack[lhs_slot] = result;
                                sp -= 1;
                            } else if let (ValueKind::String(a), ValueKind::String(b)) =
                                (lhs.decode(), rhs.decode())
                            {
                                if heap.string(a).is_empty() {
                                    stack[lhs_slot] = Value::string(b);
                                    sp -= 1;
                                } else if heap.string(b).is_empty() {
                                    sp -= 1;
                                } else {
                                    break FastBreak::ConcatStrings {
                                        a,
                                        b,
                                        dest: lhs_slot,
                                        truncate_to: sp - 1,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                            } else if is_bigint(lhs) && is_bigint(rhs) {
                                break FastBreak::BigIntBinary {
                                    op: Opcode::Add,
                                    lhs,
                                    rhs,
                                    dest: lhs_slot,
                                    truncate_to: sp - 1,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else {
                                match check_empty_string_add(heap, lhs, rhs) {
                                    EmptyStringAdd::Inline(v) => {
                                        stack[lhs_slot] = v;
                                        sp -= 1;
                                    }
                                    EmptyStringAdd::Int(value) => {
                                        break FastBreak::IntToStringDest {
                                            value,
                                            dest: lhs_slot,
                                            truncate_to: sp - 1,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::Float(value) => {
                                        break FastBreak::FloatToStringDest {
                                            value,
                                            dest: lhs_slot,
                                            truncate_to: sp - 1,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::None => {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                }
                            }
                        }
                        FastOp::AddPutLoc(dst) => {
                            let lhs = stack[sp - 2];
                            let rhs = stack[sp - 1];
                            let dst_slot = var_base + usize::from(dst);
                            if let Some(result) =
                                fast_binary(Opcode::Add, lhs, rhs, vm.interrupt_handler.is_some())
                            {
                                stack[dst_slot] = result;
                                sp -= 2;
                            } else if let (ValueKind::String(a), ValueKind::String(b)) =
                                (lhs.decode(), rhs.decode())
                            {
                                if heap.string(a).is_empty() {
                                    stack[dst_slot] = Value::string(b);
                                    sp -= 2;
                                } else if heap.string(b).is_empty() {
                                    stack[dst_slot] = Value::string(a);
                                    sp -= 2;
                                } else {
                                    break FastBreak::ConcatStrings {
                                        a,
                                        b,
                                        dest: dst_slot,
                                        truncate_to: sp - 2,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                            } else if is_bigint(lhs) && is_bigint(rhs) {
                                break FastBreak::BigIntBinary {
                                    op: Opcode::Add,
                                    lhs,
                                    rhs,
                                    dest: dst_slot,
                                    truncate_to: sp - 2,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else {
                                match check_empty_string_add(heap, lhs, rhs) {
                                    EmptyStringAdd::Inline(v) => {
                                        stack[dst_slot] = v;
                                        sp -= 2;
                                    }
                                    EmptyStringAdd::Int(value) => {
                                        break FastBreak::IntToStringDest {
                                            value,
                                            dest: dst_slot,
                                            truncate_to: sp - 2,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::Float(value) => {
                                        break FastBreak::FloatToStringDest {
                                            value,
                                            dest: dst_slot,
                                            truncate_to: sp - 2,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::None => {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                }
                            }
                        }
                        FastOp::SubPutLoc(dst) => {
                            let lhs = stack[sp - 2];
                            let rhs = stack[sp - 1];
                            if let Some(result) =
                                fast_binary(Opcode::Sub, lhs, rhs, vm.interrupt_handler.is_some())
                            {
                                stack[var_base + usize::from(dst)] = result;
                                sp -= 2;
                            } else if let Some(result) =
                                fast_binary_with_string(heap, Opcode::Sub, lhs, rhs)
                            {
                                stack[var_base + usize::from(dst)] = result;
                                sp -= 2;
                            } else if is_bigint(lhs) && is_bigint(rhs) {
                                break FastBreak::BigIntBinary {
                                    op: Opcode::Sub,
                                    lhs,
                                    rhs,
                                    dest: var_base + usize::from(dst),
                                    truncate_to: sp - 2,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::GetLoc2Mul(a, b) => {
                            let lhs = stack[var_base + usize::from(a)];
                            let rhs = stack[var_base + usize::from(b)];
                            if let Some(result) =
                                fast_binary(Opcode::Mul, lhs, rhs, vm.interrupt_handler.is_some())
                            {
                                stack[sp] = result;
                                sp += 1;
                            } else if is_bigint(lhs) && is_bigint(rhs) {
                                break FastBreak::BigIntBinary {
                                    op: Opcode::Mul,
                                    lhs,
                                    rhs,
                                    dest: sp,
                                    truncate_to: sp + 1,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::GetLoc2MulAddLoc { dst, a, b } => {
                            let d = var_base + usize::from(dst);
                            let av = stack[var_base + usize::from(a)];
                            let bv = stack[var_base + usize::from(b)];
                            let dv = stack[d];
                            let has_int = vm.interrupt_handler.is_some();
                            if let Some(prod) = fast_binary(Opcode::Mul, av, bv, has_int)
                                && let Some(sum) = fast_binary(Opcode::Add, dv, prod, has_int)
                            {
                                stack[d] = sum;
                            } else if is_bigint(dv) && is_bigint(av) && is_bigint(bv) {
                                break FastBreak::BigIntMulAddLoc {
                                    dst_slot: d,
                                    a: av,
                                    b: bv,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::GetLoc2Add(a, b) => {
                            let Some(result) = fast_binary(
                                Opcode::Add,
                                stack[var_base + usize::from(a)],
                                stack[var_base + usize::from(b)],
                                vm.interrupt_handler.is_some(),
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp] = result;
                            sp += 1;
                        }
                        FastOp::GetLocAddLoc { dst, src } => {
                            let d = var_base + usize::from(dst);
                            let s = var_base + usize::from(src);
                            let lhs = stack[d];
                            let rhs = stack[s];
                            if let Some(result) =
                                fast_binary(Opcode::Add, lhs, rhs, vm.interrupt_handler.is_some())
                            {
                                stack[d] = result;
                            } else if let (ValueKind::String(a), ValueKind::String(b)) =
                                (lhs.decode(), rhs.decode())
                            {
                                if heap.string(a).is_empty() {
                                    stack[d] = Value::string(b);
                                } else if !heap.string(b).is_empty() {
                                    break FastBreak::ConcatStrings {
                                        a,
                                        b,
                                        dest: d,
                                        truncate_to: sp,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                            } else if is_bigint(lhs) && is_bigint(rhs) {
                                break FastBreak::BigIntBinary {
                                    op: Opcode::Add,
                                    lhs,
                                    rhs,
                                    dest: d,
                                    truncate_to: sp,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else {
                                match check_empty_string_add(heap, lhs, rhs) {
                                    EmptyStringAdd::Inline(v) => stack[d] = v,
                                    EmptyStringAdd::Int(value) => {
                                        break FastBreak::IntToStringDest {
                                            value,
                                            dest: d,
                                            truncate_to: sp,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::Float(value) => {
                                        break FastBreak::FloatToStringDest {
                                            value,
                                            dest: d,
                                            truncate_to: sp,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::None => {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                }
                            }
                        }
                        FastOp::Add
                        | FastOp::Sub
                        | FastOp::Mul
                        | FastOp::Div
                        | FastOp::Mod
                        | FastOp::Shl
                        | FastOp::Sar
                        | FastOp::Shr
                        | FastOp::And
                        | FastOp::Or
                        | FastOp::Xor
                        | FastOp::Pow => {
                            let binop = match op {
                                FastOp::Add => Opcode::Add,
                                FastOp::Sub => Opcode::Sub,
                                FastOp::Mul => Opcode::Mul,
                                FastOp::Div => Opcode::Div,
                                FastOp::Mod => Opcode::Mod,
                                FastOp::Shl => Opcode::Shl,
                                FastOp::Sar => Opcode::Sar,
                                FastOp::Shr => Opcode::Shr,
                                FastOp::And => Opcode::And,
                                FastOp::Or => Opcode::Or,
                                FastOp::Xor => Opcode::Xor,
                                FastOp::Pow => Opcode::Pow,
                                _ => unreachable!(),
                            };
                            let lhs = stack[sp - 2];
                            let rhs = stack[sp - 1];
                            if let Some(result) =
                                fast_binary(binop, lhs, rhs, vm.interrupt_handler.is_some())
                            {
                                stack[sp - 2] = result;
                                sp -= 1;
                            } else if binop == Opcode::Add
                                && let (ValueKind::String(a), ValueKind::String(b)) =
                                    (lhs.decode(), rhs.decode())
                            {
                                if heap.string(a).is_empty() {
                                    stack[sp - 2] = Value::string(b);
                                    sp -= 1;
                                } else if heap.string(b).is_empty() {
                                    sp -= 1;
                                } else {
                                    break FastBreak::ConcatStrings {
                                        a,
                                        b,
                                        dest: sp - 2,
                                        truncate_to: sp - 1,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                            } else if binop != Opcode::Add
                                && let Some(result) = fast_binary_with_string(heap, binop, lhs, rhs)
                            {
                                stack[sp - 2] = result;
                                sp -= 1;
                            } else if is_bigint(lhs) && is_bigint(rhs) {
                                break FastBreak::BigIntBinary {
                                    op: binop,
                                    lhs,
                                    rhs,
                                    dest: sp - 2,
                                    truncate_to: sp - 1,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            } else if binop == Opcode::Add {
                                match check_empty_string_add(heap, lhs, rhs) {
                                    EmptyStringAdd::Inline(v) => {
                                        stack[sp - 2] = v;
                                        sp -= 1;
                                    }
                                    EmptyStringAdd::Int(value) => {
                                        break FastBreak::IntToStringDest {
                                            value,
                                            dest: sp - 2,
                                            truncate_to: sp - 1,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::Float(value) => {
                                        break FastBreak::FloatToStringDest {
                                            value,
                                            dest: sp - 2,
                                            truncate_to: sp - 1,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                    EmptyStringAdd::None => {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                }
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::Neg
                        | FastOp::Plus
                        | FastOp::Not
                        | FastOp::Inc
                        | FastOp::Dec
                        | FastOp::PostInc
                        | FastOp::PostDec => {
                            let uop = match op {
                                FastOp::Neg => Opcode::Neg,
                                FastOp::Plus => Opcode::Plus,
                                FastOp::Not => Opcode::Not,
                                FastOp::Inc => Opcode::Inc,
                                FastOp::Dec => Opcode::Dec,
                                FastOp::PostInc => Opcode::PostInc,
                                FastOp::PostDec => Opcode::PostDec,
                                _ => unreachable!(),
                            };
                            let Some((before, after)) = fast_unary(uop, stack[sp - 1]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if matches!(op, FastOp::PostInc | FastOp::PostDec) {
                                stack[sp - 1] = before;
                                stack[sp] = after;
                                sp += 1;
                            } else {
                                stack[sp - 1] = after;
                            }
                        }
                        FastOp::Lnot => {
                            stack[sp - 1] =
                                Value::bool(!convert::to_boolean_value(heap, stack[sp - 1]));
                        }
                        FastOp::Eq
                        | FastOp::Neq
                        | FastOp::StrictEq
                        | FastOp::StrictNeq
                        | FastOp::Lt
                        | FastOp::Lte
                        | FastOp::Gt
                        | FastOp::Gte => {
                            let (Some(a), Some(b)) =
                                (stack[sp - 2].as_number(), stack[sp - 1].as_number())
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp - 2] = Value::bool(match op {
                                FastOp::Eq | FastOp::StrictEq => a == b,
                                FastOp::Neq | FastOp::StrictNeq => a != b,
                                FastOp::Lt => a < b,
                                FastOp::Lte => a <= b,
                                FastOp::Gt => a > b,
                                FastOp::Gte => a >= b,
                                _ => unreachable!(),
                            });
                            sp -= 1;
                        }
                        FastOp::IsUndefined => {
                            stack[sp - 1] = Value::bool(stack[sp - 1] == Value::UNDEFINED);
                        }
                        FastOp::IsNull => {
                            stack[sp - 1] = Value::bool(stack[sp - 1] == Value::NULL);
                        }
                        FastOp::IsUndefinedOrNull => {
                            let v = stack[sp - 1];
                            stack[sp - 1] = Value::bool(v == Value::UNDEFINED || v == Value::NULL);
                        }
                        FastOp::TypeofIsUndefined => {
                            let v = stack[sp - 1];
                            let is_undef = match v.decode() {
                                ValueKind::Undefined
                                | ValueKind::Uninitialized
                                | ValueKind::Empty => true,
                                ValueKind::Object(id) => {
                                    heap.object(id).flags.contains(ObjectFlags::IS_HTML_DDA)
                                }
                                _ => false,
                            };
                            stack[sp - 1] = Value::bool(is_undef);
                        }
                        FastOp::TypeofIsFunction => {
                            let is_fn = stack[sp - 1].as_object().is_some_and(|id| {
                                !heap.object(id).flags.contains(ObjectFlags::IS_HTML_DDA)
                                    && call::is_callable(heap, id)
                            });
                            stack[sp - 1] = Value::bool(is_fn);
                        }
                        FastOp::Goto(target) => {
                            let target = target as usize;
                            if target <= cur_ip
                                && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                            {
                                break FastBreak::Interrupted(orig_pcs[cur_ip]);
                            }
                            ip = target;
                        }
                        FastOp::IfFalse(target) => {
                            sp -= 1;
                            if !convert::to_boolean_value(heap, stack[sp]) {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::IfTrue(target) => {
                            sp -= 1;
                            if convert::to_boolean_value(heap, stack[sp]) {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::LtIfFalse(target)
                        | FastOp::LtIfTrue(target)
                        | FastOp::LteIfFalse(target)
                        | FastOp::LteIfTrue(target)
                        | FastOp::GtIfFalse(target)
                        | FastOp::GtIfTrue(target)
                        | FastOp::GteIfFalse(target)
                        | FastOp::GteIfTrue(target)
                        | FastOp::EqIfFalse(target)
                        | FastOp::EqIfTrue(target)
                        | FastOp::NeqIfFalse(target)
                        | FastOp::NeqIfTrue(target)
                        | FastOp::StrictEqIfFalse(target)
                        | FastOp::StrictEqIfTrue(target)
                        | FastOp::StrictNeqIfFalse(target)
                        | FastOp::StrictNeqIfTrue(target) => {
                            let (Some(a), Some(b)) =
                                (stack[sp - 2].as_number(), stack[sp - 1].as_number())
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            sp -= 2;
                            let cond = match op {
                                FastOp::LtIfFalse(_) | FastOp::LtIfTrue(_) => a < b,
                                FastOp::LteIfFalse(_) | FastOp::LteIfTrue(_) => a <= b,
                                FastOp::GtIfFalse(_) | FastOp::GtIfTrue(_) => a > b,
                                FastOp::GteIfFalse(_) | FastOp::GteIfTrue(_) => a >= b,
                                FastOp::EqIfFalse(_)
                                | FastOp::EqIfTrue(_)
                                | FastOp::StrictEqIfFalse(_)
                                | FastOp::StrictEqIfTrue(_) => a == b,
                                _ => a != b,
                            };
                            let branch_on_true = matches!(
                                op,
                                FastOp::LtIfTrue(_)
                                    | FastOp::LteIfTrue(_)
                                    | FastOp::GtIfTrue(_)
                                    | FastOp::GteIfTrue(_)
                                    | FastOp::EqIfTrue(_)
                                    | FastOp::NeqIfTrue(_)
                                    | FastOp::StrictEqIfTrue(_)
                                    | FastOp::StrictNeqIfTrue(_)
                            );
                            if cond == branch_on_true {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::IncLocGoto { loc, target } => {
                            let slot = var_base + usize::from(loc);
                            let Some(after) = fast_inc_value(stack[slot]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[slot] = after;
                            let target = target as usize;
                            if target <= cur_ip
                                && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                            {
                                break FastBreak::Interrupted(orig_pcs[cur_ip]);
                            }
                            ip = target;
                        }
                        FastOp::DecLocGoto { loc, target } => {
                            let slot = var_base + usize::from(loc);
                            let Some(after) = fast_dec_value(stack[slot]) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[slot] = after;
                            let target = target as usize;
                            if target <= cur_ip
                                && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                            {
                                break FastBreak::Interrupted(orig_pcs[cur_ip]);
                            }
                            ip = target;
                        }
                        FastOp::GetLocLtArgIfFalse { loc, arg, target } => {
                            let Some(cond) = fast_lt(
                                stack[var_base + usize::from(loc)],
                                stack[arg_base + usize::from(arg)],
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if !cond {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::GetLocLtLocIfFalse { a, b, target } => {
                            let Some(cond) = fast_lt(
                                stack[var_base + usize::from(a)],
                                stack[var_base + usize::from(b)],
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if !cond {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::GetLocLtImmIfFalse { loc, imm, target } => {
                            let v = stack[var_base + usize::from(loc)];
                            let cond = match v.decode() {
                                ValueKind::Int32(n) => n < i32::from(imm),
                                ValueKind::Float64(n) => n < f64::from(imm),
                                _ => break FastBreak::Slow(orig_pcs[cur_ip]),
                            };
                            if !cond {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::GetLocLteArgIfFalse { loc, arg, target } => {
                            let Some(cond) = fast_lte(
                                stack[var_base + usize::from(loc)],
                                stack[arg_base + usize::from(arg)],
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if !cond {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::GetLocLteLocIfFalse { a, b, target } => {
                            let Some(cond) = fast_lte(
                                stack[var_base + usize::from(a)],
                                stack[var_base + usize::from(b)],
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if !cond {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::GetLocLteImmIfFalse { loc, imm, target } => {
                            let v = stack[var_base + usize::from(loc)];
                            let cond = match v.decode() {
                                ValueKind::Int32(n) => n <= i32::from(imm),
                                ValueKind::Float64(n) => n <= f64::from(imm),
                                _ => break FastBreak::Slow(orig_pcs[cur_ip]),
                            };
                            if !cond {
                                let target = target as usize;
                                if target <= cur_ip
                                    && vm.interrupt_handler.as_mut().is_some_and(|h| h())
                                {
                                    break FastBreak::Interrupted(orig_pcs[cur_ip]);
                                }
                                ip = target;
                            }
                        }
                        FastOp::GetField(ic) => {
                            let idx = usize::from(ic);
                            let Some(value) = cache::get_field(
                                heap,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                stack[sp - 1],
                                frame.realm,
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp - 1] = value;
                        }
                        FastOp::GetField2(ic) => {
                            let idx = usize::from(ic);
                            let Some(value) = cache::get_field(
                                heap,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                stack[sp - 1],
                                frame.realm,
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::GetLocGetField { loc, ic } => {
                            let idx = usize::from(ic);
                            let Some(value) = cache::get_field(
                                heap,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                stack[var_base + usize::from(loc)],
                                frame.realm,
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::GetLocGetFieldAddLoc { dst, obj, ic } => {
                            let idx = usize::from(ic);
                            let Some(field_val) = cache::get_field(
                                heap,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                stack[var_base + usize::from(obj)],
                                frame.realm,
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let d = var_base + usize::from(dst);
                            let Some(sum) = fast_binary(
                                Opcode::Add,
                                stack[d],
                                field_val,
                                vm.interrupt_handler.is_some(),
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[d] = sum;
                        }
                        FastOp::PutLocField { obj, val, ic } => {
                            let idx = usize::from(ic);
                            let receiver = stack[var_base + usize::from(obj)];
                            let value = stack[var_base + usize::from(val)];
                            match cache::put_field(
                                &mut heap.objects,
                                &heap.shapes,
                                &heap.symbols,
                                &heap.symbol_registry,
                                heap.building_shapes,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                receiver,
                                value,
                            ) {
                                cache::PutFieldResult::Done => {}
                                cache::PutFieldResult::TransitionGrow { id, to_shape } => {
                                    break FastBreak::PutFieldTransitionGrow {
                                        id,
                                        to_shape,
                                        value,
                                        truncate_to: sp,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                                cache::PutFieldResult::Slow => {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                }
                            }
                        }
                        FastOp::PutField(ic) => {
                            let idx = usize::from(ic);
                            let receiver = stack[sp - 2];
                            let value = stack[sp - 1];
                            match cache::put_field(
                                &mut heap.objects,
                                &heap.shapes,
                                &heap.symbols,
                                &heap.symbol_registry,
                                heap.building_shapes,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                receiver,
                                value,
                            ) {
                                cache::PutFieldResult::Done => {
                                    sp -= 2;
                                }
                                cache::PutFieldResult::TransitionGrow { id, to_shape } => {
                                    break FastBreak::PutFieldTransitionGrow {
                                        id,
                                        to_shape,
                                        value,
                                        truncate_to: sp - 2,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                                cache::PutFieldResult::Slow => {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                }
                            }
                        }
                        FastOp::GetLocGetVarAddLoc { dst, ic } => {
                            let idx = usize::from(ic);
                            let id = global_object;
                            let object = heap.object(id);
                            let value = match cache::global_slot(
                                heap,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                object,
                                false,
                                false,
                                vm.global_lexical_epoch,
                                modules,
                                frame.realm,
                            ) {
                                Some(cache::GlobalBinding::Property { slot, .. }) => {
                                    object.props[slot]
                                }
                                Some(cache::GlobalBinding::Lexical { binding, .. }) => {
                                    let value = var_ref_value(heap, stack, binding);
                                    if value == Value::UNINITIALIZED {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                    value
                                }
                                None => break FastBreak::Slow(orig_pcs[cur_ip]),
                            };
                            let d = var_base + usize::from(dst);
                            let Some(sum) = fast_binary(
                                Opcode::Add,
                                stack[d],
                                value,
                                vm.interrupt_handler.is_some(),
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[d] = sum;
                        }
                        FastOp::GetVar(ic)
                        | FastOp::GetVarUndef(ic)
                        | FastOp::PutVar(ic)
                        | FastOp::MakeVarRef(ic) => {
                            let idx = usize::from(ic);
                            let id = global_object;
                            let object = heap.object(id);
                            let writing = matches!(op, FastOp::PutVar(_));
                            let want_ref = matches!(op, FastOp::MakeVarRef(_));
                            let Some(binding) = cache::global_slot(
                                heap,
                                function,
                                &caches[idx],
                                ic_atoms[idx],
                                object,
                                writing,
                                want_ref,
                                vm.global_lexical_epoch,
                                modules,
                                frame.realm,
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            match (op, binding) {
                                (
                                    FastOp::PutVar(_),
                                    cache::GlobalBinding::Property { slot, .. },
                                ) => {
                                    sp -= 1;
                                    heap.objects.get_mut(id).unwrap().props[slot] = stack[sp];
                                }
                                (
                                    FastOp::MakeVarRef(_),
                                    cache::GlobalBinding::Property { key, .. },
                                ) => {
                                    stack[sp] = Value::object(id);
                                    stack[sp + 1] = key;
                                    sp += 2;
                                }
                                (_, cache::GlobalBinding::Property { slot, .. }) => {
                                    stack[sp] = object.props[slot];
                                    sp += 1;
                                }
                                (
                                    FastOp::MakeVarRef(_),
                                    cache::GlobalBinding::Lexical { binding, key, .. },
                                ) => {
                                    stack[sp] = Value::var_ref(binding);
                                    stack[sp + 1] = key;
                                    sp += 2;
                                }
                                // A binding in its temporal dead zone, or a
                                // constant being written, throws on the slow path.
                                (
                                    _,
                                    cache::GlobalBinding::Lexical {
                                        binding, writable, ..
                                    },
                                ) => {
                                    let value = var_ref_value(heap, stack, binding);
                                    if value == Value::UNINITIALIZED || (writing && !writable) {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                    if writing {
                                        sp -= 1;
                                        let value = stack[sp];
                                        set_var_ref_value(
                                            &mut heap.var_refs,
                                            &mut heap.coroutines,
                                            stack,
                                            binding,
                                            value,
                                        );
                                    } else {
                                        stack[sp] = value;
                                        sp += 1;
                                    }
                                }
                            }
                        }
                        FastOp::GetRefValue(ic) | FastOp::PutRefValue(ic) => {
                            let writing = matches!(op, FastOp::PutRefValue(_));
                            let source = sp - 2 - usize::from(writing);
                            let global = global_object;
                            if stack[source] != Value::object(global) {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            let ValueKind::String(str_id) = stack[source + 1].decode() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let object = heap.object(global);
                            let Some(slot) = cache::resolved_ref_slot(
                                heap,
                                &caches[usize::from(ic)],
                                object,
                                writing,
                                str_id,
                                frame.realm,
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if writing {
                                heap.objects.get_mut(global).unwrap().props[slot] = stack[sp - 1];
                                sp = source;
                            } else {
                                stack[sp] = object.props[slot];
                                sp += 1;
                            }
                        }
                        FastOp::GetLength => {
                            let top = stack[sp - 1];
                            if let Some(id) = top.as_object()
                                && let ObjectData::Array(array) = &heap.object(id).data
                            {
                                stack[sp - 1] = cache::length_value(array.length);
                            } else if let ValueKind::String(id) = top.decode() {
                                stack[sp - 1] = Value::int32(heap.string(id).len() as i32);
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::GetArrayEl | FastOp::GetArrayEl2 | FastOp::PutArrayEl => {
                            let writing = matches!(op, FastOp::PutArrayEl);
                            let object_slot = sp - 2 - usize::from(writing);
                            let Some(id) = stack[object_slot].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if writing {
                                let ValueKind::Int32(element_i32) = stack[object_slot + 1].decode()
                                else {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                };
                                let Ok(element) = usize::try_from(element_i32) else {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                };
                                let object = heap.object(id);
                                let value = stack[sp - 1];
                                match &object.data {
                                    ObjectData::Array(array)
                                        if object.flags.contains(
                                            ObjectFlags::FAST_ARRAY | ObjectFlags::EXTENSIBLE,
                                        ) && !object
                                            .flags
                                            .contains(ObjectFlags::IS_PROTOTYPE)
                                            && array
                                                .elements
                                                .get(element)
                                                .is_some_and(|&v| v != Value::EMPTY) =>
                                    {
                                        let ObjectData::Array(array) =
                                            &mut heap.objects.get_mut(id).unwrap().data
                                        else {
                                            unreachable!()
                                        };
                                        array.elements[element] = value;
                                    }
                                    ObjectData::Array(array)
                                        if object.flags.contains(
                                            ObjectFlags::FAST_ARRAY | ObjectFlags::EXTENSIBLE,
                                        ) && !object
                                            .flags
                                            .contains(ObjectFlags::IS_PROTOTYPE)
                                            && array.length_writable
                                            && element == array.length as usize
                                            && element == array.elements.len()
                                            && element
                                                < crate::atom::INLINE_INDEX_LIMIT as usize
                                            && heap.has_intact_array_prototype(
                                                object.shape,
                                                array.realm,
                                            ) =>
                                    {
                                        if element < array.elements.capacity() {
                                            let ObjectData::Array(array) =
                                                &mut heap.objects.get_mut(id).unwrap().data
                                            else {
                                                unreachable!()
                                            };
                                            array.elements.push_reserved(value);
                                            array.length += 1;
                                        } else if stack_cap.saturating_sub(sp) >= 2 {
                                            break FastBreak::AppendArray {
                                                id,
                                                value,
                                                truncate_to: object_slot,
                                                at: orig_pcs[cur_ip],
                                                next_pc: orig_pcs[cur_ip + 1],
                                                next_ip: ip,
                                            };
                                        } else {
                                            break FastBreak::Slow(orig_pcs[cur_ip]);
                                        }
                                    }
                                    ObjectData::TypedArray(view) => {
                                        let view = **view;
                                        if crate::builtins::typed_atomics::fast_typed_array_set(
                                            &mut heap.objects,
                                            view,
                                            element,
                                            value,
                                        )
                                        .is_none()
                                        {
                                            break FastBreak::Slow(orig_pcs[cur_ip]);
                                        }
                                    }
                                    ObjectData::Ordinary
                                        if !object.flags.contains(
                                            ObjectFlags::IS_EXOTIC | ObjectFlags::IS_PROTOTYPE,
                                        ) && object.flags.contains(ObjectFlags::EXTENSIBLE)
                                            && heap.shape(object.shape).is_dictionary()
                                            && let Some(prop_key) =
                                                PropKey::index(element_i32 as u32) =>
                                    {
                                        let shape_id = object.shape;
                                        let shape = heap.shape(shape_id);
                                        if let Some((slot, flags)) = shape.find(prop_key) {
                                            if flags.contains(PropFlags::ACCESSOR)
                                                || !flags.contains(PropFlags::WRITABLE)
                                            {
                                                break FastBreak::Slow(orig_pcs[cur_ip]);
                                            }
                                            heap.objects.get_mut(id).unwrap().props[slot] = value;
                                        } else {
                                            let proto_ok = shape.proto.is_none_or(|p_id| {
                                                let p = heap.object(p_id);
                                                !p.flags.contains(ObjectFlags::IS_EXOTIC) && {
                                                    let ps = heap.shape(p.shape);
                                                    !ps.is_mutable()
                                                        && ps.proto.is_none()
                                                        && !ps.has_index_key()
                                                }
                                            });
                                            if !proto_ok {
                                                break FastBreak::Slow(orig_pcs[cur_ip]);
                                            }
                                            if shape.dict_has_spare_capacity(&object.props) {
                                                let shape = heap.shapes.get_mut(shape_id).unwrap();
                                                let object = heap.objects.get_mut(id).unwrap();
                                                shape.dict_insert_reserved(
                                                    &mut object.props,
                                                    crate::shape::ShapeProp {
                                                        key: prop_key,
                                                        flags: PropFlags::DEFAULT_DATA,
                                                    },
                                                    value,
                                                );
                                            } else {
                                                break FastBreak::DictPut {
                                                    id,
                                                    shape_id,
                                                    key: prop_key,
                                                    value,
                                                    truncate_to: object_slot,
                                                    at: orig_pcs[cur_ip],
                                                    next_pc: orig_pcs[cur_ip + 1],
                                                    next_ip: ip,
                                                };
                                            }
                                        }
                                    }
                                    _ => break FastBreak::Slow(orig_pcs[cur_ip]),
                                }
                                sp = object_slot;
                            } else {
                                let Some(element) = fast_array_index(heap, stack[object_slot + 1])
                                else {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                };
                                let Some(value) = fast_array_get_value(heap, id, element) else {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                };
                                if matches!(op, FastOp::GetArrayEl2) {
                                    stack[sp - 1] = value;
                                } else {
                                    stack[object_slot] = value;
                                    sp -= 1;
                                }
                            }
                        }
                        FastOp::GetLocImmArrayEl { arr, idx } => {
                            let Some(id) = stack[var_base + usize::from(arr)].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let Some(value) = fast_array_get_value(heap, id, usize::from(idx))
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::GetLocImmArrayElAddLoc { dst, arr, idx } => {
                            let Some(id) = stack[var_base + usize::from(arr)].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let Some(elem_val) = fast_array_get_value(heap, id, usize::from(idx))
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let d = var_base + usize::from(dst);
                            let Some(sum) = fast_binary(
                                Opcode::Add,
                                stack[d],
                                elem_val,
                                vm.interrupt_handler.is_some(),
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[d] = sum;
                        }
                        FastOp::PutLocImmArrayEl { arr, idx, val } => {
                            let Some(id) = stack[var_base + usize::from(arr)].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let element = usize::from(idx);
                            let value = stack[var_base + usize::from(val)];
                            let object = heap.object(id);
                            match &object.data {
                                ObjectData::Array(array)
                                    if object.flags.contains(
                                        ObjectFlags::FAST_ARRAY | ObjectFlags::EXTENSIBLE,
                                    ) && !object.flags.contains(ObjectFlags::IS_PROTOTYPE)
                                        && array
                                            .elements
                                            .get(element)
                                            .is_some_and(|&v| v != Value::EMPTY) =>
                                {
                                    let ObjectData::Array(array) =
                                        &mut heap.objects.get_mut(id).unwrap().data
                                    else {
                                        unreachable!()
                                    };
                                    array.elements[element] = value;
                                }
                                ObjectData::TypedArray(view) => {
                                    let view = **view;
                                    if crate::builtins::typed_atomics::fast_typed_array_set(
                                        &mut heap.objects,
                                        view,
                                        element,
                                        value,
                                    )
                                    .is_none()
                                    {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    }
                                }
                                _ => break FastBreak::Slow(orig_pcs[cur_ip]),
                            }
                        }
                        FastOp::GetLoc2ArrayEl { arr, idx } => {
                            let Some(id) = stack[var_base + usize::from(arr)].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let Some(element) =
                                fast_array_index(heap, stack[var_base + usize::from(idx)])
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let Some(value) = fast_array_get_value(heap, id, element) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[sp] = value;
                            sp += 1;
                        }
                        FastOp::GetLoc2ArrayElAddLoc { dst, arr, idx } => {
                            let Some(id) = stack[var_base + usize::from(arr)].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let Some(element) =
                                fast_array_index(heap, stack[var_base + usize::from(idx)])
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let Some(elem_val) = fast_array_get_value(heap, id, element) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let d = var_base + usize::from(dst);
                            let Some(sum) = fast_binary(
                                Opcode::Add,
                                stack[d],
                                elem_val,
                                vm.interrupt_handler.is_some(),
                            ) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            stack[d] = sum;
                        }
                        FastOp::Delete => {
                            let Some(id) = stack[sp - 2].as_object() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let ValueKind::Int32(n) = stack[sp - 1].decode() else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if n < 0 {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            let Some(prop_key) = PropKey::index(n as u32) else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            let obj = heap.object(id);
                            if obj
                                .flags
                                .contains(ObjectFlags::IS_EXOTIC | ObjectFlags::IS_PROTOTYPE)
                                || matches!(obj.data, ObjectData::MappedArguments(_))
                                || !heap.shape(obj.shape).is_dictionary()
                            {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            let shape_id = obj.shape;
                            let shape = heap.shapes.get_mut(shape_id).unwrap();
                            let object = heap.objects.get_mut(id).unwrap();
                            match shape.dict_delete_if_configurable(
                                &heap.ledger,
                                &mut object.props,
                                prop_key,
                            ) {
                                Some(true) | None => {
                                    stack[sp - 2] = Value::bool(true);
                                    sp -= 1;
                                }
                                Some(false) if !function.strict => {
                                    stack[sp - 2] = Value::bool(false);
                                    sp -= 1;
                                }
                                Some(false) => {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                }
                            }
                        }
                        FastOp::ForOfStart => {
                            if vm.interrupt_handler.is_none()
                                && let Some(arr_id) = stack[sp - 1].as_object()
                                && matches!(heap.object(arr_id).data, ObjectData::Array(_))
                            {
                                break FastBreak::ForOfStartArray {
                                    arr_id,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                    next_ip: ip,
                                };
                            }
                            break FastBreak::Slow(orig_pcs[cur_ip]);
                        }
                        FastOp::ForOfNext(skip) => {
                            if vm.interrupt_handler.is_some() {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                            let pos = sp - usize::from(skip) - 3;
                            let iterator = stack[pos];
                            let method = stack[pos + 1];
                            let Some((val, done)) =
                                iterators::dense_array_next(&mut heap.objects, iterator, method)
                            else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            };
                            if done {
                                stack[pos] = Value::UNDEFINED;
                            }
                            stack[sp] = val;
                            stack[sp + 1] = Value::bool(done);
                            sp += 2;
                        }
                        FastOp::IteratorClose => {
                            if stack[sp - 3] == Value::UNDEFINED {
                                sp -= 3;
                            } else {
                                break FastBreak::Slow(orig_pcs[cur_ip]);
                            }
                        }
                        FastOp::ForInNext => {
                            match iterators::dense_for_in_next(&mut heap.objects, stack[sp - 1]) {
                                iterators::DenseForInStep::Done => {
                                    stack[sp] = Value::UNDEFINED;
                                    stack[sp + 1] = Value::bool(true);
                                    sp += 2;
                                }
                                iterators::DenseForInStep::Index(n) => {
                                    if n <= 9
                                        && let Some(ch) =
                                            heap.atoms.ascii_string((b'0' + n as u8) as usize)
                                    {
                                        stack[sp] = Value::string(ch);
                                        stack[sp + 1] = Value::bool(false);
                                        sp += 2;
                                    } else {
                                        break FastBreak::ForInIndex {
                                            value: n as i32,
                                            at: orig_pcs[cur_ip],
                                            next_pc: orig_pcs[cur_ip + 1],
                                            next_ip: ip,
                                        };
                                    }
                                }
                                iterators::DenseForInStep::Slow => {
                                    break FastBreak::Slow(orig_pcs[cur_ip]);
                                }
                            }
                        }
                        FastOp::NewObject => {
                            break FastBreak::NewObject {
                                realm: frame.realm,
                                dest: sp,
                                truncate_to: sp + 1,
                                at: orig_pcs[cur_ip],
                                next_pc: orig_pcs[cur_ip + 1],
                                next_ip: ip,
                            };
                        }
                        FastOp::NewNullProtoObject => {
                            break FastBreak::NewNullProtoObject {
                                at: orig_pcs[cur_ip],
                                next_pc: orig_pcs[cur_ip + 1],
                                next_ip: ip,
                            };
                        }
                        FastOp::NewArrayEmpty => {
                            break FastBreak::NewArrayEmpty {
                                at: orig_pcs[cur_ip],
                                next_pc: orig_pcs[cur_ip + 1],
                                next_ip: ip,
                            };
                        }
                        FastOp::CallConstructor(count) => {
                            if count == 0 && vm.interrupt_handler.is_none() {
                                let start = sp - 2;
                                let callee_val = stack[start];
                                if callee_val == stack[start + 1]
                                    && let Some(callee_id) = callee_val.as_object()
                                    && let ObjectData::NativeFunction(native) =
                                        &heap.object(callee_id).data
                                    && native.function.0 == FN_OBJECT_CTOR
                                    && callee_id == heap.realm(native.realm).object_ctor
                                {
                                    break FastBreak::NewObject {
                                        realm: native.realm,
                                        dest: start,
                                        truncate_to: start + 1,
                                        at: orig_pcs[cur_ip],
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                            }
                            break FastBreak::Slow(orig_pcs[cur_ip]);
                        }
                        FastOp::Call(_)
                        | FastOp::CallMethod(_)
                        | FastOp::GetField2CallMethod0(_) => {
                            let (method, count, call_pc) = match op {
                                FastOp::Call(n) => (false, usize::from(n), orig_pcs[cur_ip]),
                                FastOp::CallMethod(n) => (true, usize::from(n), orig_pcs[cur_ip]),
                                FastOp::GetField2CallMethod0(ic) => {
                                    let idx = usize::from(ic);
                                    let Some(callee) = cache::get_field(
                                        heap,
                                        function,
                                        &caches[idx],
                                        ic_atoms[idx],
                                        stack[sp - 1],
                                        frame.realm,
                                    ) else {
                                        break FastBreak::Slow(orig_pcs[cur_ip]);
                                    };
                                    stack[sp] = callee;
                                    sp += 1;
                                    (true, 0, orig_pcs[cur_ip] + 5)
                                }
                                _ => unreachable!(),
                            };
                            let base = sp - count - 1 - usize::from(method);
                            let callee_val = stack[base + usize::from(method)];
                            let Some(callee_id) = callee_val.as_object() else {
                                break FastBreak::Slow(call_pc);
                            };

                            match &heap.object(callee_id).data {
                                ObjectData::NativeFunction(native)
                                    if vm.interrupt_handler.is_none() =>
                                {
                                    let fn_id = native.function.0;
                                    let handled = match fn_id {
                                        FN_MATH_MIN | FN_MATH_MAX if count == 2 => {
                                            let a0 = stack[base + 1 + usize::from(method)];
                                            let a1 = stack[base + 2 + usize::from(method)];
                                            if let Some(res) =
                                                fast_math_minmax(a0, a1, fn_id == FN_MATH_MAX)
                                            {
                                                stack[base] = res;
                                                sp = base + 1;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        FN_DATE_NOW if count == 0 => {
                                            stack[base] = Value::number(fast_date_now());
                                            sp = base + 1;
                                            true
                                        }
                                        FN_ARRAY_POP if method && count == 0 => {
                                            if let Some(arr_id) = stack[base].as_object() {
                                                let obj = heap.objects.get_mut(arr_id).unwrap();
                                                if obj.flags.contains(ObjectFlags::FAST_ARRAY)
                                                    && !obj
                                                        .flags
                                                        .contains(ObjectFlags::IS_PROTOTYPE)
                                                    && let ObjectData::Array(array) = &mut obj.data
                                                    && array.length_writable
                                                    && array.length > 0
                                                    && array.length as usize == array.elements.len()
                                                    && array.elements[array.length as usize - 1]
                                                        != Value::EMPTY
                                                {
                                                    let popped = array.elements.pop().unwrap();
                                                    array.length -= 1;
                                                    stack[base] = popped;
                                                    sp = base + 1;
                                                    true
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_ARRAY_PUSH if method && count == 1 => {
                                            let arg0 = stack[base + 2];
                                            if let Some(arr_id) = stack[base].as_object() {
                                                let can_push = {
                                                    let obj = heap.object(arr_id);
                                                    obj.flags.contains(
                                                        ObjectFlags::FAST_ARRAY
                                                            | ObjectFlags::EXTENSIBLE,
                                                    ) && !obj
                                                        .flags
                                                        .contains(ObjectFlags::IS_PROTOTYPE)
                                                        && match &obj.data {
                                                            ObjectData::Array(array) => {
                                                                let len = array.length as usize;
                                                                array.length_writable
                                                                    && len == array.elements.len()
                                                                    && len
                                                                        < array.elements.capacity()
                                                                    && len
                                                                        < crate::atom::INLINE_INDEX_LIMIT
                                                                            as usize
                                                                    && heap
                                                                        .has_intact_array_prototype(
                                                                            obj.shape,
                                                                            array.realm,
                                                                        )
                                                            }
                                                            _ => false,
                                                        }
                                                };
                                                if can_push {
                                                    let ObjectData::Array(array) = &mut heap
                                                        .objects
                                                        .get_mut(arr_id)
                                                        .unwrap()
                                                        .data
                                                    else {
                                                        unreachable!()
                                                    };
                                                    array.elements.push_reserved(arg0);
                                                    array.length += 1;
                                                    stack[base] = cache::length_value(array.length);
                                                    sp = base + 1;
                                                    true
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_STRING_CTOR if !method && count == 1 => {
                                            if let ValueKind::Int32(n) = stack[base + 1].decode() {
                                                if (0..=9).contains(&n)
                                                    && let Some(str_id) = heap
                                                        .atoms
                                                        .ascii_string((b'0' + n as u8) as usize)
                                                {
                                                    stack[base] = Value::string(str_id);
                                                    sp = base + 1;
                                                    true
                                                } else {
                                                    break FastBreak::IntToStringDest {
                                                        value: n,
                                                        dest: base,
                                                        truncate_to: base + 1,
                                                        at: call_pc,
                                                        next_pc: orig_pcs[cur_ip + 1],
                                                        next_ip: ip,
                                                    };
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_STRING_SLICE if method && count <= 2 => {
                                            if let ValueKind::String(str_id) = stack[base].decode()
                                            {
                                                let n = heap.string(str_id).len() as usize;
                                                let parse_pos =
                                                    |v: Value, default: usize| -> Option<usize> {
                                                        match v.decode() {
                                                            ValueKind::Undefined => Some(default),
                                                            ValueKind::Int32(k) => Some(if k < 0 {
                                                                (n as i64 + i64::from(k)).max(0)
                                                                    as usize
                                                            } else {
                                                                (k as usize).min(n)
                                                            }),
                                                            _ => None,
                                                        }
                                                    };
                                                let start = if count >= 1 {
                                                    parse_pos(stack[base + 2], 0)
                                                } else {
                                                    Some(0)
                                                };
                                                let end = if count >= 2 {
                                                    parse_pos(stack[base + 3], n)
                                                } else {
                                                    Some(n)
                                                };
                                                if let (Some(start), Some(end)) = (start, end) {
                                                    let end = end.max(start);
                                                    if start == 0 && end == n {
                                                        sp = base + 1;
                                                        true
                                                    } else if start == end {
                                                        if let Some(empty) =
                                                            heap.atoms.ascii_string(128)
                                                        {
                                                            stack[base] = Value::string(empty);
                                                            sp = base + 1;
                                                            true
                                                        } else {
                                                            break FastBreak::StringSlice {
                                                                base,
                                                                start,
                                                                end,
                                                                at: call_pc,
                                                                next_pc: orig_pcs[cur_ip + 1],
                                                                next_ip: ip,
                                                            };
                                                        }
                                                    } else if end - start == 1
                                                        && let Some(unit) = JsString::unit_at(
                                                            &heap.strings,
                                                            str_id,
                                                            start as u32,
                                                        )
                                                        && unit < 128
                                                        && let Some(ch) =
                                                            heap.atoms.ascii_string(unit as usize)
                                                    {
                                                        stack[base] = Value::string(ch);
                                                        sp = base + 1;
                                                        true
                                                    } else {
                                                        break FastBreak::StringSlice {
                                                            base,
                                                            start,
                                                            end,
                                                            at: call_pc,
                                                            next_pc: orig_pcs[cur_ip + 1],
                                                            next_ip: ip,
                                                        };
                                                    }
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_STRING_CONCAT if method => {
                                            if matches!(stack[base].decode(), ValueKind::String(_))
                                            {
                                                if count == 0 {
                                                    sp = base + 1;
                                                    true
                                                } else {
                                                    break FastBreak::StringConcat {
                                                        base,
                                                        argc: count,
                                                        at: call_pc,
                                                        next_pc: orig_pcs[cur_ip + 1],
                                                        next_ip: ip,
                                                    };
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_REGEXP_EXEC if method && count == 1 => {
                                            if let Some(re_id) = stack[base].as_object()
                                                && matches!(
                                                    heap.object(re_id).data,
                                                    ObjectData::RegExp(_)
                                                )
                                                && let ValueKind::String(input_id) =
                                                    stack[base + 2].decode()
                                            {
                                                break FastBreak::RegExpExec {
                                                    base,
                                                    re_id,
                                                    input_id,
                                                    at: call_pc,
                                                    next_pc: orig_pcs[cur_ip + 1],
                                                    next_ip: ip,
                                                };
                                            }
                                            false
                                        }
                                        FN_MAP_HAS | FN_SET_HAS if method && count <= 1 => {
                                            if let Some(id) = stack[base].as_object() {
                                                let entries = match (&heap.object(id).data, fn_id) {
                                                    (ObjectData::Map(d), FN_MAP_HAS) => {
                                                        Some(&d.entries)
                                                    }
                                                    (ObjectData::Set(d), FN_SET_HAS) => {
                                                        Some(&d.entries)
                                                    }
                                                    _ => None,
                                                };
                                                if let Some(entries) = entries {
                                                    let key = if count == 1 {
                                                        stack[base + 2]
                                                    } else {
                                                        Value::UNDEFINED
                                                    };
                                                    let found = entries.len() > 0 && {
                                                        let h = entries.hash(heap, key);
                                                        entries
                                                            .locate_bucket(heap, key, h)
                                                            .is_some()
                                                    };
                                                    stack[base] = Value::bool(found);
                                                    sp = base + 1;
                                                    true
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_MAP_GET if method && count <= 1 => {
                                            if let Some(id) = stack[base].as_object()
                                                && let ObjectData::Map(d) = &heap.object(id).data
                                            {
                                                let key = if count == 1 {
                                                    stack[base + 2]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                let val = if d.entries.len() == 0 {
                                                    Value::UNDEFINED
                                                } else {
                                                    let h = d.entries.hash(heap, key);
                                                    d.entries
                                                        .locate(heap, key, h)
                                                        .map_or(Value::UNDEFINED, |row| {
                                                            d.entries.value(row)
                                                        })
                                                };
                                                stack[base] = val;
                                                sp = base + 1;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        FN_MAP_DELETE | FN_SET_DELETE if method && count <= 1 => {
                                            if let Some(id) = stack[base].as_object() {
                                                let key = if count == 1 {
                                                    stack[base + 2]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                let lookup = match (&heap.object(id).data, fn_id) {
                                                    (ObjectData::Map(d), FN_MAP_DELETE) => {
                                                        Some(if d.entries.len() == 0 {
                                                            None
                                                        } else {
                                                            let h = d.entries.hash(heap, key);
                                                            d.entries.locate_bucket(heap, key, h)
                                                        })
                                                    }
                                                    (ObjectData::Set(d), FN_SET_DELETE) => {
                                                        Some(if d.entries.len() == 0 {
                                                            None
                                                        } else {
                                                            let h = d.entries.hash(heap, key);
                                                            d.entries.locate_bucket(heap, key, h)
                                                        })
                                                    }
                                                    _ => None,
                                                };
                                                if let Some(bucket_opt) = lookup {
                                                    let removed = if let Some(bucket) = bucket_opt {
                                                        match &mut heap
                                                            .objects
                                                            .get_mut(id)
                                                            .unwrap()
                                                            .data
                                                        {
                                                            ObjectData::Map(d) => {
                                                                d.entries.remove_bucket(bucket)
                                                            }
                                                            ObjectData::Set(d) => {
                                                                d.entries.remove_bucket(bucket)
                                                            }
                                                            _ => unreachable!(),
                                                        }
                                                        true
                                                    } else {
                                                        false
                                                    };
                                                    stack[base] = Value::bool(removed);
                                                    sp = base + 1;
                                                    true
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_MAP_SET | FN_SET_ADD if method && count <= 2 => {
                                            let is_set = fn_id == FN_SET_ADD;
                                            if let Some(id) = stack[base].as_object() {
                                                let key = if count >= 1 {
                                                    stack[base + 2]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                let val = if is_set || count < 2 {
                                                    Value::UNDEFINED
                                                } else {
                                                    stack[base + 3]
                                                };
                                                let probe = match (&heap.object(id).data, is_set) {
                                                    (ObjectData::Map(d), false) => {
                                                        let h = d.entries.hash(heap, key);
                                                        Some((
                                                            h,
                                                            d.entries.locate(heap, key, h),
                                                            d.entries.has_spare_capacity(),
                                                        ))
                                                    }
                                                    (ObjectData::Set(d), true) => {
                                                        let h = d.entries.hash(heap, key);
                                                        Some((
                                                            h,
                                                            d.entries.locate(heap, key, h),
                                                            d.entries.has_spare_capacity(),
                                                        ))
                                                    }
                                                    _ => None,
                                                };
                                                if let Some((hash, existing, spare)) = probe {
                                                    if let Some(row) = existing {
                                                        if !is_set {
                                                            let ObjectData::Map(d) = &mut heap
                                                                .objects
                                                                .get_mut(id)
                                                                .unwrap()
                                                                .data
                                                            else {
                                                                unreachable!()
                                                            };
                                                            d.entries.update(row, val);
                                                        }
                                                        sp = base + 1;
                                                        true
                                                    } else if spare {
                                                        let ok = match &mut heap
                                                            .objects
                                                            .get_mut(id)
                                                            .unwrap()
                                                            .data
                                                        {
                                                            ObjectData::Map(d) => d
                                                                .entries
                                                                .insert_reserved(hash, key, val)
                                                                .is_ok(),
                                                            ObjectData::Set(d) => d
                                                                .entries
                                                                .insert_reserved(hash, key, val)
                                                                .is_ok(),
                                                            _ => unreachable!(),
                                                        };
                                                        if ok {
                                                            sp = base + 1;
                                                            true
                                                        } else {
                                                            false
                                                        }
                                                    } else {
                                                        break FastBreak::MapSet {
                                                            base,
                                                            id,
                                                            key,
                                                            value: val,
                                                            at: call_pc,
                                                            next_pc: orig_pcs[cur_ip + 1],
                                                            next_ip: ip,
                                                        };
                                                    }
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_WEAK_MAP_GET | FN_WEAK_MAP_HAS
                                            if method && count <= 1 =>
                                        {
                                            if let Some(id) = stack[base].as_object()
                                                && let ObjectData::WeakMap(d) =
                                                    &heap.object(id).data
                                            {
                                                let key = if count == 1 {
                                                    stack[base + 2]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                let found = heap
                                                    .weak_key(key)
                                                    .and_then(|k| d.entries.get(k))
                                                    .map(|entry| entry.1);
                                                stack[base] = if fn_id == FN_WEAK_MAP_HAS {
                                                    Value::bool(found.is_some())
                                                } else {
                                                    found.unwrap_or(Value::UNDEFINED)
                                                };
                                                sp = base + 1;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        FN_WEAK_MAP_DELETE if method && count <= 1 => {
                                            if let Some(id) = stack[base].as_object()
                                                && matches!(
                                                    heap.object(id).data,
                                                    ObjectData::WeakMap(_)
                                                )
                                            {
                                                let key = if count == 1 {
                                                    stack[base + 2]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                let wk = heap.weak_key(key);
                                                let removed = if let Some(wk) = wk {
                                                    let ObjectData::WeakMap(d) =
                                                        &mut heap.objects.get_mut(id).unwrap().data
                                                    else {
                                                        unreachable!()
                                                    };
                                                    d.entries.remove(wk)
                                                } else {
                                                    false
                                                };
                                                stack[base] = Value::bool(removed);
                                                sp = base + 1;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        FN_WEAK_MAP_SET if method && count <= 2 => {
                                            if let Some(id) = stack[base].as_object()
                                                && matches!(
                                                    heap.object(id).data,
                                                    ObjectData::WeakMap(_)
                                                )
                                            {
                                                let key = if count >= 1 {
                                                    stack[base + 2]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                let val = if count >= 2 {
                                                    stack[base + 3]
                                                } else {
                                                    Value::UNDEFINED
                                                };
                                                if let Some(wk) = heap.weak_key(key) {
                                                    let ObjectData::WeakMap(d) =
                                                        &mut heap.objects.get_mut(id).unwrap().data
                                                    else {
                                                        unreachable!()
                                                    };
                                                    if d.entries
                                                        .try_insert_reserved((wk, val))
                                                        .is_ok()
                                                    {
                                                        sp = base + 1;
                                                        true
                                                    } else {
                                                        break FastBreak::WeakMapSet {
                                                            base,
                                                            id,
                                                            key,
                                                            value: val,
                                                            at: call_pc,
                                                            next_pc: orig_pcs[cur_ip + 1],
                                                            next_ip: ip,
                                                        };
                                                    }
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_NUM_TO_STRING if method && count <= 1 => {
                                            let recv = stack[base];
                                            let radix_opt = if count == 0
                                                || stack[base + 2] == Value::UNDEFINED
                                            {
                                                Some(10u32)
                                            } else if let ValueKind::Int32(r) =
                                                stack[base + 2].decode()
                                                && (2..=36).contains(&r)
                                            {
                                                Some(r as u32)
                                            } else {
                                                None
                                            };
                                            if let Some(radix) = radix_opt {
                                                match recv.decode() {
                                                    ValueKind::Int32(n) if radix == 10 => {
                                                        if (0..=9).contains(&n)
                                                            && let Some(str_id) =
                                                                heap.atoms.ascii_string(
                                                                    (b'0' + n as u8) as usize,
                                                                )
                                                        {
                                                            stack[base] = Value::string(str_id);
                                                            sp = base + 1;
                                                            true
                                                        } else {
                                                            break FastBreak::IntToStringDest {
                                                                value: n,
                                                                dest: base,
                                                                truncate_to: base + 1,
                                                                at: call_pc,
                                                                next_pc: orig_pcs[cur_ip + 1],
                                                                next_ip: ip,
                                                            };
                                                        }
                                                    }
                                                    ValueKind::Int32(_) | ValueKind::Float64(_) => {
                                                        let n = recv.as_number().unwrap();
                                                        let exp = if radix == 10 {
                                                            sandbolt_dtoa::DtoaExp::Auto
                                                        } else {
                                                            sandbolt_dtoa::DtoaExp::Disabled
                                                        };
                                                        break FastBreak::FormatNumber {
                                                            base: base as u32,
                                                            value: n,
                                                            radix: radix as u8,
                                                            digits: 0,
                                                            options: sandbolt_dtoa::DtoaOptions {
                                                                exp,
                                                                ..sandbolt_dtoa::DtoaOptions::default()
                                                            },
                                                            at: call_pc,
                                                            next_pc: orig_pcs[cur_ip + 1],
                                                            next_ip: ip as u32,
                                                        };
                                                    }
                                                    _ => false,
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        FN_NUM_TO_FIXED
                                        | FN_NUM_TO_PRECISION
                                        | FN_NUM_TO_EXPONENTIAL
                                            if method && count <= 1 =>
                                        {
                                            if let Some(n) = stack[base].as_number() {
                                                let undefined = count == 0
                                                    || stack[base + 2] == Value::UNDEFINED;
                                                let spec = if fn_id == FN_NUM_TO_PRECISION
                                                    && undefined
                                                {
                                                    Some((
                                                        0usize,
                                                        sandbolt_dtoa::DtoaOptions::default(),
                                                    ))
                                                } else if undefined {
                                                    if fn_id == FN_NUM_TO_EXPONENTIAL {
                                                        let exp = if n.is_finite() {
                                                            sandbolt_dtoa::DtoaExp::Enabled
                                                        } else {
                                                            sandbolt_dtoa::DtoaExp::Auto
                                                        };
                                                        Some((
                                                            0,
                                                            sandbolt_dtoa::DtoaOptions {
                                                                format: sandbolt_dtoa::DtoaFormat::Free,
                                                                exp,
                                                                ..sandbolt_dtoa::DtoaOptions::default()
                                                            },
                                                        ))
                                                    } else if n.abs() >= 1e21 {
                                                        Some((
                                                            0,
                                                            sandbolt_dtoa::DtoaOptions::default(),
                                                        ))
                                                    } else {
                                                        Some((
                                                            0,
                                                            sandbolt_dtoa::DtoaOptions {
                                                                format: sandbolt_dtoa::DtoaFormat::Frac,
                                                                exp: sandbolt_dtoa::DtoaExp::Auto,
                                                                ..sandbolt_dtoa::DtoaOptions::default()
                                                            },
                                                        ))
                                                    }
                                                } else if let ValueKind::Int32(d) =
                                                    stack[base + 2].decode()
                                                {
                                                    let min =
                                                        i32::from(fn_id == FN_NUM_TO_PRECISION);
                                                    if (min..=100).contains(&d) {
                                                        let d = d as usize;
                                                        if fn_id != FN_NUM_TO_FIXED
                                                            && !n.is_finite()
                                                        {
                                                            Some((
                                                                0,
                                                                sandbolt_dtoa::DtoaOptions::default(
                                                                ),
                                                            ))
                                                        } else if fn_id == FN_NUM_TO_EXPONENTIAL {
                                                            Some((
                                                                d + 1,
                                                                sandbolt_dtoa::DtoaOptions {
                                                                    format:
                                                                        sandbolt_dtoa::DtoaFormat::Fixed,
                                                                    exp: sandbolt_dtoa::DtoaExp::Enabled,
                                                                    ..sandbolt_dtoa::DtoaOptions::default(
                                                                    )
                                                                },
                                                            ))
                                                        } else if fn_id == FN_NUM_TO_PRECISION {
                                                            Some((
                                                                d,
                                                                sandbolt_dtoa::DtoaOptions {
                                                                    format:
                                                                        sandbolt_dtoa::DtoaFormat::Fixed,
                                                                    exp: sandbolt_dtoa::DtoaExp::Auto,
                                                                    ..sandbolt_dtoa::DtoaOptions::default(
                                                                    )
                                                                },
                                                            ))
                                                        } else if n.abs() >= 1e21 {
                                                            Some((
                                                                0,
                                                                sandbolt_dtoa::DtoaOptions::default(
                                                                ),
                                                            ))
                                                        } else {
                                                            Some((
                                                                d,
                                                                sandbolt_dtoa::DtoaOptions {
                                                                    format:
                                                                        sandbolt_dtoa::DtoaFormat::Frac,
                                                                    exp: sandbolt_dtoa::DtoaExp::Auto,
                                                                    ..sandbolt_dtoa::DtoaOptions::default(
                                                                    )
                                                                },
                                                            ))
                                                        }
                                                    } else {
                                                        None
                                                    }
                                                } else {
                                                    None
                                                };
                                                if let Some((digits, options)) = spec {
                                                    break FastBreak::FormatNumber {
                                                        base: base as u32,
                                                        value: n,
                                                        radix: 10,
                                                        digits: digits as u8,
                                                        options,
                                                        at: call_pc,
                                                        next_pc: orig_pcs[cur_ip + 1],
                                                        next_ip: ip as u32,
                                                    };
                                                }
                                            }
                                            false
                                        }
                                        _ => false,
                                    };
                                    if handled {
                                        continue;
                                    }
                                    break FastBreak::TryPlainCall {
                                        count,
                                        method,
                                        call_pc,
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                                ObjectData::BytecodeFunction(data)
                                    if !data.is_class_constructor =>
                                {
                                    let callee_code = data.code;
                                    let callee_realm = data.realm;
                                    let Some(callee_fn) =
                                        heap.bytecode.get(callee_code).unwrap().compiled.as_ref()
                                    else {
                                        break FastBreak::Slow(call_pc);
                                    };
                                    if callee_fn.kind != FunctionKind::Normal
                                        || callee_fn.needs_arguments
                                        || !callee_fn.hoisted.is_empty()
                                        || vm.frames.len() == vm.frames.capacity()
                                    {
                                        break FastBreak::Slow(call_pc);
                                    }
                                    let arg_slots = count.max(usize::from(callee_fn.arg_count));
                                    let locals = usize::from(callee_fn.var_count);
                                    let captures =
                                        if callee_fn.needs_captures || callee_fn.eval.is_some() {
                                            arg_slots + locals
                                        } else {
                                            0
                                        };
                                    let capture_base = base + 2 + arg_slots + locals;
                                    let floor = capture_base + captures;
                                    let callee_limit = floor + usize::from(callee_fn.stack_size);
                                    let needed = callee_limit.saturating_sub(sp);
                                    let usage = sp
                                        .saturating_add(needed)
                                        .saturating_mul(size_of::<Value>())
                                        .saturating_add(
                                            vm.frames
                                                .len()
                                                .saturating_add(1)
                                                .saturating_mul(size_of::<CallFrame>()),
                                        )
                                        .saturating_add(reentry_bytes);
                                    if stack_cap < callee_limit + 1
                                        || base + 1 >= u32::MAX as usize
                                        || usage > stack_bytes_limit
                                        || vm.reentry_depth > crate::vm::MAX_REENTRY_DEPTH
                                    {
                                        break FastBreak::Slow(call_pc);
                                    }
                                    if vm.interrupt_handler.as_mut().is_some_and(|h| h()) {
                                        break FastBreak::Interrupted(call_pc);
                                    }
                                    sp = floor;
                                    vm.frames[index].last_pc = call_pc;
                                    vm.frames[index].pc = orig_pcs[cur_ip + 1];
                                    vm.frames[index].fast_ip = (cur_ip + 1) as u32;
                                    let args_ready = stack.len() >= callee_limit;
                                    if args_ready {
                                        if method {
                                            stack.swap(base, base + 1);
                                        } else {
                                            match count {
                                                0 => stack[base + 1] = Value::UNDEFINED,
                                                1 => {
                                                    stack[base + 2] = stack[base + 1];
                                                    stack[base + 1] = Value::UNDEFINED;
                                                }
                                                2 => {
                                                    stack[base + 3] = stack[base + 2];
                                                    stack[base + 2] = stack[base + 1];
                                                    stack[base + 1] = Value::UNDEFINED;
                                                }
                                                _ => {
                                                    stack.copy_within(
                                                        base + 1..base + 1 + count,
                                                        base + 2,
                                                    );
                                                    stack[base + 1] = Value::UNDEFINED;
                                                }
                                            }
                                        }
                                        stack[base + 2 + count..floor].fill(Value::UNDEFINED);
                                    }
                                    vm.frames.push_reserved(CallFrame {
                                        callee: callee_id,
                                        realm: callee_realm,
                                        caller_realm: frame.realm,
                                        code: Some(callee_code),
                                        new_target: None,
                                        pc: 0,
                                        base: base as u32,
                                        arg_count: count as u32,
                                        arg_slots: arg_slots as u32,
                                        capture_base: capture_base as u32,
                                        floor: floor as u32,
                                        last_pc: 0,
                                        fast_ip: 0,
                                        return_call: false,
                                        require_object: false,
                                        coroutine: None,
                                    });
                                    break FastBreak::EnterJs {
                                        callee_code,
                                        callee_realm,
                                        callee_limit: callee_limit as u32,
                                        base: base as u32,
                                        count: count as u16,
                                        method,
                                        args_ready,
                                    };
                                }
                                _ => {
                                    break FastBreak::TryPlainCall {
                                        count,
                                        method,
                                        call_pc,
                                        next_pc: orig_pcs[cur_ip + 1],
                                        next_ip: ip,
                                    };
                                }
                            }
                        }
                        FastOp::Return | FastOp::ReturnUndef | FastOp::ReturnAsync => {
                            let value = if matches!(op, FastOp::ReturnUndef) {
                                Value::UNDEFINED
                            } else {
                                stack[sp - 1]
                            };
                            if index == entry_index {
                                break FastBreak::ReturnTop {
                                    value,
                                    at: orig_pcs[cur_ip],
                                    next_pc: orig_pcs[cur_ip + 1],
                                };
                            }
                            bindings::close_frame_slots(&mut heap.var_refs, stack, frame);
                            stack[frame.base as usize] = value;
                            sp = frame.base as usize + 1;
                            vm.frames.pop();
                            let caller = &vm.frames[index - 1];
                            break FastBreak::ReturnToCaller {
                                caller_code: caller.code.unwrap(),
                                caller_realm: caller.realm,
                                caller_ip: caller.fast_ip,
                            };
                        }
                        FastOp::Slow => {
                            break FastBreak::Slow(orig_pcs[cur_ip]);
                        }
                    }
                }
            };

            match brk {
                FastBreak::EnterJs {
                    callee_code,
                    callee_realm,
                    callee_limit,
                    base,
                    count,
                    method,
                    args_ready,
                } => {
                    if !args_ready {
                        let vm = &mut ctx.runtime.vm;
                        let callee_limit = callee_limit as usize;
                        while vm.stack.len() < callee_limit {
                            vm.stack.push_reserved(Value::UNDEFINED);
                        }
                        let base = base as usize;
                        let count = usize::from(count);
                        let floor = vm.frames[index + 1].floor as usize;
                        let stack = &mut vm.stack[..];
                        if method {
                            stack.swap(base, base + 1);
                        } else {
                            match count {
                                0 => stack[base + 1] = Value::UNDEFINED,
                                1 => {
                                    stack[base + 2] = stack[base + 1];
                                    stack[base + 1] = Value::UNDEFINED;
                                }
                                2 => {
                                    stack[base + 3] = stack[base + 2];
                                    stack[base + 2] = stack[base + 1];
                                    stack[base + 1] = Value::UNDEFINED;
                                }
                                _ => {
                                    stack.copy_within(base + 1..base + 1 + count, base + 2);
                                    stack[base + 1] = Value::UNDEFINED;
                                }
                            }
                        }
                        stack[base + 2 + count..floor].fill(Value::UNDEFINED);
                    }
                    ctx.realm = callee_realm;
                    code = callee_code;
                    index += 1;
                    cached_ip = Some(0);
                    continue 'frames;
                }
                FastBreak::ReturnToCaller {
                    caller_code,
                    caller_realm,
                    caller_ip,
                } => {
                    index -= 1;
                    code = caller_code;
                    ctx.realm = caller_realm;
                    cached_ip = (caller_ip != u32::MAX).then_some(caller_ip as usize);
                    continue 'frames;
                }
                FastBreak::ReturnTop { value, at, next_pc } => {
                    ctx.runtime.vm.stack.truncate(sp);
                    let f = &mut ctx.runtime.vm.frames[index];
                    f.last_pc = at;
                    f.pc = next_pc;
                    return Ok(Some(value));
                }
                FastBreak::Interrupted(at) => {
                    ctx.runtime.vm.stack.truncate(sp);
                    ctx.runtime.vm.frames[index].last_pc = at;
                    ctx.take_exception();
                    return Err(Abrupt::Interrupted);
                }
                other => match handle_fast_break(ctx, frame, index, code, &mut sp, other)? {
                    BreakOutcome::Resume { next_pc, next_ip } => {
                        pc = next_pc;
                        cached_ip = next_ip;
                        continue 'resume;
                    }
                    BreakOutcome::EnterFrame(next_code) => {
                        code = next_code;
                        index += 1;
                        continue 'frames;
                    }
                    BreakOutcome::Return(result) => return Ok(result),
                },
            }
        }
    }
}

enum BreakOutcome {
    Resume {
        next_pc: u32,
        next_ip: Option<usize>,
    },
    EnterFrame(BcId),
    Return(Option<Value>),
}

#[inline(always)]
fn handle_fast_break(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    index: usize,
    code: BcId,
    sp: &mut usize,
    brk: FastBreak,
) -> JsResult<BreakOutcome> {
    match brk {
        FastBreak::TryPlainCall {
            count,
            method,
            call_pc,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            if let Some(plain) = call::prepare_plain(
                &ctx.runtime.heap,
                &ctx.runtime.vm,
                frame.realm,
                count,
                method,
            ) {
                ctx.runtime.vm.frames[index].last_pc = call_pc;
                ctx.runtime.vm.frames[index].pc = next_pc;
                if let Some(next_code) = plain.enter(ctx)? {
                    *sp = ctx.runtime.vm.stack.len();
                    return Ok(BreakOutcome::EnterFrame(next_code));
                }
                *sp = ctx.runtime.vm.stack.len();
                return Ok(BreakOutcome::Resume {
                    next_pc,
                    next_ip: Some(next_ip),
                });
            }
            let result = run_raw_step(ctx, index, code, call_pc)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(BreakOutcome::Return(result));
            }
            *sp = ctx.runtime.vm.stack.len();
            Ok(BreakOutcome::Resume {
                next_pc: ctx.runtime.vm.frames[index].pc,
                next_ip: None,
            })
        }
        FastBreak::AppendArray {
            id,
            value,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            append_array_slot(ctx, id, value)?;
            ctx.runtime.vm.stack.truncate(truncate_to);
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::DictPut {
            id,
            shape_id,
            key,
            value,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            ctx.allocate(|rt| {
                let heap = &mut rt.heap;
                let shape = heap.shapes.get_mut(shape_id).unwrap();
                let object = heap.objects.get_mut(id).unwrap();
                shape.dict_insert(
                    &heap.ledger,
                    &mut object.props,
                    crate::shape::ShapeProp {
                        key,
                        flags: PropFlags::DEFAULT_DATA,
                    },
                    value,
                )
            })?;
            ctx.runtime.vm.stack.truncate(truncate_to);
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::PutFieldTransitionGrow {
            id,
            to_shape,
            value,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let mut shape_alive = true;
            ctx.allocate(|rt| {
                let heap = &mut rt.heap;
                if heap.shapes.get_weak(to_shape).is_none() {
                    shape_alive = false;
                    return Ok(());
                }
                let obj = heap.objects.get_mut(id).unwrap();
                obj.props.push(&heap.ledger, value)?;
                obj.shape = to_shape;
                Ok(())
            })?;
            if shape_alive {
                ctx.runtime.vm.stack.truncate(truncate_to);
                *sp = truncate_to;
                return Ok(BreakOutcome::Resume {
                    next_pc,
                    next_ip: Some(next_ip),
                });
            }
            let result = run_raw_step(ctx, index, code, at)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(BreakOutcome::Return(result));
            }
            *sp = ctx.runtime.vm.stack.len();
            Ok(BreakOutcome::Resume {
                next_pc: ctx.runtime.vm.frames[index].pc,
                next_ip: None,
            })
        }
        FastBreak::ConcatStrings {
            a,
            b,
            dest,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let joined = ctx.concat_string_ids(a, b)?;
            ctx.runtime.vm.stack[dest] = Value::string(joined);
            ctx.runtime.vm.stack.truncate(truncate_to);
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::NewObject {
            realm,
            dest,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let proto = ctx.heap().realm(realm).object_proto;
            let id = ctx.allocate(|rt| {
                rt.heap
                    .insert_object(Some(proto), ObjectFlags::EXTENSIBLE, ObjectData::Ordinary)
            })?;
            if dest == *sp {
                ctx.runtime.vm.stack.push_reserved(Value::object(id));
            } else {
                ctx.runtime.vm.stack[dest] = Value::object(id);
                ctx.runtime.vm.stack.truncate(truncate_to);
            }
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::NewNullProtoObject {
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let shape = crate::realm::ensure_null_proto_shape(ctx)?;
            let id = ctx.allocate(|rt| {
                rt.heap.objects.insert(
                    &rt.heap.ledger,
                    JsObject {
                        shape,
                        props: crate::object::PropSlots::new(),
                        flags: ObjectFlags::EXTENSIBLE,
                        data: ObjectData::Ordinary,
                    },
                )
            })?;
            ctx.runtime.vm.stack.push_reserved(Value::object(id));
            *sp += 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::NewArrayEmpty {
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let realm = frame.realm;
            let id = ctx.allocate(|rt| {
                let heap = &mut rt.heap;
                let proto = heap.realm(realm).array_proto;
                let array = TrackedBox::new(
                    &heap.ledger,
                    crate::object::ArrayData {
                        elements: TrackedVec::new(),
                        length: 0,
                        length_writable: true,
                        realm,
                    },
                )?;
                heap.insert_object(
                    Some(proto),
                    ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC | ObjectFlags::FAST_ARRAY,
                    ObjectData::Array(array),
                )
            })?;
            ctx.runtime.vm.stack.push_reserved(Value::object(id));
            *sp += 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::ForOfStartArray {
            arr_id,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            if let Some((iter, next_fn)) =
                iterators::fast_for_of_start_array(ctx, arr_id, frame.realm)?
            {
                let top = *sp - 1;
                ctx.runtime.vm.stack[top] = iter;
                ctx.runtime.vm.stack.push_reserved(next_fn);
                ctx.runtime.vm.stack.push_reserved(Value::catch_offset(0));
                *sp += 2;
                return Ok(BreakOutcome::Resume {
                    next_pc,
                    next_ip: Some(next_ip),
                });
            }
            let result = run_raw_step(ctx, index, code, at)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(BreakOutcome::Return(result));
            }
            *sp = ctx.runtime.vm.stack.len();
            Ok(BreakOutcome::Resume {
                next_pc: ctx.runtime.vm.frames[index].pc,
                next_ip: None,
            })
        }
        FastBreak::CloneRegExp {
            constant,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let result = if let Some(shape) = ctx.realm().regexp_shape {
                let template_id = constant.as_object().unwrap();
                let (source, program) = {
                    let ObjectData::RegExp(data) = &ctx.heap().object(template_id).data else {
                        unreachable!()
                    };
                    (data.source, std::rc::Rc::clone(&data.program))
                };
                let obj = ctx.allocate(|rt| {
                    let heap = &mut rt.heap;
                    heap.objects.insert(
                        &heap.ledger,
                        JsObject {
                            shape,
                            props: crate::object::PropSlots::one(Value::int32(0)),
                            flags: ObjectFlags::EXTENSIBLE,
                            data: ObjectData::RegExp(crate::object::RegExpData {
                                source,
                                program: std::rc::Rc::clone(&program),
                            }),
                        },
                    )
                })?;
                Value::object(obj)
            } else {
                ctx.with_scope(|ctx| objects::clone_regexp(ctx, constant))?
            };
            ctx.runtime.vm.stack.push_reserved(result);
            *sp += 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::IntToStringDest {
            value,
            dest,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let id = fast_int_to_string_id(ctx, value)?;
            ctx.runtime.vm.stack[dest] = Value::string(id);
            ctx.runtime.vm.stack.truncate(truncate_to);
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::FloatToStringDest {
            value,
            dest,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let result =
                convert::format_f64_fast(ctx, value, 10, 0, sandbolt_dtoa::DtoaOptions::default())?;
            ctx.runtime.vm.stack[dest] = result;
            ctx.runtime.vm.stack.truncate(truncate_to);
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::FormatNumber {
            base,
            value,
            radix,
            digits,
            options,
            at,
            next_pc,
            next_ip,
        } => {
            let base = base as usize;
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let result = convert::format_f64_fast(
                ctx,
                value,
                u32::from(radix),
                usize::from(digits),
                options,
            )?;
            ctx.runtime.vm.stack[base] = result;
            ctx.runtime.vm.stack.truncate(base + 1);
            *sp = base + 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip as usize),
            })
        }
        FastBreak::BigIntBinary {
            op,
            lhs,
            rhs,
            dest,
            truncate_to,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let result = numeric::bigint_binary(ctx, op, lhs, rhs)?;
            if dest == *sp {
                ctx.runtime.vm.stack.push_reserved(result);
            } else {
                ctx.runtime.vm.stack[dest] = result;
                ctx.runtime.vm.stack.truncate(truncate_to);
            }
            *sp = truncate_to;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::BigIntMulAddLoc {
            dst_slot,
            a,
            b,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let dst_val = ctx.runtime.vm.stack[dst_slot];
            let result = numeric::bigint_mul_add(ctx, dst_val, a, b)?;
            ctx.runtime.vm.stack[dst_slot] = result;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::MapSet {
            base,
            id,
            key,
            value,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            coll_weak::fast_map_set(ctx, id, key, value)?;
            ctx.runtime.vm.stack.truncate(base + 1);
            *sp = base + 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::WeakMapSet {
            base,
            id,
            key,
            value,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            crate::ops::weak::fast_weak_map_set(ctx, id, key, value)?;
            ctx.runtime.vm.stack.truncate(base + 1);
            *sp = base + 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::RegExpExec {
            base,
            re_id,
            input_id,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            if let Some(matched) = string_re::fast_regexp_exec(ctx, re_id, input_id)? {
                ctx.runtime.vm.stack[base] = matched;
                ctx.runtime.vm.stack.truncate(base + 1);
                *sp = base + 1;
                return Ok(BreakOutcome::Resume {
                    next_pc,
                    next_ip: Some(next_ip),
                });
            }
            let result = run_raw_step(ctx, index, code, at)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(BreakOutcome::Return(result));
            }
            *sp = ctx.runtime.vm.stack.len();
            Ok(BreakOutcome::Resume {
                next_pc: ctx.runtime.vm.frames[index].pc,
                next_ip: None,
            })
        }
        FastBreak::ForInIndex {
            value,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let id = fast_int_to_string_id(ctx, value)?;
            ctx.runtime.vm.stack.push_reserved(Value::string(id));
            ctx.runtime.vm.stack.push_reserved(Value::bool(false));
            *sp += 2;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::StringSlice {
            base,
            start,
            end,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            let sliced = ctx.with_scope(|ctx| {
                let h = ctx.string_slice(Handle::at(base), start, end)?;
                Ok(ctx.get(h))
            })?;
            ctx.runtime.vm.stack[base] = sliced;
            ctx.runtime.vm.stack.truncate(base + 1);
            *sp = base + 1;
            Ok(BreakOutcome::Resume {
                next_pc,
                next_ip: Some(next_ip),
            })
        }
        FastBreak::StringConcat {
            base,
            argc,
            at,
            next_pc,
            next_ip,
        } => {
            ctx.runtime.vm.stack.truncate(*sp);
            ctx.runtime.vm.frames[index].last_pc = at;
            ctx.runtime.vm.frames[index].pc = next_pc;
            if let Some(joined) = fast_string_concat(ctx, base, argc)? {
                ctx.runtime.vm.stack[base] = joined;
                ctx.runtime.vm.stack.truncate(base + 1);
                *sp = base + 1;
                return Ok(BreakOutcome::Resume {
                    next_pc,
                    next_ip: Some(next_ip),
                });
            }
            let result = run_raw_step(ctx, index, code, at)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(BreakOutcome::Return(result));
            }
            *sp = ctx.runtime.vm.stack.len();
            Ok(BreakOutcome::Resume {
                next_pc: ctx.runtime.vm.frames[index].pc,
                next_ip: None,
            })
        }
        FastBreak::Slow(at) => {
            ctx.runtime.vm.stack.truncate(*sp);
            let result = run_raw_step(ctx, index, code, at)?;
            if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
                return Ok(BreakOutcome::Return(result));
            }
            *sp = ctx.runtime.vm.stack.len();
            Ok(BreakOutcome::Resume {
                next_pc: ctx.runtime.vm.frames[index].pc,
                next_ip: None,
            })
        }
        FastBreak::EnterJs { .. }
        | FastBreak::ReturnToCaller { .. }
        | FastBreak::ReturnTop { .. }
        | FastBreak::Interrupted(_) => unreachable!(),
    }
}

/// The value of closure or global binding `binding`.
#[inline]
fn var_ref_value(heap: &crate::heap::Heap, stack: &[Value], binding: VarRefId) -> Value {
    match heap.var_refs.get(binding).unwrap().state {
        VarRefState::OpenStack(i) => stack[i as usize],
        VarRefState::OpenCoro { coro, local } => {
            heap.coroutines.get(coro).unwrap().stack[local as usize]
        }
        VarRefState::Closed(value) => value,
    }
}

/// Stores `value` in closure or global binding `binding`. It takes the
/// heap's arenas apart, so the fast loop can call it while it holds the
/// function's bytecode.
#[inline]
fn set_var_ref_value(
    var_refs: &mut crate::arena::Arena<crate::vm::VarRef>,
    coroutines: &mut crate::arena::Arena<crate::vm::CoroutineState>,
    stack: &mut [Value],
    binding: VarRefId,
    value: Value,
) {
    let entry = var_refs.get_mut(binding).unwrap();
    match entry.state {
        VarRefState::OpenStack(i) => stack[i as usize] = value,
        VarRefState::OpenCoro { coro, local } => {
            coroutines.get_mut(coro).unwrap().stack[local as usize] = value;
        }
        VarRefState::Closed(_) => entry.state = VarRefState::Closed(value),
    }
}

#[inline(never)]
fn fast_string_concat(
    ctx: &mut ExecCtx<'_, '_>,
    base: usize,
    argc: usize,
) -> JsResult<Option<Value>> {
    const MAX_STRING_LEN: usize = (1 << 30) - 1;
    const STACK_BUF: usize = 512;
    let stack = &ctx.runtime.vm.stack;
    let ValueKind::String(s_id) = stack[base].decode() else {
        return Ok(None);
    };
    if argc == 1 {
        let ValueKind::String(b_id) = stack[base + 2].decode() else {
            return Ok(None);
        };
        let joined = ctx.concat_string_ids(s_id, b_id)?;
        return Ok(Some(Value::string(joined)));
    }
    // Single-pass stack buffer for Latin-1 concatenations up to 512 bytes.
    {
        let heap = &ctx.runtime.heap;
        let mut buf = [0u8; STACK_BUF];
        let mut pos = 0usize;
        let mut all_ascii = true;
        let mut single_pass_ok = true;
        if let Some(bytes) = heap.string(s_id).as_latin1()
            && bytes.len() <= STACK_BUF
        {
            buf[..bytes.len()].copy_from_slice(bytes);
            pos = bytes.len();
            all_ascii &= heap.string(s_id).is_ascii();
        } else {
            single_pass_ok = false;
        }
        if single_pass_ok {
            for i in 0..argc {
                let ValueKind::String(id) = stack[base + 2 + i].decode() else {
                    return Ok(None);
                };
                let s = heap.string(id);
                if let Some(bytes) = s.as_latin1()
                    && pos + bytes.len() <= STACK_BUF
                {
                    if bytes.len() == 1 {
                        buf[pos] = bytes[0];
                        pos += 1;
                    } else {
                        buf[pos..pos + bytes.len()].copy_from_slice(bytes);
                        pos += bytes.len();
                    }
                    all_ascii &= s.is_ascii();
                } else {
                    single_pass_ok = false;
                    break;
                }
            }
        }
        if single_pass_ok {
            if pos == 0 {
                let empty = ctx.with_scope(|ctx| ctx.new_latin1(&[]).map(|h| ctx.get(h)))?;
                return Ok(Some(empty));
            }
            let is_ascii = all_ascii || buf[..pos].is_ascii();
            if pos <= INLINE_LATIN1_CAPACITY {
                let mut data = [0u8; INLINE_LATIN1_CAPACITY];
                data[..pos].copy_from_slice(&buf[..pos]);
                let id = ctx.allocate(|rt| {
                    rt.heap.strings.insert(
                        &rt.heap.ledger,
                        JsString::InlineLatin1 {
                            len: pos as u8,
                            is_ascii,
                            data,
                        },
                    )
                })?;
                return Ok(Some(Value::string(id)));
            }
            let slice = &buf[..pos];
            let id = ctx.allocate(|rt| {
                let data = TrackedVec::from_slice(&rt.heap.ledger, slice)?;
                rt.heap
                    .strings
                    .insert(&rt.heap.ledger, JsString::Latin1 { data, is_ascii })
            })?;
            return Ok(Some(Value::string(id)));
        }
    }
    let stack = &ctx.runtime.vm.stack;
    let heap = &ctx.runtime.heap;
    let first = heap.string(s_id);
    if first.is_wide() {
        return Ok(None);
    }
    let mut total_len = first.len() as usize;
    let mut all_ascii = first.is_ascii();
    for i in 0..argc {
        let ValueKind::String(id) = stack[base + 2 + i].decode() else {
            return Ok(None);
        };
        let s = heap.string(id);
        if s.is_wide() {
            return Ok(None);
        }
        let Some(next) = total_len.checked_add(s.len() as usize) else {
            return Ok(None);
        };
        if next > MAX_STRING_LEN {
            return Ok(None);
        }
        total_len = next;
        all_ascii &= s.is_ascii();
    }
    if total_len == 0 {
        let empty = ctx.with_scope(|ctx| ctx.new_latin1(&[]).map(|h| ctx.get(h)))?;
        return Ok(Some(empty));
    }
    if total_len <= INLINE_LATIN1_CAPACITY {
        let mut buf = [0u8; INLINE_LATIN1_CAPACITY];
        let mut pos = 0usize;
        {
            let heap = &ctx.runtime.heap;
            let stack = &ctx.runtime.vm.stack;
            copy_latin1_inline(heap, s_id, &mut buf, &mut pos);
            for i in 0..argc {
                let ValueKind::String(id) = stack[base + 2 + i].decode() else {
                    unreachable!()
                };
                copy_latin1_inline(heap, id, &mut buf, &mut pos);
            }
        }
        let is_ascii = all_ascii || buf[..total_len].is_ascii();
        let id = ctx.allocate(|rt| {
            rt.heap.strings.insert(
                &rt.heap.ledger,
                JsString::InlineLatin1 {
                    len: total_len as u8,
                    is_ascii,
                    data: buf,
                },
            )
        })?;
        return Ok(Some(Value::string(id)));
    }
    let mut data = TrackedVec::new();
    ctx.allocate(|rt| data.try_reserve_exact(&rt.heap.ledger, total_len))?;
    {
        let heap = &ctx.runtime.heap;
        let stack = &ctx.runtime.vm.stack;
        copy_latin1_vec(heap, s_id, &mut data);
        for i in 0..argc {
            let ValueKind::String(id) = stack[base + 2 + i].decode() else {
                unreachable!()
            };
            copy_latin1_vec(heap, id, &mut data);
        }
    }
    let is_ascii = all_ascii || data.is_ascii();
    let mut data = Some(data);
    let id = ctx.allocate(|rt| {
        rt.heap.strings.try_reserve_slot(&rt.heap.ledger)?;
        Ok(rt.heap.strings.insert_reserved(JsString::Latin1 {
            data: data.take().unwrap(),
            is_ascii,
        }))
    })?;
    Ok(Some(Value::string(id)))
}

#[inline]
fn copy_latin1_inline(
    heap: &crate::heap::Heap,
    id: StrId,
    buf: &mut [u8; INLINE_LATIN1_CAPACITY],
    pos: &mut usize,
) {
    if let Some(bytes) = heap.string(id).as_latin1() {
        buf[*pos..*pos + bytes.len()].copy_from_slice(bytes);
        *pos += bytes.len();
    } else {
        for unit in heap.string_units(id) {
            buf[*pos] = unit as u8;
            *pos += 1;
        }
    }
}

#[inline]
fn copy_latin1_vec(heap: &crate::heap::Heap, id: StrId, out: &mut TrackedVec<u8>) {
    if let Some(bytes) = heap.string(id).as_latin1() {
        for &byte in bytes {
            out.push_reserved(byte);
        }
    } else {
        for unit in heap.string_units(id) {
            out.push_reserved(unit as u8);
        }
    }
}

#[cold]
fn run_raw_step(
    ctx: &mut ExecCtx<'_, '_>,
    index: usize,
    code: BcId,
    at: u32,
) -> JsResult<Option<Value>> {
    let (inst, strict, next_pc) = {
        let function = ctx
            .heap()
            .bytecode
            .get(code)
            .unwrap()
            .compiled
            .as_ref()
            .unwrap();
        let bytes = &function.code;
        let inst = Instruction::decode(bytes, at as usize).expect("verified instruction");
        let next = at + u32::from(OP_SIZE[bytes[at as usize] as usize]);
        (inst, function.strict, next)
    };
    let frame = &mut ctx.runtime.vm.frames[index];
    frame.last_pc = at;
    frame.pc = next_pc;
    let mut original = *frame;
    original.pc = at;
    slow_op(ctx, original, index, code, inst, strict, next_pc)
}

#[cold]
fn run_raw_frame(
    ctx: &mut ExecCtx<'_, '_>,
    index: usize,
    code: BcId,
    _entry_index: usize,
) -> JsResult<Option<Value>> {
    let mut pc = ctx.runtime.vm.frames[index].pc;
    loop {
        let skipped = {
            let function = ctx
                .heap()
                .bytecode
                .get(code)
                .unwrap()
                .compiled
                .as_ref()
                .unwrap();
            if !function.hoisted.is_empty()
                && let Ok(i) = function.hoisted.binary_search_by_key(&pc, |&(at, _)| at)
            {
                Some(function.hoisted[i].1)
            } else {
                None
            }
        };
        if let Some(next) = skipped {
            pc = next;
            ctx.runtime.vm.frames[index].pc = pc;
            continue;
        }
        let result = run_raw_step(ctx, index, code, pc)?;
        if result.is_some() || ctx.runtime.vm.frames.len() != index + 1 {
            return Ok(result);
        }
        pc = ctx.runtime.vm.frames[index].pc;
    }
}

fn append_array_slot(ctx: &mut ExecCtx<'_, '_>, id: ObjId, value: Value) -> JsResult<()> {
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::Array(array) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        array.elements.push(&heap.ledger, value)?;
        array.length += 1;
        Ok(())
    })
}

#[inline]
fn fast_binary_with_string(
    heap: &crate::heap::Heap,
    op: Opcode,
    lhs: Value,
    rhs: Value,
) -> Option<Value> {
    let to_num = |v: Value| match v.decode() {
        ValueKind::Int32(n) => Some(f64::from(n)),
        ValueKind::Float64(n) => Some(n),
        ValueKind::String(id) => convert::string_to_number_inline(heap, id),
        _ => None,
    };
    let a = to_num(lhs)?;
    let b = to_num(rhs)?;
    Some(numeric::number_binary(op, a, b))
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
                Mod if b > 0 => {
                    let r = a % b;
                    (r != 0 || a >= 0).then_some(r)
                }
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
        Nop => {}
        PushI32 | PushI16 | PushI8 | PushMinus1 | Push0 | Push1 | Push2 | Push3 | Push4 | Push5
        | Push6 | Push7 => {
            let Operands::Signed(n) = inst.operands else {
                unreachable!()
            };
            replace(ctx, 0, &[Value::int32(n)]);
        }
        PushBigintI32 => {
            let Operands::Signed(n) = inst.operands else {
                unreachable!()
            };
            replace(ctx, 0, &[Value::short_bigint(i64::from(n)).unwrap()]);
        }
        Undefined => replace(ctx, 0, &[Value::UNDEFINED]),
        Null => replace(ctx, 0, &[Value::NULL]),
        PushFalse | PushTrue => replace(ctx, 0, &[Value::bool(op == PushTrue)]),
        Drop => replace(ctx, 1, &[]),
        Dup => {
            let v = value(ctx, 1);
            replace(ctx, 0, &[v]);
        }
        Dup1 => {
            let a = value(ctx, 2);
            let b = value(ctx, 1);
            replace(ctx, 2, &[b, a, b]);
        }
        Swap => ctx.runtime.vm.stack.swap(tail - 2, tail - 1),
        Nip => {
            let last = value(ctx, 1);
            replace(ctx, 2, &[last]);
        }
        GetLoc0Loc1 => {
            let a = ctx.runtime.vm.stack[local_start];
            let b = ctx.runtime.vm.stack[local_start + 1];
            replace(ctx, 0, &[a, b]);
        }
        GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => {
            let Operands::Local(n) = inst.operands else {
                unreachable!()
            };
            let v = ctx.runtime.vm.stack[local_start + usize::from(n)];
            replace(ctx, 0, &[v]);
        }
        PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => {
            let Operands::Local(n) = inst.operands else {
                unreachable!()
            };
            let v = value(ctx, 1);
            ctx.runtime.vm.stack[local_start + usize::from(n)] = v;
            replace(ctx, 1, &[]);
        }
        SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 => {
            let Operands::Local(n) = inst.operands else {
                unreachable!()
            };
            ctx.runtime.vm.stack[local_start + usize::from(n)] = value(ctx, 1);
        }
        GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3 => {
            let Operands::Argument(n) = inst.operands else {
                unreachable!()
            };
            let v = ctx.runtime.vm.stack[arg_start + usize::from(n)];
            replace(ctx, 0, &[v]);
        }
        PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3 => {
            let Operands::Argument(n) = inst.operands else {
                unreachable!()
            };
            let v = value(ctx, 1);
            ctx.runtime.vm.stack[arg_start + usize::from(n)] = v;
            replace(ctx, 1, &[]);
        }
        SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3 => {
            let Operands::Argument(n) = inst.operands else {
                unreachable!()
            };
            ctx.runtime.vm.stack[arg_start + usize::from(n)] = value(ctx, 1);
        }
        Lnot => {
            let result = Value::bool(!convert::to_boolean_value(ctx.heap(), value(ctx, 1)));
            replace(ctx, 1, &[result]);
        }
        Return | ReturnUndef | ReturnAsync => {
            let ret = if op == ReturnUndef {
                Value::UNDEFINED
            } else {
                value(ctx, 1)
            };
            return Ok(Some(ret));
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
    image: &sandbolt_bytecode::ProgramImage,
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
