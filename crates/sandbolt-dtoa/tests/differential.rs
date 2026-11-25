#![deny(unsafe_code)]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use sandbolt_dtoa::{
    AtodFlags, DtoaExp, DtoaFormat, DtoaOptions, atod, dtoa, dtoa_max_len, i32toa, i64toa,
    i64toa_radix, u32toa, u64toa, u64toa_radix,
};

const SIGN: u64 = 1 << 63;
const FRACTION: u64 = (1 << 52) - 1;
const FORMATS: [DtoaFormat; 3] = [DtoaFormat::Free, DtoaFormat::Fixed, DtoaFormat::Frac];
const EXPONENTS: [DtoaExp; 3] = [DtoaExp::Auto, DtoaExp::Enabled, DtoaExp::Disabled];
const RADICES: [u32; 5] = [2, 8, 10, 16, 36];
const ASCII: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u32) -> u32 {
        (self.next() % u64::from(bound)) as u32
    }
}

fn float_inputs(rng: &mut Random) -> Vec<u64> {
    let mut values = BTreeSet::new();
    for bits in [
        0,
        1,
        FRACTION,
        1 << 52,
        f64::MAX.to_bits(),
        f64::INFINITY.to_bits(),
        f64::NAN.to_bits(),
        f64::INFINITY.to_bits() + 1,
        SIGN - 1,
    ] {
        values.extend([bits, bits | SIGN]);
    }
    let mut around = |bits: u64| {
        for offset in -2..=2 {
            let b = bits.saturating_add_signed(offset);
            values.extend([b, b | SIGN]);
        }
    };
    for exp in -1074..=1023 {
        around(if exp < -1022 {
            1 << (exp + 1074)
        } else {
            ((exp + 1023) as u64) << 52
        });
    }
    for exp in -324..=309 {
        around(format!("1e{exp}").parse::<f64>().unwrap().to_bits());
    }
    for n in 0..=256 {
        for fraction in [0.125, 0.25, 0.5, 0.75, 0.875] {
            around((f64::from(n) + fraction).to_bits());
        }
    }
    for _ in 0..2048 {
        values.insert(rng.next() & (SIGN | FRACTION));
    }
    while values.len() < 52_000 {
        values.insert(rng.next());
    }
    values.into_iter().collect()
}

fn format_cases(floats: &[u64]) -> impl Iterator<Item = (u64, u32, usize, DtoaOptions)> + '_ {
    let primary = floats
        .iter()
        .copied()
        .enumerate()
        .flat_map(|(i, bits)| RADICES.into_iter().map(move |radix| (i, bits, radix)));
    let other = floats
        .iter()
        .copied()
        .enumerate()
        .step_by(173)
        .flat_map(|(i, bits)| {
            (2..=36)
                .filter(|radix| !RADICES.contains(radix))
                .map(move |radix| (i, bits, radix))
        });
    primary.chain(other).flat_map(|(i, bits, radix)| {
        FORMATS.into_iter().flat_map(move |format| {
            let digits = match format {
                DtoaFormat::Free => 0,
                DtoaFormat::Fixed => [1, 2, 3, 16, 17, 38, 100, 101][i % 8],
                DtoaFormat::Frac => [0, 1, 2, 3, 16, 20, 38, 50, 100, 101][i % 10],
            };
            EXPONENTS.into_iter().flat_map(move |exp| {
                [false, true].into_iter().map(move |minus_zero| {
                    (
                        bits,
                        radix,
                        digits,
                        DtoaOptions {
                            format,
                            exp,
                            minus_zero,
                        },
                    )
                })
            })
        })
    })
}

fn parse_cases(rng: &mut Random) -> Vec<(String, u32, u32)> {
    let mut cases = Vec::new();
    for text in [
        "",
        " ",
        "\t\n1",
        "\u{feff}1",
        "-",
        "+",
        ".",
        "-.",
        "+.5",
        "-0",
        "+0",
        "1.",
        "1.2.3",
        "-0e999999999999",
        "-1e-999999999999",
        "1e+999999999999",
        "1e2147483638",
        "1e2147483648",
        "1e",
        "-1e+",
        "+1E-",
        "1e_2",
        "1e1_0",
        "0x",
        "-0x.",
        "0x.1",
        "-0X1Ftail",
        "0X1p+4",
        "0x1p-1074",
        "-0x1p-1075",
        "0x1.0000000000001p-1075",
        "0x1.fffffffffffffp1023",
        "0x1.fffffffffffff8p1023",
        "0x1.fffffffffffffp-1023",
        "0x1@1073741824",
        "0x1@536870912",
        "0x1@1073741825",
        "0x1@-1073741825",
        "0x1@2147483639",
        "0b",
        "0B1011.01p+3",
        "0b112",
        "0B_1",
        "0b1_01",
        "0o",
        "0o777",
        "+0O7p-2",
        "0o78",
        "00",
        "01234567",
        "0123456789",
        "071_1",
        "08_1",
        "07e2",
        "00._0",
        "01.2",
        "0_7",
        "012_9",
        "0_1_2.3_4e+0_5!",
        "1__2",
        "1_",
        "_0",
        "1._0",
        "1._1",
        "._1",
        "1_.2",
        "1\0tail",
        "3é",
        "−3",
        "Infinity",
        "-InfinityTail",
        "+Infinity",
        "INFINITY",
        "inf",
        "NaN",
        "1.7976931348623157e308",
        "1.7976931348623159e308",
        "2.2250738585072011e-308",
        "2.2250738585072012e-308",
        "4.9406564584124654e-324",
        "2.4703282292062327e-324",
        "2.4703282292062328e-324",
        "7.410984687618698e-324",
        "7.410984687618699e-324",
        "9007199254740993",
        "9007199254740995",
        "1.00000000000000011102230246251565404236316680908203125",
        "1.00000000000000033306690738754696212708950042724609375",
        "1e-325",
        "49e-325",
        "1@-1075",
        "zzz@-50",
        "1p-1024",
        "1234@1019",
        "10P+2",
        "1@_2",
        "1@+2_3",
        "A.F@+19",
    ] {
        for radix in [0, 2, 8, 10, 16, 32, 36] {
            for flags in 0..16 {
                cases.push((text.into(), radix, flags));
            }
        }
    }

    for i in 0..24_000 {
        let (radix, explicit, prefix) = match i % 12 {
            0 => (10, 0, ""),
            1 => (16, 0, "0x"),
            2 => (2, 0, "0b"),
            3 => (8, 0, "0O"),
            4 => (8, 0, "0"),
            5 => (16, 16, "0X"),
            6 => (10, 10, ""),
            _ => {
                let radix = rng.below(35) + 2;
                (radix, radix, "")
            }
        };
        let flags = rng.below(16);
        let mut text = match rng.below(5) {
            0 => "-".to_owned(),
            1 => "+".to_owned(),
            _ => String::new(),
        };
        text.push_str(prefix);
        let len = if i % 17 == 0 { 300 } else { rng.below(96) };
        let point = rng.below(len + 4);
        for j in 0..len {
            if j == point {
                text.push('.');
            }
            if j > 0 && rng.below(16) == 0 {
                text.push('_');
            }
            let byte = ASCII[rng.below(radix) as usize];
            text.push(char::from(if rng.below(3) == 0 {
                byte.to_ascii_uppercase()
            } else {
                byte
            }));
        }
        if i % 5 != 0 {
            text.push(if radix == 10 {
                'e'
            } else if radix.is_power_of_two() && radix < 32 && i % 3 == 0 {
                'p'
            } else {
                '@'
            });
            match i % 16 {
                0 => text.push_str("+99999_99_99999999"),
                1 => text.push_str("-99999999_99999999"),
                2 => text.push_str("+2147483647"),
                3 => text.push_str("-2147483639"),
                4 => text.push('+'),
                _ => text.push_str(&(rng.below(2400) as i32 - 1200).to_string()),
            }
        }
        text.push_str(match i % 9 {
            0 => "_",
            1 => "\0ignored",
            2 => "!tail",
            3 => "é!",
            4 => "\t ",
            _ => "",
        });
        cases.push((text, explicit, flags));
    }
    cases
}

fn integer_inputs(rng: &mut Random) -> Vec<u64> {
    let mut values = BTreeSet::from([0, 1, SIGN, SIGN - 1, SIGN + 1, u64::MAX]);
    for center in [u64::from(u32::MAX), i32::MAX as u64, i32::MIN as i64 as u64] {
        values.extend([center - 1, center, center + 1]);
    }
    for n in (0..64)
        .map(|i| 1u64 << i)
        .chain((0..20).map(|i| 10u64.pow(i)))
    {
        for n in [n - 1, n, n + 1] {
            values.extend([n, n.wrapping_neg()]);
        }
    }
    for _ in 0..512 {
        values.insert(rng.next());
    }
    values.into_iter().collect()
}

fn integer_cases(inputs: &[u64]) -> impl Iterator<Item = (u64, u32, u32)> + '_ {
    inputs.iter().copied().flat_map(|n| {
        (0..4)
            .map(move |mode| (n, 10, mode))
            .chain((2..=36).flat_map(move |radix| [(n, radix, 4), (n, radix, 5)]))
    })
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("sandbolt-dtoa-{}-{time}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_line<'a>(reader: &mut impl BufRead, buf: &'a mut String) -> io::Result<&'a str> {
    buf.clear();
    reader.read_line(buf)?;
    buf.strip_suffix('\n')
        .ok_or_else(|| io::Error::other("missing reference response"))
}

fn check(mismatches: &mut usize, matches: bool, describe: impl FnOnce() -> String) {
    if !matches {
        if *mismatches < 20 {
            eprintln!("{}", describe());
        }
        *mismatches += 1;
    }
}

// QJS_REF must name a QuickJS-ng v0.17.0 checkout with build/libqjs.a.
#[test]
#[ignore = "requires the QuickJS-ng C static library and a C compiler"]
fn matches_quickjs() -> io::Result<()> {
    let reference =
        PathBuf::from(env::var_os("QJS_REF").expect("set QJS_REF to the reference checkout"));
    let scratch = Scratch::new()?;
    let c_file = scratch.0.join("reference.c");
    let executable = scratch.0.join("reference");
    fs::write(&c_file, include_bytes!("reference.c"))?;
    let status = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(&reference)
        .arg(&c_file)
        .arg(reference.join("build/libqjs.a"))
        .args(["-lm", "-o"])
        .arg(&executable)
        .status()?;
    assert!(status.success(), "reference compiler failed: {status}");
    let mut child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = BufWriter::with_capacity(65536, child.stdin.take().unwrap());
    let mut output = BufReader::with_capacity(65536, child.stdout.take().unwrap());

    let mut rng = Random(0x1234_2938_7231_e1ac);
    let floats = &float_inputs(&mut rng);
    let parses = &parse_cases(&mut rng);
    let integers = &integer_inputs(&mut rng);
    thread::scope(|scope| -> io::Result<()> {
        let writer = scope.spawn(move || -> io::Result<()> {
            for (bits, radix, digits, opts) in format_cases(floats) {
                let flags = opts.format as u32
                    | ((opts.exp as u32) << 2)
                    | (u32::from(opts.minus_zero) << 4);
                writeln!(input, "d {bits:x} {radix} {digits} {flags}")?;
            }
            for (text, radix, flags) in parses {
                write!(input, "a {radix} {flags} {} ", text.len())?;
                for byte in text.bytes() {
                    write!(input, "{byte:02x}")?;
                }
                writeln!(input)?;
            }
            for (n, radix, mode) in integer_cases(integers) {
                writeln!(input, "i {n:x} {radix} {mode}")?;
            }
            input.flush()
        });

        let mut mismatches = 0;
        let mut bound_adjustments = 0;
        let mut formats = 0;
        let mut integer_count = 0;
        let mut line = String::new();
        for (bits, radix, digits, opts) in format_cases(floats) {
            let (c_bound, expected) = read_line(&mut output, &mut line)?.split_once(' ').unwrap();
            let c_bound: usize = c_bound.parse().unwrap();
            let value = f64::from_bits(bits);
            let result = dtoa(value, radix, digits, opts);
            let bound = dtoa_max_len(value, radix, digits, opts);
            // The C bound misses leading subnormal zeroes and Frac zero's exponent.
            let adjusted = bound > c_bound
                && ((value.is_subnormal()
                    && opts.exp == DtoaExp::Disabled
                    && opts.format != DtoaFormat::Frac)
                    || (value == 0.0
                        && opts.format == DtoaFormat::Frac
                        && opts.exp == DtoaExp::Enabled));
            bound_adjustments += usize::from(adjusted);
            check(
                &mut mismatches,
                result == expected && bound >= result.len() && (bound == c_bound || adjusted),
                || {
                    format!(
                        "dtoa({bits:016x}, {radix}, {digits}, {opts:?}): Rust ({bound}, {result}), C ({c_bound}, {expected})"
                    )
                },
            );
            formats += 1;
        }
        for (text, radix, flags) in parses {
            let expected = read_line(&mut output, &mut line)?;
            let (value, consumed) = atod(
                text,
                *radix,
                AtodFlags {
                    int_only: flags & 1 != 0,
                    accept_bin_oct: flags & 2 != 0,
                    accept_legacy_octal: flags & 4 != 0,
                    accept_underscores: flags & 8 != 0,
                },
            );
            let bits = if value.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                value.to_bits()
            };
            let result = format!("{bits:016x} {consumed}");
            check(&mut mismatches, result == expected, || {
                format!("atod({text:?}, {radix}, {flags}): Rust {result}, C {expected}")
            });
        }
        for (n, radix, mode) in integer_cases(integers) {
            let expected = read_line(&mut output, &mut line)?;
            let mut buf = [0xcc; 66];
            let len = match mode {
                0 => u32toa(&mut buf, n as u32),
                1 => i32toa(&mut buf, n as i32),
                2 => u64toa(&mut buf, n),
                3 => i64toa(&mut buf, n as i64),
                4 => u64toa_radix(&mut buf, n, radix),
                _ => i64toa_radix(&mut buf, n as i64, radix),
            };
            let result = std::str::from_utf8(&buf[..len]).unwrap();
            check(
                &mut mismatches,
                result == expected && buf[len..].iter().all(|&b| b == 0xcc),
                || {
                    format!(
                        "integer helper {mode} for {n:016x} in radix {radix}: Rust {result}, C {expected}"
                    )
                },
            );
            integer_count += 1;
        }
        writer.join().unwrap()?;
        line.clear();
        assert_eq!(output.read_line(&mut line)?, 0);
        assert!(child.wait()?.success());
        println!(
            "{} float inputs, {formats} dtoa results and bounds, {} atod inputs, {integer_count} integer results, {bound_adjustments} widened bounds, {mismatches} mismatches",
            floats.len(),
            parses.len(),
        );
        assert_eq!(mismatches, 0);
        Ok(())
    })
}
