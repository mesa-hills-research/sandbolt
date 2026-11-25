use sandbolt_regexp::{
    CompiledRegExp, EscapeError, RegExpError, RegExpFlags as F, RegExpHost, RegExpInput,
    UnlimitedRegExpHost, check_bytecode, compile, exec, parse_escape,
};

type Span = Option<(usize, usize)>;

fn compiled(pattern: &[u8], flags: u16) -> CompiledRegExp {
    let re = compile(pattern, F(flags), &mut UnlimitedRegExpHost).unwrap();
    check_bytecode(&re.bytecode).unwrap();
    re
}

fn captures(pattern: &[u8], flags: u16, input: RegExpInput<'_>, start: usize) -> Option<Vec<Span>> {
    exec(
        &compiled(pattern, flags),
        input,
        start,
        &mut UnlimitedRegExpHost,
    )
    .unwrap()
    .map(|m| m.captures)
}

fn ascii(pattern: &str, subject: &str) -> Option<Vec<Span>> {
    captures(
        pattern.as_bytes(),
        0,
        RegExpInput::Latin1(subject.as_bytes()),
        0,
    )
}

#[test]
fn alternation_and_empty_alternatives() {
    assert_eq!(ascii("a|ab|b", "xab"), Some(vec![Some((1, 2))]));
    assert_eq!(
        ascii("^(a|ab|b)c$", "abc"),
        Some(vec![Some((0, 3)), Some((0, 2))])
    );
    assert_eq!(
        ascii("^(a||b)c$", "c"),
        Some(vec![Some((0, 1)), Some((0, 0))])
    );
    assert_eq!(
        ascii("^((b)|(c))$", "c"),
        Some(vec![Some((0, 1)), Some((0, 1)), None, Some((0, 1))])
    );
}

#[test]
fn greedy_lazy_and_counted_repeats() {
    assert_eq!(
        ascii("(a{2,4})(a+?)", "aaaaaaa"),
        Some(vec![Some((0, 5)), Some((0, 4)), Some((4, 5))])
    );
    assert_eq!(
        ascii("(a{0,4}?)(a{2})", "aaaaaaa"),
        Some(vec![Some((0, 2)), Some((0, 0)), Some((0, 2))])
    );
    assert_eq!(ascii("^a{0,3}a$", "aaaa"), Some(vec![Some((0, 4))]));
    assert_eq!(ascii("^a{0,3}a$", "aaaaa"), None);
    assert_eq!(ascii("^a{2,}a$", "aaa"), Some(vec![Some((0, 3))]));
    assert_eq!(
        ascii("^(a{3}){2}$", "aaaaaa"),
        Some(vec![Some((0, 6)), Some((3, 6))])
    );
    assert_eq!(ascii("^(a){0}a$", "a"), Some(vec![Some((0, 1)), None]));
}

#[test]
fn capture_reset_and_zero_advance() {
    assert_eq!(
        ascii("(a(b)?)+", "aba"),
        Some(vec![Some((0, 3)), Some((2, 3)), None])
    );
    assert_eq!(
        ascii("(a|(b))*", "ba"),
        Some(vec![Some((0, 2)), Some((1, 2)), None])
    );
    assert_eq!(ascii("(a?)*", ""), Some(vec![Some((0, 0)), None]));
    assert_eq!(
        ascii("(?:()){2,4}", ""),
        Some(vec![Some((0, 0)), Some((0, 0))])
    );
    assert_eq!(ascii("(a?)?", ""), Some(vec![Some((0, 0)), None]));
    assert_eq!(ascii("(a?){1}", ""), Some(vec![Some((0, 0)), Some((0, 0))]));
    assert_eq!(ascii("(?=(a))*a", "a"), Some(vec![Some((0, 1)), None]));
}

#[test]
fn lookaround_and_capture_rollback() {
    assert_eq!(
        ascii("(?<=((?:a|b)+)(b+c))$", "abbc"),
        Some(vec![Some((4, 4)), Some((0, 1)), Some((1, 4))])
    );
    assert_eq!(ascii("(?<!ab)c", "abcxc"), Some(vec![Some((4, 5))]));
    assert_eq!(ascii("(?=(a|ab))\\1c", "abc"), None);
    assert_eq!(
        ascii("(?=(a))\\1", "a"),
        Some(vec![Some((0, 1)), Some((0, 1))])
    );
    assert_eq!(ascii("(?=(a))b|a", "a"), Some(vec![Some((0, 1)), None]));
    assert_eq!(
        ascii("^(?=(a+))a*a$", "aaaa"),
        Some(vec![Some((0, 4)), Some((0, 4))])
    );
    assert_eq!(
        ascii("^(?!(a+)b)a+c$", "aaac"),
        Some(vec![Some((0, 4)), None])
    );
    assert_eq!(ascii("(?!(a)b)a", "a"), Some(vec![Some((0, 1)), None]));
    assert_eq!(
        ascii("(?=(?!(b))(a))a", "a"),
        Some(vec![Some((0, 1)), None, Some((0, 1))])
    );
    assert_eq!(
        ascii("(?<=\\1(a))b", "aab"),
        Some(vec![Some((2, 3)), Some((1, 2))])
    );
    assert_eq!(
        ascii("(?<=(a(?=b))b)c", "abc"),
        Some(vec![Some((2, 3)), Some((0, 1))])
    );
}

#[test]
fn backreferences_and_group_names() {
    assert_eq!(ascii(r"^(a)?b\1$", "b"), Some(vec![Some((0, 1)), None]));
    assert_eq!(
        ascii(r"^\1(a)$", "a"),
        Some(vec![Some((0, 1)), Some((0, 1))])
    );
    assert_eq!(
        ascii(r"(?<x>a)|(?<x>b)\k<x>", "bb"),
        Some(vec![Some((0, 2)), None, Some((0, 1))])
    );
    assert_eq!(
        ascii(r"\k<x>(?<x>a)", "a"),
        Some(vec![Some((0, 1)), Some((0, 1))])
    );
    assert_eq!(
        ascii(r"(?<\u0078>a)\k<x>", "aa"),
        Some(vec![Some((0, 2)), Some((0, 1))])
    );
    let re = compiled(br"(a)(?<x>b)|(?<$c2>c)(?<x>d)", F::INDICES);
    assert_eq!(re.capture_count, 5);
    assert_eq!(
        re.group_names,
        vec![
            None,
            None,
            Some("x".into()),
            Some("$c2".into()),
            Some("x".into())
        ]
    );
    assert_eq!(re.flags.0, F::INDICES | F::NAMED_GROUPS);
    assert_eq!(
        CompiledRegExp::from_bytecode(re.bytecode.clone(), &mut UnlimitedRegExpHost).unwrap(),
        re
    );
}

#[test]
fn flags_and_inline_modifiers() {
    let input = RegExpInput::Latin1(b"x\nAB\ncD");
    assert_eq!(
        captures(b"(?im:^ab$)(?s:.)c(?i:d)", 0, input, 0),
        Some(vec![Some((2, 7))])
    );
    assert_eq!(
        captures(b"(?i:a(?-i:b))", 0, RegExpInput::Latin1(b"ABAb"), 0),
        Some(vec![Some((2, 4))])
    );
    assert_eq!(
        captures(b"a.b", F::DOTALL, RegExpInput::Latin1(b"a\nb"), 0),
        Some(vec![Some((0, 3))])
    );
    assert_eq!(
        captures(b"a(?-s:.)b", F::DOTALL, RegExpInput::Latin1(b"a\nb"), 0),
        None
    );
    assert_eq!(
        captures(br"(a)\1", F::IGNORECASE, RegExpInput::Latin1(b"aA"), 0),
        Some(vec![Some((0, 2)), Some((0, 1))])
    );
    assert_eq!(
        captures(b"$", 0, RegExpInput::Latin1(b"a\n"), 0),
        Some(vec![Some((2, 2))])
    );
}

#[test]
fn sticky_and_start_index() {
    let input = RegExpInput::Latin1(b"xxaax");
    assert_eq!(captures(b"a+", F::GLOBAL | F::STICKY, input, 1), None);
    assert_eq!(
        captures(b"a+", F::GLOBAL | F::STICKY, input, 2),
        Some(vec![Some((2, 4))])
    );
    assert_eq!(
        captures(b"a+", F::GLOBAL, input, 1),
        Some(vec![Some((2, 4))])
    );
    assert_eq!(
        captures(b"a*", F::STICKY, input, 5),
        Some(vec![Some((5, 5))])
    );
    assert_eq!(captures(b"a*", F::STICKY, input, 6), None);
}

#[test]
fn utf16_indexing_and_surrogates() {
    let units = [0xd83d, 0xde00, 0xd800, 0x2028, b'z'.into()];
    let input = RegExpInput::Utf16(&units);
    assert_eq!(
        captures(b".", F::UNICODE | F::STICKY, input, 1),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(b".", F::STICKY, input, 1),
        Some(vec![Some((1, 2))])
    );
    assert_eq!(
        captures("😀".as_bytes(), F::UNICODE, input, 0),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(b"\xed\xa0\xbd\xed\xb8\x80", 0, input, 0),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(br"\uD83D\uDE00", F::UNICODE, input, 0),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(b"\xed\xa0\x80", F::UNICODE, input, 0),
        Some(vec![Some((2, 3))])
    );
    assert_eq!(
        captures(br"(?<=\u{1F600}).", F::UNICODE, input, 0),
        Some(vec![Some((2, 3))])
    );
    assert_eq!(
        captures(b"^z", F::MULTILINE, input, 0),
        Some(vec![Some((4, 5))])
    );
}

#[test]
fn escapes_and_predefined_classes() {
    assert_eq!(
        ascii(r"^\cA\123\8\08\x41\u0062\z$", "\u{1}S8\08Abz"),
        Some(vec![Some((0, 8))])
    );
    assert_eq!(ascii(r"^\c0$", r"\c0"), Some(vec![Some((0, 3))]));
    assert_eq!(ascii("a\0b", "a\0b"), Some(vec![Some((0, 3))]));
    assert_eq!(
        ascii(r"\b\w+\s\d+\S\b", "-ab 123c."),
        Some(vec![Some((1, 8))])
    );
    assert_eq!(
        captures(br"\s", 0, RegExpInput::Latin1(&[0xa0]), 0),
        Some(vec![Some((0, 1))])
    );
    assert_eq!(ascii(r"^(?:a){b}$", "a{b}"), Some(vec![Some((0, 4))]));
    assert_eq!(parse_escape(br"uD83D\uDE00x", 2), Ok((0x1f600, 11)));
    assert_eq!(parse_escape(br"uD83D\uDE00x", 1), Ok((0xd83d, 5)));
    assert_eq!(parse_escape(br"u{}", 2), Err(EscapeError::Malformed));
    assert_eq!(parse_escape(br"u{41}", 0), Err(EscapeError::Malformed));
    assert_eq!(parse_escape(br"u{41}", 1), Ok((65, 5)));
    assert_eq!(parse_escape(b"400", 0), Ok((32, 2)));
    assert_eq!(parse_escape(b"08", 2), Err(EscapeError::Malformed));
    assert_eq!(parse_escape(b"8", 2), Err(EscapeError::Unrecognized));
}

#[test]
fn unicode_sets_punctuators_are_reserved_only_inside_a_class() {
    for pattern in ["/", "-", "a&&b", "==", "[a&&b]"] {
        assert!(
            compile(
                pattern.as_bytes(),
                F(F::UNICODE_SETS),
                &mut UnlimitedRegExpHost
            )
            .is_ok(),
            "{pattern}"
        );
    }
    for pattern in ["[a&&]", "[&&]"] {
        assert!(
            matches!(
                compile(
                    pattern.as_bytes(),
                    F(F::UNICODE_SETS),
                    &mut UnlimitedRegExpHost
                ),
                Err(RegExpError::Syntax(_))
            ),
            "{pattern}"
        );
    }
}

#[test]
fn diagnostics() {
    for (pattern, flags, error) in [
        ("(?", 0, "invalid group"),
        ("(a", 0, "expecting ')'"),
        ("a)", 0, "extraneous characters at the end"),
        ("(?ii:a)", 0, "duplicate modifier: 'i'"),
        ("(?i-i:a)", 0, "invalid modifiers"),
        ("(?-:a)", 0, "invalid modifiers"),
        ("(?<x>a)(?<x>b)", 0, "duplicate group name"),
        ("(?<0>a)", 0, "invalid group name"),
        (r"\k<x>", F::UNICODE, "group name not defined"),
        (r"\k", F::UNICODE, "expecting group name"),
        (
            r"\x",
            F::UNICODE,
            "invalid escape sequence in regular expression",
        ),
        (
            r"\08",
            F::UNICODE,
            "invalid decimal escape in regular expression",
        ),
        (
            r"\2(a)",
            F::UNICODE,
            "back reference out of range in regular expression",
        ),
        ("a{3,2}", 0, "invalid repetition count"),
        ("a{1", F::UNICODE, "expecting '}'"),
        ("*", 0, "nothing to repeat"),
        ("[z-a]", 0, "invalid class range"),
        ("(?=a)?", F::UNICODE, "nothing to repeat"),
        ("\\", 0, "unexpected end"),
        (
            "[a==b]",
            F::UNICODE_SETS,
            "invalid class set operation in regular expression",
        ),
        (
            "[a-]",
            F::UNICODE_SETS,
            "invalid character in class in regular expression",
        ),
        (
            "[/]",
            F::UNICODE_SETS,
            "invalid character in class in regular expression",
        ),
        (r"\p", F::UNICODE, "expecting '{' after \\p"),
        (r"[\q]", F::UNICODE_SETS, "expecting '{' after \\q"),
    ] {
        assert_eq!(
            compile(pattern.as_bytes(), F(flags), &mut UnlimitedRegExpHost),
            Err(RegExpError::Syntax(error.into())),
            "{pattern}"
        );
    }
    let repeats = format!("{}a{}", "(?:".repeat(256), "){2}".repeat(256));
    assert_eq!(
        compile(repeats.as_bytes(), F(0), &mut UnlimitedRegExpHost),
        Err(RegExpError::Syntax(
            "too many imbricated quantifiers".into()
        ))
    );
    let captures = "()".repeat(255);
    assert_eq!(
        compile(captures.as_bytes(), F(0), &mut UnlimitedRegExpHost),
        Err(RegExpError::Syntax("too many captures".into()))
    );
}

#[derive(Default)]
struct Host {
    interrupt: bool,
    polls: usize,
    calls: usize,
    fail_at: Option<usize>,
    bytes: usize,
    peak: usize,
    limit: Option<usize>,
    stack_limit: Option<usize>,
}

impl RegExpHost for Host {
    fn poll_interrupt(&mut self) -> bool {
        self.polls += 1;
        self.interrupt
    }

    fn charge(&mut self, bytes: usize) -> Result<(), RegExpError> {
        self.calls += 1;
        if self.fail_at == Some(self.calls)
            || self.limit.is_some_and(|limit| self.bytes + bytes > limit)
        {
            return Err(RegExpError::OutOfMemory);
        }
        self.bytes += bytes;
        self.peak = self.peak.max(self.bytes);
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.bytes -= bytes;
    }

    fn check_stack(&mut self, bytes: usize) -> bool {
        self.stack_limit.is_none_or(|limit| bytes <= limit)
    }
}

#[test]
fn interrupts_and_stack_limit() {
    let subject = format!("{}b", "a".repeat(40));
    let input = RegExpInput::Latin1(subject.as_bytes());
    let re = compiled(b"^(a+)+$", F::STICKY);
    let mut host = Host {
        interrupt: true,
        ..Default::default()
    };
    assert_eq!(
        exec(&re, input, 0, &mut host),
        Err(RegExpError::Interrupted)
    );
    assert_eq!(host.polls, 1);
    assert_eq!(host.bytes, 0);
    let mut host = Host {
        stack_limit: Some(256),
        ..Default::default()
    };
    assert_eq!(
        exec(&re, input, 0, &mut host),
        Err(RegExpError::StackOverflow)
    );
    assert_eq!(host.bytes, 0);
    let nested = format!("{}a{}", "(?:".repeat(2048), ")".repeat(2048));
    assert_eq!(
        compile(nested.as_bytes(), F(0), &mut host),
        Err(RegExpError::StackOverflow)
    );
    assert_eq!(host.bytes, 0);
    assert_eq!(ascii(&nested, "a"), Some(vec![Some((0, 1))]));
    let look = format!("{}a{}a", "(?=".repeat(2048), ")".repeat(2048));
    assert_eq!(ascii(&look, "a"), Some(vec![Some((0, 1))]));
    let re = compiled(b"^(?:){2147483647}$", 0);
    let mut host = Host {
        interrupt: true,
        ..Default::default()
    };
    assert_eq!(
        exec(&re, RegExpInput::Latin1(b""), 0, &mut host),
        Err(RegExpError::Interrupted)
    );
    assert_eq!(host.polls, 1);
}

#[test]
fn long_character_repeats_use_bounded_backtracking_space() {
    let units: Vec<_> = "α😀".repeat(550_000).encode_utf16().collect();
    let input = RegExpInput::Utf16(&units);
    let mut host = Host {
        stack_limit: Some(256),
        limit: Some(1024),
        ..Default::default()
    };
    for (pattern, expected) in [
        (
            br"^\P{ASCII}+$".as_slice(),
            Some(vec![Some((0, units.len()))]),
        ),
        (
            br"^([^\d]*)\u{1F600}$",
            Some(vec![Some((0, units.len())), Some((0, units.len() - 2))]),
        ),
        (br"^\S+a$", None),
    ] {
        let re = compiled(pattern, F::UNICODE);
        assert_eq!(
            exec(&re, input, 0, &mut host).map(|m| m.map(|m| m.captures)),
            Ok(expected),
            "{pattern:?}"
        );
        assert_eq!(host.bytes, 0);
    }
    host.interrupt = true;
    let re = compiled(br"^\P{ASCII}+$", F::UNICODE);
    assert_eq!(
        exec(&re, input, 0, &mut host),
        Err(RegExpError::Interrupted)
    );
    assert_eq!(host.bytes, 0);
}

#[test]
fn split_runs_rewind_at_code_point_boundaries() {
    let input = RegExpInput::Utf16(&[0xdc00, 0x3b1, 0xd83d, 0xde00, 0xd800, 0x3b2, 0xde00]);
    for flags in [F::UNICODE, F::UNICODE_SETS] {
        assert_eq!(
            captures(
                br"^(\P{ASCII}+)(\u{1f600}\uD800\u03B2\uDE00)$",
                flags,
                input,
                0
            ),
            Some(vec![Some((0, 7)), Some((0, 2)), Some((2, 7))]),
        );
    }
    assert_eq!(
        captures(br"^(.+)(\uDE00\uD800\u03B2\uDE00)$", 0, input, 0),
        Some(vec![Some((0, 7)), Some((0, 3)), Some((3, 7))]),
    );
}

#[test]
fn allocation_failures_release_charges() {
    let pattern = br"(?<start>(?:(a|b){2,14}?)+)(?<end>a*)c";
    let mut host = Host::default();
    let re = compile(pattern, F(0), &mut host).unwrap();
    assert_eq!(host.bytes, 0);
    for fail_at in 1..=host.calls {
        let mut host = Host {
            fail_at: Some(fail_at),
            ..Default::default()
        };
        assert_eq!(
            compile(pattern, F(0), &mut host),
            Err(RegExpError::OutOfMemory),
            "allocation {fail_at}"
        );
        assert_eq!(host.bytes, 0);
    }
    let input = RegExpInput::Latin1(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaac");
    let mut host = Host::default();
    assert!(exec(&re, input, 0, &mut host).unwrap().is_some());
    assert_eq!(host.bytes, 0);
    for fail_at in 1..=host.calls {
        let mut host = Host {
            fail_at: Some(fail_at),
            ..Default::default()
        };
        assert_eq!(
            exec(&re, input, 0, &mut host),
            Err(RegExpError::OutOfMemory),
            "allocation {fail_at}"
        );
        assert_eq!(host.bytes, 0);
    }
    let mut host = Host {
        limit: Some(host.peak - 1),
        ..Default::default()
    };
    assert_eq!(
        exec(&re, input, 0, &mut host),
        Err(RegExpError::OutOfMemory)
    );
}

#[test]
fn character_sets_respect_allocation_limits() {
    let patterns = [
        br"[[a-z]--[aeiou]]".as_slice(),
        br"[\q{ab|a|xy|}]",
        br"\p{RGI_Emoji}",
    ];
    for pattern in patterns {
        let mut host = Host::default();
        let _ = compile(pattern, F(F::UNICODE_SETS), &mut host).unwrap();
        assert_eq!(host.bytes, 0);
        for fail_at in [1, host.calls / 2, host.calls] {
            let mut host = Host {
                fail_at: Some(fail_at),
                ..Default::default()
            };
            assert_eq!(
                compile(pattern, F(F::UNICODE_SETS), &mut host),
                Err(RegExpError::OutOfMemory)
            );
            assert_eq!(host.bytes, 0);
        }
    }
}

#[test]
fn rejects_malformed_bytecode() {
    let re = compiled(br"(?<a>\d{2,3}|b)\k<a>", F::STICKY);
    for truncated in 0..re.bytecode.len() {
        assert_eq!(
            check_bytecode(&re.bytecode[..truncated]),
            Err(RegExpError::InvalidBytecode)
        );
    }
    for (pos, byte) in [
        (2, 0),
        (3, 0),
        (4, 255),
        (8, 0xff),
        (9, 100),
        (11, 255),
        (13, 1),
        (re.bytecode.len() - 3, 0xff),
    ] {
        let mut damaged = re.clone();
        damaged.bytecode[pos] = byte;
        assert_eq!(
            check_bytecode(&damaged.bytecode),
            Err(RegExpError::InvalidBytecode),
            "byte {pos}"
        );
        assert_eq!(
            exec(
                &damaged,
                RegExpInput::Latin1(b"b"),
                0,
                &mut UnlimitedRegExpHost
            ),
            Err(RegExpError::InvalidBytecode)
        );
    }
    let mut damaged = compiled(b"(?=a)a", F::STICKY);
    damaged.bytecode[10] = 17;
    assert_eq!(
        exec(
            &damaged,
            RegExpInput::Latin1(b"a"),
            0,
            &mut UnlimitedRegExpHost
        ),
        Err(RegExpError::InvalidBytecode)
    );
}

#[test]
fn unicode_classes_properties_and_case_folding() {
    assert_eq!(ascii(r"^[a-z][\w-][\D]$", "a-b"), Some(vec![Some((0, 3))]));
    assert_eq!(ascii(r"[\s-0]", "-"), Some(vec![Some((0, 1))]));
    let input: Vec<_> = "αΩ ſK K β".encode_utf16().collect();
    let input = RegExpInput::Utf16(&input);
    assert_eq!(
        captures(br"\p{Script=Greek}+", F::UNICODE, input, 0),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(br"\p{scx=Grek}+", F::UNICODE, input, 0),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(br"\p{gc=Uppercase_Letter}", F::UNICODE, input, 0),
        Some(vec![Some((1, 2))])
    );
    assert_eq!(
        captures(br"\b[A-Z]+\b", F::UNICODE | F::IGNORECASE, input, 0),
        Some(vec![Some((3, 5))])
    );
    assert_eq!(
        captures(br"\b[A-Z]+\b", F::IGNORECASE, input, 0),
        Some(vec![Some((4, 5))])
    );
    assert_eq!(
        captures(
            "(?<β>\\P{ASCII})\\k<β>".as_bytes(),
            F::UNICODE,
            RegExpInput::Utf16(&[0x3b2, 0x3b2]),
            0
        ),
        Some(vec![Some((0, 2)), Some((0, 1))])
    );
    assert_eq!(
        captures(br"\s", 0, RegExpInput::Utf16(&[0xfeff]), 0),
        Some(vec![Some((0, 1))])
    );
    assert_eq!(
        captures(
            br"[^a]",
            F::UNICODE,
            RegExpInput::Utf16(&[0xd83d, 0xde00]),
            0
        ),
        Some(vec![Some((0, 2))])
    );
}

#[test]
fn unicode_set_operations_and_strings() {
    let v = F::UNICODE_SETS;
    let input = RegExpInput::Latin1(b"aabb");
    assert_eq!(
        captures(br"[[a-z]--[b-z]]+", v, input, 0),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        captures(br"[[a-z]&&[b-z]]+", v, input, 0),
        Some(vec![Some((2, 4))])
    );
    assert_eq!(
        captures(br"[\q{a|aa|aab|}]b", v, input, 0),
        Some(vec![Some((0, 4))])
    );
    assert_eq!(
        captures(br"[\q{aab|aa|a}--\q{aab|aa}]", v, input, 0),
        Some(vec![Some((0, 1))])
    );
    assert_eq!(
        captures(br"[\q{aa|bb}&&\q{bb|cc}]", v, input, 0),
        Some(vec![Some((2, 4))])
    );
    assert_eq!(
        captures(br"[\q{AA|aab}]", v | F::IGNORECASE, input, 0),
        Some(vec![Some((0, 3))])
    );
    let input: Vec<_> = "x🇪🇺🤦‍♀️y".encode_utf16().collect();
    assert_eq!(
        captures(
            br"\p{RGI_Emoji}+$",
            v,
            RegExpInput::Utf16(&input[..input.len() - 1]),
            0
        ),
        Some(vec![Some((1, input.len() - 1))])
    );
}
