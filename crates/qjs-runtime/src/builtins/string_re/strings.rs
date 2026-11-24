use qjs_unicode::{
    CaseConv, UnicodeNormalization, case_conv, is_case_ignorable, is_cased, normalize,
};

use crate::ledger::TrackedBox;
use crate::object::{ObjectData, ObjectFlags, StringIteratorData, StringObjectData};
use crate::ops::convert::{space, to_integer_or_infinity, to_length, to_number, to_uint32};
use crate::root::append_latin1_bytes;
use crate::string::{INLINE_LATIN1_CAPACITY, INLINE_UTF16_CAPACITY, JsString};
use crate::symbol::SymbolDescription;
use crate::value::ValueKind;

use super::*;

const PROTO_METHODS: &[(u16, &str, u32)] = &[
    (4, "at", 1),
    (5, "charAt", 1),
    (6, "charCodeAt", 1),
    (7, "codePointAt", 1),
    (8, "concat", 1),
    (9, "endsWith", 1),
    (10, "includes", 1),
    (11, "indexOf", 1),
    (12, "isWellFormed", 0),
    (13, "lastIndexOf", 1),
    (14, "localeCompare", 1),
    (15, "match", 1),
    (16, "matchAll", 1),
    (17, "normalize", 0),
    (18, "padEnd", 1),
    (19, "padStart", 1),
    (20, "repeat", 1),
    (21, "replace", 2),
    (22, "replaceAll", 2),
    (23, "search", 1),
    (24, "slice", 2),
    (25, "split", 2),
    (26, "startsWith", 1),
    (27, "substring", 2),
    (28, "toLocaleLowerCase", 0),
    (29, "toLocaleUpperCase", 0),
    (30, "toLowerCase", 0),
    (31, "toString", 0),
    (32, "toUpperCase", 0),
    (33, "toWellFormed", 0),
    (34, "trim", 0),
    (35, "trimEnd", 0),
    (36, "trimStart", 0),
    (37, "valueOf", 0),
    (38, "substr", 2),
    (39, "anchor", 1),
    (40, "big", 0),
    (41, "blink", 0),
    (42, "bold", 0),
    (43, "fixed", 0),
    (44, "fontcolor", 1),
    (45, "fontsize", 1),
    (46, "italics", 0),
    (47, "link", 1),
    (48, "small", 0),
    (49, "strike", 0),
    (50, "sub", 0),
    (51, "sup", 0),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = super::super::configure_ctor(ctx, ctx.realm().string_ctor, STRING, "String", 1)?;
    global(ctx, "String", ctor)?;
    let id = ctx.realm().string_proto;
    let proto = ctx.root(Value::object(id))?;
    let empty = ctx.new_string("")?;
    init_wrapper(ctx, proto, empty)?;
    methods(
        ctx,
        ctor,
        STRING.0,
        &[
            (1, "fromCharCode", 1),
            (2, "fromCodePoint", 1),
            (3, "raw", 1),
        ],
    )?;
    methods(ctx, proto, STRING.0, PROTO_METHODS)?;
    for (alias, method) in [("trimLeft", "trimStart"), ("trimRight", "trimEnd")] {
        let key = ctx.intern_str(method)?;
        let method = get(ctx, proto, key)?;
        let key = ctx.intern_str(alias)?;
        define_data(ctx, proto, key, method, method_flags())?;
    }
    symbol_method(
        ctx,
        proto,
        WellKnown::Iterator,
        NativeFnId(STRING.0 + 52),
        "[Symbol.iterator]",
        0,
    )?;
    iterator_proto(
        ctx,
        ctx.realm().string_iterator_proto,
        NativeFnId(STRING.0 + 53),
        "String Iterator",
    )
}

fn init_wrapper<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    string: Handle<'s>,
) -> JsResult<()> {
    let value = ctx.get(string);
    let payload =
        ctx.allocate(|rt| TrackedBox::new(&rt.heap.ledger, StringObjectData { value }))?;
    let id = ctx.object_id(target)?;
    let object = ctx.runtime.heap.objects.get_mut(id).unwrap();
    object.data = ObjectData::StringObject(payload);
    object.flags |= ObjectFlags::IS_EXOTIC;
    let len = ctx.heap().string(string_id(value)).len();
    let len = ctx.root(Value::int32(len as i32))?;
    define_data(ctx, target, LENGTH, len, PropFlags::default())
}

fn construct<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let target = ctx.new_target();
    let s = if let Some(value) = args.get(0) {
        if target.is_none()
            && let ValueKind::Symbol(id) = ctx.get(value).decode()
        {
            let mut out = Builder::new();
            out.ascii(ctx, b"Symbol(")?;
            match ctx.heap().symbols.get(id).unwrap().description {
                SymbolDescription::None => (),
                SymbolDescription::String(id) => out.string(ctx, Value::string(id))?,
                SymbolDescription::WellKnown(name) => {
                    out.ascii(ctx, name.description().as_bytes())?
                }
            }
            out.unit(ctx, b')'.into())?;
            return out.value(ctx);
        }
        to_string(ctx, value)?
    } else {
        ctx.new_string("")?
    };
    let Some(target) = target else {
        return Ok(ctx.get(s));
    };
    let value = ctx.get(s);
    let wrapper = ctx.ordinary_create_from_constructor(
        target,
        |r| r.string_proto,
        ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC,
        |heap| {
            Ok(ObjectData::StringObject(TrackedBox::new(
                &heap.ledger,
                StringObjectData { value },
            )?))
        },
    )?;
    let len = ctx.heap().string(string_id(value)).len();
    let len = ctx.root(Value::int32(len as i32))?;
    define_data(ctx, wrapper, LENGTH, len, PropFlags::default())?;
    Ok(ctx.get(wrapper))
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match op {
        0 => return construct(ctx, args),
        1 | 2 => return from_codes(ctx, args, op == 2),
        3 => return raw(ctx, args),
        31 | 37 => {
            let value = ctx.get(this);
            if as_string(value).is_some() {
                return Ok(value);
            }
            if let Some(id) = value.as_object()
                && let ObjectData::StringObject(data) = &ctx.heap().object(id).data
            {
                return Ok(data.value);
            }
            return Err(ctx.throw_type_error("expected a String receiver"));
        }
        53 => return next(ctx, this),
        _ => (),
    }
    let s = if as_string(ctx.get(this)).is_some() {
        if matches!(op, 15 | 16 | 21 | 22 | 23 | 25) {
            return pattern_method(op, ctx, this, args);
        }
        this
    } else {
        require_object_coercible(ctx, ctx.get(this))?;
        if matches!(op, 15 | 16 | 21 | 22 | 23 | 25) {
            return pattern_method(op, ctx, this, args);
        }
        to_string(ctx, this)?
    };
    if op == 52 {
        return iterator(ctx, s);
    }
    if op == 8 {
        return concat(ctx, s, args);
    }
    match op {
        4..=7 => return indexed(op, ctx, s, args.get(0)),
        9..=11 | 13 | 26 => {
            let a = arg(ctx, args, 0)?;
            let b = arg(ctx, args, 1)?;
            return find(op, ctx, s, a, b);
        }
        20 => {
            let a = arg(ctx, args, 0)?;
            return repeat(ctx, s, a);
        }
        24 | 27 | 38 => return slice(op, ctx, s, args.get(0), args.get(1)),
        28..=30 | 32 => return case(ctx, s, matches!(op, 28 | 30)),
        34..=36 => return trim(op, ctx, s),
        _ => (),
    }
    let input = units(ctx, ctx.get(s))?;
    match op {
        12 | 33 => well_formed(ctx, s, &input, op == 33),
        14 | 17 | 39..=51 => {
            let a = arg(ctx, args, 0)?;
            match op {
                14 => compare(ctx, &input, a),
                17 => normalize_string(ctx, s, &input, a),
                _ => html(op, ctx, &input, a),
            }
        }
        18 | 19 => {
            let a = arg(ctx, args, 0)?;
            let b = arg(ctx, args, 1)?;
            pad(ctx, s, &input, a, b, op == 19)
        }
        _ => Err(ctx.throw_type_error("unknown String builtin")),
    }
}

fn from_codes<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, points: bool) -> JsResult<Value> {
    if !points && args.len() <= 32 {
        let mut buf = [0u16; 32];
        for (i, slot) in buf.iter_mut().take(args.len()).enumerate() {
            let arg = args.get(i).unwrap();
            *slot = if let ValueKind::Int32(n) = ctx.get(arg).decode() {
                n as u16
            } else {
                to_uint32(ctx, arg)? as u16
            };
        }
        return ctx.new_utf16(&buf[..args.len()]).map(|s| ctx.get(s));
    }
    let mut out = Builder::new();
    out.reserve(ctx, args.len(), true)?;
    for i in 0..args.len() {
        if i > 0 && (i & 1023) == 0 {
            ctx.check_interrupt()?;
        }
        let arg = args.get(i).unwrap();
        if points {
            let c = to_number(ctx, arg)?;
            if !(0.0..=0x10ffff as f64).contains(&c) || c != c.trunc() {
                return Err(ctx.throw_range_error("invalid code point"));
            }
            out.code_point(ctx, c as u32)?;
        } else {
            let c = if let ValueKind::Int32(n) = ctx.get(arg).decode() {
                n as u16
            } else {
                to_uint32(ctx, arg)? as u16
            };
            out.unit(ctx, c)?;
        }
    }
    out.value(ctx)
}

fn raw<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let template = arg(ctx, args, 0)?;
    let template = to_object(ctx, template)?;
    let key = ctx.intern_str("raw")?;
    let raw = get(ctx, template, key)?;
    let raw = to_object(ctx, raw)?;
    let len = get(ctx, raw, LENGTH)?;
    let len = to_length(ctx, len)? as usize;
    let mut out = Builder::new();
    let raw = ctx.get(raw);
    for i in 0..len {
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let raw = ctx.root(raw)?;
            let item = get_index(ctx, raw, i)?;
            let item = to_string(ctx, item)?;
            out.string(ctx, ctx.get(item))
        })?;
        if i + 1 < len
            && let Some(sub) = args.get(i + 1)
        {
            let sub = ctx.get(sub);
            ctx.with_scope(|ctx| {
                let sub = ctx.root(sub)?;
                let sub = to_string(ctx, sub)?;
                out.string(ctx, ctx.get(sub))
            })?;
        }
    }
    out.value(ctx)
}

fn append_latin1_inline(heap: &crate::heap::Heap, id: StrId, dst: &mut [u8], pos: &mut usize) {
    match heap.string(id) {
        JsString::Latin1 { data, .. } => {
            dst[*pos..*pos + data.len()].copy_from_slice(data);
            *pos += data.len();
        }
        JsString::InlineLatin1 { len, data, .. } => {
            let len = usize::from(*len);
            dst[*pos..*pos + len].copy_from_slice(&data[..len]);
            *pos += len;
        }
        JsString::Slice {
            base, start, len, ..
        } => {
            let start = *start as usize;
            let len = *len as usize;
            let bytes = heap.string(*base).as_latin1().expect("latin1 slice");
            dst[*pos..*pos + len].copy_from_slice(&bytes[start..start + len]);
            *pos += len;
        }
        JsString::Rope { .. } => {
            for unit in heap.string_units(id) {
                dst[*pos] = unit as u8;
                *pos += 1;
            }
        }
        JsString::Utf16(_) | JsString::InlineUtf16 { .. } => unreachable!("wide string in latin1"),
    }
}

fn append_utf16_units(heap: &crate::heap::Heap, id: StrId, out: &mut TrackedVec<u16>) {
    match heap.string(id) {
        JsString::Utf16(data) => {
            out.extend_from_slice(&heap.ledger, data)
                .expect("pre-reserved concat utf16");
        }
        JsString::InlineUtf16 { len, data } => {
            out.extend_from_slice(&heap.ledger, &data[..usize::from(*len)])
                .expect("pre-reserved concat inline utf16");
        }
        JsString::Latin1 { data, .. } => {
            for &b in data.iter() {
                out.push_reserved(u16::from(b));
            }
        }
        JsString::InlineLatin1 { len, data, .. } => {
            for &b in &data[..usize::from(*len)] {
                out.push_reserved(u16::from(b));
            }
        }
        JsString::Slice {
            base, start, len, ..
        } => {
            let start = *start as usize;
            let end = start + *len as usize;
            let base = heap.string(*base);
            if let Some(units) = base.as_utf16() {
                out.extend_from_slice(&heap.ledger, &units[start..end])
                    .expect("pre-reserved concat utf16 slice");
            } else {
                for &b in &base.as_latin1().unwrap()[start..end] {
                    out.push_reserved(u16::from(b));
                }
            }
        }
        JsString::Rope { .. } => {
            for unit in heap.string_units(id) {
                out.push_reserved(unit);
            }
        }
    }
}

fn concat<'s>(ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    if args.is_empty() {
        return Ok(ctx.get(s));
    }
    if args.len() == 1 {
        let a0 = args.get(0).unwrap();
        let a0 = if as_string(ctx.get(a0)).is_some() {
            a0
        } else {
            to_string(ctx, a0)?
        };
        let s_len = ctx.heap().string(string_id(ctx.get(s))).len() as usize;
        let a_len = ctx.heap().string(string_id(ctx.get(a0))).len() as usize;
        if s_len.saturating_add(a_len) > MAX_STRING_LEN {
            return Err(string_error(ctx));
        }
        let result = ctx.concat_strings(s, a0)?;
        return Ok(ctx.get(result));
    }
    let s_id = string_id(ctx.get(s));
    let heap = ctx.heap();
    let s_str = heap.string(s_id);
    let s_len = s_str.len() as usize;
    let mut total_len = s_len;
    let mut any_wide = s_str.is_wide();
    let mut all_ascii = s_str.is_ascii()
        || matches!(s_str, JsString::Slice { base, is_wide: false, .. } if heap.string(*base).is_ascii());
    let mut all_strings = true;
    for i in 0..args.len() {
        if let Some(id) = as_string(ctx.get(args.get(i).unwrap())) {
            let st = heap.string(id);
            total_len = total_len.saturating_add(st.len() as usize);
            any_wide |= st.is_wide();
            if all_ascii && !st.is_ascii() {
                all_ascii = matches!(
                    st,
                    JsString::Slice { base, is_wide: false, .. } if heap.string(*base).is_ascii()
                );
            }
        } else {
            all_strings = false;
            break;
        }
    }
    if all_strings {
        if total_len > MAX_STRING_LEN {
            return Err(string_error(ctx));
        }
        if total_len == 0 {
            return ctx.new_latin1(&[]).map(|h| ctx.get(h));
        }
        if total_len == s_len {
            return Ok(ctx.get(s));
        }
        if args.len() >= 1024 || total_len >= 65536 {
            ctx.check_interrupt()?;
        }
        if !any_wide {
            if total_len <= INLINE_LATIN1_CAPACITY {
                let mut buf = [0u8; INLINE_LATIN1_CAPACITY];
                let mut pos = 0;
                {
                    let heap = ctx.heap();
                    append_latin1_inline(heap, s_id, &mut buf, &mut pos);
                    for i in 0..args.len() {
                        let id = string_id(ctx.get(args.get(i).unwrap()));
                        append_latin1_inline(heap, id, &mut buf, &mut pos);
                    }
                }
                if total_len == 1 {
                    return ctx.new_latin1(&buf[..1]).map(|h| ctx.get(h));
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
                return Ok(Value::string(id));
            }
            let mut data = TrackedVec::new();
            ctx.allocate(|rt| data.try_reserve_exact(&rt.heap.ledger, total_len))?;
            {
                let heap = ctx.heap();
                append_latin1_bytes(heap, s_id, &mut data);
                for i in 0..args.len() {
                    let id = string_id(ctx.get(args.get(i).unwrap()));
                    append_latin1_bytes(heap, id, &mut data);
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
            return Ok(Value::string(id));
        }
        if total_len <= INLINE_UTF16_CAPACITY {
            let mut buf = [0u16; INLINE_UTF16_CAPACITY];
            let mut pos = 0;
            {
                let heap = ctx.heap();
                for unit in heap.string_units(s_id) {
                    buf[pos] = unit;
                    pos += 1;
                }
                for i in 0..args.len() {
                    let id = string_id(ctx.get(args.get(i).unwrap()));
                    for unit in heap.string_units(id) {
                        buf[pos] = unit;
                        pos += 1;
                    }
                }
            }
            return ctx.new_utf16(&buf[..total_len]).map(|h| ctx.get(h));
        }
        let mut data = TrackedVec::new();
        ctx.allocate(|rt| data.try_reserve_exact(&rt.heap.ledger, total_len))?;
        {
            let heap = ctx.heap();
            append_utf16_units(heap, s_id, &mut data);
            for i in 0..args.len() {
                let id = string_id(ctx.get(args.get(i).unwrap()));
                append_utf16_units(heap, id, &mut data);
            }
        }
        let mut data = Some(data);
        let id = ctx.allocate(|rt| {
            rt.heap.strings.try_reserve_slot(&rt.heap.ledger)?;
            Ok(rt
                .heap
                .strings
                .insert_reserved(JsString::Utf16(data.take().unwrap())))
        })?;
        return Ok(Value::string(id));
    }
    let mut out = Builder::new();
    out.string(ctx, ctx.get(s))?;
    for i in 0..args.len() {
        let arg = ctx.get(args.get(i).unwrap());
        if as_string(arg).is_some() {
            out.string(ctx, arg)?;
        } else {
            ctx.with_scope(|ctx| {
                let arg = ctx.root(arg)?;
                let arg = to_string(ctx, arg)?;
                out.string(ctx, ctx.get(arg))
            })?;
        }
    }
    out.value(ctx)
}

fn bounded(pos: f64, len: usize) -> usize {
    pos.clamp(0.0, len as f64) as usize
}

fn relative(pos: f64, len: usize) -> usize {
    bounded(if pos < 0.0 { pos + len as f64 } else { pos }, len)
}

fn indexed<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    index: Option<Handle<'s>>,
) -> JsResult<Value> {
    ctx.check_interrupt()?;
    let id = string_id(ctx.get(s));
    let len = ctx.heap().string(id).len() as f64;
    let mut i = index.map_or(Ok(0.0), |i| to_integer_or_infinity(ctx, i))?;
    if op == 4 && i < 0.0 {
        i += len;
    }
    if i < 0.0 || i >= len {
        return match op {
            5 => ctx.new_latin1(&[]).map(|s| ctx.get(s)),
            6 => Ok(Value::number(f64::NAN)),
            _ => Ok(Value::UNDEFINED),
        };
    }
    let i = i as u32;
    let arena = &ctx.heap().strings;
    let unit = JsString::unit_at(arena, id, i).unwrap();
    match op {
        4 | 5 => ctx.new_utf16(&[unit]).map(|s| ctx.get(s)),
        6 => Ok(Value::int32(i32::from(unit))),
        _ => {
            let point = if is_high(unit)
                && let Some(low) = JsString::unit_at(arena, id, i + 1).filter(|&u| is_low(u))
            {
                0x10000 + ((i32::from(unit) - 0xd800) << 10) + (i32::from(low) - 0xdc00)
            } else {
                i32::from(unit)
            };
            Ok(Value::int32(point))
        }
    }
}

fn slice<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    a: Option<Handle<'s>>,
    b: Option<Handle<'s>>,
) -> JsResult<Value> {
    ctx.check_interrupt()?;
    let n = ctx.heap().string(string_id(ctx.get(s))).len() as usize;
    let (start, end) = if op == 24 {
        let start = match a {
            None => 0,
            Some(a) => match ctx.get(a).decode() {
                ValueKind::Int32(v) => {
                    if v < 0 {
                        (n as i64 + i64::from(v)).max(0) as usize
                    } else {
                        (v as usize).min(n)
                    }
                }
                ValueKind::Undefined => 0,
                _ => relative(to_integer_or_infinity(ctx, a)?, n),
            },
        };
        let end = match b {
            None => n,
            Some(b) => match ctx.get(b).decode() {
                ValueKind::Undefined => n,
                ValueKind::Int32(v) => {
                    if v < 0 {
                        (n as i64 + i64::from(v)).max(0) as usize
                    } else {
                        (v as usize).min(n)
                    }
                }
                _ => relative(to_integer_or_infinity(ctx, b)?, n),
            },
        };
        (start, end.max(start))
    } else {
        let start = a.map_or(Ok(0.0), |a| to_integer_or_infinity(ctx, a))?;
        let end = match b {
            Some(b) if ctx.get(b) != Value::UNDEFINED => to_integer_or_infinity(ctx, b)?,
            _ => n as f64,
        };
        match op {
            27 => {
                let (start, end) = (bounded(start, n), bounded(end, n));
                (start.min(end), start.max(end))
            }
            _ => {
                let start = relative(start, n);
                (start, start + bounded(end, n - start))
            }
        }
    };
    let result = ctx.string_slice(s, start, end)?;
    Ok(ctx.get(result))
}

fn find<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    source: Handle<'s>,
    search: Handle<'s>,
    pos: Handle<'s>,
) -> JsResult<Value> {
    let hay = ctx.get(source);
    let hay_id = string_id(hay);
    let len = ctx.heap().string(hay_id).len() as usize;
    let poll = ctx.runtime.vm.interrupt_handler.is_some();
    let input = if poll { Some(units(ctx, hay)?) } else { None };
    if matches!(op, 9 | 10 | 26) && regexp::is_regexp(ctx, search)? {
        return Err(ctx.throw_type_error("expected a string, not a RegExp"));
    }
    let needle = to_string(ctx, search)?;
    let needle = ctx.get(needle);
    let needle_id = string_id(needle);
    let needle_units = if poll {
        Some(units(ctx, needle)?)
    } else {
        None
    };
    let pos = if op == 13 {
        let pos = to_number(ctx, pos)?;
        if pos.is_nan() {
            len as f64
        } else {
            pos.trunc()
        }
    } else if op == 9 && ctx.get(pos) == Value::UNDEFINED {
        len as f64
    } else {
        to_integer_or_infinity(ctx, pos)?
    };
    let pos = bounded(pos, len);
    let heap = ctx.heap();
    if !poll
        && let (Some(input), Some(needle)) = (
            heap.string(hay_id).as_latin1(),
            heap.string(needle_id).as_latin1(),
        )
    {
        return Ok(match op {
            9 => Value::bool(input[..pos].ends_with(needle)),
            26 => Value::bool(input[pos..].starts_with(needle)),
            _ => {
                let found = match (op, needle) {
                    (_, []) => Some(pos),
                    (13, [byte]) => input[..pos.saturating_add(1).min(len)]
                        .iter()
                        .rposition(|x| x == byte),
                    (13, _) => input[..pos.saturating_add(needle.len()).min(len)]
                        .windows(needle.len())
                        .rposition(|w| w == needle),
                    (_, [byte]) => input[pos..].iter().position(|x| x == byte).map(|i| i + pos),
                    _ => input[pos..]
                        .windows(needle.len())
                        .position(|w| w == needle)
                        .map(|i| i + pos),
                };
                if op == 10 {
                    Value::bool(found.is_some())
                } else {
                    Value::int32(found.map_or(-1, |i| i as i32))
                }
            }
        });
    }
    let input = input.map_or_else(|| units(ctx, hay), Ok)?;
    let needle = needle_units.map_or_else(|| units(ctx, needle), Ok)?;
    match op {
        9 => Ok(Value::bool(input[..pos].ends_with(&needle))),
        26 => Ok(Value::bool(input[pos..].starts_with(&needle))),
        13 => {
            if needle.is_empty() {
                return Ok(Value::int32(pos as i32));
            }
            if needle.len() > input.len() {
                return Ok(Value::int32(-1));
            }
            for i in (0..=pos.min(input.len() - needle.len())).rev() {
                if i & 1023 == 0 {
                    ctx.check_interrupt()?;
                }
                if input[i..].starts_with(&needle) {
                    return Ok(Value::int32(i as i32));
                }
            }
            Ok(Value::int32(-1))
        }
        _ => {
            let found = index_of(ctx, &input, &needle, pos)?;
            Ok(if op == 10 {
                Value::bool(found.is_some())
            } else {
                Value::int32(found.map_or(-1, |i| i as i32))
            })
        }
    }
}

pub(super) fn index_of(
    ctx: &mut ExecCtx<'_, '_>,
    input: &[u16],
    needle: &[u16],
    start: usize,
) -> JsResult<Option<usize>> {
    let Some(end) = input.len().checked_sub(needle.len()) else {
        return Ok(None);
    };
    for i in start..=end {
        if (i - start) & 1023 == 0 {
            ctx.check_interrupt()?;
        }
        if input[i..].starts_with(needle) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn repeat<'s>(ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>, n: Handle<'s>) -> JsResult<Value> {
    ctx.check_interrupt()?;
    let n = to_integer_or_infinity(ctx, n)?;
    if n < 0.0 || !n.is_finite() {
        return Err(ctx.throw_range_error("invalid repeat count"));
    }
    let id = string_id(ctx.get(s));
    let input_len = ctx.heap().string(id).len();
    if input_len == 0 || n == 1.0 {
        return Ok(ctx.get(s));
    }
    if n == 0.0 {
        return ctx.new_latin1(&[]).map(|s| ctx.get(s));
    }
    let len = n * f64::from(input_len);
    if len > MAX_STRING_LEN as f64 {
        return Err(string_error(ctx));
    }
    let len = len as usize;
    if ctx.runtime.vm.interrupt_handler.is_none() && !ctx.heap().string(id).is_wide() {
        ctx.flatten_string(s)?;
        let result = ctx.make_string(|heap| {
            let bytes = heap.string(id).as_latin1().unwrap();
            if let [byte] = bytes {
                JsString::from_latin1_iter(&heap.ledger, len, std::iter::repeat_n(*byte, len))
            } else {
                JsString::from_latin1_iter(
                    &heap.ledger,
                    len,
                    bytes.iter().copied().cycle().take(len),
                )
            }
        })?;
        return Ok(ctx.get(result));
    }
    let input = units(ctx, ctx.get(s))?;
    let mut out = Builder::new();
    out.reserve(ctx, len, true)?;
    append_repeated(ctx, &mut out, &input, len)?;
    out.value(ctx)
}

fn append_repeated(
    ctx: &mut ExecCtx<'_, '_>,
    out: &mut Builder,
    input: &[u16],
    len: usize,
) -> JsResult<()> {
    for i in 0..len {
        if i & 4095 == 0 {
            ctx.check_interrupt()?;
        }
        out.0.push_reserved(input[i % input.len()]);
    }
    Ok(())
}

fn pad<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    input: &[u16],
    n: Handle<'s>,
    fill: Handle<'s>,
    start: bool,
) -> JsResult<Value> {
    let n = to_length(ctx, n)?;
    if n <= input.len() as u64 {
        return Ok(ctx.get(s));
    }
    let filler;
    let fill = if ctx.get(fill) == Value::UNDEFINED {
        &[32][..]
    } else {
        filler = text(ctx, fill)?;
        &filler
    };
    if fill.is_empty() {
        return Ok(ctx.get(s));
    }
    if n > MAX_STRING_LEN as u64 {
        return Err(string_error(ctx));
    }
    let n = n as usize;
    let mut out = Builder::new();
    out.reserve(ctx, n, true)?;
    if !start {
        out.extend(ctx, input)?;
    }
    append_repeated(ctx, &mut out, fill, n - input.len())?;
    if start {
        out.extend(ctx, input)?;
    }
    out.value(ctx)
}

fn trim<'s>(op: u16, ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>) -> JsResult<Value> {
    ctx.check_interrupt()?;
    ctx.flatten_string(s)?;
    let id = string_id(ctx.get(s));
    let (mut start, mut end) = (0, ctx.heap().string(id).len() as usize);
    let unit = |ctx: &ExecCtx<'_, '_>, i: usize| {
        JsString::unit_at(&ctx.heap().strings, id, i as u32).unwrap()
    };
    if op != 35 {
        while start < end && space(unit(ctx, start)) {
            if start & 2047 == 0 {
                ctx.check_interrupt()?;
            }
            start += 1;
        }
    }
    if op != 36 {
        while start < end && space(unit(ctx, end - 1)) {
            if end & 2047 == 0 {
                ctx.check_interrupt()?;
            }
            end -= 1;
        }
    }
    let s = ctx.string_slice(s, start, end)?;
    Ok(ctx.get(s))
}

fn well_formed<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    input: &[u16],
    repair: bool,
) -> JsResult<Value> {
    let mut out = None;
    let mut i = 0;
    while i < input.len() {
        if i & 2047 < 2 {
            ctx.check_interrupt()?;
        }
        let (cp, next) = code_point(input, i);
        if (0xd800..=0xdfff).contains(&cp) {
            if !repair {
                return Ok(Value::bool(false));
            }
            if out.is_none() {
                let mut b = Builder::new();
                b.reserve(ctx, input.len(), true)?;
                b.extend(ctx, &input[..i])?;
                out = Some(b);
            }
            out.as_mut().unwrap().unit(ctx, 0xfffd)?;
        } else if let Some(b) = &mut out {
            b.code_point(ctx, cp)?;
        }
        i = next;
    }
    match out {
        Some(out) => out.value(ctx),
        None if repair => Ok(ctx.get(s)),
        None => Ok(Value::bool(true)),
    }
}

fn case<'s>(ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>, lower: bool) -> JsResult<Value> {
    let id = string_id(ctx.get(s));
    if ctx.runtime.vm.interrupt_handler.is_none() && !ctx.heap().string(id).is_wide() {
        ctx.flatten_string(s)?;
        let string = ctx.heap().string(id);
        if string.is_ascii() {
            let bytes = string.as_latin1().unwrap();
            let change = if lower {
                u8::is_ascii_uppercase
            } else {
                u8::is_ascii_lowercase
            };
            if !bytes.iter().any(change) {
                return Ok(ctx.get(s));
            }
            let convert = if lower {
                u8::to_ascii_lowercase
            } else {
                u8::to_ascii_uppercase
            };
            if let [byte] = bytes {
                return ctx.new_latin1(&[convert(byte)]).map(|s| ctx.get(s));
            }
            let result = ctx.make_string(|heap| {
                let bytes = heap.string(id).as_latin1().unwrap();
                JsString::from_latin1_iter(&heap.ledger, bytes.len(), bytes.iter().map(convert))
            })?;
            return Ok(ctx.get(result));
        }
    }
    let input = units(ctx, ctx.get(s))?;
    let mut out = Builder::new();
    out.reserve(ctx, input.len(), true)?;
    let conv = if lower {
        CaseConv::Lower
    } else {
        CaseConv::Upper
    };
    let mut prev_cased = false;
    let mut i = 0;
    while i < input.len() {
        if i & 2047 < 2 {
            ctx.check_interrupt()?;
        }
        let (cp, next) = code_point(&input, i);
        i = next;
        if lower && cp == 0x03a3 && prev_cased {
            let mut j = i;
            let mut followed = false;
            while j < input.len() {
                if (j - i) & 2047 < 2 {
                    ctx.check_interrupt()?;
                }
                let (cp, next) = code_point(&input, j);
                if !is_case_ignorable(cp) {
                    followed = is_cased(cp);
                    break;
                }
                j = next;
            }
            out.unit(ctx, if followed { 0x03c3 } else { 0x03c2 })?;
        } else {
            let (case, n) = case_conv(cp, conv);
            for &cp in &case[..n] {
                out.code_point(ctx, cp)?;
            }
        }
        if lower && !is_case_ignorable(cp) {
            prev_cased = is_cased(cp);
        }
    }
    out.value(ctx)
}

fn points(ctx: &mut ExecCtx<'_, '_>, input: &[u16]) -> JsResult<TrackedVec<u32>> {
    let mut points = TrackedVec::new();
    ctx.allocate(|rt| points.try_reserve_exact(&rt.heap.ledger, input.len()))?;
    let mut i = 0;
    while i < input.len() {
        if i & 2047 < 2 {
            ctx.check_interrupt()?;
        }
        let (cp, end) = code_point(input, i);
        points.push_reserved(cp);
        i = end;
    }
    Ok(points)
}

fn normalized(
    ctx: &mut ExecCtx<'_, '_>,
    input: &[u16],
    form: UnicodeNormalization,
) -> JsResult<(Vec<u32>, Charge)> {
    let points = points(ctx, input)?;
    // U+FDFA has the longest compatibility decomposition (18 code points).
    let per_point = match form {
        UnicodeNormalization::Nfc | UnicodeNormalization::Nfd => 4,
        _ => 18,
    };
    let max = points
        .iter()
        .map(|&c| {
            if c < 128 || (0xd800..=0xdfff).contains(&c) {
                1
            } else {
                per_point
            }
        })
        .sum::<usize>();
    let bytes = max
        .checked_mul(2 * size_of::<u32>())
        .ok_or_else(|| ctx.throw_oom())?;
    let charge = Charge::new(ctx, bytes.max(4 * size_of::<u32>()))?;
    let result = normalize(&points, form).map_err(|_| ctx.throw_oom())?;
    ctx.check_interrupt()?;
    Ok((result, charge))
}

fn normalize_string<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    input: &[u16],
    form: Handle<'s>,
) -> JsResult<Value> {
    use UnicodeNormalization::*;
    let form = if ctx.get(form) == Value::UNDEFINED {
        Nfc
    } else {
        let form = text(ctx, form)?;
        match &*form {
            [78, 70, 67] => Nfc,
            [78, 70, 68] => Nfd,
            [78, 70, 75, 67] => Nfkc,
            [78, 70, 75, 68] => Nfkd,
            _ => return Err(ctx.throw_range_error("invalid normalization form")),
        }
    };
    if input.iter().all(|&u| u < 128) {
        return Ok(ctx.get(s));
    }
    let (points, _charge) = normalized(ctx, input, form)?;
    let mut out = Builder::new();
    for (i, cp) in points.into_iter().enumerate() {
        if i & 2047 == 0 {
            ctx.check_interrupt()?;
        }
        out.code_point(ctx, cp)?;
    }
    out.value(ctx)
}

fn compare<'s>(ctx: &mut ExecCtx<'_, 's>, a: &[u16], b: Handle<'s>) -> JsResult<Value> {
    let b = text(ctx, b)?;
    let (a, _charge_a) = normalized(ctx, a, UnicodeNormalization::Nfc)?;
    let (b, _charge_b) = normalized(ctx, &b, UnicodeNormalization::Nfc)?;
    Ok(Value::int32(a.cmp(&b) as i32))
}

fn html<'s>(op: u16, ctx: &mut ExecCtx<'_, 's>, input: &[u16], arg: Handle<'s>) -> JsResult<Value> {
    let (tag, attr): (&[u8], &[u8]) = match op {
        39 => (b"a", b"name"),
        40 => (b"big", b""),
        41 => (b"blink", b""),
        42 => (b"b", b""),
        43 => (b"tt", b""),
        44 => (b"font", b"color"),
        45 => (b"font", b"size"),
        46 => (b"i", b""),
        47 => (b"a", b"href"),
        48 => (b"small", b""),
        49 => (b"strike", b""),
        50 => (b"sub", b""),
        _ => (b"sup", b""),
    };
    let mut out = Builder::new();
    out.ascii(ctx, b"<")?;
    out.ascii(ctx, tag)?;
    if !attr.is_empty() {
        let attr_val = text(ctx, arg)?;
        out.ascii(ctx, b" ")?;
        out.ascii(ctx, attr)?;
        out.ascii(ctx, b"=\"")?;
        for (i, &u) in attr_val.iter().enumerate() {
            if i & 2047 == 0 {
                ctx.check_interrupt()?;
            }
            if u == 34 {
                out.ascii(ctx, b"&quot;")?;
            } else {
                out.unit(ctx, u)?;
            }
        }
        out.ascii(ctx, b"\"")?;
    }
    out.ascii(ctx, b">")?;
    out.extend(ctx, input)?;
    out.ascii(ctx, b"</")?;
    out.ascii(ctx, tag)?;
    out.ascii(ctx, b">")?;
    out.value(ctx)
}

fn iterator<'s>(ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>) -> JsResult<Value> {
    ctx.flatten_string(s)?;
    let string = ctx.get(s);
    let iterator = ctx.make_object(
        Some(ctx.realm().string_iterator_proto),
        ObjectFlags::EXTENSIBLE,
        |ledger| {
            Ok(ObjectData::StringIterator(TrackedBox::new(
                ledger,
                StringIteratorData { string, index: 0 },
            )?))
        },
    )?;
    Ok(ctx.get(iterator))
}

fn next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    let id = ctx.object_id(this)?;
    let ObjectData::StringIterator(data) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a String Iterator"));
    };
    let mut len = 0;
    let mut result = [0; 2];
    if let Some(s) = as_string(data.string) {
        let i = data.index;
        let s = ctx.heap().string(s);
        if let Some(byte) = s.as_latin1().and_then(|data| data.get(i)) {
            len = 1;
            result[0] = (*byte).into();
        } else if let Some(data) = s.as_utf16().filter(|data| i < data.len()) {
            len = advance(data, i, true) - i;
            result[..len].copy_from_slice(&data[i..i + len]);
        }
    }
    let ObjectData::StringIterator(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
    else {
        unreachable!()
    };
    if len == 0 {
        data.string = Value::UNDEFINED;
    } else {
        data.index += len;
    }
    let value = if len == 0 {
        ctx.root(Value::UNDEFINED)?
    } else {
        ctx.new_utf16(&result[..len])?
    };
    iterator_result(ctx, value, len == 0)
}

fn pattern_method<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    receiver: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let pattern = arg(ctx, args, 0)?;
    let extra = arg(ctx, args, 1)?;
    let symbol = match op {
        15 => WellKnown::Match,
        16 => WellKnown::MatchAll,
        21 | 22 => WellKnown::Replace,
        23 => WellKnown::Search,
        _ => WellKnown::Split,
    };
    let key = sym(ctx, symbol);
    if ctx.get(pattern).as_object().is_some() {
        if matches!(op, 16 | 22) {
            regexp::check_global(ctx, pattern)?;
        }
        if let Some(method) = get_method(ctx, pattern, key)? {
            return if matches!(op, 21 | 22 | 25) {
                call_with(ctx, method, pattern, &[receiver, extra])
            } else {
                call_with(ctx, method, pattern, &[receiver])
            };
        }
    }
    let string = to_string(ctx, receiver)?;
    match op {
        21 | 22 => replace_string(ctx, string, pattern, extra, op == 22),
        25 => split_string(ctx, string, pattern, extra),
        _ => {
            let flags = if op == 16 {
                ctx.new_string("g")?
            } else {
                ctx.root(Value::UNDEFINED)?
            };
            let re = regexp_literal(ctx, pattern, flags)?;
            let re = ctx.root(re)?;
            let method = get(ctx, re, key)?;
            call_with(ctx, method, re, &[string])
        }
    }
}

fn split_string<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    separator: Handle<'s>,
    limit: Handle<'s>,
) -> JsResult<Value> {
    let result = ctx.new_array()?;
    let limit = if ctx.get(limit) == Value::UNDEFINED {
        u32::MAX
    } else {
        to_uint32(ctx, limit)?
    } as usize;
    let undefined = ctx.get(separator) == Value::UNDEFINED;
    let sep = text(ctx, separator)?;
    let result = ctx.get(result);
    if limit == 0 {
        return Ok(result);
    }
    if undefined {
        let result = ctx.root(result)?;
        put_index(ctx, result, 0, s)?;
        return Ok(ctx.get(result));
    }
    let input = units(ctx, ctx.get(s))?;
    let mut n = 0;
    let mut start = 0;
    let mut push = |ctx: &mut ExecCtx<'_, '_>, part: &[u16]| -> JsResult<()> {
        ctx.with_scope(|ctx| {
            let result = ctx.root(result)?;
            let item = ctx.new_utf16(part)?;
            put_index(ctx, result, n, item)
        })?;
        n += 1;
        Ok(())
    };
    if sep.is_empty() {
        for i in 0..limit.min(input.len()) {
            if i & 1023 == 0 {
                ctx.check_interrupt()?;
            }
            push(ctx, &input[i..i + 1])?;
        }
        return Ok(result);
    }
    for _ in 0..limit {
        if let Some(next) = index_of(ctx, &input, &sep, start)? {
            push(ctx, &input[start..next])?;
            start = next + sep.len();
        } else {
            push(ctx, &input[start..])?;
            break;
        }
    }
    Ok(result)
}

fn replace_string<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    search: Handle<'s>,
    replacement: Handle<'s>,
    all: bool,
) -> JsResult<Value> {
    let needle = text(ctx, search)?;
    let replacement = ctx.get(replacement);
    let template = if callable(ctx, replacement) {
        None
    } else {
        let replacement = ctx.root(replacement)?;
        Some(text(ctx, replacement)?)
    };
    let string = ctx.get(s);
    let input = units(ctx, string)?;
    let mut out = Builder::new();
    let mut end = 0;
    let mut i = 0;
    let mut found = false;
    while let Some(start) = index_of(ctx, &input, &needle, i)? {
        found = true;
        out.extend(ctx, &input[end..start])?;
        if let Some(template) = &template {
            Substitution {
                input: &input,
                matched: &needle,
                position: start,
                captures: &[],
                groups: None,
            }
            .append(ctx, &mut out, template)?;
        } else {
            ctx.with_scope(|ctx| {
                let matched = ctx.new_utf16(&needle)?;
                let index = ctx.root(Value::int32(start as i32))?;
                let s = ctx.root(string)?;
                let f = ctx.root(replacement)?;
                let this = ctx.root(Value::UNDEFINED)?;
                let value = call_with(ctx, f, this, &[matched, index, s])?;
                let value = ctx.root(value)?;
                let value = to_string(ctx, value)?;
                out.string(ctx, ctx.get(value))
            })?;
        }
        end = start + needle.len();
        if !all {
            break;
        }
        i = start + needle.len().max(1);
    }
    if !found {
        return Ok(string);
    }
    out.extend(ctx, &input[end..])?;
    out.value(ctx)
}

#[cfg(test)]
mod tests {
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

    #[test]
    fn string_paths_keep_utf16_indices_and_coercion_order() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let result = engine
            .eval_to_string(
                r#"
            const latin = 'x'.repeat(65536);
            const lead = latin + '\uD83D';
            const rope = lead + '\uDE00z';
            const calls = [];
            const part = rope.slice({ valueOf() { calls.push('start'); return -3; } },
                                    { valueOf() { calls.push('end'); return -1; } });
            const search = latin + 'éé';
            [part, rope.codePointAt(65536), rope.charCodeAt(65537), rope.at(-1),
             latin.substr(-2), latin.substring(2, 0), search.indexOf('é'),
             search.lastIndexOf('é', NaN), search.startsWith('é', 65536),
             search.endsWith('é', 65537), !search.includes('a'),
             '\u00a0é\u3000'.trim(), 'Az'.toUpperCase(), 'Σ'.toLowerCase(),
             String(-2147483648), String(-0), calls.join('-')].join('|');
        "#,
                EvalOptions::default(),
            )
            .unwrap();
        assert_eq!(
            result,
            "😀|128512|56832|z|xx|xx|65536|65537|true|true|true|é|AZ|σ|-2147483648|0|start-end"
        );
    }
}
