mod bigint;
mod date;
mod math;
#[cfg(test)]
mod tests;

use qjs_dtoa::{AtodFlags, DtoaExp, DtoaFormat, DtoaOptions, atod};

use crate::atom::PropKey;
use crate::builtins::{NUM_DATE_BASE, configure_ctor, define_data, install_method, make_native_fn};
use crate::ledger::{HeapLedger, TrackedBox, TrackedVec};
use crate::object::{BooleanObjectData, NumberObjectData, ObjectData, ObjectFlags};
use crate::ops::convert::{
    PreferredType, to_boolean, to_int32, to_integer_or_infinity, to_number, to_primitive, to_string,
};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::{JsResult, ObjId, StrId, Value, ValueKind};

fn mutable() -> PropFlags {
    PropFlags::WRITABLE | PropFlags::CONFIGURABLE
}
const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

#[repr(u16)]
enum Numeric {
    IsNaN,
    IsFinite,
    ParseInt,
    ParseFloat,
    Number,
    NumberIsFinite,
    NumberIsInteger,
    NumberIsNaN,
    NumberIsSafeInteger,
    ToExponential,
    ToFixed,
    ToLocaleString,
    ToPrecision,
    ToString,
    NumberValueOf,
    Boolean,
    BooleanToString,
    BooleanValueOf,
}

impl Numeric {
    const fn id(self) -> NativeFnId {
        NativeFnId(NUM_DATE_BASE + self as u16)
    }
}

type Method = (&'static str, u32, NativeFnId);

fn install_methods<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    methods: &[Method],
) -> JsResult<()> {
    for &(name, length, id) in methods {
        let key = ctx.intern_str(name)?;
        install_method(ctx, target, key, id, name, length)?;
    }
    Ok(())
}

fn define<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    name: &str,
    value: Handle<'s>,
    flags: PropFlags,
) -> JsResult<()> {
    let key = ctx.intern_str(name)?;
    define_data(ctx, target, key, value, flags)
}

fn constants<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    entries: &[(&str, f64)],
) -> JsResult<()> {
    for &(name, value) in entries {
        let value = ctx.root(Value::number(value))?;
        define(ctx, target, name, value, PropFlags::default())?;
    }
    Ok(())
}

fn symbol_key(ctx: &ExecCtx<'_, '_>, symbol: WellKnown) -> PropKey {
    let heap = ctx.heap();
    heap.symbols
        .get(heap.symbol_registry.well_known(symbol))
        .unwrap()
        .key()
}

fn set_tag<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>, name: &str) -> JsResult<()> {
    let key = symbol_key(ctx, WellKnown::ToStringTag);
    let tag = ctx.new_string(name)?;
    define_data(ctx, target, key, tag, PropFlags::CONFIGURABLE)
}

fn constructor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    global: Handle<'s>,
    ctor: ObjId,
    id: NativeFnId,
    name: &str,
    length: u32,
) -> JsResult<Handle<'s>> {
    let function = configure_ctor(ctx, ctor, id, name, length)?;
    define(ctx, global, name, function, mutable())?;
    Ok(function)
}

fn set_payload(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    make: impl Fn(&HeapLedger) -> crate::ledger::AllocResult<ObjectData>,
) -> JsResult<()> {
    let payload = ctx.allocate(|rt| make(&rt.heap.ledger))?;
    ctx.runtime.heap.objects.get_mut(id).unwrap().data = payload;
    Ok(())
}

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    use Numeric::*;

    ctx.with_scope(|ctx| {
        let global = ctx.root(Value::object(ctx.realm().global_object))?;
        constants(
            ctx,
            global,
            &[("NaN", f64::NAN), ("Infinity", f64::INFINITY)],
        )?;
        let undefined = ctx.root(Value::UNDEFINED)?;
        define(ctx, global, "undefined", undefined, PropFlags::default())?;
        install_methods(
            ctx,
            global,
            &[("isNaN", 1, IsNaN.id()), ("isFinite", 1, IsFinite.id())],
        )?;

        let ctor = constructor(
            ctx,
            global,
            ctx.realm().number_ctor,
            Number.id(),
            "Number",
            1,
        )?;
        for (name, id, length) in [
            ("parseInt", ParseInt.id(), 2),
            ("parseFloat", ParseFloat.id(), 1),
        ] {
            let function = make_native_fn(ctx, id, name, length, false)?;
            define(ctx, global, name, function, mutable())?;
            define(ctx, ctor, name, function, mutable())?;
        }
        constants(
            ctx,
            ctor,
            &[
                ("EPSILON", f64::EPSILON),
                ("MAX_SAFE_INTEGER", MAX_SAFE),
                ("MIN_SAFE_INTEGER", -MAX_SAFE),
                ("MIN_VALUE", f64::from_bits(1)),
                ("MAX_VALUE", f64::MAX),
                ("NaN", f64::NAN),
                ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
                ("POSITIVE_INFINITY", f64::INFINITY),
            ],
        )?;
        install_methods(
            ctx,
            ctor,
            &[
                ("isFinite", 1, NumberIsFinite.id()),
                ("isInteger", 1, NumberIsInteger.id()),
                ("isNaN", 1, NumberIsNaN.id()),
                ("isSafeInteger", 1, NumberIsSafeInteger.id()),
            ],
        )?;
        let proto_id = ctx.realm().number_proto;
        let proto = ctx.root(Value::object(proto_id))?;
        number_payload(ctx, proto_id, Value::int32(0))?;
        install_methods(
            ctx,
            proto,
            &[
                ("toExponential", 1, ToExponential.id()),
                ("toFixed", 1, ToFixed.id()),
                ("toLocaleString", 0, ToLocaleString.id()),
                ("toPrecision", 1, ToPrecision.id()),
                ("toString", 1, ToString.id()),
                ("valueOf", 0, NumberValueOf.id()),
            ],
        )?;
        constructor(
            ctx,
            global,
            ctx.realm().boolean_ctor,
            Boolean.id(),
            "Boolean",
            1,
        )?;
        let proto_id = ctx.realm().boolean_proto;
        let proto = ctx.root(Value::object(proto_id))?;
        boolean_payload(ctx, proto_id, Value::bool(false))?;
        install_methods(
            ctx,
            proto,
            &[
                ("toString", 0, BooleanToString.id()),
                ("valueOf", 0, BooleanValueOf.id()),
            ],
        )
    })?;
    ctx.with_scope(math::install)?;
    ctx.with_scope(bigint::install)?;
    ctx.with_scope(date::install)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    use Numeric::*;
    let offset = id.0 - NUM_DATE_BASE;
    #[cfg(test)]
    if offset >= tests::FIXTURE_START {
        return tests::dispatch(offset, ctx, this);
    }
    match offset {
        math::START..=math::END => return math::dispatch(offset, ctx, args),
        bigint::START..=bigint::END => return bigint::dispatch(offset, ctx, this, args),
        date::START..=date::END => return date::dispatch(offset, ctx, this, args),
        _ => {}
    }
    let arg = argument(ctx, args, 0)?;
    match id {
        n if n == IsNaN.id() => Ok(Value::bool(to_number(ctx, arg)?.is_nan())),
        n if n == IsFinite.id() => Ok(Value::bool(to_number(ctx, arg)?.is_finite())),
        n if n == ParseInt.id() || n == ParseFloat.id() => {
            let string = to_string(ctx, arg)?;
            let radix = if n == ParseInt.id() {
                let arg = argument(ctx, args, 1)?;
                let radix = to_int32(ctx, arg)?;
                if radix != 0 && !(2..=36).contains(&radix) {
                    return Ok(Value::number(f64::NAN));
                }
                Some(radix as u32)
            } else {
                None
            };
            let text = ascii_text(ctx, string_id(ctx.get(string)), false)?;
            Ok(Value::number(parse(text_ref(&text), radix)))
        }
        n if n == Number.id() => {
            let value = if args.is_empty() {
                Value::int32(0)
            } else {
                let v = to_primitive(ctx, arg, PreferredType::Number)?;
                let v = ctx.root(v)?;
                Value::number(number(ctx, v)?)
            };
            if let Some(target) = ctx.new_target() {
                let object = ctx.ordinary_create_from_constructor(
                    target,
                    |realm| realm.number_proto,
                    ObjectFlags::EXTENSIBLE,
                    |heap| number_data(&heap.ledger, value),
                )?;
                Ok(ctx.get(object))
            } else {
                Ok(value)
            }
        }
        n if n == NumberIsFinite.id()
            || n == NumberIsNaN.id()
            || n == NumberIsInteger.id()
            || n == NumberIsSafeInteger.id() =>
        {
            Ok(Value::bool(ctx.get(arg).as_number().is_some_and(|x| {
                if n == NumberIsFinite.id() {
                    x.is_finite()
                } else if n == NumberIsNaN.id() {
                    x.is_nan()
                } else {
                    x.is_finite()
                        && x.trunc() == x
                        && (n != NumberIsSafeInteger.id() || x.abs() <= MAX_SAFE)
                }
            })))
        }
        n if n == Boolean.id() => {
            let value = Value::bool(to_boolean(ctx, arg));
            if let Some(target) = ctx.new_target() {
                let object = ctx.ordinary_create_from_constructor(
                    target,
                    |realm| realm.boolean_proto,
                    ObjectFlags::EXTENSIBLE,
                    |heap| boolean_data(&heap.ledger, value),
                )?;
                Ok(ctx.get(object))
            } else {
                Ok(value)
            }
        }
        n if n == BooleanValueOf.id() || n == BooleanToString.id() => {
            let value = receiver_value(ctx, this, Wrapper::Boolean)?;
            if n == BooleanValueOf.id() {
                Ok(value)
            } else {
                let string = ctx.new_string(if value == Value::bool(true) {
                    "true"
                } else {
                    "false"
                })?;
                Ok(ctx.get(string))
            }
        }
        n if (ToExponential.id().0..=NumberValueOf.id().0).contains(&n.0) => {
            let value = receiver_value(ctx, this, Wrapper::Number)?;
            if n == NumberValueOf.id() {
                return Ok(value);
            }
            format_number(ctx, n, value.as_number().unwrap(), arg)
        }
        _ => Err(ctx.throw_type_error("unknown numeric builtin")),
    }
}

fn argument<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, n: usize) -> JsResult<Handle<'s>> {
    match args.get(n) {
        Some(value) => Ok(value),
        None => ctx.root(Value::UNDEFINED),
    }
}

fn string_id(value: Value) -> StrId {
    let ValueKind::String(id) = value.decode() else {
        unreachable!()
    };
    id
}

// These grammars use ASCII after normalizing ECMAScript whitespace. Keep a
// non-ASCII unit as a delimiter that cannot be read as a digit in any radix.
fn ascii_text(ctx: &mut ExecCtx<'_, '_>, id: StrId, date: bool) -> JsResult<TrackedVec<u8>> {
    ctx.allocate(|rt| {
        let heap = &rt.heap;
        let mut out = TrackedVec::new();
        out.try_reserve_exact(&heap.ledger, heap.string(id).len() as usize)?;
        for unit in heap.string_units(id) {
            out.push_reserved(match unit {
                u if qjs_unicode::is_space(u.into()) => b' ',
                0x2212 if date => b'-',
                0..=127 => unit as u8,
                _ => b'?',
            });
        }
        Ok(out)
    })
}

fn text_ref(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn parse(text: &str, radix: Option<u32>) -> f64 {
    let text = text.trim_start_matches(' ');
    if let Some(radix) = radix {
        return atod(
            text,
            radix,
            AtodFlags {
                int_only: true,
                ..AtodFlags::default()
            },
        )
        .0;
    }
    let (n, end) = atod(text, 10, AtodFlags::default());
    if n.is_nan()
        && let Some(e) = text[..end].find(['e', 'E'])
    {
        // An incomplete exponent is outside the longest decimal prefix.
        atod(&text[..e], 10, AtodFlags::default()).0
    } else {
        n
    }
}

fn number<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<f64> {
    match ctx.get(value).decode() {
        ValueKind::ShortBigInt(n) => Ok(n as f64),
        ValueKind::BigInt(id) => bigint::to_number(ctx, id),
        ValueKind::String(id) => {
            let bytes = ascii_text(ctx, id, false)?;
            let text = text_ref(&bytes).trim_matches(' ');
            if text.is_empty() {
                return Ok(0.0);
            }
            let (radix, flags) = if matches!(
                text.as_bytes(),
                [b'0', b'x' | b'X' | b'o' | b'O' | b'b' | b'B', ..]
            ) {
                (
                    0,
                    AtodFlags {
                        accept_bin_oct: true,
                        int_only: true,
                        ..AtodFlags::default()
                    },
                )
            } else {
                (10, AtodFlags::default())
            };
            let (n, end) = atod(text, radix, flags);
            Ok(if end == text.len() { n } else { f64::NAN })
        }
        _ => to_number(ctx, value),
    }
}

fn number_payload(ctx: &mut ExecCtx<'_, '_>, id: ObjId, value: Value) -> JsResult<()> {
    set_payload(ctx, id, |ledger| number_data(ledger, value))
}

fn boolean_payload(ctx: &mut ExecCtx<'_, '_>, id: ObjId, value: Value) -> JsResult<()> {
    set_payload(ctx, id, |ledger| boolean_data(ledger, value))
}

fn number_data(ledger: &HeapLedger, value: Value) -> crate::ledger::AllocResult<ObjectData> {
    Ok(ObjectData::NumberObject(TrackedBox::new(
        ledger,
        NumberObjectData { value },
    )?))
}

fn boolean_data(ledger: &HeapLedger, value: Value) -> crate::ledger::AllocResult<ObjectData> {
    Ok(ObjectData::BooleanObject(TrackedBox::new(
        ledger,
        BooleanObjectData { value },
    )?))
}

#[derive(Clone, Copy)]
enum Wrapper {
    Number,
    Boolean,
    BigInt,
}

fn receiver_value<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    kind: Wrapper,
) -> JsResult<Value> {
    let value = ctx.get(this);
    let value = if let Some(id) = value.as_object() {
        match (&ctx.heap().object(id).data, kind) {
            (ObjectData::NumberObject(data), Wrapper::Number) => data.value,
            (ObjectData::BooleanObject(data), Wrapper::Boolean) => data.value,
            (ObjectData::BigIntObject(data), Wrapper::BigInt) => data.value,
            _ => Value::UNDEFINED,
        }
    } else {
        value
    };
    let valid = match kind {
        Wrapper::Number => value.as_number().is_some(),
        Wrapper::Boolean => matches!(value.decode(), ValueKind::Bool(_)),
        Wrapper::BigInt => matches!(
            value.decode(),
            ValueKind::ShortBigInt(_) | ValueKind::BigInt(_)
        ),
    };
    if valid {
        Ok(value)
    } else {
        Err(ctx.throw_type_error(match kind {
            Wrapper::Number => "not a Number",
            Wrapper::Boolean => "not a Boolean",
            Wrapper::BigInt => "not a BigInt",
        }))
    }
}

fn radix<'s>(ctx: &mut ExecCtx<'_, 's>, arg: Handle<'s>) -> JsResult<u32> {
    if ctx.get(arg) == Value::UNDEFINED {
        return Ok(10);
    }
    let radix = to_integer_or_infinity(ctx, arg)?;
    if (2.0..=36.0).contains(&radix) {
        Ok(radix as u32)
    } else {
        Err(ctx.throw_range_error("radix must be between 2 and 36"))
    }
}

fn format_number<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    n: f64,
    arg: Handle<'s>,
) -> JsResult<Value> {
    use Numeric::*;
    let default = DtoaOptions::default();
    let undefined = ctx.get(arg) == Value::UNDEFINED;
    if id == ToString.id() || id == ToLocaleString.id() {
        let radix = if id == ToString.id() {
            radix(ctx, arg)?
        } else {
            10
        };
        let exp = if radix == 10 {
            DtoaExp::Auto
        } else {
            DtoaExp::Disabled
        };
        return render_number(ctx, n, radix, 0, DtoaOptions { exp, ..default });
    }
    if id == ToPrecision.id() && undefined {
        return render_number(ctx, n, 10, 0, default);
    }
    let digits = to_integer_or_infinity(ctx, arg)?;
    if id != ToFixed.id() && !n.is_finite() {
        return render_number(ctx, n, 10, 0, default);
    }
    let min = if id == ToPrecision.id() { 1.0 } else { 0.0 };
    if !(min..=100.0).contains(&digits) {
        return Err(ctx.throw_range_error("invalid number of digits"));
    }
    let digits = digits as usize;
    let (digits, format, exp) = if id == ToExponential.id() {
        if undefined {
            (0, DtoaFormat::Free, DtoaExp::Enabled)
        } else {
            (digits + 1, DtoaFormat::Fixed, DtoaExp::Enabled)
        }
    } else if id == ToPrecision.id() {
        (digits, DtoaFormat::Fixed, DtoaExp::Auto)
    } else if n.abs() >= 1e21 {
        (0, DtoaFormat::Free, DtoaExp::Auto)
    } else {
        (digits, DtoaFormat::Frac, DtoaExp::Auto)
    };
    render_number(
        ctx,
        n,
        10,
        digits,
        DtoaOptions {
            format,
            exp,
            ..default
        },
    )
}

fn render_number(
    ctx: &mut ExecCtx<'_, '_>,
    n: f64,
    radix: u32,
    digits: usize,
    options: DtoaOptions,
) -> JsResult<Value> {
    let string = crate::ops::convert::format_number(ctx, n, radix, digits, options)?;
    Ok(ctx.get(string))
}
