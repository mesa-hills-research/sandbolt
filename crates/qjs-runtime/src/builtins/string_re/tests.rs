use std::cell::Cell;
use std::rc::Rc;

use crate::object::{ErrorKind, ObjectFlags};
use crate::ops::call::construct;
use crate::ops::property::{get_own_property_descriptor, get_prototype_of, set_property};
use crate::{Abrupt, EngineLimits, Runtime};

use super::*;

fn engine() -> Runtime {
    let mut rt = Runtime::new(EngineLimits::default()).unwrap();
    rt.with_scope(rt.default_realm(), install).unwrap();
    rt
}

fn named<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>, name: &str) -> Handle<'s> {
    let key = ctx.intern_str(name).unwrap();
    get(ctx, obj, key).unwrap()
}

fn global_value<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str) -> Handle<'s> {
    let global = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
    named(ctx, global, name)
}

fn method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    obj: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> JsResult<Value> {
    let boxed = to_object(ctx, obj)?;
    let f = named(ctx, boxed, name);
    call_with(ctx, f, obj, args)
}

fn protocol<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    obj: Handle<'s>,
    symbol: WellKnown,
    args: &[Handle<'s>],
) -> Value {
    let boxed = to_object(ctx, obj).unwrap();
    let f = get(ctx, boxed, sym(ctx, symbol)).unwrap();
    call_with(ctx, f, obj, args).unwrap()
}

fn property<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>, name: &str) -> Value {
    let key = ctx.intern_str(name).unwrap();
    get_property(ctx, obj, key).unwrap()
}

fn assert_units(ctx: &ExecCtx<'_, '_>, val: Value, want: &[u16]) {
    let id = val_as_string(val);
    let actual = ctx.heap().string_units(id).collect::<Vec<_>>();
    assert_eq!(actual, want);
}

fn val_as_string(val: Value) -> StrId {
    as_string(val).unwrap_or_else(|| panic!("expected a string, got {val:?}"))
}

fn assert_text(ctx: &ExecCtx<'_, '_>, val: Value, want: &str) {
    assert_units(ctx, val, &want.encode_utf16().collect::<Vec<_>>());
}

fn exception_kind(ctx: &mut ExecCtx<'_, '_>, result: JsResult<Value>, kind: ErrorKind) {
    assert_eq!(result, Err(Abrupt::Thrown));
    let value = ctx.take_exception().unwrap().0;
    let ObjectData::Error(error) = &ctx.heap().object(value.as_object().unwrap()).data else {
        panic!("expected error");
    };
    assert_eq!(error.kind, kind);
}

fn re<'s>(ctx: &mut ExecCtx<'_, 's>, pattern: &str, flags: &str) -> Handle<'s> {
    let ctor = global_value(ctx, "RegExp");
    let pat = ctx.new_string(pattern).unwrap();
    let flags = ctx.new_string(flags).unwrap();
    let args = ctx.args(&[pat, flags]).unwrap();
    let re = construct(ctx, ctor, args, ctor).unwrap();
    ctx.root(re).unwrap()
}

fn num<'s>(ctx: &mut ExecCtx<'_, 's>, value: f64) -> Handle<'s> {
    ctx.root(Value::number(value)).unwrap()
}

#[test]
fn string_constructor_symbol_wrapper_and_static_methods() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = global_value(ctx, "String");
        let ordinary = ctx.new_object().unwrap();
        let desc = ctx.new_utf16(&[0xd800]).unwrap();
        let symbol = ctx.new_symbol(Some(desc), false).unwrap();
        let text = call_with(ctx, ctor, ordinary, &[symbol]).unwrap();
        assert_units(ctx, text, &[83, 121, 109, 98, 111, 108, 40, 0xd800, 41]);
        let args = ctx.args(&[symbol]).unwrap();
        let result = construct(ctx, ctor, args, ctor);
        exception_kind(ctx, result, ErrorKind::Type);
        let pair = ctx.new_string("🐈").unwrap();
        let args = ctx.args(&[pair]).unwrap();
        let wrapper = construct(ctx, ctor, args, ctor).unwrap();
        let wrapper = ctx.root(wrapper).unwrap();
        assert_eq!(property(ctx, wrapper, "length").as_number(), Some(2.0));
        let desc = get_own_property_descriptor(ctx, wrapper, LENGTH)
            .unwrap()
            .unwrap();
        assert_eq!(
            (desc.writable, desc.enumerable, desc.configurable),
            (Some(false), Some(false), Some(false))
        );
        let value = method(ctx, wrapper, "valueOf", &[]).unwrap();
        assert_text(ctx, value, "🐈");
        let proto = named(ctx, ctor, "prototype");
        let value = method(ctx, proto, "toString", &[]).unwrap();
        assert_text(ctx, value, "");
        let left = named(ctx, proto, "trimLeft");
        let start = named(ctx, proto, "trimStart");
        assert_eq!(ctx.get(left), ctx.get(start));

        let a = num(ctx, 0x1f408 as f64);
        let lone = num(ctx, 0xd800 as f64);
        let pair_and_lone = method(ctx, ctor, "fromCodePoint", &[a, lone]).unwrap();
        assert_units(ctx, pair_and_lone, &[0xd83d, 0xdc08, 0xd800]);
        let neg = num(ctx, -1.0);
        let from_char = method(ctx, ctor, "fromCharCode", &[neg, a]).unwrap();
        assert_units(ctx, from_char, &[0xffff, 0xf408]);
        let fract = num(ctx, 2.3);
        let result = method(ctx, ctor, "fromCodePoint", &[fract]);
        exception_kind(ctx, result, ErrorKind::Range);

        let template = ctx.new_object().unwrap();
        let raw = ctx.new_array().unwrap();
        let a = ctx.new_string(r"\n").unwrap();
        let b = ctx.new_string("!").unwrap();
        put_index(ctx, raw, 0, a).unwrap();
        put_index(ctx, raw, 1, b).unwrap();
        let key = ctx.intern_str("raw").unwrap();
        define_data(ctx, template, key, raw, PropFlags::DEFAULT_DATA).unwrap();
        let value = method(ctx, ctor, "raw", &[template, pair]).unwrap();
        assert_text(ctx, value, r"\n🐈!");
    });
}

#[test]
fn unicode_case_normalization_and_unpaired_surrogates() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let s = ctx.new_string("AΣ AΣ\u{301}B Σ").unwrap();
        for name in ["toLowerCase", "toLocaleLowerCase"] {
            let value = method(ctx, s, name, &[]).unwrap();
            assert_text(ctx, value, "aς aσ\u{301}b σ");
        }
        let s = ctx.new_string("ß𐐨").unwrap();
        let value = method(ctx, s, "toUpperCase", &[]).unwrap();
        assert_text(ctx, value, "SS𐐀");
        for (form, src, expected) in [
            ("NFC", "e\u{301}", "é"),
            ("NFD", "é", "e\u{301}"),
            ("NFKC", "ﬁﬀ", "fiff"),
            ("NFKD", "éﬁ", "e\u{301}fi"),
        ] {
            let mut src = src.encode_utf16().collect::<Vec<_>>();
            src.push(0xdc00);
            let mut expected = expected.encode_utf16().collect::<Vec<_>>();
            expected.push(0xdc00);
            let s = ctx.new_utf16(&src).unwrap();
            let arg = ctx.new_string(form).unwrap();
            let value = method(ctx, s, "normalize", &[arg]).unwrap();
            assert_units(ctx, value, &expected);
        }
        let left = ctx.new_string("é").unwrap();
        let right = ctx.new_string("e\u{301}").unwrap();
        assert_eq!(
            method(ctx, left, "localeCompare", &[right]).unwrap(),
            Value::int32(0)
        );
        let s = ctx
            .new_utf16(&[0xdc00, 0xd800, 0xd83d, 0xdc08, 0x61])
            .unwrap();
        assert_eq!(
            method(ctx, s, "isWellFormed", &[]).unwrap(),
            Value::bool(false)
        );
        let value = method(ctx, s, "toWellFormed", &[]).unwrap();
        assert_units(ctx, value, &[0xfffd, 0xfffd, 0xd83d, 0xdc08, 0x61]);
        let value = ctx.root(value).unwrap();
        assert_eq!(
            method(ctx, value, "isWellFormed", &[]).unwrap(),
            Value::bool(true)
        );
    });
}

#[test]
fn string_indices_search_padding_slices_and_html() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let s = ctx.new_string("a🐈ab").unwrap();
        let pos = num(ctx, -4.0);
        let value = method(ctx, s, "at", &[pos]).unwrap();
        assert_units(ctx, value, &[0xd83d]);
        let pos = num(ctx, 1.0);
        assert_eq!(
            method(ctx, s, "codePointAt", &[pos]).unwrap().as_number(),
            Some(0x1f408 as f64)
        );
        assert_eq!(
            method(ctx, s, "charCodeAt", &[pos]).unwrap().as_number(),
            Some(0xd83d as f64)
        );
        let a = ctx.new_string("a").unwrap();
        assert_eq!(
            method(ctx, s, "indexOf", &[a, pos]).unwrap(),
            Value::int32(3)
        );
        assert_eq!(
            method(ctx, s, "lastIndexOf", &[a]).unwrap(),
            Value::int32(3)
        );
        assert_eq!(
            method(ctx, s, "endsWith", &[a, pos]).unwrap(),
            Value::bool(true)
        );
        let r = re(ctx, "a", "");
        let result = method(ctx, s, "includes", &[r]);
        exception_kind(ctx, result, ErrorKind::Type);
        let end = num(ctx, 3.0);
        let value = method(ctx, s, "substring", &[end, pos]).unwrap();
        assert_text(ctx, value, "🐈");
        let start = num(ctx, -2.0);
        let value = method(ctx, s, "slice", &[start]).unwrap();
        assert_text(ctx, value, "ab");
        let value = method(ctx, s, "substr", &[start, pos]).unwrap();
        assert_text(ctx, value, "a");
        let fill = ctx.new_string("🐈").unwrap();
        let len = num(ctx, 4.0);
        let value = method(ctx, a, "padEnd", &[len, fill]).unwrap();
        assert_units(ctx, value, &[97, 0xd83d, 0xdc08, 0xd83d]);
        let value = method(ctx, a, "padStart", &[len, fill]).unwrap();
        assert_units(ctx, value, &[0xd83d, 0xdc08, 0xd83d, 97]);
        let value = method(ctx, a, "repeat", &[len]).unwrap();
        assert_text(ctx, value, "aaaa");
        let url = ctx.new_string(r#""<&"#).unwrap();
        let value = method(ctx, a, "link", &[url]).unwrap();
        assert_text(ctx, value, r#"<a href="&quot;<&">a</a>"#);
    });
}

#[test]
fn string_iterator_preserves_code_points_and_releases_input() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let a = ctx.new_utf16(&[0xd83d]).unwrap();
        let b = ctx.new_utf16(&[0xdc08, 0xdc00, 97]).unwrap();
        let rope = ctx.concat_strings(a, b).unwrap();
        let iterator = protocol(ctx, rope, WellKnown::Iterator, &[]);
        let iterator = ctx.root(iterator).unwrap();
        let proto = get_prototype_of(ctx, iterator).unwrap();
        assert_eq!(proto, Some(ctx.realm().string_iterator_proto));
        let tag = get_property(ctx, iterator, sym(ctx, WellKnown::ToStringTag)).unwrap();
        assert_text(ctx, tag, "String Iterator");
        for want in [&[0xd83d, 0xdc08][..], &[0xdc00][..], &[97][..]] {
            let result = method(ctx, iterator, "next", &[]).unwrap();
            let result = ctx.root(result).unwrap();
            assert_eq!(property(ctx, result, "done"), Value::bool(false));
            let value = property(ctx, result, "value");
            assert_units(ctx, value, want);
        }
        for _ in 0..2 {
            let result = method(ctx, iterator, "next", &[]).unwrap();
            let result = ctx.root(result).unwrap();
            assert_eq!(property(ctx, result, "done"), Value::bool(true));
            assert_eq!(property(ctx, result, "value"), Value::UNDEFINED);
        }
        let id = ctx.get(iterator).as_object().unwrap();
        let ObjectData::StringIterator(data) = &ctx.heap().object(id).data else {
            panic!("expected iterator")
        };
        assert_eq!(data.string, Value::UNDEFINED);
    });
}

#[test]
fn repeat_and_pad_charge_before_large_allocation() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let s = ctx.new_string("x").unwrap();
        let n = num(ctx, 128.0 * 1024.0 * 1024.0);
        let baseline = ctx.heap().ledger.used_bytes();
        for name in ["repeat", "padStart", "padEnd"] {
            assert_eq!(method(ctx, s, name, &[n]), Err(Abrupt::Thrown));
            assert!(ctx.take_exception().unwrap().1);
        }
        assert!(ctx.heap().ledger.used_bytes() < baseline * 2);
        let empty = ctx.new_string("").unwrap();
        let n = num(ctx, 2_147_483_620.0);
        let result = method(ctx, empty, "padEnd", &[n, s]);
        exception_kind(ctx, result, ErrorKind::Range);
        let value = method(ctx, s, "padEnd", &[n, empty]).unwrap();
        assert_text(ctx, value, "x");
    });
}

#[test]
fn repeat_and_pad_poll_during_copy() {
    for name in ["repeat", "padStart", "padEnd"] {
        let mut rt = engine();
        rt.with_scope(rt.default_realm(), |ctx| {
            let s = ctx.new_string("x").unwrap();
            let n = num(ctx, 200_000.0);
            let count = Rc::new(Cell::new(0));
            let polls = count.clone();
            ctx.runtime.set_interrupt_handler(Some(Box::new(move || {
                let n = polls.get() + 1;
                polls.set(n);
                n >= 16
            })));
            assert_eq!(method(ctx, s, name, &[n]), Err(Abrupt::Interrupted));
            assert!(ctx.pending_exception().is_none());
            assert_eq!(count.get(), 16);
        });
    }
}

#[test]
fn regexp_construct_flags_escape_source_and_copy() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = global_value(ctx, "RegExp");
        let r = re(ctx, "a", "yvmsigd");
        let value = property(ctx, r, "flags");
        assert_text(ctx, value, "dgimsvy");
        let bracket = re(ctx, "[/]/\n\u{2028}", "u");
        let bracket = property(ctx, bracket, "source");
        assert_text(ctx, bracket, "[/]\\/\\n\\u2028");
        let value = method(ctx, r, "toString", &[]).unwrap();
        assert_text(ctx, value, "/a/dgimsvy");
        let proto = named(ctx, ctor, "prototype");
        let value = property(ctx, proto, "source");
        assert_text(ctx, value, "(?:)");
        assert_eq!(property(ctx, proto, "global"), Value::UNDEFINED);
        let value = call_with(ctx, ctor, proto, &[r]).unwrap();
        assert_eq!(value, ctx.get(r));
        let args = ctx.args(&[r]).unwrap();
        let copy = construct(ctx, ctor, args, ctor).unwrap();
        assert_ne!(copy, ctx.get(r));
        let copy = ctx.root(copy).unwrap();
        let value = property(ctx, copy, "flags");
        assert_text(ctx, value, "dgimsvy");
        let key = ctx.intern_str("lastIndex").unwrap();
        let desc = get_own_property_descriptor(ctx, copy, key)
            .unwrap()
            .unwrap();
        assert_eq!(
            (desc.writable, desc.configurable, desc.enumerable),
            (Some(true), Some(false), Some(false))
        );
        let subject = ctx.new_string("x").unwrap();
        let new = method(ctx, copy, "compile", &[subject]).unwrap();
        assert_eq!(new, ctx.get(copy));
        let value = property(ctx, copy, "source");
        assert_text(ctx, value, "x");
        let result = method(ctx, copy, "compile", &[r, subject]);
        exception_kind(ctx, result, ErrorKind::Type);
        let both = ctx.new_string("uv").unwrap();
        let bad = call_with(ctx, ctor, proto, &[subject, both]);
        exception_kind(ctx, bad, ErrorKind::Syntax);
        let s = ctx
            .new_utf16(&[
                97, 45, 91, 93, 47, 32, 10, 0xe9, 0xd83d, 0xdc08, 0xd800, 0x2028,
            ])
            .unwrap();
        let value = method(ctx, ctor, "escape", &[s]).unwrap();
        assert_text(ctx, value, r"\x61\x2d\[\]\/\x20\né🐈\ud800\u2028");
    });
}

#[test]
fn regexp_literal_uses_the_intrinsic_prototype() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let global = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
        let undef = ctx.root(Value::UNDEFINED).unwrap();
        let key = ctx.intern_str("RegExp").unwrap();
        assert!(set_property(ctx, global, key, undef).unwrap());
        let source = ctx.new_string("a").unwrap();
        let flags = ctx.new_string("g").unwrap();
        let literal = regexp_literal(ctx, source, flags).unwrap();
        let literal = ctx.root(literal).unwrap();
        assert_eq!(
            get_prototype_of(ctx, literal).unwrap(),
            Some(ctx.realm().regexp_proto)
        );
        assert_eq!(
            method(ctx, literal, "test", &[source]).unwrap(),
            Value::bool(true)
        );
        assert_eq!(property(ctx, literal, "lastIndex").as_number(), Some(1.0));
    });
}

#[test]
fn regexp_exec_indices_duplicate_groups_and_unicode_sets() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let r = re(ctx, "(?<x>a)|(?<x>b)", "dg");
        let input = ctx.new_string("🐈b").unwrap();
        let m = method(ctx, r, "exec", &[input]).unwrap();
        let m = ctx.root(m).unwrap();
        assert_eq!(property(ctx, m, "index").as_number(), Some(2.0));
        assert_eq!(property(ctx, m, "input"), ctx.get(input));
        assert_eq!(property(ctx, r, "lastIndex").as_number(), Some(3.0));
        let groups = named(ctx, m, "groups");
        assert_eq!(get_prototype_of(ctx, groups).unwrap(), None);
        let value = property(ctx, groups, "x");
        assert_text(ctx, value, "b");
        let indices = named(ctx, m, "indices");
        let missing = get_index(ctx, indices, 1).unwrap();
        assert_eq!(ctx.get(missing), Value::UNDEFINED);
        let groups = named(ctx, indices, "groups");
        let pair = named(ctx, groups, "x");
        let first = get_index(ctx, pair, 0).unwrap();
        let last = get_index(ctx, pair, 1).unwrap();
        assert_eq!(
            (ctx.get(first).as_number(), ctx.get(last).as_number()),
            (Some(2.0), Some(3.0))
        );
        assert_eq!(method(ctx, r, "exec", &[input]).unwrap(), Value::NULL);
        assert_eq!(property(ctx, r, "lastIndex").as_number(), Some(0.0));
        let r = re(ctx, r"[\p{ASCII}&&\p{Letter}]", "v");
        assert_eq!(method(ctx, r, "test", &[input]).unwrap(), Value::bool(true));
        let digits = ctx.new_string("42π").unwrap();
        assert_eq!(
            method(ctx, r, "test", &[digits]).unwrap(),
            Value::bool(false)
        );
        let sticky = re(ctx, ".", "uy");
        let index = ctx.intern_str("lastIndex").unwrap();
        let one = num(ctx, 1.0);
        assert!(set_property(ctx, sticky, index, one).unwrap());
        let m = method(ctx, sticky, "exec", &[input]).unwrap();
        let m = ctx.root(m).unwrap();
        assert_eq!(property(ctx, m, "index").as_number(), Some(0.0));
        assert_eq!(property(ctx, sticky, "lastIndex").as_number(), Some(2.0));
    });
}

#[test]
fn catastrophic_regexp_backtracking_is_interruptible() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = ctx
            .new_string("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!")
            .unwrap();
        let re = re(ctx, "^(a+)+$", "");
        let count = Rc::new(Cell::new(0));
        let polls = count.clone();
        ctx.runtime.set_interrupt_handler(Some(Box::new(move || {
            let n = polls.get() + 1;
            polls.set(n);
            n >= 50
        })));
        assert_eq!(method(ctx, re, "exec", &[input]), Err(Abrupt::Interrupted));
        assert!(ctx.pending_exception().is_none());
        assert_eq!(count.get(), 50);
    });
}

fn replace_callback<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    assert_eq!(args.len(), 5);
    assert_text(ctx, ctx.get(args.get(0).unwrap()), "b");
    assert_text(ctx, ctx.get(args.get(1).unwrap()), "b");
    assert_eq!(ctx.get(args.get(2).unwrap()).as_number(), Some(1.0));
    assert_text(ctx, ctx.get(args.get(3).unwrap()), "abc");
    let value = property(ctx, args.get(4).unwrap(), "middle");
    assert_text(ctx, value, "b");
    ctx.new_string("!").map(|s| ctx.get(s))
}

#[test]
fn regexp_replace_and_literal_replace_all() {
    let mut rt = engine();
    rt.set_native_dispatch(&[replace_callback]);
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = ctx.new_string("abc").unwrap();
        let re = re(ctx, "(?<middle>b)", "g");
        let replacement = ctx
            .new_string("$$|$&|$`|$'|$1|$01|$10|$99|$0|$<middle>|$<other>")
            .unwrap();
        let value = method(ctx, input, "replaceAll", &[re, replacement]).unwrap();
        assert_text(ctx, value, "a$|b|a|c|b|b|b0|$99|$0|b|c");
        let callback = ctx.new_native(NativeFnId(0), false).unwrap();
        let value = method(ctx, input, "replace", &[re, callback]).unwrap();
        assert_text(ctx, value, "a!c");
        let empty = ctx.new_string("").unwrap();
        let hyphen = ctx.new_string("-").unwrap();
        let value = method(ctx, input, "replaceAll", &[empty, hyphen]).unwrap();
        assert_text(ctx, value, "-a-b-c-");
    });
}

#[test]
fn string_pattern_hooks_apply_to_objects_only() {
    use crate::api::{EvalOptions, JsEngine};

    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    engine
        .eval(
            r#"
            "use strict";
            const input = "a1b1c";
            const boxed = Object(1);
            for (const [name, extra, expected] of [
                ["match", undefined, "1"],
                ["matchAll", undefined, "1,3"],
                ["replace", "X", "aXb1c"],
                ["replaceAll", "X", "aXbXc"],
                ["search", undefined, 1],
                ["split", 2, "a,b"],
            ]) {
                const key = Symbol[name === "replaceAll" ? "replace" : name];
                Object.defineProperty(Number.prototype, key, {
                    configurable: true,
                    get() {
                        if (this !== boxed) throw new Error(name + " read a primitive hook");
                        return function (receiver) {
                            if (this !== boxed || receiver !== input) {
                                throw new Error(name + " called the hook with the wrong arguments");
                            }
                            return "hook";
                        };
                    },
                });
                let result = input[name](1, extra);
                if (name === "match") result = result[0];
                if (name === "matchAll") result = Array.from(result, match => match.index).join(",");
                if (name === "split") result = result.join(",");
                if (result !== expected) throw new Error(name + " used the wrong fallback");
                if (input[name](boxed, extra) !== "hook") {
                    throw new Error(name + " skipped the object hook");
                }
                delete Number.prototype[key];
            }
            "#,
            EvalOptions::default(),
        )
        .unwrap();
}

#[test]
fn regexp_match_all_search_and_split_protocols() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = ctx.new_string("a🐈b").unwrap();
        let empty = re(ctx, "(?:)", "gu");
        let matches = method(ctx, input, "match", &[empty]).unwrap();
        let matches = ctx.root(matches).unwrap();
        assert_eq!(property(ctx, matches, "length").as_number(), Some(4.0));
        let iterator = method(ctx, input, "matchAll", &[empty]).unwrap();
        let iterator = ctx.root(iterator).unwrap();
        let tag = get_property(ctx, iterator, sym(ctx, WellKnown::ToStringTag)).unwrap();
        assert_text(ctx, tag, "RegExp String Iterator");
        for want in [0.0, 1.0, 3.0, 4.0] {
            let next = method(ctx, iterator, "next", &[]).unwrap();
            let next = ctx.root(next).unwrap();
            assert_eq!(property(ctx, next, "done"), Value::bool(false));
            let value = named(ctx, next, "value");
            assert_eq!(property(ctx, value, "index").as_number(), Some(want));
        }
        let next = method(ctx, iterator, "next", &[]).unwrap();
        let next = ctx.root(next).unwrap();
        assert_eq!(property(ctx, next, "done"), Value::bool(true));
        let re = re(ctx, "(🐈)", "u");
        let result = method(ctx, input, "matchAll", &[re]);
        exception_kind(ctx, result, ErrorKind::Type);
        let index = ctx.intern_str("lastIndex").unwrap();
        let neg_zero = num(ctx, -0.0);
        assert!(set_property(ctx, re, index, neg_zero).unwrap());
        assert_eq!(
            method(ctx, input, "search", &[re]).unwrap().as_number(),
            Some(1.0)
        );
        assert!(
            property(ctx, re, "lastIndex")
                .as_number()
                .unwrap()
                .is_sign_negative()
        );
        let pieces = method(ctx, input, "split", &[re]).unwrap();
        let pieces = ctx.root(pieces).unwrap();
        assert_eq!(property(ctx, pieces, "length").as_number(), Some(3.0));
        for (i, want) in ["a", "🐈", "b"].into_iter().enumerate() {
            let val = get_index(ctx, pieces, i).unwrap();
            assert_text(ctx, ctx.get(val), want);
        }
    });
}

fn species_split<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    assert_eq!(args.len(), 2);
    assert_text(ctx, ctx.get(args.get(1).unwrap()), "uy");
    let matcher = re(ctx, "a", "y");
    assert!(
        ctx.heap()
            .object(ctx.get(this).as_object().unwrap())
            .flags
            .contains(ObjectFlags::EXTENSIBLE)
    );
    Ok(ctx.get(matcher))
}

#[test]
fn regexp_split_calls_species_with_sticky_flags() {
    let mut rt = engine();
    rt.set_native_dispatch(&[species_split]);
    rt.with_scope(rt.default_realm(), |ctx| {
        let original = re(ctx, "x", "u");
        let constructor = ctx.new_object().unwrap();
        let species_fn = ctx.new_native(NativeFnId(0), true).unwrap();
        define_data(
            ctx,
            constructor,
            sym(ctx, WellKnown::Species),
            species_fn,
            PropFlags::DEFAULT_DATA,
        )
        .unwrap();
        let key = ctx.intern_str("constructor").unwrap();
        define_data(ctx, original, key, constructor, PropFlags::DEFAULT_DATA).unwrap();
        let input = ctx.new_string("xax").unwrap();
        let result = method(ctx, input, "split", &[original]).unwrap();
        let result = ctx.root(result).unwrap();
        assert_eq!(property(ctx, result, "length").as_number(), Some(2.0));
    });
}

#[test]
fn uri_encoders_and_decoders_preserve_their_distinct_sets() {
    let mut rt = engine();
    rt.with_scope(rt.default_realm(), |ctx| {
        let undef = ctx.root(Value::UNDEFINED).unwrap();
        for (name, input, output) in [
            ("encodeURI", "a #/🐈", "a%20#/%F0%9F%90%88"),
            ("encodeURIComponent", "a #/🐈", "a%20%23%2F%F0%9F%90%88"),
            ("decodeURI", "%23%2f%3F%F0%9F%90%88", "%23%2f%3F🐈"),
            ("decodeURIComponent", "%23%2f%3F%C2%A0", "#/?\u{a0}"),
            ("escape", "@+é🐈", "@+%E9%uD83D%uDC08"),
            ("unescape", "%e9%uD83d%uDC08%ufoo%U0041%", "é🐈%ufoo%U0041%"),
        ] {
            let f = global_value(ctx, name);
            let src = ctx.new_string(input).unwrap();
            let actual = call_with(ctx, f, undef, &[src]).unwrap();
            assert_text(ctx, actual, output);
        }
        let lone = ctx.new_utf16(&[0xd800]).unwrap();
        let encode = global_value(ctx, "encodeURIComponent");
        let result = call_with(ctx, encode, undef, &[lone]);
        exception_kind(ctx, result, ErrorKind::Uri);
        let decode = global_value(ctx, "decodeURIComponent");
        for len in [20, 21] {
            let head_text = "x".repeat(len);
            let head = ctx.new_string(&head_text).unwrap();
            let tail = ctx.new_string("%F0%9F%90%88").unwrap();
            let rope = ctx.concat_strings(head, tail).unwrap();
            let actual = call_with(ctx, decode, undef, &[rope]).unwrap();
            assert_text(ctx, actual, &(head_text + "🐈"));
        }
        let actual = call_with(ctx, decode, undef, &[lone]).unwrap();
        assert_units(ctx, actual, &[0xd800]);
        for invalid in [
            "%",
            "%A",
            "%u0041",
            "%C0%80",
            "%E2%82",
            "%E2x%AC",
            "%ED%A0%80",
            "%F4%90%80%80",
            "%80",
            "%GG",
        ] {
            let s = ctx.new_string(invalid).unwrap();
            let result = call_with(ctx, decode, undef, &[s]);
            exception_kind(ctx, result, ErrorKind::Uri);
        }
        let unescape = global_value(ctx, "unescape");
        let escaped = ctx.new_string("%ud800").unwrap();
        let actual = call_with(ctx, unescape, undef, &[escaped]).unwrap();
        assert_units(ctx, actual, &[0xd800]);
    });
}
