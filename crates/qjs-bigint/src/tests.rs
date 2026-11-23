use super::*;

fn dec(s: &str) -> BigInt {
    BigInt::parse_radix(s, 10, &mut UnlimitedBigIntHost).unwrap()
}

fn hex(s: &str) -> BigInt {
    BigInt::parse_radix(s, 16, &mut UnlimitedBigIntHost).unwrap()
}

fn int(n: i128) -> BigInt {
    dec(&n.to_string())
}

fn power_of_two(bit: usize) -> BigInt {
    BigInt::from_i64(1)
        .shl(&BigInt::from_u64(bit as u64), &mut UnlimitedBigIntHost)
        .unwrap()
}

#[test]
fn normalized_limbs_and_wrapping() {
    let max = BigInt::from_u64(u64::MAX);
    let min = BigInt::from_i64(i64::MIN);
    assert_eq!(max.limbs(), &[u64::MAX, 0]);
    assert_eq!(max.to_i64_exact(), None);
    assert_eq!(max.to_i64_wrapping(), -1);
    assert_eq!(min.to_u64_wrapping(), 1 << 63);
    assert_eq!(min.to_i64_exact(), Some(i64::MIN));
    assert_eq!(dec("-18446744073709551616").limbs(), &[0, u64::MAX]);
    assert_eq!(
        hex("-8000000000000001").limbs(),
        &[i64::MAX as u64, u64::MAX]
    );
    assert_eq!(hex("-8000000000000000"), min);
    assert_eq!(dec("-000"), BigInt::zero());
    assert_eq!(BigInt::zero().bit_len(), 0);
    assert_eq!(BigInt::zero().signum(), 0);
    assert_eq!(max.signum(), 1);
    assert_eq!(min.signum(), -1);
    assert!(!max.is_even());
    assert!(min.is_even());
}

#[test]
fn integer_comparisons_and_carries() {
    let host = &mut UnlimitedBigIntHost;
    let samples = [
        i128::MIN / 2,
        -(1 << 64),
        i64::MIN as i128,
        -1,
        0,
        1,
        i64::MAX as i128,
        1 << 64,
        i128::MAX / 2,
    ];
    for a in samples {
        let lhs = int(a);
        assert_eq!(lhs.neg(host).unwrap(), int(-a));
        for b in samples {
            let rhs = int(b);
            assert_eq!(lhs.cmp_bigint(&rhs), a.cmp(&b));
            assert_eq!(lhs.add(&rhs, host).unwrap(), int(a + b));
            assert_eq!(lhs.sub(&rhs, host).unwrap(), int(a - b));
        }
    }
    assert_eq!(int(i128::MIN).neg(host).unwrap(), power_of_two(127));
    let all = hex("ffffffffffffffffffffffffffffffffffffffffffffffff");
    assert_eq!(all.add(&int(1), host).unwrap(), power_of_two(192));
    assert_eq!(power_of_two(192).sub(&int(1), host).unwrap(), all);
}

#[test]
fn multiplication_corrects_both_signs() {
    let host = &mut UnlimitedBigIntHost;
    let a = (1i128 << 64) - 1;
    let b = (1i128 << 62) + 1;
    for a in [-a, 0, a] {
        for b in [-b, 0, b] {
            assert_eq!(int(a).mul(&int(b), host).unwrap(), int(a * b));
        }
    }
    assert_eq!(
        power_of_two(190).mul(&power_of_two(128), host).unwrap(),
        power_of_two(318)
    );
}

#[test]
fn division_truncates_and_preserves_dividend_remainder_sign() {
    let host = &mut UnlimitedBigIntHost;
    let a = (1 << 125) + (1 << 75) + 123;
    for a in [-a, -17, 0, 17, a] {
        let b = (1 << 66) + 17;
        for b in [-b, -(u64::MAX as i128), -1, 1, 7, u64::MAX as i128, b] {
            assert_eq!(
                int(a).divrem(&int(b), host).unwrap(),
                (int(a / b), int(a % b))
            );
        }
    }
    assert_eq!(
        int(0).divrem(&int(0), host),
        Err(BigIntError::DivisionByZero)
    );
    assert_eq!(
        int(i128::MIN).divrem(&int(-1), host).unwrap(),
        (power_of_two(127), int(0))
    );
}

#[test]
fn division_repairs_quotient_estimates() {
    let d = hex("8000000000000000ffffffffffffffff");
    let r = hex("8000000000000000fffffffffffffffe");
    for (dividend, quotient) in [
        ("10000000000000001fffffffffffffffd", "1"),
        (
            "400000000000000100000000000000007ffffffffffffffe",
            "8000000000000000",
        ),
        (
            "8000000000000000fffffffffffffffeffffffffffffffff",
            "ffffffffffffffff",
        ),
    ] {
        assert_eq!(
            hex(dividend).divrem(&d, &mut UnlimitedBigIntHost).unwrap(),
            (hex(quotient), r.clone())
        );
    }
}

#[test]
fn powers_cover_zero_units_and_both_signs() {
    let host = &mut UnlimitedBigIntHost;
    let huge = power_of_two(100);
    let huge_odd = huge.add(&int(1), host).unwrap();
    assert_eq!(int(0).pow(&int(0), host).unwrap(), int(1));
    assert_eq!(int(0).pow(&huge, host).unwrap(), int(0));
    assert_eq!(int(1).pow(&huge, host).unwrap(), int(1));
    assert_eq!(int(-1).pow(&huge, host).unwrap(), int(1));
    assert_eq!(int(-1).pow(&huge_odd, host).unwrap(), int(-1));
    assert_eq!(
        int(-8).pow(&int(101), host).unwrap(),
        power_of_two(303).neg(host).unwrap()
    );
    assert_eq!(int(-3).pow(&int(32), host).unwrap(), int(3i128.pow(32)));
    assert_eq!(int(-3).pow(&int(33), host).unwrap(), int((-3i128).pow(33)));
    assert_eq!(
        int(0).pow(&int(-1), host),
        Err(BigIntError::NegativeExponent)
    );
    assert_eq!(int(3).pow(&huge, host), Err(TOO_LARGE));
}

#[test]
fn bitwise_operations_use_infinite_sign_extension() {
    let host = &mut UnlimitedBigIntHost;
    let cases = [i128::MIN, -(1 << 75), -1, 0, 1, 1 << 75, i128::MAX];
    for a in cases {
        assert_eq!(int(a).bitnot(host).unwrap(), int(!a));
        for b in cases {
            assert_eq!(int(a).bitand(&int(b), host).unwrap(), int(a & b));
            assert_eq!(int(a).bitor(&int(b), host).unwrap(), int(a | b));
            assert_eq!(int(a).bitxor(&int(b), host).unwrap(), int(a ^ b));
        }
    }
}

#[test]
fn shifts_reverse_direction_for_negative_counts() {
    let host = &mut UnlimitedBigIntHost;
    for a in [-0x1234i128, -1, 0, 1, 0x1234] {
        for k in [0, 1, 31, 32, 63, 64, 65, 100] {
            assert_eq!(int(a).shl(&int(k), host).unwrap(), int(a << k));
            assert_eq!(int(a).sar(&int(-k), host).unwrap(), int(a << k));
            assert_eq!(int(a).sar(&int(k), host).unwrap(), int(a >> k));
            assert_eq!(int(a).shl(&int(-k), host).unwrap(), int(a >> k));
        }
        let huge = power_of_two(130);
        assert_eq!(int(a).sar(&huge, host).unwrap(), int(a >> 127));
        assert_eq!(
            int(a).shl(&huge.neg(host).unwrap(), host).unwrap(),
            int(a >> 127)
        );
        assert_eq!(
            int(a).shl(&int(i64::MIN as i128), host).unwrap(),
            int(a >> 127)
        );
        assert_eq!(
            int(a).sar(&int(i64::MIN as i128), host),
            if a == 0 { Ok(int(0)) } else { Err(TOO_LARGE) }
        );
    }
}

#[test]
fn as_n_truncates_and_extends_across_word_edges() {
    let host = &mut UnlimitedBigIntHost;
    for a in [i128::MIN, -955, -1, 0, 1, 955, i128::MAX] {
        for bits in [0, 1, 8, 31, 32, 63, 64, 65, 127] {
            let signed = if bits == 0 {
                0
            } else {
                (a << (128 - bits)) >> (128 - bits)
            };
            let unsigned = a & ((1u128 << bits) - 1) as i128;
            assert_eq!(int(a).as_int_n(bits, host).unwrap(), int(signed));
            assert_eq!(int(a).as_uint_n(bits, host).unwrap(), int(unsigned));
        }
        assert_eq!(int(a).as_int_n(u64::MAX, host).unwrap(), int(a));
        assert_eq!(
            int(a).as_uint_n(u64::MAX, host),
            if a < 0 { Err(TOO_LARGE) } else { Ok(int(a)) }
        );
    }
}

#[test]
fn max_bits_is_a_magnitude_limit() {
    let host = &mut UnlimitedBigIntHost;
    let max = power_of_two(MAX_BITS - 1);
    let neg = max.neg(host).unwrap();
    let all = int(-1).as_uint_n(MAX_BITS as u64, host).unwrap();
    assert_eq!(max.bit_len(), MAX_BITS);
    assert_eq!(neg.bit_len(), MAX_BITS);
    assert_eq!(all.bit_len(), MAX_BITS);
    assert_eq!(max.as_int_n(MAX_BITS as u64, host).unwrap(), neg);
    assert_eq!(max.as_int_n(MAX_BITS as u64 + 1, host).unwrap(), max);
    assert_eq!(all.as_uint_n(MAX_BITS as u64 + 1, host).unwrap(), all);
    assert_eq!(max.add(&max, host), Err(TOO_LARGE));
    assert_eq!(neg.sub(&max, host), Err(TOO_LARGE));
    assert_eq!(all.bitnot(host), Err(TOO_LARGE));
    assert_eq!(max.shl(&int(1), host), Err(TOO_LARGE));
    assert_eq!(max.mul(&int(2), host), Err(TOO_LARGE));
    let below = max.sub(&int(1), host).unwrap();
    assert_eq!(below.mul(&int(3), host), Err(TOO_LARGE));
    assert_eq!(int(-1).as_uint_n(MAX_BITS as u64 + 1, host), Err(TOO_LARGE));
    assert_eq!(int(2).pow(&int(MAX_BITS as i128 - 1), host).unwrap(), max);
    assert_eq!(int(2).pow(&int(MAX_BITS as i128), host), Err(TOO_LARGE));
    assert_eq!(int(3).pow(&int(MAX_BITS as i128), host), Err(TOO_LARGE));
    assert_eq!(max.mul(&int(1), host).unwrap(), max);
    assert_eq!(max.divrem(&int(-1), host).unwrap(), (neg, int(0)));
    let mut binary = max.to_string_radix(2, host).unwrap();
    assert_eq!(binary.len(), MAX_BITS);
    assert_eq!(BigInt::parse_radix(&binary, 2, host).unwrap(), max);
    binary.push('0');
    assert_eq!(BigInt::parse_radix(&binary, 2, host), Err(TOO_LARGE));
    assert_eq!(
        BigInt::parse_radix(&"1".repeat(MAX_BITS + 1), 3, host),
        Err(TOO_LARGE)
    );
}

#[test]
fn string_parsing_uses_ecmascript_whitespace_and_prefixes() {
    let host = &mut UnlimitedBigIntHost;
    for (s, value) in [
        ("", 0),
        ("\u{2029}\u{feff}\r\n\t", 0),
        ("\u{a0}-123\u{2028}", -123),
        ("+123", 123),
        ("01", 1),
        ("-0", 0),
        ("0xA4", 164),
        ("0Xff", 255),
        (" 0b0001", 1),
        ("0B11", 3),
        ("0o077", 63),
        ("0O077", 63),
    ] {
        assert_eq!(BigInt::from_str_js(s, host).unwrap(), int(value), "{s:?}");
    }
    for s in [
        "+", "-", "-0x1", "+0b1", "0o8", "0x", "1_0", "123n", "1.0", "1e1", "\u{0085}", "\u{180e}",
        "０", "1 0", "1\0",
    ] {
        assert_eq!(
            BigInt::from_str_js(s, host),
            Err(BigIntError::InvalidString),
            "{s:?}"
        );
    }
}

#[test]
fn all_radixes_round_trip_with_ascii_digits() {
    let host = &mut UnlimitedBigIntHost;
    let a = hex("800000000000000000ffe0ccc2bb98cb1ade9192626");
    for radix in 2..=36 {
        for a in [int(0), a.clone(), a.neg(host).unwrap()] {
            let s = a.to_string_radix(radix, host).unwrap();
            assert_eq!(BigInt::parse_radix(&s, radix, host).unwrap(), a);
            assert_eq!(
                BigInt::parse_radix(&s.to_ascii_uppercase(), radix, host).unwrap(),
                a
            );
        }
    }
    assert_eq!(int(35).to_string_radix(36, host).unwrap(), "z");
    assert_eq!(BigInt::parse_radix("+007f", 16, host).unwrap(), int(127));
    for s in ["", "-", "+", "0x1", " 1", "2"] {
        assert_eq!(
            BigInt::parse_radix(s, 2, host),
            Err(BigIntError::InvalidString)
        );
    }
    assert!(matches!(
        BigInt::parse_radix("0", 1, host),
        Err(BigIntError::RangeError(_))
    ));
    assert!(matches!(
        int(0).to_string_radix(37, host),
        Err(BigIntError::RangeError(_))
    ));
}

#[test]
fn float_conversions_handle_fractions_and_special_values() {
    let host = &mut UnlimitedBigIntHost;
    for (v, expected) in [
        (-1.75, -1),
        (0.75, 0),
        (1.75, 1),
        (-0.0, 0),
        (f64::from_bits(1), 0),
        (-f64::from_bits(1), 0),
    ] {
        assert_eq!(BigInt::from_f64_trunc(v, host).unwrap(), int(expected));
        if v != 0.0 {
            assert!(matches!(
                BigInt::from_f64_exact(v, host),
                Err(BigIntError::RangeError(_))
            ));
        }
    }
    for f in [
        f64::MAX,
        -f64::MAX,
        2f64.powi(80),
        -2f64.powi(80),
        -0.0,
        1.0,
    ] {
        assert_eq!(BigInt::from_f64_exact(f, host).unwrap().to_f64(), f);
    }
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            BigInt::from_f64_trunc(v, host),
            Err(BigIntError::RangeError(_))
        ));
    }
    assert_eq!(int(0).to_f64().to_bits(), 0);
}

#[test]
fn float_rounding_is_nearest_even_and_can_overflow() {
    let host = &mut UnlimitedBigIntHost;
    let big = 1i128 << 53;
    for (i, want) in [(1, 0), (2, 2), (3, 4), (5, 4), (7, 8)] {
        assert_eq!(int(big + i).to_f64(), (big + want) as f64);
        assert_eq!(int(-big - i).to_f64(), (-big - want) as f64);
    }
    let bit = power_of_two(90);
    let half_ulp = power_of_two(37);
    let tie = bit.add(&half_ulp, host).unwrap();
    assert_eq!(tie.to_f64(), bit.to_f64());
    let above_tie = tie.add(&int(1), host).unwrap();
    assert_eq!(above_tie.to_f64().to_bits(), bit.to_f64().to_bits() + 1);
    assert_eq!(above_tie.neg(host).unwrap().to_f64(), -above_tie.to_f64());
    let infinity_tie = power_of_two(1024).sub(&power_of_two(970), host).unwrap();
    assert_eq!(infinity_tie.to_f64(), f64::INFINITY);
    assert_eq!(infinity_tie.neg(host).unwrap().to_f64(), f64::NEG_INFINITY);
    assert_eq!(infinity_tie.sub(&int(1), host).unwrap().to_f64(), f64::MAX);
    assert_eq!(power_of_two(1050).to_f64(), f64::INFINITY);
}

#[test]
fn float_comparisons_keep_integer_precision() {
    let host = &mut UnlimitedBigIntHost;
    let a = power_of_two(512);
    let a1 = a.add(&int(1), host).unwrap();
    assert_eq!(a.cmp_f64(a.to_f64()), Some(Ordering::Equal));
    assert_eq!(a1.cmp_f64(a.to_f64()), Some(Ordering::Greater));
    assert_eq!(
        a1.neg(host).unwrap().cmp_f64(-a.to_f64()),
        Some(Ordering::Less)
    );
    assert_eq!(int(1).cmp_f64(1.5), Some(Ordering::Less));
    assert_eq!(int(-1).cmp_f64(-1.5), Some(Ordering::Greater));
    assert_eq!(int(0).cmp_f64(-0.0), Some(Ordering::Equal));
    assert_eq!(int(0).cmp_f64(-f64::from_bits(1)), Some(Ordering::Greater));
    assert_eq!(a.cmp_f64(f64::NAN), None);
    assert_eq!(a.cmp_f64(f64::INFINITY), Some(Ordering::Less));
    assert_eq!(a.cmp_f64(f64::NEG_INFINITY), Some(Ordering::Greater));
}

#[test]
fn negative_float_comparisons_reverse_exponent_order() {
    // QuickJS-ng 0.17.0 misses the sign reversal when the exponents differ.
    assert_eq!(int(-2).cmp_f64(-0.5), Some(Ordering::Less));
    assert_eq!(int(-1).cmp_f64(-2.0), Some(Ordering::Greater));
    assert_eq!(int(-1).cmp_f64(-f64::from_bits(1)), Some(Ordering::Less));
    assert_eq!(
        power_of_two(1024)
            .neg(&mut UnlimitedBigIntHost)
            .unwrap()
            .cmp_f64(-f64::MAX),
        Some(Ordering::Less)
    );
}

#[derive(Default)]
struct TestHost {
    used: usize,
    peak: usize,
    charges: usize,
    polls: usize,
    limit: Option<usize>,
    fail_at: Option<usize>,
    interrupt_at: Option<usize>,
}

impl BigIntHost for TestHost {
    fn charge(&mut self, bytes: usize) -> Result<(), BigIntError> {
        assert_ne!(bytes, 0);
        self.charges += 1;
        let next = self.used + bytes;
        if Some(self.charges) == self.fail_at || self.limit.is_some_and(|limit| next > limit) {
            return Err(BigIntError::OutOfMemory);
        }
        self.used = next;
        self.peak = self.peak.max(next);
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        assert!(bytes > 0 && bytes <= self.used);
        self.used -= bytes;
    }

    fn poll_interrupt(&mut self) -> bool {
        self.polls += 1;
        self.interrupt_at.is_some_and(|n| self.polls >= n)
    }
}

#[test]
fn host_cap_is_checked_before_growth() {
    let mut host = TestHost {
        limit: Some(7),
        ..TestHost::default()
    };
    assert_eq!(
        int(1).add(&int(1), &mut host),
        Err(BigIntError::OutOfMemory)
    );
    assert_eq!(
        BigInt::from_f64_trunc(0.75, &mut host),
        Err(BigIntError::OutOfMemory)
    );
    assert_eq!((host.used, host.peak, host.charges), (0, 0, 2));
    host.limit = Some(8);
    let a = BigInt::parse_radix("123", 10, &mut host).unwrap();
    assert_eq!(host.used, a.byte_size());
    assert_eq!(a.neg(&mut host), Err(BigIntError::OutOfMemory));
    assert_eq!(host.used, 8);
    let bytes = a.byte_size();
    drop(a);
    host.release(bytes);
    assert_eq!(
        BigInt::from_f64_trunc(2f64.powi(100), &mut host),
        Err(BigIntError::OutOfMemory)
    );
    assert_eq!(
        int(i64::MAX as i128).add(&int(1), &mut host),
        Err(BigIntError::OutOfMemory)
    );
    assert_eq!(host.used, 0);
    assert_eq!(host.peak, 8);
}

#[test]
fn host_releases_each_temporary_and_failed_result() {
    fn check<T: std::fmt::Debug>(
        op: impl Fn(&mut TestHost) -> Result<T, BigIntError>,
        size: impl Fn(T) -> usize,
    ) {
        let mut host = TestHost::default();
        let result = op(&mut host).unwrap();
        assert_eq!(host.used, size(result));
        for n in 1..=host.charges {
            let mut fail = TestHost {
                fail_at: Some(n),
                ..TestHost::default()
            };
            assert_eq!(
                op(&mut fail).unwrap_err(),
                BigIntError::OutOfMemory,
                "charge {n}"
            );
            assert_eq!(fail.used, 0, "charge {n}");
        }
    }
    let a = hex(&"feddcc31".repeat(150));
    let d = hex(&"530f29f4".repeat(40))
        .neg(&mut UnlimitedBigIntHost)
        .unwrap();
    check(
        |host| a.divrem(&d, host),
        |(q, r)| q.byte_size() + r.byte_size(),
    );
    check(
        |host| a.divrem(&int(-7), host),
        |(q, r)| q.byte_size() + r.byte_size(),
    );
    check(
        |host| int(-1).divrem(&a, host),
        |(q, r)| q.byte_size() + r.byte_size(),
    );
    check(|host| a.mul(&d, host), |n| n.byte_size());
    check(|host| a.pow(&int(7), host), |n| n.byte_size());
    check(
        |host| BigInt::parse_radix(&"0123456789".repeat(200), 10, host),
        |n| n.byte_size(),
    );
    check(|host| a.to_string_radix(13, host), |s| s.capacity());
    let mut host = TestHost::default();
    assert_eq!(
        power_of_two(MAX_BITS - 1).add(&power_of_two(MAX_BITS - 1), &mut host),
        Err(TOO_LARGE)
    );
    assert_eq!(host.used, 0);
}

#[test]
fn host_interrupts_inner_loops_and_releases_scratch() {
    fn check<T: std::fmt::Debug>(
        poll: usize,
        op: impl FnOnce(&mut TestHost) -> Result<T, BigIntError>,
    ) {
        let mut host = TestHost {
            interrupt_at: Some(poll),
            ..TestHost::default()
        };
        assert_eq!(op(&mut host).unwrap_err(), BigIntError::Interrupted);
        assert_eq!(host.polls, poll);
        assert_eq!(host.used, 0);
        assert!(host.peak > 0);
    }
    let a = hex(&"feedface9abcdef0".repeat(180));
    let d = hex(&"bbc3205f".repeat(91));
    check(2, |h| a.mul(&d, h));
    check(3, |h| a.divrem(&d, h));
    check(3, |h| a.pow(&int(7), h));
    check(7, |h| BigInt::parse_radix(&"456523963".repeat(120), 10, h));
    check(7, |h| BigInt::parse_radix(&"100100001".repeat(120), 2, h));
    check(2, |h| a.to_string_radix(36, h));
    check(2, |h| a.to_string_radix(8, h));
    let mut host = TestHost {
        interrupt_at: Some(1),
        ..TestHost::default()
    };
    assert_eq!(
        a.to_string_radix(2, &mut host),
        Err(BigIntError::Interrupted)
    );
    assert_eq!(host.charges, 0);
}
