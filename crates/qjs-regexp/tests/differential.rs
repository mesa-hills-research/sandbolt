use qjs_regexp::{
    RegExpError, RegExpFlags as F, RegExpInput, UnlimitedRegExpHost, check_bytecode, compile, exec,
};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Reference(PathBuf);

impl Reference {
    fn build(test: &str) -> Self {
        let source = PathBuf::from(
            std::env::var_os("QJS_REF").expect("set QJS_REF to a built QuickJS-ng checkout"),
        );
        let output = std::env::temp_dir().join(format!("qjs-regexp-{}-{test}", std::process::id()));
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libregexp.c");
        let result = Command::new("cc")
            .arg("-I")
            .arg(&source)
            .arg(fixture)
            .arg(source.join("build/libqjs.a"))
            .args(["-lm", "-pthread", "-ldl", "-o"])
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        Self(output)
    }

    fn compare(&self, pattern: &[u8], flags: u16, input: RegExpInput<'_>, start: usize) {
        let (wide, subject) = match input {
            RegExpInput::Latin1(s) => ("0", hex(s)),
            RegExpInput::Utf16(s) => (
                "1",
                s.iter().fold(String::new(), |mut hex, unit| {
                    write!(hex, "{unit:04x}").unwrap();
                    hex
                }),
            ),
        };
        let output = Command::new(&self.0)
            .args([
                flags.to_string(),
                start.to_string(),
                wide.into(),
                hex(pattern),
                subject,
            ])
            .output()
            .unwrap();
        let label = String::from_utf8_lossy(pattern);
        assert!(
            output.status.success(),
            "pattern {label:?}, flags {flags}, input {input:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reference = String::from_utf8(output.stdout).unwrap();
        let mut host = UnlimitedRegExpHost;
        match compile(pattern, F(flags), &mut host) {
            Err(RegExpError::Syntax(error)) => assert_eq!(
                format!("error {error}\n"),
                reference,
                "pattern {label:?}, flags {flags}"
            ),
            Ok(re) => {
                check_bytecode(&re.bytecode)
                    .unwrap_or_else(|e| panic!("pattern {label:?}, flags {flags}: {e:?}"));
                let mut rust = hex(&re.bytecode);
                rust.push('\n');
                let result = exec(&re, input, start, &mut host).unwrap_or_else(|e| {
                    panic!(
                        "pattern {label:?}, flags {flags}, input {input:?}, start {start}: {e:?}"
                    )
                });
                if let Some(result) = result {
                    for span in result.captures {
                        if let Some((lo, hi)) = span {
                            write!(rust, " {lo}:{hi}").unwrap();
                        } else {
                            rust.push_str(" -");
                        }
                    }
                    rust.push('\n');
                } else {
                    rust.push_str("result 0\n");
                }
                assert_eq!(
                    rust, reference,
                    "pattern {label:?}, flags {flags}, input {input:?}, start {start}"
                );
            }
            err => panic!("{err:?}, pattern {label:?}, flags {flags}"),
        }
    }
}

impl Drop for Reference {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        write!(hex, "{byte:02x}").unwrap();
        hex
    })
}

#[test]
#[ignore = "requires QJS_REF and a C compiler"]
fn ascii_libregexp() {
    let c = Reference::build("ascii");
    let patterns = [
        "",
        "a",
        ".",
        "$",
        "a|ab|b",
        "(a|ab|b)c",
        "(a||b)c",
        "((b)|(c))",
        "(?:a{2,4})(a+?)",
        "(a{0,4}?)(a{2})",
        "a{0,3}a",
        "a{2,}a",
        "(a{3}){2}",
        "(a){0}",
        "(a(b)?)+",
        "(a|(b))*",
        "(a?)*",
        "(?:()){2,4}",
        "(a?)?",
        "(a?){1}",
        "(?=(a))*a",
        "(?<=((?:a|b)+)(b+c))$",
        "(?<!ab)c",
        r"(?=(a|ab))\1c",
        r"(?=(a))\1",
        "(?=(a))b|a",
        "(?!(a)b)a",
        "(?=(?!(b))(a))a",
        r"(?<=\1(a))b",
        "(?<=(a(?=b))b)c",
        r"(a)?b\1",
        r"\1(a)",
        r"(?<x>a)|(?<x>b)\k<x>",
        r"\k<x>(?<x>a)",
        r"(?<\u0078>a)\k<x>",
        r"(?<x>(a))\k<x>",
        r"\k<x>(?<x>(?<y>x))",
        r"\k<y>(?<x>(?<y>x))",
        "(?im:^ab$)(?s:.)c(?i:d)",
        "(?i:a(?-i:b))",
        "a.b",
        "a(?-s:.)b",
        r"(a)\1",
        r"\cA\123\8\08\x41\u0062\z",
        r"\c0",
        "a\0b",
        r"\b\w+\s\d+\S\b",
        r"(?:a){b}",
        "(?",
        "(a",
        "a)",
        "(?ii:a)",
        "(?i-i:a)",
        "(?-:a)",
        "(?<x>a)(?<x>b)",
        "(?<0>a)",
        r"\k<x>",
        r"\k",
        r"\x",
        r"\08",
        r"\2(a)",
        "a{3,2}",
        "a{1",
        "*",
        "[z-a]",
        "(?=a)?",
        "\\",
        r"\p",
        "(?:a{0}|){2,4}",
        "(?<=a|ab|b)c",
        "(?:){0,5}a",
        "(?:(a)|(b)){0}a",
        "(?:(a)|(b))?a",
        "(?:(?=a)|b){2,4}a",
    ];
    for pattern in patterns {
        for flags in [
            0,
            F::UNICODE,
            F::IGNORECASE | F::GLOBAL | F::INDICES,
            F::STICKY,
            F::UNICODE | F::IGNORECASE | F::MULTILINE | F::DOTALL,
        ] {
            for input in [
                b"".as_slice(),
                b"abc",
                b"aaaabcad\nx-ab 12c.",
                b"bbbAAA",
                b"a\0b",
            ] {
                c.compare(pattern.as_bytes(), flags, RegExpInput::Latin1(input), 0);
            }
        }
    }
    for pattern in [
        b".".as_slice(),
        br"\uD83D\uDE00",
        br"\u{1F600}",
        "😀".as_bytes(),
        b"\xed\xa0\xbd\xed\xb8\x80",
        b"\xed\xa0\x80",
        b"(?<=.).",
    ] {
        for flags in [
            0,
            F::UNICODE,
            F::STICKY | F::UNICODE,
            F::UNICODE_SETS | F::STICKY,
        ] {
            let units = [0xd83d, 0xde00, 0xd800, b'a'.into()];
            for i in 0..=units.len() {
                c.compare(pattern, flags, RegExpInput::Utf16(&units), i);
            }
        }
    }
    let mut state = 0x3210u32;
    fn take<'a>(state: &mut u32, list: &'a [&'a str]) -> &'a str {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        list[(*state >> 16) as usize % list.len()]
    }
    for _ in 0..400 {
        let mut pattern = String::new();
        for _ in 0..3 {
            pattern.push_str(take(
                &mut state,
                &[
                    "a", "b", ".", "(a)", "(a|b)", "(?:a?)", "(?=a)", "(?!b)", "(?<=a)", r"\b",
                    r"\d", r"\1", "|", "^",
                ],
            ));
            pattern.push_str(take(
                &mut state,
                &[
                    "", "*", "*?", "+", "+?", "?", "??", "{0}", "{1,2}", "{0,3}?", "{3}", "{2,}",
                ],
            ));
        }
        let input = take(
            &mut state,
            &[
                "", "a", "b", "aba", "aab", "abbb", "abc", "a3a", "aaab", "bbbaaa",
            ],
        );
        let flags = [0, F::UNICODE, F::STICKY, F::IGNORECASE][(state >> 24) as usize % 4];
        c.compare(
            pattern.as_bytes(),
            flags,
            RegExpInput::Latin1(input.as_bytes()),
            0,
        );
    }
}

#[test]
#[ignore = "requires QJS_REF, a C compiler and the Unicode tables"]
fn unicode_libregexp() {
    let c = Reference::build("unicode");
    let patterns = [
        "[a-z]",
        "[A-Z]",
        "[]",
        "[^]",
        "[a-zA-Z0-9]",
        "[a-]",
        "[[a]]",
        "[z-a]",
        r"[\d-\w]",
        r"[a-\w]",
        r"[\w-a]",
        r"[\W]",
        r"[\s-0]",
        r"[\c0]",
        r"[\c_]",
        r"\D\W\S",
        "[[a-z]--[b-z]]+",
        "[[a-z]&&[b-z]]+",
        "[a&&&b]",
        "[a&&b--c]",
        "[a--b&&c]",
        "[a-z&&b]",
        "[a&&b-c]",
        "[aac--b]",
        "[a--]",
        "[a---b]",
        "[[a]&&[a]&&[a]]",
        "[a!!]",
        r"[a\!]",
        "[a\x00b]",
        r"[\q{}]",
        r"[\q{a|aa|aab|}]b",
        r"[\q{aab|aa|a}--\q{aab|aa}]",
        r"[\q{aa|bb}&&\q{bb|cc}]",
        r"[\q{AA|aab}]",
        r"[^\q{abc|ab}]",
        r"[\q{ab|fghi|cd|ef|aa|bb|cc|dd|xy|abc|zzt|p|no|fg|pq|rs|tu|vw|xx|yy|zzz|zz}]",
        r"[\q{a|}]",
        r"[\q{a|\d}]",
        r"\p{ASCII}",
        r"\P{Lowercase_Letter}",
        r"\p{Script=Greek}+",
        r"\p{scx=Grek}+",
        r"\p{gc=Uppercase_Letter}",
        r"\p{gc=bad}",
        r"\p{sc=bad}",
        r"\p{not_a_property}",
        r"\p{Emoji}",
        r"\p{RGI_Emoji}",
        r"\p{Basic_Emoji}",
        r"\P{Basic_Emoji}",
        r"[\p{Basic_Emoji}--\p{RGI_Emoji_Flag_Sequence}]",
        r"(?<β>\P{ASCII})\k<β>",
        r"(?<\u{1D4B6}>.)\k<𝒶>",
        r"\b[A-Z]+\b",
        r"\W",
        r"\w",
        r"(ſ)\1",
        r"[\uFFFF]",
        r"[^\uFFFF]",
        r"[\u{10000}-\u{10FFFF}]",
        "(?<=[a-z]+)b",
        r"(?<=[\q{ab|b}])a",
    ];
    let samples = [
        "",
        "aabb",
        "αΩ ſK K ββ",
        "\u{a0}\u{feff}\u{2028}",
        "aİ𐀀\u{ffff}ſS",
        "x🇪🇺🤦‍♀️y",
        "Aéé",
        "abcd",
        "𝒶𝒶",
    ];
    for pattern in patterns {
        for flags in [
            0,
            F::IGNORECASE,
            F::UNICODE,
            F::UNICODE | F::IGNORECASE,
            F::UNICODE_SETS,
            F::UNICODE_SETS | F::IGNORECASE,
        ] {
            for sample in samples {
                let input: Vec<_> = sample.encode_utf16().collect();
                c.compare(pattern.as_bytes(), flags, RegExpInput::Utf16(&input), 0);
            }
        }
    }
}
