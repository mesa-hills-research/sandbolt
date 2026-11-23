use qjs_unicode::{
    CaseConv, UnicodeNormalization, case_conv, is_case_ignorable, is_cased, normalize,
};

use crate::ledger::TrackedBox;
use crate::object::{ObjectData, ObjectFlags, StringIteratorData, StringObjectData};
use crate::ops::convert::{space, to_integer_or_infinity, to_length, to_number, to_uint32};
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
    require_object_coercible(ctx, ctx.get(this))?;
    if matches!(op, 15 | 16 | 21 | 22 | 23 | 25) {
        return pattern_method(op, ctx, this, args);
    }
    let s = to_string(ctx, this)?;
    if op == 52 {
        return iterator(ctx, s);
    }
    if op == 8 {
        return concat(ctx, s, args);
    }
    let input = units(ctx, ctx.get(s))?;
    let a = arg(ctx, args, 0)?;
    let b = arg(ctx, args, 1)?;
    match op {
        4..=7 => {
            let i = to_integer_or_infinity(ctx, a)?;
            let i = if op == 4 && i < 0.0 {
                input.len() as f64 + i
            } else {
                i
            };
            if i < 0.0 || i >= input.len() as f64 {
                return match op {
                    5 => ctx.new_string("").map(|s| ctx.get(s)),
                    6 => Ok(Value::number(f64::NAN)),
                    _ => Ok(Value::UNDEFINED),
                };
            }
            let i = i as usize;
            match op {
                4 | 5 => ctx.new_utf16(&input[i..=i]).map(|s| ctx.get(s)),
                6 => Ok(Value::int32(i32::from(input[i]))),
                _ => Ok(Value::int32(code_point(&input, i).0 as i32)),
            }
        }
        9..=11 | 13 | 26 => find(op, ctx, &input, a, b),
        12 | 33 => well_formed(ctx, s, &input, op == 33),
        14 => compare(ctx, &input, a),
        17 => normalize_string(ctx, s, &input, a),
        18 | 19 => pad(ctx, s, &input, a, b, op == 19),
        20 => repeat(ctx, s, &input, a),
        24 | 27 | 38 => slice(op, ctx, &input, a, b),
        28..=30 | 32 => case(ctx, &input, matches!(op, 28 | 30)),
        34..=36 => trim(op, ctx, &input),
        39..=51 => html(op, ctx, &input, a),
        _ => Err(ctx.throw_type_error("unknown String builtin")),
    }
}

fn from_codes<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, points: bool) -> JsResult<Value> {
    let mut out = Builder::new();
    out.reserve(ctx, args.len(), true)?;
    for i in 0..args.len() {
        if i & 1023 == 0 {
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
            let c = to_uint32(ctx, arg)?;
            out.unit(ctx, c as u16)?;
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

fn concat<'s>(ctx: &mut ExecCtx<'_, 's>, s: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    if args.is_empty() {
        return Ok(ctx.get(s));
    }
    let mut out = Builder::new();
    out.string(ctx, ctx.get(s))?;
    for i in 0..args.len() {
        let arg = ctx.get(args.get(i).unwrap());
        ctx.with_scope(|ctx| {
            let arg = ctx.root(arg)?;
            let arg = to_string(ctx, arg)?;
            out.string(ctx, ctx.get(arg))
        })?;
    }
    out.value(ctx)
}

fn bounded(pos: f64, len: usize) -> usize {
    pos.clamp(0.0, len as f64) as usize
}

fn relative(pos: f64, len: usize) -> usize {
    bounded(if pos < 0.0 { pos + len as f64 } else { pos }, len)
}

fn slice<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    input: &[u16],
    a: Handle<'s>,
    b: Handle<'s>,
) -> JsResult<Value> {
    let start = to_integer_or_infinity(ctx, a)?;
    let n = input.len();
    let end = if ctx.get(b) == Value::UNDEFINED {
        n as f64
    } else {
        to_integer_or_infinity(ctx, b)?
    };
    let (start, end) = match op {
        24 => {
            let start = relative(start, n);
            (start, relative(end, n).max(start))
        }
        27 => {
            let (start, end) = (bounded(start, n), bounded(end, n));
            (start.min(end), start.max(end))
        }
        _ => {
            let start = relative(start, n);
            (start, start + bounded(end, n - start))
        }
    };
    let result = ctx.new_utf16(&input[start..end])?;
    Ok(ctx.get(result))
}

fn find<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    input: &[u16],
    search: Handle<'s>,
    pos: Handle<'s>,
) -> JsResult<Value> {
    if matches!(op, 9 | 10 | 26) && regexp::is_regexp(ctx, search)? {
        return Err(ctx.throw_type_error("expected a string, not a RegExp"));
    }
    let needle = text(ctx, search)?;
    let pos = if op == 13 {
        let pos = to_number(ctx, pos)?;
        if pos.is_nan() {
            input.len() as f64
        } else {
            pos.trunc()
        }
    } else if op == 9 && ctx.get(pos) == Value::UNDEFINED {
        input.len() as f64
    } else {
        to_integer_or_infinity(ctx, pos)?
    };
    let pos = bounded(pos, input.len());
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
            let found = index_of(ctx, input, &needle, pos)?;
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

fn repeat<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    s: Handle<'s>,
    input: &[u16],
    n: Handle<'s>,
) -> JsResult<Value> {
    let n = to_integer_or_infinity(ctx, n)?;
    if n < 0.0 || !n.is_finite() {
        return Err(ctx.throw_range_error("invalid repeat count"));
    }
    if input.is_empty() || n == 1.0 {
        return Ok(ctx.get(s));
    }
    let len = n * input.len() as f64;
    if len > MAX_STRING_LEN as f64 {
        return Err(string_error(ctx));
    }
    let mut out = Builder::new();
    out.reserve(ctx, len as usize, true)?;
    append_repeated(ctx, &mut out, input, len as usize)?;
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

fn trim(op: u16, ctx: &mut ExecCtx<'_, '_>, input: &[u16]) -> JsResult<Value> {
    let (mut start, mut end) = (0, input.len());
    if op != 35 {
        while start < end && space(input[start]) {
            if start & 2047 == 0 {
                ctx.check_interrupt()?;
            }
            start += 1;
        }
    }
    if op != 36 {
        while start < end && space(input[end - 1]) {
            if end & 2047 == 0 {
                ctx.check_interrupt()?;
            }
            end -= 1;
        }
    }
    let s = ctx.new_utf16(&input[start..end])?;
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

fn case(ctx: &mut ExecCtx<'_, '_>, input: &[u16], lower: bool) -> JsResult<Value> {
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
        let (cp, next) = code_point(input, i);
        i = next;
        if lower && cp == 0x03a3 && prev_cased {
            let mut j = i;
            let mut followed = false;
            while j < input.len() {
                if (j - i) & 2047 < 2 {
                    ctx.check_interrupt()?;
                }
                let (cp, next) = code_point(input, j);
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
        match ctx.heap().string(s) {
            JsString::Latin1 { data, .. } if i < data.len() => {
                len = 1;
                result[0] = data[i].into();
            }
            JsString::Utf16(data) if i < data.len() => {
                len = advance(data, i, true) - i;
                result[..len].copy_from_slice(&data[i..i + len]);
            }
            _ => (),
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
    if !nullish(ctx.get(pattern)) {
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
