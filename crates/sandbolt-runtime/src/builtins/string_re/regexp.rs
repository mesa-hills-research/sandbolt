use std::rc::Rc;

use sandbolt_regexp::{CompiledRegExp, RegExpError, RegExpFlags as F, RegExpHost, RegExpInput};

use crate::arena::Arena;
use crate::ledger::{AllocError, TrackedBox};
use crate::object::{
    ArrayData, ErrorKind, JsObject, ObjectFlags, PropSlots, RegExpData, RegExpProgram,
    RegExpStringIteratorData,
};
use crate::ops::call::construct;
use crate::ops::compare::same_value;
use crate::ops::convert::{space, to_boolean, to_integer_or_infinity, to_length, to_uint32};
use crate::ops::property::set_property;
use crate::vm::VmState;

use super::*;

const LAST_INDEX: PropKey = PropKey::atom(Atom(sandbolt_bytecode::BuiltinAtom::LastIndex as u32));

const FLAGS: [(u8, u16, &str); 8] = [
    (b'd', F::INDICES, "hasIndices"),
    (b'g', F::GLOBAL, "global"),
    (b'i', F::IGNORECASE, "ignoreCase"),
    (b'm', F::MULTILINE, "multiline"),
    (b's', F::DOTALL, "dotAll"),
    (b'u', F::UNICODE, "unicode"),
    (b'v', F::UNICODE_SETS, "unicodeSets"),
    (b'y', F::STICKY, "sticky"),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = super::super::configure_ctor(ctx, ctx.realm().regexp_ctor, REGEXP, "RegExp", 2)?;
    global(ctx, "RegExp", ctor)?;
    methods(ctx, ctor, REGEXP.0, &[(1, "escape", 1)])?;
    let species = sym(ctx, WellKnown::Species);
    super::super::install_accessor(
        ctx,
        ctor,
        species,
        Some((NativeFnId(REGEXP.0 + 2), "get [Symbol.species]")),
        None,
        true,
    )?;
    let proto = ctx.root(Value::object(ctx.realm().regexp_proto))?;
    methods(
        ctx,
        proto,
        REGEXP.0,
        &[
            (3, "exec", 1),
            (4, "test", 1),
            (5, "toString", 0),
            (6, "compile", 2),
        ],
    )?;
    for (op, symbol, name, len) in [
        (7, WellKnown::Match, "[Symbol.match]", 1),
        (8, WellKnown::MatchAll, "[Symbol.matchAll]", 1),
        (9, WellKnown::Replace, "[Symbol.replace]", 2),
        (10, WellKnown::Search, "[Symbol.search]", 1),
        (11, WellKnown::Split, "[Symbol.split]", 2),
    ] {
        symbol_method(ctx, proto, symbol, NativeFnId(REGEXP.0 + op), name, len)?;
    }
    iterator_proto(
        ctx,
        ctx.realm().regexp_string_iterator_proto,
        NativeFnId(REGEXP.0 + 12),
        "RegExp String Iterator",
    )?;
    let names = [
        "get hasIndices",
        "get global",
        "get ignoreCase",
        "get multiline",
        "get dotAll",
        "get unicode",
        "get unicodeSets",
        "get sticky",
    ];
    for (op, name, getter) in [(13, "flags", "get flags"), (14, "source", "get source")]
        .into_iter()
        .chain(
            FLAGS
                .iter()
                .zip(names)
                .enumerate()
                .map(|(i, ((_, _, name), getter))| (15 + i as u16, *name, getter)),
        )
    {
        let key = ctx.intern_str(name)?;
        super::super::install_accessor(
            ctx,
            proto,
            key,
            Some((NativeFnId(REGEXP.0 + op), getter)),
            None,
            true,
        )?;
    }
    Ok(())
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match op {
        0 => constructor(ctx, args),
        1 => {
            let s = arg(ctx, args, 0)?;
            escape(ctx, s)
        }
        2 => Ok(ctx.get(this)),
        3 => {
            let s = arg(ctx, args, 0)?;
            builtin_exec(ctx, this, s)
        }
        4 => {
            ctx.object_id(this)?;
            let s = arg(ctx, args, 0)?;
            let s = to_string(ctx, s)?;
            Ok(Value::bool(regexp_exec(ctx, this, s)? != Value::NULL))
        }
        5 => to_text(ctx, this),
        6 => compile_method(ctx, this, args),
        7..=11 => {
            ctx.object_id(this)?;
            let s = arg(ctx, args, 0)?;
            let s = to_string(ctx, s)?;
            match op {
                7 => match_method(ctx, this, s),
                8 => match_all(ctx, this, s),
                9 => {
                    let replace = arg(ctx, args, 1)?;
                    replace_method(ctx, this, s, replace)
                }
                10 => search(ctx, this, s),
                _ => {
                    let limit = arg(ctx, args, 1)?;
                    split(ctx, this, s, limit)
                }
            }
        }
        12 => iterator_next(ctx, this),
        13 => flags_getter(ctx, this),
        14 => source_getter(ctx, this),
        15..=22 => {
            let value = ctx.get(this);
            if value == Value::object(ctx.realm().regexp_proto) {
                return Ok(Value::UNDEFINED);
            }
            let id = branded(ctx, this)?;
            Ok(Value::bool(
                regexp_data(ctx, id)
                    .program
                    .compiled
                    .flags
                    .contains(FLAGS[(op - 15) as usize].1),
            ))
        }
        _ => Err(ctx.throw_type_error("unknown RegExp builtin")),
    }
}

fn branded<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<ObjId> {
    let id = ctx.object_id(value)?;
    if matches!(ctx.heap().object(id).data, ObjectData::RegExp(_)) {
        Ok(id)
    } else {
        Err(ctx.throw_type_error("expected a RegExp receiver"))
    }
}

fn regexp_data<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> &'a RegExpData {
    let ObjectData::RegExp(data) = &ctx.heap().object(id).data else {
        unreachable!()
    };
    data
}

fn internal<'a>(ctx: &'a ExecCtx<'_, '_>, value: Value) -> Option<&'a RegExpData> {
    let id = value.as_object()?;
    match &ctx.heap().object(id).data {
        ObjectData::RegExp(data) => Some(data),
        _ => None,
    }
}

fn named<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>, name: &str) -> JsResult<Handle<'s>> {
    let key = ctx.intern_str(name)?;
    get(ctx, obj, key)
}

pub(super) fn is_regexp<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<bool> {
    if ctx.get(value).as_object().is_none() {
        return Ok(false);
    }
    let matcher = get(ctx, value, sym(ctx, WellKnown::Match))?;
    Ok(if ctx.get(matcher) == Value::UNDEFINED {
        internal(ctx, ctx.get(value)).is_some()
    } else {
        to_boolean(ctx, matcher)
    })
}

pub(super) fn check_global<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<()> {
    if is_regexp(ctx, value)? {
        let flags = named(ctx, value, "flags")?;
        require_object_coercible(ctx, ctx.get(flags))?;
        if !text(ctx, flags)?.contains(&u16::from(b'g')) {
            return Err(ctx.throw_type_error("RegExp must have the global flag"));
        }
    }
    Ok(())
}

fn constructor<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let pattern = arg(ctx, args, 0)?;
    let flags = arg(ctx, args, 1)?;
    let pattern_is_re = is_regexp(ctx, pattern)?;
    let intrinsic = ctx.realm().regexp_ctor;
    let no_flags = ctx.get(flags) == Value::UNDEFINED;
    let target = ctx.new_target();
    if target.is_none() && pattern_is_re && no_flags {
        let ctor = named(ctx, pattern, "constructor")?;
        if ctx.get(ctor) == Value::object(intrinsic) {
            return Ok(ctx.get(pattern));
        }
    }
    let (pattern, flags, program) = if let Some(data) = internal(ctx, ctx.get(pattern)) {
        let source = data.source;
        let program = no_flags.then(|| data.program.clone());
        (ctx.root(source)?, flags, program)
    } else if pattern_is_re {
        let source = named(ctx, pattern, "source")?;
        let flags = if no_flags {
            named(ctx, pattern, "flags")?
        } else {
            flags
        };
        (source, flags, None)
    } else {
        (pattern, flags, None)
    };
    let obj = ctx.ordinary_create_from_constructor(
        target.unwrap_or(intrinsic),
        |r| r.regexp_proto,
        ObjectFlags::EXTENSIBLE,
        |_| Ok(ObjectData::Ordinary),
    )?;
    initialize(ctx, obj, pattern, flags, program)?;
    Ok(ctx.get(obj))
}

pub(crate) fn create<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    pattern: Handle<'s>,
    flags: Handle<'s>,
) -> JsResult<Value> {
    let source = if ctx.get(pattern) == Value::UNDEFINED {
        ctx.new_string("")?
    } else {
        to_string(ctx, pattern)?
    };
    let program = compile(ctx, source, flags)?;
    let shape = crate::realm::ensure_regexp_shape(ctx)?;
    let source = ctx.get(source);
    let obj = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let props = PropSlots::one(Value::int32(0));
        heap.objects.insert(
            &heap.ledger,
            JsObject {
                shape,
                props,
                flags: ObjectFlags::EXTENSIBLE,
                data: ObjectData::RegExp(RegExpData {
                    source,
                    program: Rc::clone(&program),
                }),
            },
        )
    })?;
    Ok(Value::object(obj))
}

fn initialize<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    obj: Handle<'s>,
    pattern: Handle<'s>,
    flags: Handle<'s>,
    program: Option<Rc<RegExpProgram>>,
) -> JsResult<()> {
    let source = if ctx.get(pattern) == Value::UNDEFINED {
        ctx.new_string("")?
    } else {
        to_string(ctx, pattern)?
    };
    let program = match program {
        Some(program) => program,
        None => compile(ctx, source, flags)?,
    };
    let source = ctx.get(source);
    let id = ctx.object_id(obj)?;
    let object = ctx.runtime.heap.objects.get_mut(id).unwrap();
    let existing = matches!(object.data, ObjectData::RegExp(_));
    object.data = ObjectData::RegExp(RegExpData {
        source,
        program: program.clone(),
    });
    if existing {
        set_last_index(ctx, obj, 0.0)
    } else {
        let key = ctx.intern_str("lastIndex")?;
        let zero = ctx.root(Value::int32(0))?;
        define_data(ctx, obj, key, zero, PropFlags::WRITABLE)
    }
}

fn compile_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    branded(ctx, this)?;
    let pattern = arg(ctx, args, 0)?;
    let flags = arg(ctx, args, 1)?;
    let (pattern, program) = if let Some(data) = internal(ctx, ctx.get(pattern)) {
        if ctx.get(flags) != Value::UNDEFINED {
            return Err(ctx.throw_type_error("flags must be undefined with a RegExp pattern"));
        }
        let (source, program) = (data.source, data.program.clone());
        (ctx.root(source)?, Some(program))
    } else {
        (pattern, None)
    };
    initialize(ctx, this, pattern, flags, program)?;
    Ok(ctx.get(this))
}

struct Host<'a, 'rt, 's>(&'a mut ExecCtx<'rt, 's>);

impl RegExpHost for Host<'_, '_, '_> {
    fn poll_interrupt(&mut self) -> bool {
        self.0.check_interrupt().is_err()
    }

    fn charge(&mut self, bytes: usize) -> Result<(), RegExpError> {
        let result = self.0.heap().ledger.charge_bytes(bytes);
        if matches!(result, Err(AllocError::Limit | AllocError::Allocator)) {
            self.0.collect();
            self.0
                .heap()
                .ledger
                .charge_bytes(bytes)
                .map_err(|_| RegExpError::OutOfMemory)
        } else {
            result.map_err(|_| RegExpError::OutOfMemory)
        }
    }

    fn release(&mut self, bytes: usize) {
        self.0.heap().ledger.release_bytes(bytes);
    }

    fn check_stack(&mut self, bytes: usize) -> bool {
        self.0
            .runtime
            .vm
            .check_budget(bytes.div_ceil(size_of::<Value>()), 0, 0)
    }
}

fn re_error(ctx: &mut ExecCtx<'_, '_>, error: RegExpError) -> crate::Abrupt {
    match error {
        RegExpError::Interrupted => crate::Abrupt::Interrupted,
        RegExpError::OutOfMemory => ctx.throw_oom(),
        RegExpError::StackOverflow => ctx.stack_overflow(),
        RegExpError::Syntax(text) => ctx.throw_error(ErrorKind::Syntax, &text),
        RegExpError::InvalidBytecode => {
            ctx.throw_error(ErrorKind::Internal, "invalid RegExp bytecode")
        }
    }
}

fn compile<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    source: Handle<'s>,
    flags: Handle<'s>,
) -> JsResult<Rc<RegExpProgram>> {
    let mut mask = 0;
    if ctx.get(flags) != Value::UNDEFINED {
        let flags = text(ctx, flags)?;
        for &flag in flags.iter() {
            let bit = FLAGS
                .iter()
                .find_map(|&(ch, bit, _)| (u16::from(ch) == flag).then_some(bit));
            match bit {
                Some(bit) if mask & bit == 0 => mask |= bit,
                _ => return Err(ctx.throw_error(ErrorKind::Syntax, "invalid RegExp flags")),
            }
        }
        if mask & F::UNICODE != 0 && mask & F::UNICODE_SETS != 0 {
            return Err(ctx.throw_error(ErrorKind::Syntax, "cannot combine RegExp flags u and v"));
        }
    }
    let flags = F(mask);
    let source = units(ctx, ctx.get(source))?;
    let mut bytes = TrackedVec::new();
    let unicode = flags.contains(F::UNICODE | F::UNICODE_SETS);
    let mut i = 0;
    while i < source.len() {
        if i & 2047 < 2 {
            ctx.check_interrupt()?;
        }
        let cp;
        if unicode {
            (cp, i) = code_point(&source, i);
        } else {
            cp = u32::from(source[i]);
            i += 1;
        }
        let mut tmp = [0; 4];
        let seq = if let Some(c) = char::from_u32(cp) {
            c.encode_utf8(&mut tmp).as_bytes()
        } else {
            tmp = [
                0xe0 | (cp >> 12) as u8,
                0x80 | ((cp >> 6) & 63) as u8,
                0x80 | (cp & 63) as u8,
                0,
            ];
            &tmp[..3]
        };
        if seq.len() > bytes.capacity() - bytes.len() {
            ctx.allocate(|rt| bytes.try_reserve(&rt.heap.ledger, seq.len()))?;
        }
        for &byte in seq {
            bytes.push_reserved(byte);
        }
    }
    ctx.check_interrupt()?;
    let compiled =
        sandbolt_regexp::compile(&bytes, flags, &mut Host(ctx)).map_err(|e| re_error(ctx, e))?;
    let bytes = compiled.bytecode.capacity()
        + compiled.group_names.capacity() * size_of::<Option<String>>()
        + compiled
            .group_names
            .iter()
            .flatten()
            .map(String::capacity)
            .sum::<usize>()
        + size_of::<RegExpProgram>()
        + 2 * size_of::<usize>();
    ctx.allocate(|rt| rt.heap.ledger.charge_bytes(bytes))?;
    Ok(Rc::new(RegExpProgram {
        compiled,
        ledger: ctx.heap().ledger.clone(),
        bytes,
    }))
}

fn flags_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    ctx.object_id(this)?;
    let mut flags = Builder::new();
    for (flag, _, prop) in FLAGS {
        let value = named(ctx, this, prop)?;
        if to_boolean(ctx, value) {
            flags.unit(ctx, flag.into())?;
        }
    }
    flags.value(ctx)
}

fn to_text<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    ctx.object_id(this)?;
    let source = named(ctx, this, "source")?;
    let source = to_string(ctx, source)?;
    let flags = named(ctx, this, "flags")?;
    let flags = to_string(ctx, flags)?;
    let mut out = Builder::new();
    out.unit(ctx, b'/'.into())?;
    out.string(ctx, ctx.get(source))?;
    out.unit(ctx, b'/'.into())?;
    out.string(ctx, ctx.get(flags))?;
    out.value(ctx)
}

fn source_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    if ctx.get(this) == Value::object(ctx.realm().regexp_proto) {
        return ctx.new_string("(?:)").map(|s| ctx.get(s));
    }
    let id = branded(ctx, this)?;
    let re = regexp_data(ctx, id);
    let source = re.source;
    let sets = re.program.compiled.flags.contains(F::UNICODE_SETS);
    let input = units(ctx, source)?;
    if input.is_empty() {
        return ctx.new_string("(?:)").map(|s| ctx.get(s));
    }
    let mut out = Builder::new();
    let mut depth = 0usize;
    let mut escaped = false;
    for (i, &c) in input.iter().enumerate() {
        if i & 2047 == 0 {
            ctx.check_interrupt()?;
        }
        match c {
            10 | 13 | 0x2028 | 0x2029 => {
                if escaped {
                    out.0.pop();
                }
                match c {
                    10 => out.ascii(ctx, b"\\n")?,
                    13 => out.ascii(ctx, b"\\r")?,
                    0x2028 => out.ascii(ctx, b"\\u2028")?,
                    _ => out.ascii(ctx, b"\\u2029")?,
                }
            }
            47 if !escaped && depth == 0 => out.ascii(ctx, b"\\/")?,
            _ => {
                if !escaped {
                    if c == 91 && (sets || depth == 0) {
                        depth += 1;
                    }
                    if c == 93 {
                        depth = depth.saturating_sub(1);
                    }
                }
                out.unit(ctx, c)?;
            }
        }
        escaped = c == 92 && !escaped;
    }
    out.value(ctx)
}

fn escaped_hex(ctx: &mut ExecCtx<'_, '_>, out: &mut Builder, c: u16, wide: bool) -> JsResult<()> {
    const HEX: &[u8] = b"0123456789abcdef";
    if wide {
        out.ascii(
            ctx,
            &[
                b'\\',
                b'u',
                HEX[(c >> 12) as usize],
                HEX[((c >> 8) & 15) as usize],
                HEX[((c >> 4) & 15) as usize],
                HEX[(c & 15) as usize],
            ],
        )
    } else {
        out.ascii(
            ctx,
            &[b'\\', b'x', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]],
        )
    }
}

fn escape<'s>(ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>) -> JsResult<Value> {
    let s = ctx.get(s);
    if as_string(s).is_none() {
        return Err(ctx.throw_type_error("expected a string"));
    }
    let input = units(ctx, s)?;
    let mut out = Builder::new();
    let mut i = 0;
    while i < input.len() {
        if i & 2047 < 2 {
            ctx.check_interrupt()?;
        }
        let (c, end) = code_point(&input, i);
        if end == i + 2 {
            out.extend(ctx, &input[i..end])?;
        } else {
            let c = c as u16;
            let ascii = u8::try_from(c).ok();
            if i == 0 && ascii.is_some_and(|b| b.is_ascii_alphanumeric()) {
                escaped_hex(ctx, &mut out, c, false)?;
            } else if let Some(b) = ascii.filter(|b| b"^$\\.*+?()[]{}|/".contains(b)) {
                out.ascii(ctx, &[b'\\', b])?;
            } else if (9..=13).contains(&c) {
                out.ascii(ctx, &[b'\\', b"tnvfr"[(c - 9) as usize]])?;
            } else if ascii.is_some_and(|b| b",-=<>#&!%:;@~'`\"".contains(&b))
                || space(c)
                || is_surrogate(c)
            {
                escaped_hex(ctx, &mut out, c, c > 0xff)?;
            } else {
                out.unit(ctx, c)?;
            }
        }
        i = end;
    }
    out.value(ctx)
}

fn set<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    obj: Handle<'s>,
    key: PropKey,
    val: Handle<'s>,
) -> JsResult<()> {
    if !set_property(ctx, obj, key, val)? {
        return Err(ctx.throw_type_error("cannot assign to RegExp lastIndex"));
    }
    Ok(())
}

fn set_last_index<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>, index: f64) -> JsResult<()> {
    if let Some(id) = ctx.get(obj).as_object()
        && Some(ctx.heap().object(id).shape) == ctx.realm().regexp_shape
    {
        ctx.runtime.heap.objects.get_mut(id).unwrap().props[0] = Value::number(index);
        return Ok(());
    }
    let val = ctx.root(Value::number(index))?;
    set(ctx, obj, LAST_INDEX, val)
}

struct DirectHost<'a> {
    vm: &'a mut VmState,
    ledger: &'a HeapLedger,
    hit_limit: bool,
}

impl RegExpHost for DirectHost<'_> {
    fn poll_interrupt(&mut self) -> bool {
        if self
            .vm
            .interrupt_handler
            .as_mut()
            .is_some_and(|handler| handler())
        {
            self.vm.exception = None;
            true
        } else {
            false
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), RegExpError> {
        match self.ledger.charge_bytes(bytes) {
            Ok(()) => Ok(()),
            Err(AllocError::Limit | AllocError::Allocator) => {
                self.hit_limit = true;
                Err(RegExpError::OutOfMemory)
            }
            Err(_) => Err(RegExpError::OutOfMemory),
        }
    }

    fn release(&mut self, bytes: usize) {
        self.ledger.release_bytes(bytes);
    }

    fn check_stack(&mut self, bytes: usize) -> bool {
        self.vm
            .check_budget(bytes.div_ceil(size_of::<Value>()), 0, 0)
    }
}

fn flat_regexp_input<'a>(strings: &'a Arena<JsString>, id: StrId) -> RegExpInput<'a> {
    match strings.get(id).expect("missing string") {
        JsString::Latin1 { data, .. } => RegExpInput::Latin1(data),
        JsString::InlineLatin1 { len, data, .. } => RegExpInput::Latin1(&data[..usize::from(*len)]),
        JsString::Utf16(data) => RegExpInput::Utf16(data),
        JsString::InlineUtf16 { len, data } => RegExpInput::Utf16(&data[..usize::from(*len)]),
        JsString::Slice {
            base, start, len, ..
        } => {
            let start = *start as usize;
            let end = start + *len as usize;
            let base = strings.get(*base).expect("missing string slice base");
            if let Some(bytes) = base.as_latin1() {
                RegExpInput::Latin1(&bytes[start..end])
            } else {
                RegExpInput::Utf16(&base.as_utf16().unwrap()[start..end])
            }
        }
        JsString::Rope { .. } => unreachable!("expected flattened string"),
    }
}

fn regexp_exec<'s>(ctx: &mut ExecCtx<'_, 's>, re: Handle<'s>, s: Handle<'s>) -> JsResult<Value> {
    let exec = named(ctx, re, "exec")?;
    if callable(ctx, ctx.get(exec)) {
        let result = call_with(ctx, exec, re, &[s])?;
        if result == Value::NULL || result.as_object().is_some() {
            return Ok(result);
        }
        return Err(ctx.throw_type_error("RegExp exec must return an object or null"));
    }
    builtin_exec(ctx, re, s)
}

// The caller keeps both ids rooted. None asks the caller to use the general builtin.
pub(crate) fn fast_regexp_exec(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    input: StrId,
) -> JsResult<Option<Value>> {
    let heap = ctx.heap();
    let object = heap.object(id);
    let ObjectData::RegExp(data) = &object.data else {
        return Ok(None);
    };
    if Some(object.shape) != ctx.realm().regexp_shape
        || data.program.compiled.capture_count != 1
        || data
            .program
            .compiled
            .flags
            .contains(F::NAMED_GROUPS | F::INDICES)
        || matches!(heap.string(input), JsString::Rope { .. })
    {
        return Ok(None);
    }
    let index = match object.props[0].decode() {
        ValueKind::Int32(index) if index >= 0 => index as usize,
        _ => return Ok(None),
    };
    let update = data.program.compiled.flags.contains(F::GLOBAL | F::STICKY);
    let start = if update { index } else { 0 };
    if start > heap.string(input).len() as usize {
        ctx.runtime.heap.objects.get_mut(id).unwrap().props[0] = Value::number(0.0);
        return Ok(Some(Value::NULL));
    }
    ctx.check_interrupt()?;
    let (result, hit_limit) = {
        let rt = &mut *ctx.runtime;
        let ObjectData::RegExp(data) = &rt.heap.objects.get(id).unwrap().data else {
            unreachable!()
        };
        let input_slice = flat_regexp_input(&rt.heap.strings, input);
        let mut host = DirectHost {
            vm: &mut rt.vm,
            ledger: &rt.heap.ledger,
            hit_limit: false,
        };
        let result =
            sandbolt_regexp::exec_single(&data.program.compiled, input_slice, start, &mut host);
        (result, host.hit_limit)
    };
    let result = match result {
        Err(RegExpError::OutOfMemory) if hit_limit => {
            let program = {
                let ObjectData::RegExp(data) = &ctx.heap().object(id).data else {
                    unreachable!()
                };
                Rc::clone(&data.program)
            };
            ctx.collect();
            let copied = units(ctx, Value::string(input))?;
            sandbolt_regexp::exec_single(
                &program.compiled,
                RegExpInput::Utf16(&copied),
                start,
                &mut Host(ctx),
            )
            .map_err(|e| re_error(ctx, e))?
        }
        other => other.map_err(|e| re_error(ctx, e))?,
    };
    if update {
        ctx.runtime.heap.objects.get_mut(id).unwrap().props[0] =
            Value::number(result.map_or(0.0, |(_, end)| end as f64));
    }
    match result {
        Some((start, end)) => build_single_result(ctx, input, start, end).map(Some),
        None => Ok(Some(Value::NULL)),
    }
}

fn exec_single_match(
    ctx: &mut ExecCtx<'_, '_>,
    program: &CompiledRegExp,
    input_id: StrId,
    start: usize,
) -> JsResult<Option<(usize, usize)>> {
    let (result, hit_limit) = {
        let rt = &mut *ctx.runtime;
        let input_slice = flat_regexp_input(&rt.heap.strings, input_id);
        let mut host = DirectHost {
            vm: &mut rt.vm,
            ledger: &rt.heap.ledger,
            hit_limit: false,
        };
        let result = sandbolt_regexp::exec_single(program, input_slice, start, &mut host);
        (result, host.hit_limit)
    };
    match result {
        Err(RegExpError::OutOfMemory) if hit_limit => {
            ctx.collect();
            let copied = units(ctx, Value::string(input_id))?;
            sandbolt_regexp::exec_single(
                program,
                RegExpInput::Utf16(&copied),
                start,
                &mut Host(ctx),
            )
            .map_err(|e| re_error(ctx, e))
        }
        other => other.map_err(|e| re_error(ctx, e)),
    }
}

fn builtin_exec<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    input: Handle<'s>,
) -> JsResult<Value> {
    let id = branded(ctx, this)?;
    let input = if as_string(ctx.get(input)).is_some() {
        input
    } else {
        to_string(ctx, input)?
    };
    let input_id = string_id(ctx.get(input));
    if let Some(result) = fast_regexp_exec(ctx, id, input_id)? {
        return Ok(result);
    }
    let index = if Some(ctx.heap().object(id).shape) == ctx.realm().regexp_shape {
        let raw = ctx.heap().object(id).props[0];
        match raw.decode() {
            ValueKind::Int32(n) if n >= 0 => n as u64,
            _ => {
                let h = ctx.root(raw)?;
                to_length(ctx, h)?
            }
        }
    } else {
        let index = get(ctx, this, LAST_INDEX)?;
        to_length(ctx, index)?
    };
    let program = regexp_data(ctx, id).program.clone();
    let update = program.compiled.flags.contains(F::GLOBAL | F::STICKY);
    let start = if update { index } else { 0 };
    if start > u64::from(ctx.heap().string(input_id).len()) {
        set_last_index(ctx, this, 0.0)?;
        return Ok(Value::NULL);
    }
    if matches!(ctx.heap().string(input_id), JsString::Rope { .. }) {
        ctx.flatten_string(input)?;
    }
    ctx.check_interrupt()?;
    let flags = program.compiled.flags;
    if program.compiled.capture_count == 1 && !flags.contains(F::NAMED_GROUPS | F::INDICES) {
        let result = exec_single_match(ctx, &program.compiled, input_id, start as usize)?;
        let Some((m_start, m_end)) = result else {
            if update {
                set_last_index(ctx, this, 0.0)?;
            }
            return Ok(Value::NULL);
        };
        if update {
            set_last_index(ctx, this, m_end as f64)?;
        }
        return build_single_result(ctx, input_id, m_start, m_end);
    }
    let (result, hit_limit) = {
        let rt = &mut *ctx.runtime;
        let input_slice = flat_regexp_input(&rt.heap.strings, input_id);
        let mut host = DirectHost {
            vm: &mut rt.vm,
            ledger: &rt.heap.ledger,
            hit_limit: false,
        };
        let res =
            sandbolt_regexp::exec_spans(&program.compiled, input_slice, start as usize, &mut host);
        (res, host.hit_limit)
    };
    let result = match result {
        Err(RegExpError::OutOfMemory) if hit_limit => {
            ctx.collect();
            let copied = units(ctx, ctx.get(input))?;
            sandbolt_regexp::exec_spans(
                &program.compiled,
                RegExpInput::Utf16(&copied),
                start as usize,
                &mut Host(ctx),
            )
            .map_err(|e| re_error(ctx, e))?
        }
        other => other.map_err(|e| re_error(ctx, e))?,
    };
    let Some(result) = result else {
        if update {
            set_last_index(ctx, this, 0.0)?;
        }
        return Ok(Value::NULL);
    };
    let heap_bytes = result.heap_bytes();
    let _charge = if heap_bytes > 0 {
        Some(Charge::new(ctx, heap_bytes)?)
    } else {
        None
    };
    let spans = result.as_slice();
    let (start, end) = spans[0].unwrap();
    if update {
        set_last_index(ctx, this, end as f64)?;
    }
    build_result(ctx, &program.compiled, spans, input, start)
}

fn groups<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    result: Handle<'s>,
    has_names: bool,
) -> JsResult<Handle<'s>> {
    let groups = if has_names {
        ctx.new_object_with_proto(None)?
    } else {
        ctx.root(Value::UNDEFINED)?
    };
    let key = ctx.intern_str("groups")?;
    define_data(ctx, result, key, groups, PropFlags::DEFAULT_DATA)?;
    Ok(groups)
}

fn alloc_result_array<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    len: usize,
    first: Option<Value>,
    start: usize,
    input: Value,
    groups_val: Value,
) -> JsResult<ObjId> {
    let shape = crate::realm::ensure_regexp_result_shape(ctx)?;
    let realm = ctx.realm_id();
    ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let mut elements = TrackedVec::new();
        elements.try_reserve_exact(&heap.ledger, len)?;
        if let Some(val) = first {
            elements.push_reserved(val);
            for _ in 1..len {
                elements.push_reserved(Value::UNDEFINED);
            }
        } else {
            for _ in 0..len {
                elements.push_reserved(Value::UNDEFINED);
            }
        }
        let array = TrackedBox::new(
            &heap.ledger,
            ArrayData {
                elements,
                length: len as u32,
                length_writable: true,
                realm,
            },
        )?;
        let mut props = PropSlots::new();
        props.try_reserve_exact(&heap.ledger, 3)?;
        props.push_reserved(Value::int32(start as i32));
        props.push_reserved(input);
        props.push_reserved(groups_val);
        heap.objects.insert(
            &heap.ledger,
            JsObject {
                shape,
                props,
                flags: ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC | ObjectFlags::FAST_ARRAY,
                data: ObjectData::Array(array),
            },
        )
    })
}

fn build_single_result(
    ctx: &mut ExecCtx<'_, '_>,
    input_id: StrId,
    start: usize,
    end: usize,
) -> JsResult<Value> {
    let input_len = ctx.heap().string(input_id).len() as usize;
    let slice_len = end - start;
    let input_val = Value::string(input_id);
    if slice_len <= 1 || slice_len == input_len {
        let cap0 = if slice_len == input_len {
            input_val
        } else if slice_len == 0 {
            let empty = ctx.new_latin1(&[])?;
            ctx.get(empty)
        } else {
            let unit = JsString::unit_at(&ctx.heap().strings, input_id, start as u32).unwrap();
            let unit = ctx.new_utf16(&[unit])?;
            ctx.get(unit)
        };
        let obj = alloc_result_array(ctx, 1, Some(cap0), start, input_val, Value::UNDEFINED)?;
        return Ok(Value::object(obj));
    }
    let shape = crate::realm::ensure_regexp_result_shape(ctx)?;
    let realm = ctx.realm_id();
    let obj = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let (base, offset) = match heap.string(input_id) {
            JsString::Slice { base, start, .. } => (*base, *start as usize),
            _ => (input_id, 0),
        };
        let backing = heap.string(base);
        let matched_string = if (slice_len >= 4096 || backing.is_wide())
            && backing.len() as usize <= slice_len.saturating_mul(4)
        {
            JsString::Slice {
                base,
                start: (offset + start) as u32,
                len: slice_len as u32,
                is_wide: backing.is_wide(),
            }
        } else {
            let range = offset + start..offset + end;
            if let Some(bytes) = backing.as_latin1() {
                let slice = &bytes[range];
                let is_ascii = backing.is_ascii() || slice.is_ascii();
                if slice_len <= crate::string::INLINE_LATIN1_CAPACITY {
                    let mut data = [0u8; crate::string::INLINE_LATIN1_CAPACITY];
                    data[..slice_len].copy_from_slice(slice);
                    JsString::InlineLatin1 {
                        len: slice_len as u8,
                        is_ascii,
                        data,
                    }
                } else {
                    JsString::Latin1 {
                        data: TrackedVec::from_slice(&heap.ledger, slice)?,
                        is_ascii,
                    }
                }
            } else {
                JsString::from_utf16(&heap.ledger, &backing.as_utf16().unwrap()[range])?
            }
        };
        let cap0_id = heap.strings.insert(&heap.ledger, matched_string)?;
        let mut elements = TrackedVec::new();
        elements.try_reserve_exact(&heap.ledger, 1)?;
        elements.push_reserved(Value::string(cap0_id));
        let array = TrackedBox::new(
            &heap.ledger,
            ArrayData {
                elements,
                length: 1,
                length_writable: true,
                realm,
            },
        )?;
        let mut props = PropSlots::new();
        props.try_reserve_exact(&heap.ledger, 3)?;
        props.push_reserved(Value::int32(start as i32));
        props.push_reserved(input_val);
        props.push_reserved(Value::UNDEFINED);
        heap.objects.insert(
            &heap.ledger,
            JsObject {
                shape,
                props,
                flags: ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC | ObjectFlags::FAST_ARRAY,
                data: ObjectData::Array(array),
            },
        )
    })?;
    Ok(Value::object(obj))
}

fn build_result<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    program: &CompiledRegExp,
    captures: &[Option<(usize, usize)>],
    input: Handle<'s>,
    start: usize,
) -> JsResult<Value> {
    let has_names = program.flags.contains(F::NAMED_GROUPS);
    let has_indices = program.flags.contains(F::INDICES);
    if !has_names && !has_indices {
        if captures.len() == 1 {
            let cap0 = match captures[0] {
                Some((s, e)) => ctx.string_slice(input, s, e)?,
                None => ctx.root(Value::UNDEFINED)?,
            };
            let obj = alloc_result_array(
                ctx,
                1,
                Some(ctx.get(cap0)),
                start,
                ctx.get(input),
                Value::UNDEFINED,
            )?;
            return Ok(Value::object(obj));
        }
        let obj = alloc_result_array(
            ctx,
            captures.len(),
            None,
            start,
            ctx.get(input),
            Value::UNDEFINED,
        )?;
        let result = ctx.root(Value::object(obj))?;
        let input_val = ctx.get(input);
        for (i, &capture) in captures.iter().enumerate() {
            if let Some((s, e)) = capture {
                let value = ctx.with_scope(|ctx| {
                    let input = ctx.root(input_val)?;
                    let slice = ctx.string_slice(input, s, e)?;
                    Ok(ctx.get(slice))
                })?;
                let ObjectData::Array(array) =
                    &mut ctx.runtime.heap.objects.get_mut(obj).unwrap().data
                else {
                    unreachable!()
                };
                array.elements[i] = value;
            }
        }
        return Ok(ctx.get(result));
    }
    let names = if has_names {
        ctx.new_object_with_proto(None)?
    } else {
        ctx.root(Value::UNDEFINED)?
    };
    let obj = alloc_result_array(
        ctx,
        captures.len(),
        None,
        start,
        ctx.get(input),
        ctx.get(names),
    )?;
    let result = ctx.root(Value::object(obj))?;
    let indices = if has_indices {
        let indices = ctx.new_array()?;
        let key = ctx.intern_str("indices")?;
        define_data(ctx, result, key, indices, PropFlags::DEFAULT_DATA)?;
        let names = groups(ctx, indices, has_names)?;
        Some((indices, names))
    } else {
        None
    };
    let (result_val, names_val, input_val) = (ctx.get(result), ctx.get(names), ctx.get(input));
    let indices = indices.map(|(a, b)| (ctx.get(a), ctx.get(b)));
    for (i, &capture) in captures.iter().enumerate() {
        ctx.with_scope(|ctx| {
            let input = ctx.root(input_val)?;
            let value = if let Some((s, e)) = capture {
                ctx.string_slice(input, s, e)?
            } else {
                ctx.root(Value::UNDEFINED)?
            };
            let val = ctx.get(value);
            {
                let ObjectData::Array(array) =
                    &mut ctx.runtime.heap.objects.get_mut(obj).unwrap().data
                else {
                    unreachable!()
                };
                array.elements[i] = val;
            }
            let name = program.group_names.get(i).and_then(Option::as_deref);
            if let Some(name) = name {
                let names = ctx.root(names_val)?;
                put_group(ctx, names, name, value)?;
            }
            if let Some((indices, names)) = indices {
                let indices = ctx.root(indices)?;
                let value = if let Some((s, e)) = capture {
                    let pair = ctx.new_array()?;
                    let s = ctx.root(Value::int32(s as i32))?;
                    let e = ctx.root(Value::int32(e as i32))?;
                    put_index(ctx, pair, 0, s)?;
                    put_index(ctx, pair, 1, e)?;
                    pair
                } else {
                    ctx.root(Value::UNDEFINED)?
                };
                put_index(ctx, indices, i, value)?;
                if let Some(name) = name {
                    let names = ctx.root(names)?;
                    put_group(ctx, names, name, value)?;
                }
            }
            Ok(())
        })?;
    }
    Ok(result_val)
}

fn put_group<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    groups: Handle<'s>,
    name: &str,
    value: Handle<'s>,
) -> JsResult<()> {
    let key = ctx.intern_str(name)?;
    if ctx.get(value) == Value::UNDEFINED && get_property(ctx, groups, key)? != Value::UNDEFINED {
        return Ok(());
    }
    define_data(ctx, groups, key, value, PropFlags::DEFAULT_DATA)
}

fn read_flags<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>) -> JsResult<Handle<'s>> {
    let flags = named(ctx, obj, "flags")?;
    to_string(ctx, flags)
}

fn full_unicode(flags: &[u16]) -> bool {
    flags.contains(&117) || flags.contains(&118)
}

fn species<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>) -> JsResult<Handle<'s>> {
    let ctor = named(ctx, obj, "constructor")?;
    let default = Value::object(ctx.realm().regexp_ctor);
    if ctx.get(ctor) == Value::UNDEFINED {
        return ctx.root(default);
    }
    ctx.object_id(ctor)?;
    let species = get(ctx, ctor, sym(ctx, WellKnown::Species))?;
    if nullish(ctx.get(species)) {
        return ctx.root(default);
    }
    if let Some(id) = ctx.get(species).as_object()
        && ctx
            .heap()
            .object(id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
    {
        return Ok(species);
    }
    Err(ctx.throw_type_error("RegExp species must be a constructor"))
}

fn advance_last<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    obj: Handle<'s>,
    input: &[u16],
    unicode: bool,
) -> JsResult<()> {
    let index = named(ctx, obj, "lastIndex")?;
    let i = to_length(ctx, index)?;
    let next = if i < input.len() as u64 {
        advance(input, i as usize, unicode) as u64
    } else {
        i + 1
    };
    set_last_index(ctx, obj, next as f64)
}

fn match_method<'s>(ctx: &mut ExecCtx<'_, 's>, re: Handle<'s>, s: Handle<'s>) -> JsResult<Value> {
    let flags = read_flags(ctx, re)?;
    let flags = units(ctx, ctx.get(flags))?;
    if !flags.contains(&103) {
        return regexp_exec(ctx, re, s);
    }
    set_last_index(ctx, re, 0.0)?;
    let unicode = full_unicode(&flags);
    let (re, s) = (ctx.get(re), ctx.get(s));
    let units = units(ctx, s)?;
    let arr = ctx.new_array()?;
    let arr = ctx.get(arr);
    for n in 0.. {
        let done = ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let re = ctx.root(re)?;
            let s = ctx.root(s)?;
            let m = regexp_exec(ctx, re, s)?;
            if m == Value::NULL {
                return Ok(true);
            }
            let m = ctx.root(m)?;
            let matched = get_index(ctx, m, 0)?;
            let matched = to_string(ctx, matched)?;
            let arr = ctx.root(arr)?;
            put_index(ctx, arr, n, matched)?;
            if ctx.heap().string(string_id(ctx.get(matched))).is_empty() {
                advance_last(ctx, re, &units, unicode)?;
            }
            Ok(false)
        })?;
        if done {
            return Ok(if n == 0 { Value::NULL } else { arr });
        }
    }
    unreachable!()
}

fn match_all<'s>(ctx: &mut ExecCtx<'_, 's>, re: Handle<'s>, s: Handle<'s>) -> JsResult<Value> {
    let ctor = species(ctx, re)?;
    let flags = read_flags(ctx, re)?;
    let args = ctx.args(&[re, flags])?;
    let matcher = construct(ctx, ctor, args, ctor)?;
    let matcher = ctx.root(matcher)?;
    let index = named(ctx, re, "lastIndex")?;
    let index = to_length(ctx, index)?;
    set_last_index(ctx, matcher, index as f64)?;
    let flags = units(ctx, ctx.get(flags))?;
    let data = RegExpStringIteratorData {
        matcher: ctx.get(matcher),
        string: ctx.get(s),
        global: flags.contains(&103),
        unicode: full_unicode(&flags),
        done: false,
    };
    let proto = ctx.realm().regexp_string_iterator_proto;
    let iterator = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::RegExpStringIterator(TrackedBox::new(
            ledger,
            RegExpStringIteratorData { ..data },
        )?))
    })?;
    Ok(ctx.get(iterator))
}

fn iterator_next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    let id = ctx.object_id(this)?;
    let ObjectData::RegExpStringIterator(data) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a RegExp String Iterator"));
    };
    let (matcher, string, global, unicode, done) = (
        data.matcher,
        data.string,
        data.global,
        data.unicode,
        data.done,
    );
    if done {
        let undef = ctx.root(Value::UNDEFINED)?;
        return iterator_result(ctx, undef, true);
    }
    let matcher = ctx.root(matcher)?;
    let string = ctx.root(string)?;
    let m = regexp_exec(ctx, matcher, string)?;
    let empty = m == Value::NULL;
    let value = ctx.root(if empty { Value::UNDEFINED } else { m })?;
    if empty || !global {
        let ObjectData::RegExpStringIterator(data) =
            &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
        else {
            unreachable!()
        };
        data.done = true;
        data.matcher = Value::UNDEFINED;
        data.string = Value::UNDEFINED;
    } else {
        let matched = get_index(ctx, value, 0)?;
        let matched = to_string(ctx, matched)?;
        if ctx.heap().string(string_id(ctx.get(matched))).is_empty() {
            let units = units(ctx, ctx.get(string))?;
            advance_last(ctx, matcher, &units, unicode)?;
        }
    }
    iterator_result(ctx, value, empty)
}

fn search<'s>(ctx: &mut ExecCtx<'_, 's>, re: Handle<'s>, s: Handle<'s>) -> JsResult<Value> {
    let previous = named(ctx, re, "lastIndex")?;
    let zero = ctx.root(Value::int32(0))?;
    let key = ctx.intern_str("lastIndex")?;
    if !same_value(ctx, previous, zero) {
        set(ctx, re, key, zero)?;
    }
    let result = regexp_exec(ctx, re, s)?;
    let result = ctx.root(result)?;
    let current = get(ctx, re, key)?;
    if !same_value(ctx, current, previous) {
        set(ctx, re, key, previous)?;
    }
    if ctx.get(result) == Value::NULL {
        Ok(Value::int32(-1))
    } else {
        let index = named(ctx, result, "index")?;
        Ok(ctx.get(index))
    }
}

fn split<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    re: Handle<'s>,
    s: Handle<'s>,
    limit: Handle<'s>,
) -> JsResult<Value> {
    let ctor = species(ctx, re)?;
    let flags = read_flags(ctx, re)?;
    let flag_units = units(ctx, ctx.get(flags))?;
    let unicode = full_unicode(&flag_units);
    let flags = if flag_units.contains(&121) {
        flags
    } else {
        let y = ctx.new_string("y")?;
        ctx.concat_strings(flags, y)?
    };
    let args = ctx.args(&[re, flags])?;
    let splitter = construct(ctx, ctor, args, ctor)?;
    let splitter = ctx.root(splitter)?;
    let result = ctx.new_array()?;
    let limit = if ctx.get(limit) == Value::UNDEFINED {
        u32::MAX
    } else {
        to_uint32(ctx, limit)?
    } as usize;
    if limit == 0 {
        return Ok(ctx.get(result));
    }
    let units = units(ctx, ctx.get(s))?;
    if units.is_empty() {
        if regexp_exec(ctx, splitter, s)? == Value::NULL {
            put_index(ctx, result, 0, s)?;
        }
        return Ok(ctx.get(result));
    }
    let mut start = 0;
    let mut q = 0;
    let mut n = 0;
    let (splitter, result, s) = (ctx.get(splitter), ctx.get(result), ctx.get(s));
    while q < units.len() {
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let splitter = ctx.root(splitter)?;
            let s = ctx.root(s)?;
            set_last_index(ctx, splitter, q as f64)?;
            let m = regexp_exec(ctx, splitter, s)?;
            if m == Value::NULL {
                q = advance(&units, q, unicode);
                return Ok(());
            }
            let m = ctx.root(m)?;
            let e = named(ctx, splitter, "lastIndex")?;
            let e = to_length(ctx, e)?.min(units.len() as u64) as usize;
            if e == start {
                q = advance(&units, q, unicode);
                return Ok(());
            }
            let result = ctx.root(result)?;
            let piece = ctx.new_utf16(&units[start..q])?;
            put_index(ctx, result, n, piece)?;
            n += 1;
            if n == limit {
                return Ok(());
            }
            start = e;
            let captures = get(ctx, m, LENGTH)?;
            let captures = to_length(ctx, captures)?.saturating_sub(1) as usize;
            let result = ctx.get(result);
            let m = ctx.get(m);
            for i in 1..=captures.min(limit - n) {
                ctx.with_scope(|ctx| {
                    ctx.check_interrupt()?;
                    let result = ctx.root(result)?;
                    let m = ctx.root(m)?;
                    let capture = get_index(ctx, m, i)?;
                    put_index(ctx, result, n, capture)
                })?;
                n += 1;
            }
            q = start;
            Ok(())
        })?;
        if n == limit {
            return Ok(result);
        }
    }
    let arr = ctx.root(result)?;
    let tail = ctx.new_utf16(&units[start..])?;
    put_index(ctx, arr, n, tail)?;
    Ok(result)
}

fn replace_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    re: Handle<'s>,
    s: Handle<'s>,
    replace: Handle<'s>,
) -> JsResult<Value> {
    let functional = callable(ctx, ctx.get(replace));
    let replacement = if functional {
        None
    } else {
        Some(text(ctx, replace)?)
    };
    let flags = read_flags(ctx, re)?;
    let flags = units(ctx, ctx.get(flags))?;
    let global = flags.contains(&103);
    let unicode = full_unicode(&flags);
    if global {
        set_last_index(ctx, re, 0.0)?;
    }
    let input = units(ctx, ctx.get(s))?;
    let records = ctx.new_array()?;
    let (re, s, records, replace) = (ctx.get(re), ctx.get(s), ctx.get(records), ctx.get(replace));
    let mut count = 0;
    loop {
        let done = ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let re = ctx.root(re)?;
            let s = ctx.root(s)?;
            let record = regexp_exec(ctx, re, s)?;
            if record == Value::NULL {
                return Ok(true);
            }
            let record = ctx.root(record)?;
            let records = ctx.root(records)?;
            put_index(ctx, records, count, record)?;
            count += 1;
            if !global {
                return Ok(true);
            }
            let matched = get_index(ctx, record, 0)?;
            let matched = to_string(ctx, matched)?;
            if ctx.heap().string(string_id(ctx.get(matched))).is_empty() {
                advance_last(ctx, re, &input, unicode)?;
            }
            Ok(false)
        })?;
        if done {
            break;
        }
    }
    if count == 0 {
        return Ok(s);
    }
    let mut out = Builder::new();
    let mut end = 0;
    for i in 0..count {
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let records = ctx.root(records)?;
            let record = get_index(ctx, records, i)?;
            let length = get(ctx, record, LENGTH)?;
            let length = to_length(ctx, length)?.saturating_sub(1) as usize;
            let matched = get_index(ctx, record, 0)?;
            let matched = to_string(ctx, matched)?;
            let matched_units = units(ctx, ctx.get(matched))?;
            let pos = named(ctx, record, "index")?;
            let pos = to_integer_or_infinity(ctx, pos)?.clamp(0.0, input.len() as f64) as usize;
            let mut captures = TrackedVec::new();
            if functional {
                append_handle(ctx, &mut captures, matched)?;
            }
            for i in 1..=length {
                ctx.check_interrupt()?;
                let capture = get_index(ctx, record, i)?;
                let capture = if ctx.get(capture) == Value::UNDEFINED {
                    capture
                } else {
                    to_string(ctx, capture)?
                };
                append_handle(ctx, &mut captures, capture)?;
            }
            let groups = named(ctx, record, "groups")?;
            let groups = (ctx.get(groups) != Value::UNDEFINED).then_some(groups);
            let substitution = if let Some(replacement) = &replacement {
                let groups = groups.map(|g| to_object(ctx, g)).transpose()?;
                let mut subst = Builder::new();
                Substitution {
                    input: &input,
                    matched: &matched_units,
                    position: pos,
                    captures: &captures,
                    groups,
                }
                .append(ctx, &mut subst, replacement)?;
                subst.finish(ctx)?
            } else {
                let position = ctx.root(Value::int32(pos as i32))?;
                let s = ctx.root(s)?;
                append_handle(ctx, &mut captures, position)?;
                append_handle(ctx, &mut captures, s)?;
                if let Some(groups) = groups {
                    append_handle(ctx, &mut captures, groups)?;
                }
                let f = ctx.root(replace)?;
                let this = ctx.root(Value::UNDEFINED)?;
                let part = call_with(ctx, f, this, &captures)?;
                let part = ctx.root(part)?;
                to_string(ctx, part)?
            };
            if pos >= end {
                out.extend(ctx, &input[end..pos])?;
                out.string(ctx, ctx.get(substitution))?;
                end = pos + matched_units.len();
            }
            Ok(())
        })?;
    }
    out.extend(ctx, &input[end.min(input.len())..])?;
    out.value(ctx)
}

fn append_handle<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    vec: &mut TrackedVec<Handle<'s>>,
    value: Handle<'s>,
) -> JsResult<()> {
    if vec.len() == vec.capacity() {
        ctx.allocate(|rt| vec.try_reserve(&rt.heap.ledger, 1))?;
    }
    vec.push_reserved(value);
    Ok(())
}
