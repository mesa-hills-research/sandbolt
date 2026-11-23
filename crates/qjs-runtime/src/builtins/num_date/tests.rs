use crate::ops::call::{call, construct};
use crate::ops::property::{
    get_own_property_descriptor, get_property, get_property_with_receiver, get_prototype_of,
    set_property,
};
use crate::value::Abrupt;
use crate::{EngineLimits, Runtime};

use super::*;

pub(super) const FIXTURE_START: u16 = 0x1f0;
const FIXTURE_NEXT: u16 = FIXTURE_START + 1;
const FIXTURE_RETURN: u16 = FIXTURE_START + 2;
const FIXTURE_VALUE_OF: u16 = FIXTURE_START + 3;
const FIXTURE_INTERRUPT: u16 = FIXTURE_START + 4;

fn runtime(f: impl for<'s> FnOnce(&mut ExecCtx<'_, 's>, Handle<'s>)) {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        install(ctx).unwrap();
        let global = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
        f(ctx, global);
    });
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, name: &str) -> Handle<'s> {
    let key = ctx.intern_str(name).unwrap();
    let base = crate::ops::convert::to_object(ctx, object).unwrap();
    let value = get_property_with_receiver(ctx, base, key, object).unwrap();
    ctx.root(value).unwrap()
}

fn invoke<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> JsResult<Value> {
    let function = get(ctx, object, name);
    let args = ctx.args(args)?;
    call(ctx, function, object, args)
}

fn new<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    ctor: Handle<'s>,
    args: &[Handle<'s>],
) -> JsResult<Handle<'s>> {
    let args = ctx.args(args)?;
    let value = construct(ctx, ctor, args, ctor)?;
    ctx.root(value)
}

fn text(ctx: &ExecCtx<'_, '_>, value: Value) -> String {
    String::from_utf16(
        &ctx.heap()
            .string_units(string_id(value))
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

fn expect_error(
    ctx: &mut ExecCtx<'_, '_>,
    result: JsResult<Value>,
    kind: crate::object::ErrorKind,
) {
    assert_eq!(result, Err(Abrupt::Thrown));
    let (error, oom) = ctx.take_exception().unwrap();
    assert!(!oom);
    let ObjectData::Error(data) = &ctx.heap().object(error.as_object().unwrap()).data else {
        panic!("expected error object")
    };
    assert_eq!(data.kind, kind);
}

#[test]
fn global_constants_and_shared_parser_descriptors() {
    runtime(|ctx, global| {
        for name in ["NaN", "Infinity", "undefined"] {
            let key = ctx.intern_str(name).unwrap();
            let desc = get_own_property_descriptor(ctx, global, key)
                .unwrap()
                .unwrap();
            assert_eq!(
                (desc.writable, desc.enumerable, desc.configurable),
                (Some(false), Some(false), Some(false))
            );
        }
        let number = get(ctx, global, "Number");
        for (name, length) in [("parseInt", 2), ("parseFloat", 1)] {
            let global_fn = get(ctx, global, name);
            let number_fn = get(ctx, number, name);
            assert_eq!(ctx.get(global_fn), ctx.get(number_fn));
            let name_prop = get(ctx, global_fn, "name");
            let len_prop = get(ctx, global_fn, "length");
            assert_eq!(text(ctx, ctx.get(name_prop)), name);
            assert_eq!(ctx.get(len_prop), Value::int32(length));
        }
        let arg = ctx.new_string("Infinity").unwrap();
        assert_eq!(
            invoke(ctx, global, "isFinite", &[arg]),
            Ok(Value::bool(false))
        );
        let arg = ctx.new_string("NaN").unwrap();
        assert_eq!(invoke(ctx, global, "isNaN", &[arg]), Ok(Value::bool(true)));
        assert_eq!(invoke(ctx, number, "isNaN", &[arg]), Ok(Value::bool(false)));
    });
}

#[test]
fn integer_parser_respects_radix_prefix_whitespace_and_sign() {
    runtime(|ctx, global| {
        for (text, radix, expected) in [
            ("\u{feff}\u{2028}\u{3000}-0XFtail", 0, -15.0),
            ("0x10", 10, 0.0),
            ("-0xyz", 10, -0.0),
            ("+0x10", 16, 16.0),
            ("1012", 2, 5.0),
            ("zz", 36, 1295.0),
            ("0b10", 0, 0.0),
            ("9007199254740995", 10, 9007199254740996.0),
            ("-0", 0, -0.0),
            ("1", 1, f64::NAN),
            ("\u{0085}7", 0, f64::NAN),
            ("0x", 0, f64::NAN),
        ] {
            let input = ctx.new_string(text).unwrap();
            let radix = ctx.root(Value::int32(radix)).unwrap();
            let actual = invoke(ctx, global, "parseInt", &[input, radix]).unwrap();
            assert_eq!(actual, Value::number(expected), "{text}");
        }
    });
}

#[test]
fn float_parser_uses_the_longest_decimal_prefix() {
    runtime(|ctx, global| {
        for (text, expected) in [
            ("\u{2007} -3.125e+2junk", -312.5),
            ("\u{2029}+.125e-2", 0.00125),
            ("1e+", 1.0),
            ("-0e-", -0.0),
            ("02e5e2", 200000.0),
            ("+Infinityjunk", f64::INFINITY),
            ("-0xF", -0.0),
            ("1.2\u{3000}3", 1.2),
            (".e2", f64::NAN),
            ("-", f64::NAN),
        ] {
            let input = ctx.new_string(text).unwrap();
            let actual = invoke(ctx, global, "parseFloat", &[input]).unwrap();
            assert_eq!(actual, Value::number(expected), "{text}");
        }
    });
}

#[test]
fn number_formatting_uses_each_rounding_and_exponent_mode() {
    runtime(|ctx, _| {
        for (n, method, argument, expected) in [
            (1.125, "toFixed", 2, "1.13"),
            (-0.001, "toFixed", 2, "-0.00"),
            (1e21, "toFixed", 2, "1e+21"),
            (1.25, "toExponential", 1, "1.3e+0"),
            (123.456, "toString", 16, "7b.74bc6a7ef9dc"),
            (1.0, "toPrecision", 3, "1.00"),
            (1e-7, "toPrecision", 2, "1.0e-7"),
            (-0.0, "toString", 2, "0"),
            (f64::INFINITY, "toExponential", 101, "Infinity"),
            (f64::NAN, "toPrecision", 101, "NaN"),
            (1.25, "toLocaleString", 2, "1.25"),
        ] {
            let number = ctx.root(Value::number(n)).unwrap();
            let arg = ctx.root(Value::int32(argument)).unwrap();
            let actual = invoke(ctx, number, method, &[arg]).unwrap();
            assert_eq!(text(ctx, actual), expected);
        }
        let inf = ctx.root(Value::number(f64::INFINITY)).unwrap();
        let digits = ctx.root(Value::int32(101)).unwrap();
        let result = invoke(ctx, inf, "toFixed", &[digits]);
        expect_error(ctx, result, crate::object::ErrorKind::Range);
    });
}

#[test]
fn number_and_boolean_construct_with_brand_and_new_target_prototype() {
    runtime(|ctx, global| {
        let number = get(ctx, global, "Number");
        let boolean = get(ctx, global, "Boolean");
        let proto = get(ctx, number, "prototype");
        assert_eq!(invoke(ctx, proto, "valueOf", &[]), Ok(Value::int32(0)));
        let money = ctx.new_string("0x20000000000003").unwrap();
        assert_eq!(
            invoke(ctx, global, "Number", &[money]),
            Ok(Value::number(9007199254740996.0))
        );
        let boolean_object = new(ctx, boolean, &[money]).unwrap();
        assert_eq!(
            invoke(ctx, boolean_object, "valueOf", &[]),
            Ok(Value::bool(true))
        );
        let boolean_function = call(ctx, boolean, boolean_object, Args::empty());
        assert_eq!(boolean_function, Ok(Value::bool(false)));
        let target = ctx
            .new_native(NativeFnId(NUM_DATE_BASE + FIXTURE_START), true)
            .unwrap();
        let default_args = ctx.args(&[money]).unwrap();
        let object = construct(ctx, number, default_args, target).unwrap();
        let object = ctx.root(object).unwrap();
        assert_eq!(
            get_prototype_of(ctx, object).unwrap(),
            ctx.get(proto).as_object()
        );
        assert_eq!(
            invoke(ctx, object, "valueOf", &[]),
            Ok(Value::number(9007199254740996.0))
        );
        let value_of = get(ctx, proto, "valueOf");
        let result = call(ctx, value_of, boolean_object, Args::empty());
        expect_error(ctx, result, crate::object::ErrorKind::Type);
    });
}

#[test]
fn math_preserves_signed_zero_and_has_es_power_edges() {
    runtime(|ctx, global| {
        let math = get(ctx, global, "Math");
        for (name, inputs, expected) in [
            ("round", &[-0.5][..], -0.0),
            ("round", &[-0.49999999999999994][..], -0.0),
            ("round", &[0.49999999999999994][..], 0.0),
            ("round", &[-1.5][..], -1.0),
            ("min", &[0.0, -0.0][..], -0.0),
            ("max", &[-0.0, 0.0][..], 0.0),
            ("pow", &[1.0, f64::NAN][..], f64::NAN),
            ("pow", &[-1.0, f64::NEG_INFINITY][..], f64::NAN),
            ("pow", &[f64::NAN, -0.0][..], 1.0),
            ("pow", &[-0.0, -3.0][..], f64::NEG_INFINITY),
            ("hypot", &[f64::NAN, f64::MAX, f64::MAX][..], f64::NAN),
            ("hypot", &[f64::NEG_INFINITY, f64::NAN][..], f64::INFINITY),
            ("hypot", &[3e199, 4e199][..], 5e199),
            ("hypot", &[3e-299, 4e-299][..], 5e-299),
        ] {
            let args = inputs
                .iter()
                .map(|&x| ctx.root(Value::number(x)).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                invoke(ctx, math, name, &args),
                Ok(Value::number(expected)),
                "{name} {inputs:?}"
            );
        }
        for _ in 0..16 {
            let value = invoke(ctx, math, "random", &[])
                .unwrap()
                .as_number()
                .unwrap();
            assert!((0.0..1.0).contains(&value));
        }
    });
}

#[test]
fn f16round_handles_ties_subnormals_and_overflow() {
    runtime(|ctx, global| {
        let math = get(ctx, global, "Math");
        let epsilon = 2_f64.powi(-24);
        let midpoint = 1.0 + 2_f64.powi(-11);
        for (n, expected) in [
            (65519.999, 65504.0),
            (65520.0, f64::INFINITY),
            (-65520.0, f64::NEG_INFINITY),
            (1.5 * epsilon, 2.0 * epsilon),
            (-0.5 * epsilon, -0.0),
            (-f64::from_bits(1), -0.0),
            (midpoint, 1.0),
            (
                f64::from_bits(midpoint.to_bits() + 1),
                1.0 + 2_f64.powi(-10),
            ),
            (f64::NAN, f64::NAN),
        ] {
            let arg = ctx.root(Value::number(n)).unwrap();
            assert_eq!(
                invoke(ctx, math, "f16round", &[arg]),
                Ok(Value::number(expected))
            );
        }
    });
}

#[test]
fn sum_precise_rounds_once_even_across_intermediate_overflow() {
    runtime(|ctx, global| {
        let math = get(ctx, global, "Math");
        let epsilon = f64::EPSILON;
        let min = f64::from_bits(1);
        for (inputs, expected) in [
            (&[][..], -0.0),
            (&[-0.0, -0.0][..], -0.0),
            (&[-0.0, 0.0][..], 0.0),
            (&[1.0, 1e100, 1.0, -1e100][..], 2.0),
            (&[f64::MAX, f64::MAX, -f64::MAX][..], f64::MAX),
            (&[-f64::MAX, -f64::MAX][..], f64::NEG_INFINITY),
            (&[f64::INFINITY, f64::NEG_INFINITY][..], f64::NAN),
            (&[1.0, epsilon / 2.0][..], 1.0),
            (&[1.0, epsilon / 2.0, min][..], 1.0 + epsilon),
            (&[min, -2.0 * min][..], -min),
            (&[min, -min][..], 0.0),
        ] {
            let values = inputs.iter().map(|&x| Value::number(x)).collect::<Vec<_>>();
            let iter = iterable(ctx, &values);
            assert_eq!(
                invoke(ctx, math, "sumPrecise", &[iter]),
                Ok(Value::number(expected)),
                "{inputs:?}"
            );
        }
    });
}

#[test]
fn sum_precise_closes_the_iterator_on_non_numbers_even_after_infinity() {
    runtime(|ctx, global| {
        let math = get(ctx, global, "Math");
        let invalid = ctx.new_string("2").unwrap();
        let values = [Value::number(f64::INFINITY), ctx.get(invalid)];
        let iter = iterable(ctx, &values);
        let old = get(ctx, iter, "closed");
        assert_eq!(ctx.get(old), Value::UNDEFINED);
        let result = invoke(ctx, math, "sumPrecise", &[iter]);
        expect_error(ctx, result, crate::object::ErrorKind::Type);
        let closed = get(ctx, iter, "closed");
        assert_eq!(ctx.get(closed), Value::bool(true));
    });
}

#[test]
fn sum_precise_closes_at_the_item_limit_and_propagates_host_interrupts() {
    runtime(|ctx, global| {
        let input = iterable(ctx, &[Value::int32(1)]);
        let result = math::sum_precise(ctx, input, (1 << 53) - 1).map(Value::number);
        expect_error(ctx, result, crate::object::ErrorKind::Range);
        let closed = get(ctx, input, "closed");
        assert_eq!(ctx.get(closed), Value::bool(true));

        let invalid = ctx.new_string("2").unwrap();
        let input = iterable(ctx, &[ctx.get(invalid)]);
        let interrupt = ctx
            .new_native(NativeFnId(NUM_DATE_BASE + FIXTURE_INTERRUPT), false)
            .unwrap();
        define(ctx, input, "return", interrupt, mutable()).unwrap();
        let math = get(ctx, global, "Math");
        assert_eq!(
            invoke(ctx, math, "sumPrecise", &[input]),
            Err(Abrupt::Interrupted)
        );
        assert!(ctx.pending_exception().is_none());
        ctx.runtime.set_interrupt_handler(None);
    });
}

#[test]
fn bigint_bridges_short_and_large_values_and_truncates_bits() {
    runtime(|ctx, global| {
        let bigint = get(ctx, global, "BigInt");
        let source = ctx
            .new_string("\u{feff} 0xfffffffffffffffe\u{202f}")
            .unwrap();
        let value = invoke(ctx, global, "BigInt", &[source]).unwrap();
        let value = ctx.root(value).unwrap();
        let radix = ctx.root(Value::int32(16)).unwrap();
        let val = invoke(ctx, value, "toString", &[radix]).unwrap();
        assert_eq!(text(ctx, val), "fffffffffffffffe");
        let width = ctx.root(Value::int32(64)).unwrap();
        let val = invoke(ctx, bigint, "asIntN", &[width, value]).unwrap();
        assert_eq!(val, Value::short_bigint(-2).unwrap());
        let val = ctx.root(val).unwrap();
        let val = invoke(ctx, bigint, "asUintN", &[width, val]).unwrap();
        let val = ctx.root(val).unwrap();
        let result = invoke(ctx, val, "toString", &[radix]).unwrap();
        assert_eq!(text(ctx, result), "fffffffffffffffe");
        assert_eq!(
            invoke(ctx, global, "Number", &[value]),
            Ok(Value::number(18_446_744_073_709_551_616.0))
        );
        let min = ctx
            .root(Value::number(-18_446_744_073_709_551_616.0))
            .unwrap();
        let val = invoke(ctx, global, "BigInt", &[min]).unwrap();
        let val = ctx.root(val).unwrap();
        let result = invoke(ctx, val, "toString", &[radix]).unwrap();
        assert_eq!(text(ctx, result), "-10000000000000000");
        let wrapper = crate::ops::convert::to_object(ctx, val).unwrap();
        assert_eq!(invoke(ctx, wrapper, "valueOf", &[]), Ok(ctx.get(val)));
    });
}

#[test]
fn bigint_conversion_checks_argument_type_number_and_index() {
    use crate::object::ErrorKind::*;
    runtime(|ctx, global| {
        let bigint = get(ctx, global, "BigInt");
        let n = ctx.root(Value::number(0.5)).unwrap();
        let result = invoke(ctx, global, "BigInt", &[n]);
        expect_error(ctx, result, Range);
        let source = ctx.new_string("-0x1").unwrap();
        let result = invoke(ctx, global, "BigInt", &[source]);
        expect_error(ctx, result, Syntax);
        let n = ctx.root(Value::int32(1)).unwrap();
        let result = invoke(ctx, bigint, "asIntN", &[n, n]);
        expect_error(ctx, result, Type);
        let n = ctx.root(Value::int32(-1)).unwrap();
        let result = invoke(ctx, bigint, "asUintN", &[n]);
        expect_error(ctx, result, Range);
        let result = construct(ctx, bigint, Args::empty(), bigint);
        expect_error(ctx, result, Type);
    });
}

#[test]
fn date_parses_iso_legacy_and_unicode_whitespace() {
    runtime(|ctx, global| {
        let date = get(ctx, global, "Date");
        for (input, expected) in [
            ("2024-02-29T03:04:05.678Z", 1709175845678.0),
            ("1999-12-31T24:00:00Z", 946684800000.0),
            ("1970-01-01T00:30:00+00:30", 0.0),
            ("Thu, 01 Jan 1970 00:00:00 GMT", 0.0),
            ("Thu Jan 01 1970 01:00:00 GMT+0100 (CET)", 0.0),
            ("\u{3000}01/01/1970, 00:00:00 GMT\u{feff}", 0.0),
            ("\u{2028}1970-01-01\u{202f}", 0.0),
            ("+275760-09-13T01:00:00+01:00", 8.64e15),
            ("-271821-04-20T00:00:00.000Z", -8.64e15),
            ("+275760-09-13T00:00:00.001Z", f64::NAN),
            ("-000000-01-01", f64::NAN),
            ("1970-01-01T24:00:00.001Z", f64::NAN),
            ("1970-01-01T00:00+24:01", f64::NAN),
        ] {
            let arg = ctx.new_string(input).unwrap();
            assert_eq!(
                invoke(ctx, date, "parse", &[arg]),
                Ok(Value::number(expected)),
                "{input}"
            );
        }
    });
}

#[test]
fn date_utc_uses_calendar_normalization_and_ieee_arithmetic_order() {
    runtime(|ctx, global| {
        let date = get(ctx, global, "Date");
        for (input, expected) in [
            (&[99.0][..], 915148800000.0),
            (&[1970.0, 0.0, 0.0][..], -86400000.0),
            (&[2000.0, 1.0, 29.0][..], 951782400000.0),
            (&[1900.0, 1.0, 29.0][..], -2203891200000.0),
            (&[2000.0, -1.0, 1.0][..], 944006400000.0),
            (&[275761.0, 0.0, -500.0][..], 8639966217600000.0),
            (&[-271822.0, 0.0, 700.0][..], -8639980560000000.0),
            (
                &[
                    1970.0,
                    0.0,
                    1.0,
                    80063993375.0,
                    29.0,
                    1.0,
                    -288230376151711740.0,
                ][..],
                29312.0,
            ),
            (&[][..], f64::NAN),
        ] {
            let args = input
                .iter()
                .map(|&value| ctx.root(Value::number(value)).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                invoke(ctx, date, "UTC", &args),
                Ok(Value::number(expected)),
                "{input:?}"
            );
        }
        let n = ctx.root(Value::number(f64::NAN)).unwrap();
        let month = ctx.new_object().unwrap();
        let value_of = ctx
            .new_native(NativeFnId(NUM_DATE_BASE + FIXTURE_VALUE_OF), false)
            .unwrap();
        define(ctx, month, "valueOf", value_of, PropFlags::DEFAULT_DATA).unwrap();
        assert_eq!(
            invoke(ctx, date, "UTC", &[n, month]),
            Ok(Value::number(f64::NAN))
        );
        let visited = get(ctx, month, "visited");
        assert_eq!(ctx.get(visited), Value::bool(true));
    });
}

#[test]
fn date_time_clip_and_mutation_handle_invalid_dates() {
    use crate::object::ErrorKind::*;
    runtime(|ctx, global| {
        let date = get(ctx, global, "Date");
        let t = ctx.root(Value::number(-0.99)).unwrap();
        let d = new(ctx, date, &[t]).unwrap();
        assert_eq!(invoke(ctx, d, "getTime", &[]), Ok(Value::number(0.0)));
        let utc = invoke(ctx, d, "toUTCString", &[]).unwrap();
        assert_eq!(text(ctx, utc), "Thu, 01 Jan 1970 00:00:00 GMT");
        let utc = get(ctx, d, "toUTCString");
        let gmt = get(ctx, d, "toGMTString");
        assert_eq!(ctx.get(utc), ctx.get(gmt));
        let epoch = invoke(ctx, d, "toISOString", &[]).unwrap();
        assert_eq!(text(ctx, epoch), "1970-01-01T00:00:00.000Z");
        let t = ctx.root(Value::number(8.64e15 + 1.0)).unwrap();
        assert_eq!(invoke(ctx, d, "setTime", &[t]), Ok(Value::number(f64::NAN)));
        assert_eq!(invoke(ctx, d, "toJSON", &[]), Ok(Value::NULL));
        let result = invoke(ctx, d, "toISOString", &[]);
        expect_error(ctx, result, Range);
        let zero = ctx.root(Value::int32(0)).unwrap();
        assert_eq!(
            invoke(ctx, d, "setUTCFullYear", &[zero]),
            Ok(Value::number(-62167219200000.0))
        );
        let year = ctx.root(Value::int32(-1)).unwrap();
        assert_eq!(
            invoke(ctx, d, "setUTCFullYear", &[year]),
            Ok(Value::number(-62198755200000.0))
        );
        let iso = invoke(ctx, d, "toISOString", &[]).unwrap();
        assert_eq!(text(ctx, iso), "-000001-01-01T00:00:00.000Z");
        let copy = new(ctx, date, &[d]).unwrap();
        assert_eq!(
            invoke(ctx, copy, "getUTCFullYear", &[]),
            Ok(Value::number(-1.0))
        );
        assert_eq!(
            invoke(ctx, d, "setUTCFullYear", &[]),
            Ok(Value::number(f64::NAN))
        );
        assert_eq!(invoke(ctx, d, "getTime", &[]), Ok(Value::number(f64::NAN)));
    });
}

#[test]
fn date_local_construction_getters_and_setters_agree_on_wall_time() {
    runtime(|ctx, global| {
        let date = get(ctx, global, "Date");
        let args = [2024, 6, 7, 12, 34, 56, 789].map(|n| ctx.root(Value::int32(n)).unwrap());
        let local = new(ctx, date, &args).unwrap();
        for (name, expected) in [
            ("getYear", 124.0),
            ("getFullYear", 2024.0),
            ("getMonth", 6.0),
            ("getDate", 7.0),
            ("getHours", 12.0),
            ("getMinutes", 34.0),
            ("getSeconds", 56.0),
            ("getMilliseconds", 789.0),
            ("getDay", 0.0),
        ] {
            assert_eq!(invoke(ctx, local, name, &[]), Ok(Value::number(expected)));
        }
        let utc = invoke(ctx, date, "UTC", &args)
            .unwrap()
            .as_number()
            .unwrap();
        let local_time = invoke(ctx, local, "getTime", &[])
            .unwrap()
            .as_number()
            .unwrap();
        let offset = invoke(ctx, local, "getTimezoneOffset", &[])
            .unwrap()
            .as_number()
            .unwrap();
        assert_eq!(local_time, utc + offset * 60_000.0);
        let text_value = invoke(ctx, local, "toLocaleString", &[]).unwrap();
        assert_eq!(text(ctx, text_value), "07/07/2024, 12:34:56 PM");
        let year = ctx.root(Value::int32(99)).unwrap();
        invoke(ctx, local, "setYear", &[year]).unwrap();
        assert_eq!(
            invoke(ctx, local, "getFullYear", &[]),
            Ok(Value::number(1999.0))
        );
        let hour = ctx.root(Value::int32(23)).unwrap();
        invoke(ctx, local, "setUTCHours", &[hour]).unwrap();
        assert_eq!(
            invoke(ctx, local, "getUTCHours", &[]),
            Ok(Value::number(23.0))
        );
    });
}

#[test]
fn date_to_primitive_validates_hints_and_can_be_borrowed() {
    use crate::object::ErrorKind::*;
    runtime(|ctx, global| {
        let date = get(ctx, global, "Date");
        let key = symbol_key(ctx, WellKnown::ToPrimitive);
        let proto = get(ctx, date, "prototype");
        let desc = get_own_property_descriptor(ctx, proto, key)
            .unwrap()
            .unwrap();
        assert_eq!(
            (desc.writable, desc.enumerable, desc.configurable),
            (Some(false), Some(false), Some(true))
        );
        let method = ctx.root(desc.value.unwrap()).unwrap();
        let value_of = ctx
            .new_native(NativeFnId(NUM_DATE_BASE + FIXTURE_VALUE_OF), false)
            .unwrap();
        let obj = ctx.new_object().unwrap();
        define(ctx, obj, "valueOf", value_of, PropFlags::DEFAULT_DATA).unwrap();
        let n = ctx.new_string("number").unwrap();
        let args = ctx.args(&[n]).unwrap();
        assert_eq!(call(ctx, method, obj, args), Ok(Value::int32(42)));
        for hint in ["string", "default"] {
            let input = ctx.new_string(hint).unwrap();
            let args = ctx.args(&[input]).unwrap();
            let result = call(ctx, method, obj, args).unwrap();
            assert_eq!(text(ctx, result), "[object Object]");
        }
        let invalid = ctx.new_string("integer").unwrap();
        let args = ctx.args(&[invalid]).unwrap();
        let result = call(ctx, method, obj, args);
        expect_error(ctx, result, Type);
        let result = call(ctx, date, obj, args).unwrap();
        assert!(text(ctx, result).ends_with(|ch: char| ch.is_ascii_digit()));
    });
}

fn iterable<'s>(ctx: &mut ExecCtx<'_, 's>, values: &[Value]) -> Handle<'s> {
    let iter = ctx.new_object().unwrap();
    let method = ctx
        .new_native(NativeFnId(NUM_DATE_BASE + FIXTURE_START), false)
        .unwrap();
    define_data(
        ctx,
        iter,
        symbol_key(ctx, WellKnown::Iterator),
        method,
        mutable(),
    )
    .unwrap();
    for (name, offset) in [("next", FIXTURE_NEXT), ("return", FIXTURE_RETURN)] {
        let method = ctx
            .new_native(NativeFnId(NUM_DATE_BASE + offset), false)
            .unwrap();
        define(ctx, iter, name, method, mutable()).unwrap();
    }
    for (i, &value) in values.iter().enumerate() {
        let value = ctx.root(value).unwrap();
        define_data(
            ctx,
            iter,
            PropKey::index(i as u32).unwrap(),
            value,
            mutable(),
        )
        .unwrap();
    }
    let index = ctx.root(Value::int32(0)).unwrap();
    define(ctx, iter, "index", index, mutable()).unwrap();
    iter
}

pub(super) fn dispatch<'s>(
    offset: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
) -> JsResult<Value> {
    match offset {
        FIXTURE_START => Ok(ctx.get(this)),
        FIXTURE_NEXT => {
            let index = ctx.intern_str("index")?;
            let n = get_property(ctx, this, index)?.as_number().unwrap() as u32;
            let next = ctx.root(Value::number(f64::from(n + 1)))?;
            set_property(ctx, this, index, next)?;
            let value = get_property(ctx, this, PropKey::index(n).unwrap())?;
            let value = ctx.root(value)?;
            ctx.collect();
            let result = ctx.new_object()?;
            let done = ctx.root(Value::bool(ctx.get(value) == Value::UNDEFINED))?;
            define(ctx, result, "done", done, mutable())?;
            define(ctx, result, "value", value, mutable())?;
            Ok(ctx.get(result))
        }
        FIXTURE_RETURN => {
            let value = ctx.root(Value::bool(true))?;
            define(ctx, this, "closed", value, mutable())?;
            Err(ctx.throw_range_error("iterator closed"))
        }
        FIXTURE_INTERRUPT => {
            ctx.runtime.set_interrupt_handler(Some(Box::new(|| true)));
            ctx.check_interrupt()?;
            unreachable!()
        }
        FIXTURE_VALUE_OF => {
            let value = ctx.root(Value::bool(true))?;
            define(ctx, this, "visited", value, mutable())?;
            Ok(Value::int32(42))
        }
        _ => unreachable!(),
    }
}
