//! Set `QJS_BIGINT_ORACLE` to a QuickJS-ng 0.17.0 CLI.
//! Run `cargo test -p qjs-bigint --test quickjs -- --ignored`.

use qjs_bigint::{BigInt, BigIntError, MAX_BITS, UnlimitedBigIntHost};
use std::cmp::Ordering;
use std::process::Command;

const JS: &str = r#"
const buf = new DataView(new ArrayBuffer(8));
function f64(bits) { buf.setBigUint64(0, bits); return buf.getFloat64(0); }
function numberBits(n) { buf.setFloat64(0, n); return buf.getBigUint64(0).toString(16); }
function cmp(a, b) { return a < b ? -1 : a > b ? 1 : a == b ? 0 : 'unordered'; }
function parseRadix(s, radix) {
    const n = [...s.replace(/^-/, '')].reduce((n, c) => n * BigInt(radix) + BigInt(parseInt(c, radix)), 0n);
    return s.startsWith('-') ? -n : n;
}
function emit(f) { try { print(String(f())); } catch (e) { print(e.name); } }
"#;

struct Sample {
    n: BigInt,
    js: String,
}

impl Sample {
    fn new(hex: &str) -> Self {
        Self {
            n: BigInt::parse_radix(hex, 16, &mut UnlimitedBigIntHost).unwrap(),
            js: match hex.strip_prefix('-') {
                Some(mag) => format!("(-0x{mag}n)"),
                None => format!("0x{hex}n"),
            },
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bigint(&mut self, limbs: usize, negative: bool) -> Sample {
        use std::fmt::Write;
        let mut value = String::new();
        if negative {
            value.push('-');
        }
        for _ in 0..limbs {
            write!(&mut value, "{:016x}", self.next()).unwrap();
        }
        Sample::new(&value)
    }
}

#[derive(Default)]
struct Checks(Vec<(String, String)>);

impl Checks {
    fn push(&mut self, expr: String, value: impl ToString) {
        self.0.push((expr, value.to_string()));
    }

    fn integer(&mut self, expr: String, n: Result<BigInt, BigIntError>) {
        let value = match n {
            Ok(n) => n.to_string_radix(10, &mut UnlimitedBigIntHost).unwrap(),
            Err(BigIntError::InvalidString) => "SyntaxError".into(),
            Err(
                BigIntError::RangeError(_)
                | BigIntError::DivisionByZero
                | BigIntError::NegativeExponent,
            ) => "RangeError".into(),
            Err(e) => panic!("{e:?}"),
        };
        self.push(expr, value);
    }

    fn comparison(&mut self, expr: String, cmp: Option<Ordering>) {
        self.push(
            expr,
            match cmp {
                Some(Ordering::Less) => "-1",
                Some(Ordering::Greater) => "1",
                Some(Ordering::Equal) => "0",
                None => "unordered",
            },
        );
    }

    fn number(&mut self, a: &Sample) {
        self.push(
            format!("numberBits(Number({}))", a.js),
            format!("{:x}", a.n.to_f64().to_bits()),
        );
    }

    fn compare_number(&mut self, a: &Sample, d: f64) {
        self.comparison(
            format!("cmp({}, f64(0x{:x}n))", a.js, d.to_bits()),
            a.n.cmp_f64(d),
        );
    }

    fn run(self, qjs: &str) -> usize {
        let mut offset = 0;
        let mut batches = 0;
        while offset < self.0.len() {
            let mut script = String::from(JS);
            let start = offset;
            while offset < self.0.len() && script.len() < 60_000 {
                let (expr, _) = &self.0[offset];
                use std::fmt::Write;
                writeln!(script, "emit(() => ({expr}));").unwrap();
                offset += 1;
            }
            let output = Command::new(qjs).arg("-e").arg(script).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let actual: Vec<_> = stdout.lines().collect();
            assert_eq!(actual.len(), offset - start);
            for (line, (expr, want)) in actual.into_iter().zip(&self.0[start..offset]) {
                assert_eq!(line, want, "{expr}");
            }
            batches += 1;
        }
        eprintln!(
            "{} QuickJS checks in {batches} batches, 0 mismatches",
            self.0.len()
        );
        self.0.len()
    }
}

#[test]
#[ignore = "requires QJS_BIGINT_ORACLE pointing to QuickJS-ng 0.17.0"]
fn matches_quickjs_v017() {
    let qjs =
        std::env::var("QJS_BIGINT_ORACLE").expect("set QJS_BIGINT_ORACLE to the QuickJS-ng CLI");
    let version = Command::new(&qjs).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(String::from_utf8(version.stdout).unwrap().trim(), "0.17.0");

    let mut rng = Rng(0xc429_92a6_6734_3bc5);
    let mut checks = Checks::default();
    let host = &mut UnlimitedBigIntHost;
    for i in 0..320 {
        let a = rng.bigint(2 + i % 16, i & 1 != 0);
        let b = rng.bigint(1 + (i * 5) % 19, i & 2 != 0);
        let (x, y) = (&a.js, &b.js);
        checks.integer(format!("{x} + {y}"), a.n.add(&b.n, host));
        checks.integer(format!("{x} - {y}"), a.n.sub(&b.n, host));
        checks.integer(format!("{x} * {y}"), a.n.mul(&b.n, host));
        let (quot, rem) = a.n.divrem(&b.n, host).unwrap();
        checks.integer(format!("{x} / {y}"), Ok(quot));
        checks.integer(format!("{x} % {y}"), Ok(rem));
        checks.integer(format!("{x} & {y}"), a.n.bitand(&b.n, host));
        checks.integer(format!("{x} | {y}"), a.n.bitor(&b.n, host));
        checks.integer(format!("{x} ^ {y}"), a.n.bitxor(&b.n, host));
        checks.integer(format!("~{x}"), a.n.bitnot(host));
        checks.integer(format!("-{x}"), a.n.neg(host));
        checks.comparison(format!("cmp({x}, {y})"), Some(a.n.cmp_bigint(&b.n)));
        let k = [-511, -129, -64, -32, -1, 0, 1, 31, 32, 63, 64, 65, 129, 511][i % 14];
        let shift = BigInt::from_i64(k);
        checks.integer(format!("{x} << ({k}n)"), a.n.shl(&shift, host));
        checks.integer(format!("{x} >> ({k}n)"), a.n.sar(&shift, host));
        let k = [
            0,
            1,
            31,
            32,
            33,
            63,
            64,
            65,
            127,
            128,
            129,
            200,
            613,
            9007199254740991,
        ][i % 14];
        checks.integer(format!("BigInt.asIntN({k}, {x})"), a.n.as_int_n(k, host));
        checks.integer(format!("BigInt.asUintN({k}, {x})"), a.n.as_uint_n(k, host));
        checks.number(&a);
        let rounded = a.n.to_f64();
        if rounded.is_finite() {
            checks.compare_number(&a, rounded);
            let other = f64::from_bits(rounded.to_bits() ^ 1);
            checks.compare_number(&a, other);
        }
        checks.compare_number(
            &a,
            if a.n.is_negative() {
                0.75
            } else {
                f64::from_bits(1)
            },
        );
        if i % 3 == 0 {
            let e = (i * 5 % 24) as u64;
            checks.integer(format!("{y} ** {e}n"), b.n.pow(&BigInt::from_u64(e), host));
        }
    }
    for a in [
        Sample::new("0"),
        Sample::new("1"),
        Sample::new("-1"),
        Sample::new("-11377ffffa891123f6"),
        Sample::new("3ffffacb27161087fff"),
    ] {
        let huge = Sample::new("10000000000000000000000000");
        let huge_neg = Sample::new("-10000000000000000000000000");
        let x = &a.js;
        for s in [&huge, &huge_neg] {
            checks.integer(format!("{x} << {}", s.js), a.n.shl(&s.n, host));
            checks.integer(format!("{x} >> {}", s.js), a.n.sar(&s.n, host));
            checks.integer(format!("{x} ** {}", s.js), a.n.pow(&s.n, host));
        }
        checks.integer(
            format!("{x} / 0n"),
            a.n.divrem(&BigInt::zero(), host).map(|(q, _)| q),
        );
        let min = BigInt::from_i64(i64::MIN);
        checks.integer(format!("{x} << -(1n << 63n)"), a.n.shl(&min, host));
        checks.integer(format!("{x} >> -(1n << 63n)"), a.n.sar(&min, host));
        checks.compare_number(&a, f64::INFINITY);
        checks.compare_number(&a, f64::NEG_INFINITY);
        checks.compare_number(&a, f64::NAN);
        checks.compare_number(&a, -0.0);
    }

    for exponent in [
        53, 54, 62, 63, 64, 65, 85, 116, 117, 118, 128, 255, 511, 1022, 1023, 1024, 1200,
    ] {
        let base = BigInt::from_u64(1)
            .shl(&BigInt::from_u64(exponent), host)
            .unwrap();
        let half = BigInt::from_u64(1)
            .shl(&BigInt::from_u64(exponent - 53), host)
            .unwrap();
        for scale in [-1, 0, 1, 2, 3] {
            let tie = base
                .add(&half.mul(&BigInt::from_i64(scale), host).unwrap(), host)
                .unwrap();
            for off in [-1, 0, 1] {
                let n = tie.add(&BigInt::from_i64(off), host).unwrap();
                let hex = n.to_string_radix(16, host).unwrap();
                checks.number(&Sample::new(&hex));
                checks.number(&Sample::new(&format!("-{hex}")));
            }
        }
    }
    let at_overflow = BigInt::from_f64_exact(f64::MAX, host)
        .unwrap()
        .add(&BigInt::from_f64_exact(2f64.powi(970), host).unwrap(), host)
        .unwrap();
    for offset in [-1, 0, 1] {
        let value = at_overflow.add(&BigInt::from_i64(offset), host).unwrap();
        let hex = value.to_string_radix(16, host).unwrap();
        checks.number(&Sample::new(&hex));
        checks.number(&Sample::new(&format!("-{hex}")));
    }
    for (hex, number) in [
        ("1", 1.25),
        ("1", 0.75),
        ("-1", -1.25),
        ("-3", -2.75),
        ("20000000000001", 9_007_199_254_740_992.0),
        ("-20000000000001", -9_007_199_254_740_992.0),
    ] {
        checks.compare_number(&Sample::new(hex), number);
    }
    for raw in [
        0,
        1,
        0x8000000000000001,
        0x3ff8000000000000,
        0xbff8000000000000,
        0xfff8000000000000,
        0x7ff0000000000000,
        0xfff0000000000000,
        0x7fefffffffffffff,
        0xffefffffffffffff,
        0x8000000000000000,
    ]
    .into_iter()
    .chain((0..200).map(|_| rng.next()))
    {
        let f = f64::from_bits(raw);
        checks.integer(
            format!("BigInt(f64(0x{raw:x}n))"),
            BigInt::from_f64_exact(f, host),
        );
        checks.integer(
            format!("BigInt(Math.trunc(f64(0x{raw:x}n)))"),
            BigInt::from_f64_trunc(f, host),
        );
    }

    let radix_samples: Vec<_> = (0..18).map(|i| rng.bigint(1 + i * 2, i % 2 == 0)).collect();
    for radix in 2..=36 {
        for a in &radix_samples {
            let digits = a.n.to_string_radix(radix, host).unwrap();
            checks.push(format!("{}.toString({radix})", a.js), &digits);
            checks.integer(
                format!("parseRadix({digits:?}, {radix})"),
                BigInt::parse_radix(&digits, radix, host),
            );
        }
    }
    for s in [
        "",
        "\u{feff}\u{a0}",
        " -123456789012345678901234567890 ",
        "+11111111111111111111111111",
        "-0",
        " 0x123ABbCDeff20e4",
        "0Xabcd",
        "0o7654321076543210",
        "0B1001010110001",
        "00001",
        "-0x03",
        "+0b11",
        "0o",
        "0O8",
        "0b2",
        "1_000",
        "12n",
        "\u{0085}",
        "\u{180e}",
        "1.0",
        "1e0",
        "１２３",
    ] {
        checks.integer(format!("BigInt({s:?})"), BigInt::from_str_js(s, host));
    }
    let max = MAX_BITS as u64;
    let all = BigInt::from_i64(-1).as_uint_n(max, host).unwrap();
    checks.integer(
        format!("BigInt.asUintN({max}, -1n) >> {}n", max - 1),
        all.sar(&BigInt::from_u64(max - 1), host),
    );
    checks.integer(
        format!("BigInt.asUintN({}, -1n)", max + 1),
        BigInt::from_i64(-1).as_uint_n(max + 1, host),
    );
    checks.integer(
        format!("1n << {max}n"),
        BigInt::from_u64(1).shl(&BigInt::from_u64(max), host),
    );
    assert!(checks.run(&qjs) > 8_000);
}
