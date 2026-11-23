use super::*;

fn script(text: &str) -> Result<ParsedProgram, ParseError> {
    parse(
        &SourceText::from_utf8("test.js", text),
        ParseOptions::default(),
    )
}

fn module(text: &str) -> Result<ParsedProgram, ParseError> {
    parse(
        &SourceText::from_utf8("test.mjs", text),
        ParseOptions {
            goal: ParseGoal::Module,
            ..ParseOptions::default()
        },
    )
}

fn valid(cases: &[&str]) {
    for &text in cases {
        script(text).unwrap_or_else(|e| panic!("{text}\n{e}"));
    }
}

fn invalid(cases: &[&str]) {
    for &text in cases {
        assert_eq!(
            script(text).expect_err(text).kind,
            ParseErrorKind::Syntax,
            "{text}"
        );
    }
}

fn expression(tree: &ParsedProgram, n: usize) -> &Expr {
    let Stmt::Expression(id) = tree.arena.stmt(tree.body[n]).kind else {
        panic!("expected expression")
    };
    &tree.arena.expr(id).kind
}

#[test]
fn source_spans_and_lone_surrogates() {
    let utf8 = SourceText::from_utf8("sample.js", "'😀';\r\n'β'\u{2028}ok\u{2029}");
    assert_eq!(utf8.line_starts, [0, 9, 16, 21]);
    assert_eq!(utf8.line_col(8), (1, 9));
    assert_eq!(utf8.line_col(16), (3, 1));
    assert_eq!(utf8.slice(Span::new(1, 5)), "😀".as_bytes());
    let original = vec![39, 0xd800, 39, 59, 13, 10, 39, 0xd83d, 0xde00, 39];
    let utf16 = SourceText::from_utf16("<eval>", original.clone());
    let parsed = parse(&utf16, ParseOptions::default()).unwrap();
    assert_eq!(utf16.line_starts, [0, 8]);
    assert_eq!(utf16.slice_utf16(parsed.span), original);
    assert!(
        matches!(expression(&parsed, 0), Expr::String(s) if s.value == JsStrLit::Utf16(vec![0xd800]))
    );
    assert!(
        matches!(expression(&parsed, 1), Expr::String(s) if s.value == JsStrLit::Utf16(vec![0xd83d, 0xde00]))
    );
}

#[test]
fn exact_function_source() {
    let raw = "function /* preserve */ f(π) {\r\n return π;\r\n}";
    let source = SourceText::from_utf8("sample.js", &format!("// comment\n{raw}"));
    let parsed = parse(&source, ParseOptions::default()).unwrap();
    let Stmt::Function(f) = &parsed.arena.stmt(parsed.body[0]).kind else {
        panic!()
    };
    assert_eq!(source.slice(f.span), raw.as_bytes());
    let method = "async *f() {}";
    let source = SourceText::from_utf8(
        "sample.js",
        &format!("class C {{ static /* comment */ {method} }}"),
    );
    let tree = parse(&source, ParseOptions::default()).unwrap();
    let Stmt::Class(class) = &tree.arena.stmt(tree.body[0]).kind else {
        panic!()
    };
    let ClassElement::Method { function, .. } = &class.elements[0] else {
        panic!()
    };
    assert_eq!(source.slice(function.span), method.as_bytes());
}

#[test]
fn spans_separate_parentheses_from_the_expression() {
    let source = SourceText::from_utf8("sample.js", "( a, b /* comment */ );");
    let tree = parse(&source, ParseOptions::default()).unwrap();
    let Expr::Parenthesized(inner) = expression(&tree, 0) else {
        panic!()
    };
    assert_eq!(source.slice(tree.arena.expr(*inner).span), b"a, b");
}

#[test]
fn lexical_goals() {
    let source = SourceText::from_utf8("sample.js", "/=a[\\/]/giu / /=");
    let mut lexer = Lexer::new(&source, true, true);
    assert!(
        matches!(lexer.next(LexicalGoal::RegExp).unwrap().kind, TokenKind::RegExp(r) if r.flags == "giu")
    );
    assert_eq!(
        lexer.next(LexicalGoal::Div).unwrap().kind,
        TokenKind::Punct("/")
    );
    assert_eq!(
        lexer.next(LexicalGoal::Div).unwrap().kind,
        TokenKind::Punct("/=")
    );
    let source = SourceText::from_utf8("sample.js", "}a${");
    assert!(matches!(
        Lexer::new(&source, false, false)
            .next(LexicalGoal::TemplateTail)
            .unwrap()
            .kind,
        TokenKind::Template { tail: false, .. }
    ));
    valid(&[
        "if (a) /x/.test(a); a /= 3 / 2;",
        "let a = /[\\]]/v;",
        "let x = /x/d;",
        "function f(){return /=/.test('=');}",
    ]);
    invalid(&[
        "/x/uu",
        "/x/uv",
        "/x/foo",
        "/x/\\u0067",
        "/x\n/",
        "/(/",
        "/[z-a]/",
        r"/\a/u",
        "/(?i-i:a)/",
    ]);
    valid(&["/😀+/u", "/😀+/", "/(?i:a)/", "/[a&&b]/v"]);
}

#[test]
fn unicode_identifiers_and_keywords() {
    valid(&[
        "let\u{00a0}π = 1;\u{feff} π;",
        "const 𐐀 = 1; 𐐀;",
        r"const \u{10400} = 1;",
        "var a\u{200c} = 1",
        r"({\u0069f: 1}).\u0069f",
        r"var \u0061sync = 0",
        r"class C { #\u0078; f(o){return o.#x;} }",
    ]);
    invalid(&[
        r"var \u0069f = 1",
        r"\u0074his;",
        r"if (1) \u0076ar a;",
        r"let \u{110000};",
        "let \u{200d}a;",
        r"let x\u{20};",
    ]);
    let source = SourceText::from_utf16("<eval>", "var 𐐀 = 1;".encode_utf16().collect());
    assert!(parse(&source, ParseOptions::default()).is_ok());
}

#[test]
fn hashbang_html_and_comments() {
    valid(&[
        "#!/usr/bin/env qjs\nlet x = 0;",
        "<!-- outer\nx\n /* c */ --> rest\n x;",
        "a /*\n*/ --> rest\nb;",
    ]);
    invalid(&[
        " #! late",
        "\n#! second",
        "/* unclosed",
        "a -->",
        "\u{feff}#! late",
    ]);
    assert!(module("#!go\nexport {};").is_ok());
    assert!(module("<!-- x\nexport {};").is_err());
    assert!(module("\n--> x\nexport {};").is_err());
    assert!(
        parse(
            &SourceText::from_utf8("sample.js", "<!-- x"),
            ParseOptions {
                annex_b: false,
                ..ParseOptions::default()
            }
        )
        .is_err()
    );
}

#[test]
fn numbers_keep_raw_digits_and_parse_on_demand() {
    let raw = "0; .25; 1_024.5_0e+1; 0Xf_F; 0b10_10; 0O7_0; 077; 08.5; 12_3n; 0xF_Fn;";
    let tree = script(raw).unwrap();
    let expected = [
        Some(0.0),
        Some(0.25),
        Some(10245.0),
        Some(255.0),
        Some(10.0),
        Some(56.0),
        Some(63.0),
        Some(8.5),
        None,
        None,
    ];
    for (i, value) in expected.into_iter().enumerate() {
        let Expr::Number(lit) = expression(&tree, i) else {
            panic!()
        };
        assert_eq!(lit.raw, raw.split(';').nth(i).unwrap().trim());
        assert_eq!(lit.parse_numeric_value(), value);
    }
    assert!(
        matches!(expression(&tree, 9), Expr::Number(n) if n.kind == NumericKind::BigInt { radix: 16 })
    );
    invalid(&[
        "00n",
        "0_1",
        "1__0",
        "1_",
        "0x_F",
        "0b2",
        "0o8",
        "0x",
        "1._0",
        "1e_0",
        "1e+",
        "1.0n",
        "07e1",
        "09_1",
        "1foo",
        r"1\u0061",
        "'use strict'; 077",
        "'use strict'; 08",
    ]);
}

#[test]
fn strings_and_template_raw_cooked() {
    let tree =
        script("tag`a\\n\r\n${1 + 2}\\u{1f600}`; tag`\\xQ`; '\\uD800'; 'a\\\r\nb';").unwrap();
    let Expr::TaggedTemplate { template, .. } = expression(&tree, 0) else {
        panic!()
    };
    assert_eq!(
        template.elements[0].raw,
        JsStrLit::Latin1(b"a\\n\n".to_vec())
    );
    assert_eq!(
        template.elements[0].cooked,
        Some(JsStrLit::Latin1(b"a\n\n".to_vec()))
    );
    assert_eq!(
        template.elements[1].cooked,
        Some(JsStrLit::Utf16(vec![0xd83d, 0xde00]))
    );
    assert!(
        matches!(expression(&tree, 1), Expr::TaggedTemplate { template, .. } if template.elements[0].cooked.is_none())
    );
    assert!(
        matches!(expression(&tree, 2), Expr::String(s) if s.value == JsStrLit::Utf16(vec![0xd800]))
    );
    assert!(
        matches!(expression(&tree, 3), Expr::String(s) if s.value == JsStrLit::Latin1(b"ab".to_vec()))
    );
    valid(&[
        r"'\377\8\0\x41\u0042';",
        r"tag`\1\8\u{110000}`",
        "`a${`b${c}d`}e`",
        "'a\u{2028}b'",
    ]);
    invalid(&[
        r"`\8`",
        r"`\1`",
        r"'\x'",
        r"'\u{110000}'",
        "`unclosed",
        "`a${x`",
        "'a\nb'",
    ]);
}

#[test]
fn asi_and_restricted_productions() {
    let tree = script("function f() { return /*\n*/ 0\n} x\n++y; while(1) { break\nbreakLabel; }")
        .unwrap();
    let Stmt::Function(f) = &tree.arena.stmt(tree.body[0]).kind else {
        panic!()
    };
    let FunctionBody::Block(body) = &f.body else {
        panic!()
    };
    assert!(matches!(tree.arena.stmt(body[0]).kind, Stmt::Return(None)));
    assert!(matches!(
        expression(&tree, 2),
        Expr::Update { prefix: true, .. }
    ));
    valid(&[
        "async\nfunction f(){}",
        "async\n(x)",
        "function* g(){ yield\n1; yield* g(); }",
        "label: while(true){continue\nlabel}",
        "do{}while(1) 2;",
        "using\nx = null;",
        "async function f(){ await\nusing; }",
        "async function f(){ await using\nx = null; }",
        "a --> rest",
    ]);
    invalid(&[
        "throw\n1",
        "throw /*\n*/ 1",
        "x\n=> x",
        "(x)\n=> x",
        "async\nx => x y",
        "do let x=0; while (0)",
    ]);
}

#[test]
fn cover_grammar_and_reference_shape() {
    valid(&[
        "(a, {b = 1}, [c], ...rest) => a",
        "async ({a=1}, b=2) => await b",
        "async a => a",
        "async(x, ...y,)",
        "([,a, ...[b]] = value)",
        "for ([a,b] of xs) {}",
        "function f(...[x]){}",
        "([obj.a = 0] = xs)",
    ]);
    invalid(&[
        "()",
        "(...args)",
        "(a,)",
        "((a)) => a",
        "(obj.a) => a",
        "(obj.a=0) => a",
        "(...a,) => a",
        "(...a=0) => a",
        "({a=1})",
        "({a(){}}=x)",
        "([...a,]=x)",
        "({...a,}=x)",
        "({...{a}}=x)",
        "(({a})=x)",
        "({a:f()}=x)",
        "1+(x)=>x",
    ]);
    let tree = script("({[sourceKey()]: target[targetKey()] = makeDefault()} = source)").unwrap();
    let Expr::Parenthesized(assignment) = expression(&tree, 0) else {
        panic!()
    };
    let Expr::Assignment { target, .. } = tree.arena.expr(*assignment).kind else {
        panic!()
    };
    let Pattern::Object { properties, .. } = &tree.arena.pattern(target).kind else {
        panic!()
    };
    assert!(
        matches!(properties[0].key, Name::Computed(id) if matches!(tree.arena.expr(id).kind, Expr::Call { .. }))
    );
    let Pattern::Default { target, .. } = tree.arena.pattern(properties[0].target).kind else {
        panic!()
    };
    let Pattern::Reference(reference) = tree.arena.pattern(target).kind else {
        panic!()
    };
    assert!(matches!(
        tree.arena.expr(reference).kind,
        Expr::Member {
            key: Name::Computed(_),
            ..
        }
    ));
}

#[test]
fn operators_and_optional_chains() {
    valid(
        &[
            "a?.b.c?.(x)?.[y] ?? z",
            "(a?.b).c = 1",
            "(a ?? b) && c",
            "a ?? (b || c)",
            "x ** -(y ** z)",
            "(-x) ** y",
            "a &&= b; a ||= c; a ??= d",
            "x = typeof void delete f().x;",
            "new new C()(); new C().m(); new (x?.C)();",
            "(a?.tag)`x`;",
            "a ? .2 : b",
            "a >>>= 2;",
            "delete a?.b",
        ][..],
    );
    invalid(&[
        "a?.b = 1",
        "a?.b++",
        "new a?.b()",
        "a?.b`x`",
        "a ?? b || c",
        "a && b ?? c",
        "-x ** y",
        "x ** -y ** z",
        "[a] += b",
        "++1",
        "new import('x')",
        "import(...s)",
    ]);
}

#[test]
fn directives_and_strict_restrictions() {
    valid(&[
        r#"'\x75se strict'; with(o) {}"#,
        "('use strict'); with(o) {}",
        "x; 'use strict'; with(o) {}",
        "function f(a,a){}",
        "function* f(a,a){}",
        "async function f(a,a){}",
        "async function await() {}",
        "function* yield() {}",
        "function f(a = 0) { ('use strict'); }",
        "var arguments; eval=1; with({}) {}",
        "{ 'use strict'; with(o) {} }",
    ]);
    invalid(&[
        "'use strict'; with(o) {}",
        r"'\1'; 'use strict';",
        "'use strict'; delete (a)",
        "'use strict'; arguments++",
        "'use strict'; ({x: eval} = o)",
        "function f(a=0){'use strict'}",
        "function f(a,a){'use strict'}",
        "function f(a,a=1){}",
        "(a,a)=>a",
        "({f(a,a){}})",
        "function eval(){'use strict'}",
        "'use strict'; label: function f(){}",
        "'use strict'; var implements",
        "(async function await() {})",
        "(function* yield() {})",
        "async function f(){function await(){}}",
    ]);
}

#[test]
fn statement_and_binding_scopes() {
    valid(&[
        "var x; var x; {let x;}",
        "function f(x){var x; {let x;}}",
        "try {} catch(e){var e;} finally {debugger;}",
        "try {} catch {}",
        "{function f(){} function f(){}}",
        "if (a) function f(){} else function f(){}",
        "again: next: while (true) { continue again; break next; }",
        "switch(x){case 1: let a; break; default: {let a;}}",
        "for (let i=0;i<3;i++){}",
        "for(var x = 1 in o){}",
        "for (using item of items) {}",
        "async function f(){ for await (const x of y) {} }",
        "function f(){using x = y, z = w;}",
        "using [x] = y;",
        "for (let in y) {}",
        "for (using of of [0, 1, 2]) {}",
        "for (using of = null;;) {}",
        "'use strict'; try {} catch(e){var e;}",
    ]);
    invalid(&[
        "let a; let a;",
        "let a; {var a;}",
        "{var a; let a;}",
        "try{} catch({x}){var x;}",
        "try{} catch(x){let x;}",
        "function f(x){let x;}",
        "function f(){let f; {var f;}}",
        "{function f(){} var f;}",
        "'use strict'; {function f(){} function f(){}}",
        "if (x) label: function f(){}",
        "label: function* f(){}",
        "switch (x) { case 0: let a; case 1: const a = 0; }",
        "switch(x){default: default:}",
        "label: { continue label; }",
        "while (true) {function f(){break;}}",
        "a: a: ;",
        "return 0",
        "continue",
        "break",
        "let {x};",
        "const x;",
        "let let;",
        "for (const x=0 of y){}",
        "for (using x in y){}",
        "for await (x of y){}",
        "for (let of y){}",
        "using x;",
        "using {x} = y;",
        "using x = y;",
        "switch(x){ case true: using z = x; }",
        "async function g(){switch(x){ default: await using z = x; }}",
    ]);
}

#[test]
fn annex_b_call_targets() {
    valid(&[
        "f() = rhs()",
        "f() += rhs()",
        "++f()",
        "f()--",
        "for(f() in x) {}",
        "for(f() of x) {}",
        "async(x) = rhs()",
    ]);
    invalid(&[
        "'use strict'; f() = 1",
        "'use strict'; f()++",
        "f?.() = 1",
        "[f()] = x",
        "import('x') = 1",
        "f() &&= 1",
        "(f()) ??= 1",
    ]);
    assert!(
        parse(
            &SourceText::from_utf8("sample.js", "f() = 1"),
            ParseOptions {
                annex_b: false,
                ..ParseOptions::default()
            }
        )
        .is_err()
    );
}

#[test]
fn class_elements_private_scopes_and_contexts() {
    valid(&[
        "class C extends Base { constructor(){ super(); } static { super.x; new.target; var a; } static #x=1; #y; f(o){ return o.#y ?? (super.foo + new.target); } get #z(){ return this.#y; } set #z(v){ this.#y = v; } ['computed'](){} async *g(){ yield await x; } }",
        "class C { get\n *one(){} set\n *two(){} async\n *three(){} }",
        "class C { get\n x() {} }",
        "class C { f(o){return #x in o;} #x; }",
        "class O { #x; f(){return class I { f(o){return o.#x;} };} }",
        "class C { x = (()=>new.target)(); [#x in {}](){} #x; }",
        "({m(){ return (()=>super.x)(); }})",
        "class C { static name; static constructor(){} }",
    ]);
    invalid(&[
        "class C { #x; #x; }",
        "class C { get #x(){} static set #x(v){} }",
        "class C { #constructor; }",
        "class C { constructor; }",
        "class C { static prototype(){} }",
        "class C { constructor(){} 'constructor'(){} }",
        "class C { async constructor(){} }",
        "class C { get x(v){} }",
        "class C { set x(...v){} }",
        "class C { f(o){ #x in o = 1; } #x; }",
        "class C { f(o){ return o.#x; } }",
        "class C extends (#x in {}) { #x; }",
        "class C extends () => {} {}",
        "class C { #x; g(){delete this?.#x;} }",
        "class C { static { return; } }",
        "class C { static { await x; } }",
        "class C { x = (()=>arguments)(); }",
        "class C { constructor(){ super(); } }",
        "class C extends B { f(){ super(); } }",
        "class C extends B { f(){ super?.x; } }",
        "new.target",
        "super.x",
        "({ m(){ function f(){ return super.x; } } })",
    ]);
}

#[test]
fn module_imports_exports_and_resource_bindings() {
    for text in [
        "import 'a' with { type: 'text' }; import * as data from 'b' with { 'type': 'bytes' };",
        "import value, { 'arbitrary key' as k, if as q } from 'x' with { type: 'json', extra: 'yes', }; export { value as 'other key', k, q };",
        "export * as 'ns value' from 'x'; export * from 'y'; export { 'from here' as to } from 'z' with {};",
        "export default async function* () { yield await import('x', { with: { type: 'json' } }); }",
        "using x = null; await using y = null; export {x, y}; await import.meta;",
        "export {x}; { var x; } export let {foo: [bar]} = data;",
    ] {
        module(text).unwrap_or_else(|e| panic!("{text}\n{e}"));
    }
    for text in [
        "export {missing};",
        "export {x}; { let x; }",
        "export using x = y;",
        "export await using x = y;",
        "export {x}; export const x=0;",
        "export * as 'default' from 'x'; export default 0;",
        "export default 1; export default 2;",
        "import {x} from 'a'; let x;",
        "import {'x'} from 'a';",
        r"export {'\ud800' as x} from 'a';",
        "import {default} from 'a';",
        "import 'a' with {type:'json','type':'json'};",
        "import 'a' with {type: 0};",
        "{ import 'x'; }",
        "await: ;",
        "function f(){ let await; }",
        "var yield;",
        "export default (await = 1)",
    ] {
        assert_eq!(module(text).expect_err(text).kind, ParseErrorKind::Syntax);
    }
    invalid(&[
        "export default 0",
        "import 'x';",
        "import.meta",
        "await using x = y;",
    ]);
}

#[test]
fn eval_and_function_body_goals() {
    let common = ParseOptions::default();
    let goal = ParseGoal::FunctionBody {
        is_async: true,
        is_generator: true,
    };
    let source = SourceText::from_utf8("<function>", "yield await x; return new.target;");
    assert!(parse(&source, ParseOptions { goal, ..common }).is_ok());
    let goal = ParseGoal::DirectEval {
        allow_new_target: true,
        allow_super_property: true,
        allow_super_call: true,
        in_class_field_init: false,
    };
    let source = SourceText::from_utf8("<eval>", "super(); super.x; new.target;");
    assert!(parse(&source, ParseOptions { goal, ..common }).is_ok());
    let goal = ParseGoal::DirectEval {
        allow_new_target: true,
        allow_super_property: true,
        allow_super_call: false,
        in_class_field_init: true,
    };
    let source = SourceText::from_utf8("<eval>", "arguments;");
    assert!(parse(&source, ParseOptions { goal, ..common }).is_err());
}

#[test]
fn depth_budget() {
    let options = ParseOptions {
        max_depth: 16,
        ..ParseOptions::default()
    };
    for text in [
        format!("{}0{}", "(".repeat(2000), ")".repeat(2000)),
        "!".repeat(2000) + "0",
        "a+".repeat(2000) + "0",
        "{".repeat(2000) + &"}".repeat(2000),
    ] {
        let source = SourceText::from_utf8("sample.js", &text);
        assert_eq!(
            parse(&source, options).unwrap_err().kind,
            ParseErrorKind::StackOverflow
        );
    }
    let source = SourceText::from_utf8(
        "sample.js",
        &format!("{}0{}", "(".repeat(5000), ")".repeat(5000)),
    );
    assert_eq!(
        parse(&source, ParseOptions::default()).unwrap_err().kind,
        ParseErrorKind::StackOverflow
    );
    let nested = format!("{}0{}", "(".repeat(600), ")".repeat(600));
    let source = SourceText::from_utf8("sample.js", &nested);
    assert!(
        parse(
            &source,
            ParseOptions {
                max_depth: 2048,
                ..ParseOptions::default()
            }
        )
        .is_ok()
    );
}

#[test]
fn bytes_budget() {
    let options = ParseOptions {
        max_bytes: 1000,
        ..ParseOptions::default()
    };
    for text in [" ".repeat(2500), "0;".repeat(40)] {
        let source = SourceText::from_utf8("sample.js", &text);
        assert_eq!(
            parse(&source, options).unwrap_err().kind,
            ParseErrorKind::OutOfMemory
        );
    }
    let source = SourceText::from_utf8("sample.js", "let a = 1;");
    assert!(parse(&source, options).is_ok());
}
