use super::*;

const DECIMAL_MIDPOINT: f64 = 1_000_000_000_000_000.0 + 0.25;

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
fn simple_decimals_match_the_general_parser() {
    let plain = AtodFlags::default();
    let general = AtodFlags {
        accept_underscores: true,
        ..plain
    };
    let mut bits = 0x24a2_5fa3_ce4a_51b1u64;
    for i in 0..20_000 {
        bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
        let digits = (bits % 1_000_000_000_000_000).to_string();
        let point = i % (digits.len() + 1);
        let sign = ["", "-", "+"][i % 3];
        let suffix = ["", "x", ".", "e-2", "e+"][i % 5];
        let text = format!(
            "{sign}{}.{part}{suffix}",
            &digits[..point],
            part = &digits[point..]
        );
        let (normal, n) = atod(&text, 10, plain);
        let (fallback, f) = atod(&text, 10, general);
        assert_eq!((normal.to_bits(), n), (fallback.to_bits(), f), "{text:?}");
    }
    check_atod("-0.0", 10, plain, -0.0, 4);
    check_atod("123456789012345.6", 10, plain, 123456789012345.6, 17);
}

#[test]
fn whole_ascii_decimal_fast_path_requires_every_byte() {
    for (bytes, expected) in [
        (&b"-.0"[..], -0.0f64),
        (b"5.", 5.0),
        (b"+0.25", 0.25),
        (b"12345678901234.5", 12345678901234.5),
    ] {
        assert_eq!(
            parse_simple_decimal(bytes).unwrap().to_bits(),
            expected.to_bits()
        );
    }
    for bytes in [
        &b""[..],
        b"-",
        b"+.",
        b"0x10",
        b"NaN",
        b"1e2",
        b"1\x00",
        b"5..",
        b"1\xff",
        b"1234567890123456",
    ] {
        assert_eq!(parse_simple_decimal(bytes), None, "{bytes:?}");
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
        let mut buf = [0xff; 128];
        let len = dtoa_into(&mut buf, value, 10, 0, free);
        assert_eq!(&buf[..len], expected.as_bytes());
    }
}

#[test]
fn shortest_rounds_a_midpoint_to_even_but_explicit_precision_rounds_away() {
    let value = DECIMAL_MIDPOINT;
    for (format, exp, digits, expected) in [
        (DtoaFormat::Free, DtoaExp::Auto, 0, "1000000000000000.2"),
        (DtoaFormat::Frac, DtoaExp::Auto, 1, "1000000000000000.3"),
        (DtoaFormat::Fixed, DtoaExp::Auto, 17, "1000000000000000.3"),
        (
            DtoaFormat::Fixed,
            DtoaExp::Enabled,
            17,
            "1.0000000000000003e+15",
        ),
    ] {
        assert_eq!(dtoa(value, 10, digits, opts(format, exp)), expected);
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
#[ignore = "requires QJS_REF to name a built QuickJS-ng 0.17.0 checkout"]
fn number_precisions_match_quickjs() {
    use std::fmt::Write;
    use std::process::Command;

    let mut cases = vec![
        0,
        1 << 63,
        1,
        FRACTION_MASK,
        FRACTION_BIT,
        f64::NAN.to_bits(),
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
    ];
    let mut around = |n: f64| {
        let bits = n.to_bits();
        for bits in [bits - 1, bits, bits + 1] {
            cases.extend([bits, bits | (1 << 63)]);
        }
    };
    for power in -24..=24 {
        around(10_f64.powi(power));
        around(2_f64.powi(power));
    }
    for power in 1..=21 {
        for odd in [1, 3, 5, 11, 123, 1999] {
            around(f64::from(odd) * 2_f64.powi(-power));
        }
    }
    around(DECIMAL_MIDPOINT);
    let mut rand = 0x333e_a913_287e_ad53u64;
    for i in 0..2048 {
        rand ^= rand << 13;
        rand ^= rand >> 7;
        rand ^= rand << 17;
        cases.push(if i % 2 == 0 {
            rand
        } else {
            ((rand >> 52) % 128 + 959) << 52 | (rand & FRACTION_MASK)
        });
    }
    let mut expr = String::from("const bits = [");
    for bits in &cases {
        write!(expr, "0x{bits:x}n,").unwrap();
    }
    expr.push_str(
        r#"];
        const view = new DataView(new ArrayBuffer(8));
        for (const b of bits) {
            view.setBigUint64(0, b);
            const n = view.getFloat64(0);
            print(n.toString(), n.toExponential());
            for (let d = 0; d <= 20; d++)
                print(n.toFixed(d), n.toPrecision(d + 1), n.toExponential(d));
        }
    "#,
    );
    let reference = std::path::PathBuf::from(std::env::var_os("QJS_REF").expect("set QJS_REF"));
    let output = Command::new(reference.join("build/qjs"))
        .args(["-e", &expr])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    let mut expected = output.split_ascii_whitespace();
    for &bits in &cases {
        let n = f64::from_bits(bits);
        let mut compare = |format, exp, digits| {
            assert_eq!(
                dtoa(n, 10, digits, opts(format, exp)),
                expected.next().unwrap(),
                "{bits:016x} {format:?} {exp:?} {digits}"
            );
        };
        compare(DtoaFormat::Free, DtoaExp::Auto, 0);
        compare(DtoaFormat::Free, DtoaExp::Enabled, 0);
        for d in 0..=20 {
            if n.abs() >= 1e21 {
                compare(DtoaFormat::Free, DtoaExp::Auto, 0);
            } else {
                compare(DtoaFormat::Frac, DtoaExp::Auto, d);
            }
            compare(DtoaFormat::Fixed, DtoaExp::Auto, d + 1);
            compare(DtoaFormat::Fixed, DtoaExp::Enabled, d + 1);
        }
    }
    assert_eq!(expected.next(), None);
    println!("{} decimal strings, 0 mismatches", cases.len() * 65);
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
