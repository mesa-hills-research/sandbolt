use super::*;

fn opts(format: DtoaFormat, exp: DtoaExp) -> DtoaOptions {
    DtoaOptions {
        format,
        exp,
        minus_zero: false,
    }
}

fn check_atod(input: &str, radix: u32, flags: AtodFlags, expected: f64, consumed: usize) {
    let (result, end) = atod(input, radix, flags);
    assert_eq!(end, consumed, "{input:?}");
    if expected.is_nan() {
        assert!(result.is_nan(), "{input:?}: {result}");
    } else {
        assert_eq!(result.to_bits(), expected.to_bits(), "{input:?}");
    }
}

#[test]
fn shortest_decimal_uses_ecmascript_thresholds() {
    let free = DtoaOptions::default();
    for (value, expected) in [
        (1e-7, "1e-7"),
        (1e-6, "0.000001"),
        (1e20, "100000000000000000000"),
        (1e21, "1e+21"),
        (9_007_199_254_740_991.0, "9007199254740991"),
        (f64::from_bits(1.0f64.to_bits() + 1), "1.0000000000000002"),
        (f64::from_bits(1), "5e-324"),
        (f64::MAX, "1.7976931348623157e+308"),
    ] {
        assert_eq!(dtoa(value, 10, 0, free), expected);
    }
}

#[test]
fn signed_zero_infinity_and_nan() {
    let free = DtoaOptions::default();
    assert_eq!(dtoa(-0.0, 10, 0, free), "0");
    let minus_zero = DtoaOptions {
        minus_zero: true,
        ..free
    };
    assert_eq!(dtoa(-0.0, 10, 0, minus_zero), "-0");
    assert_eq!(dtoa(0.0, 10, 0, minus_zero), "0");
    assert_eq!(dtoa(f64::INFINITY, 36, 0, free), "Infinity");
    assert_eq!(dtoa(f64::NEG_INFINITY, 2, 0, free), "-Infinity");
    assert_eq!(
        dtoa(f64::from_bits(0xfff0_0000_0000_0001), 16, 0, free),
        "NaN"
    );
}

#[test]
fn radix_specific_exponent_markers() {
    let exponential = opts(DtoaFormat::Free, DtoaExp::Enabled);
    for (radix, expected) in [
        (2, "1.111101p+4"),
        (4, "1.331p+4"),
        (8, "3.72p+3"),
        (10, "3.125e+1"),
        (16, "1.f4p+4"),
        (32, "v.8@+0"),
        (36, "v.9@+0"),
    ] {
        assert_eq!(dtoa(31.25, radix, 0, exponential), expected);
        check_atod(expected, radix, AtodFlags::default(), 31.25, expected.len());
    }
}

#[test]
fn fixed_precision_rounds_ties_away_and_carries() {
    let fixed = opts(DtoaFormat::Fixed, DtoaExp::Auto);
    assert_eq!(dtoa(1.25, 10, 2, fixed), "1.3");
    assert_eq!(dtoa(-1.25, 10, 2, fixed), "-1.3");
    assert_eq!(dtoa(9.5, 10, 1, fixed), "1e+1");
    assert_eq!(dtoa(99.5, 10, 2, fixed), "1.0e+2");
    assert_eq!(
        dtoa(99.5, 10, 2, opts(DtoaFormat::Fixed, DtoaExp::Disabled)),
        "100"
    );
    assert_eq!(dtoa(0.0, 10, 101, fixed), format!("0.{}", "0".repeat(100)));
}

#[test]
fn fraction_precision_rounds_ties_away_without_an_exponent() {
    let fraction = opts(DtoaFormat::Frac, DtoaExp::Enabled);
    assert_eq!(dtoa(1.125, 10, 2, fraction), "1.13");
    assert_eq!(dtoa(-0.125, 10, 2, fraction), "-0.13");
    assert_eq!(dtoa(9.875, 2, 2, fraction), "1010.00");
    assert_eq!(dtoa(-0.01, 10, 0, fraction), "-0");
    assert_eq!(
        dtoa(1.0, 10, 101, fraction),
        format!("1.{}", "0".repeat(101))
    );
}

#[test]
fn disabled_exponent_bound_covers_subnormal_leading_zeroes() {
    let fixed = opts(DtoaFormat::Fixed, DtoaExp::Disabled);
    let value = f64::from_bits(1);
    let expected = format!("0.{}1{}", "0".repeat(1073), "0".repeat(100));
    // QuickJS estimates 1135 bytes from the encoded exponent but writes 1176.
    assert_eq!(expected.len(), 1176);
    assert_eq!(dtoa(value, 2, 101, fixed), expected);
    assert!(dtoa_max_len(value, 2, 101, fixed) >= expected.len());
    assert!(dtoa_max_len(-value, 2, 101, fixed) > expected.len());
}

#[test]
fn fraction_zero_bound_includes_explicit_exponent() {
    let fraction = DtoaOptions {
        minus_zero: true,
        ..opts(DtoaFormat::Frac, DtoaExp::Enabled)
    };
    assert_eq!(dtoa(0.0, 16, 0, fraction), "0p+0");
    let expected = format!("-0.{}e+0", "0".repeat(101));
    // QuickJS's fraction bound omits the suffix that it prints for zero.
    assert_eq!(dtoa(-0.0, 10, 101, fraction), expected);
    assert!(dtoa_max_len(-0.0, 10, 101, fraction) >= expected.len());
}

#[test]
fn round_trips_float_boundaries_in_decimal_and_hexadecimal() {
    let exponential = opts(DtoaFormat::Free, DtoaExp::Enabled);
    for bits in [
        1,
        2,
        FRACTION_MASK - 1,
        FRACTION_MASK,
        FRACTION_BIT,
        FRACTION_BIT + 1,
        0x3fef_ffff_ffff_ffff,
        0x3ff0_0000_0000_0000,
        0x3ff0_0000_0000_0001,
        f64::MAX.to_bits(),
    ] {
        for radix in [10, 16] {
            for bits in [bits, bits | (1 << 63)] {
                let value = f64::from_bits(bits);
                let text = dtoa(value, radix, 0, exponential);
                check_atod(&text, radix, AtodFlags::default(), value, text.len());
            }
        }
    }
}

#[test]
fn malformed_prefix_and_exponent_report_where_parsing_stopped() {
    for (text, value, consumed) in [
        ("", f64::NAN, 0),
        ("  1", f64::NAN, 0),
        ("\u{feff}1", f64::NAN, 0),
        ("+", f64::NAN, 1),
        ("-.", f64::NAN, 1),
        ("-0x.", f64::NAN, 3),
        ("1e+", f64::NAN, 3),
        ("-NaN", f64::NAN, 1),
        ("+Infinitytail", f64::INFINITY, 9),
        ("-Infinity", f64::NEG_INFINITY, 9),
        ("1.2.3", 1.2, 3),
        ("12é", 12.0, 2),
        ("1\0é", 1.0, 1),
    ] {
        check_atod(text, 0, AtodFlags::default(), value, consumed);
    }
}

#[test]
fn radix_and_integer_flags_control_prefixes() {
    let no_flags = AtodFlags::default();
    let prefixes = AtodFlags {
        accept_bin_oct: true,
        accept_legacy_octal: true,
        ..no_flags
    };
    let integers = AtodFlags {
        int_only: true,
        ..prefixes
    };
    check_atod("0b101", 0, no_flags, 0.0, 1);
    check_atod("+0B101", 0, prefixes, 5.0, 6);
    check_atod("-0O17", 0, prefixes, -15.0, 5);
    check_atod("0b101", 2, prefixes, 0.0, 1);
    check_atod("0xFF", 16, no_flags, 255.0, 4);
    check_atod("0123", 0, prefixes, 83.0, 4);
    check_atod("0129", 0, prefixes, 129.0, 4);
    check_atod("-0xDEAD.X", 0, integers, -57005.0, 7);
    check_atod("1e8", 16, integers, 488.0, 3);
    check_atod("1e8", 10, integers, 1.0, 1);
    check_atod("Infinity", 0, integers, f64::NAN, 0);
}

#[test]
fn separators_follow_quickjs_digit_scanning() {
    let separators = AtodFlags {
        accept_underscores: true,
        ..AtodFlags::default()
    };
    for (text, value, consumed) in [
        ("1_2.3_4e+0_2!", 1234.0, 12),
        ("1__2", 1.0, 1),
        ("1._1", 1.1, 4),
        ("._1", f64::NAN, 0),
        ("1e+_2", f64::NAN, 3),
        ("0x_1", f64::NAN, 2),
    ] {
        check_atod(text, 0, separators, value, consumed);
    }
    let legacy = AtodFlags {
        accept_legacy_octal: true,
        ..separators
    };
    check_atod("07_1", 0, legacy, 7.0, 2);
    check_atod("08_1", 0, legacy, 8.0, 2);
}

#[test]
fn underflow_and_overflow_round_at_float_boundaries() {
    for (text, bits) in [
        ("2.4703282292062327e-324", 0),
        ("2.4703282292062328e-324", 1),
        ("-2.4703282292062327e-324", 1 << 63),
        ("0x1p-1075", 0),
        ("0x3p-1075", 2),
        ("0x1.fffffffffffffp-1023", f64::MIN_POSITIVE.to_bits()),
        ("0x1.fffffffffffffp1023", f64::MAX.to_bits()),
        ("0x1.fffffffffffff8p1023", f64::INFINITY.to_bits()),
        ("1.7976931348623157e308", f64::MAX.to_bits()),
        ("1.7976931348623159e308", f64::INFINITY.to_bits()),
    ] {
        check_atod(
            text,
            0,
            AtodFlags::default(),
            f64::from_bits(bits),
            text.len(),
        );
    }
}

#[test]
fn binary_sticky_bits_distinguish_a_tie_from_rounding_up() {
    let tie = "0x1.000000000000080000000000000000000000000000000000000";
    let above = "0x1.000000000000080000000000000000000000000000000000001";
    check_atod(tie, 0, AtodFlags::default(), 1.0, tie.len());
    check_atod(
        above,
        0,
        AtodFlags::default(),
        f64::from_bits(1.0f64.to_bits() + 1),
        above.len(),
    );
}

#[test]
fn large_exponents_consume_all_digits() {
    for (text, value) in [
        ("1e9999999999999999", f64::INFINITY),
        ("-1e-9999999999999999", -0.0),
        ("-0e9999999999999999", -0.0),
        ("0x1@1073741824", 1.0),
        ("0x1@1073741825", 16.0),
        ("0x1@-1073741825", 0.0625),
    ] {
        check_atod(text, 0, AtodFlags::default(), value, text.len());
    }
}

#[test]
fn decimal_mantissas_round_to_even_after_the_digit_limit() {
    check_atod(
        "9007199254740993",
        0,
        AtodFlags::default(),
        9_007_199_254_740_992.0,
        16,
    );
    check_atod(
        "9007199254740995",
        0,
        AtodFlags::default(),
        9_007_199_254_740_996.0,
        16,
    );
    // The decimal cutoff places the retained digits just below the exact tie.
    let text = "1.00000000000000033306690738754696212708950042724609375";
    check_atod(
        text,
        0,
        AtodFlags::default(),
        f64::from_bits(1.0f64.to_bits() + 1),
        text.len(),
    );
}

#[test]
fn decimal_integer_buffers_cover_signed_and_unsigned_extremes() {
    let mut buf = [0xff; 21];
    assert_eq!(u32toa(&mut buf, u32::MAX), 10);
    assert_eq!(&buf[..11], b"4294967295\xff");
    assert_eq!(i32toa(&mut buf, i32::MIN), 11);
    assert_eq!(&buf[..12], b"-2147483648\xff");
    assert_eq!(u64toa(&mut buf, u64::MAX), 20);
    assert_eq!(&buf, b"18446744073709551615\xff");
    assert_eq!(i64toa(&mut buf, i64::MIN), 20);
    assert_eq!(&buf, b"-9223372036854775808\xff");
    assert_eq!(i64toa(&mut buf, 0), 1);
    assert_eq!(buf[0], b'0');
}

#[test]
fn integer_radixes_use_lowercase_and_write_no_terminator() {
    for radix in 2..=36 {
        for value in [0, 1, 9, 10, u64::from(u32::MAX), i64::MAX as u64, u64::MAX] {
            let mut buf = [0xff; 66];
            let len = u64toa_radix(&mut buf, value, radix);
            let text = std::str::from_utf8(&buf[..len]).unwrap();
            assert_eq!(u64::from_str_radix(text, radix), Ok(value));
            assert!(!text.bytes().any(|b| b.is_ascii_uppercase()));
            assert_eq!(buf[len], 0xff);
        }
        for value in [i64::MIN, -1, 0, 1, i64::MAX] {
            let mut buf = [0xff; 66];
            let len = i64toa_radix(&mut buf, value, radix);
            let text = std::str::from_utf8(&buf[..len]).unwrap();
            assert_eq!(i64::from_str_radix(text, radix), Ok(value));
            assert_eq!(buf[len], 0xff);
        }
    }
}

#[test]
fn limb_rounding_propagates_carry_and_respects_sticky_bits() {
    let mut value = Limbs::new(u64::MAX);
    value.mul_add(2, 1);
    value.round_shift(1, Round::Even);
    assert_eq!(value.floor_log2(), 64);
    assert_eq!(value.low_limb(), 0);
    value.round_shift(32, Round::Truncate);
    assert_eq!(value.as_u64(), 1 << 32);

    for (low, expected) in [(0, 2), (1, 3)] {
        let mut value = Limbs::new(5);
        value.mul_add(1 << 32, low);
        value.round_shift(33, Round::Even);
        assert_eq!(value.as_u64(), expected);
    }
}
