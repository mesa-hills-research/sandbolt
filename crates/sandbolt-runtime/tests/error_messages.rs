use sandbolt_runtime::api::{EvalOptions, JsEngine};
use sandbolt_runtime::{EngineError, EngineLimits};

fn eval_error(source: &str) -> String {
    let mut engine = JsEngine::new(EngineLimits::default()).expect("engine");
    match engine.eval(source, EvalOptions::default()) {
        Err(EngineError::Thrown { message, .. }) => message,
        Ok(value) => panic!("expected error for `{source}`, got value `{value:?}`"),
        Err(other) => panic!("expected thrown error for `{source}`, got `{other:?}`"),
    }
}

#[test]
fn undeclared_and_tdz_bindings_name_the_identifier() {
    for (source, expected) in [
        (
            "undeclared + 1",
            "ReferenceError: undeclared is not defined",
        ),
        ("undeclared()", "ReferenceError: undeclared is not defined"),
        (
            "'use strict'; undeclared = 1",
            "ReferenceError: undeclared is not defined",
        ),
        ("undeclared++", "ReferenceError: undeclared is not defined"),
        ("undeclared.a", "ReferenceError: undeclared is not defined"),
        (
            "(() => { let x = x; })()",
            "ReferenceError: x is not initialized",
        ),
        (
            "(() => { x = 1; let x = 2; })()",
            "ReferenceError: x is not initialized",
        ),
        (
            "(() => { const f = () => y; f(); let y = 1; })()",
            "ReferenceError: y is not initialized",
        ),
        (
            "(() => { const f = () => { y = 1; }; f(); let y = 2; })()",
            "ReferenceError: y is not initialized",
        ),
        (
            "(() => { x = 1; const x = 2; })()",
            "ReferenceError: x is not initialized",
        ),
        (
            "class A {} class B extends A { constructor() { this.a = 1; } } new B();",
            "ReferenceError: this is not initialized",
        ),
        (
            "class A {} class B extends A { constructor() { super(); super(); } } new B();",
            "ReferenceError: 'this' can be initialized only once",
        ),
        (
            "class A {} class B extends A { constructor() { (() => super())(); (() => super())(); } } new B();",
            "ReferenceError: 'this' can be initialized only once",
        ),
    ] {
        assert_eq!(eval_error(source), expected, "{source}");
    }
}

#[test]
fn global_redeclaration_and_constant_writes_name_the_binding() {
    let mut engine = JsEngine::new(EngineLimits::default()).expect("engine");
    engine
        .eval("let taken = 1;", EvalOptions::default())
        .expect("initial script");
    let err = engine
        .eval("let taken = 2;", EvalOptions::default())
        .unwrap_err();
    match err {
        EngineError::Thrown { message, .. } => {
            assert_eq!(message, "SyntaxError: redeclaration of 'taken'");
        }
        other => panic!("expected thrown error, got {other:?}"),
    }

    for (source, expected) in [
        ("const x = 1; x = 2;", "TypeError: 'x' is read-only"),
        (
            "(() => { const x = 1; x = 2; })()",
            "TypeError: 'x' is read-only",
        ),
        ("const x = 1; x++;", "TypeError: 'x' is read-only"),
        ("const x = 1; x += 2;", "TypeError: 'x' is read-only"),
        ("const x = 1; [x] = [2];", "TypeError: 'x' is read-only"),
        (
            "const c = 1; (() => { c = 2; })();",
            "TypeError: 'c' is read-only",
        ),
        (
            "for (const x of [1]) { x = 2; }",
            "TypeError: 'x' is read-only",
        ),
    ] {
        assert_eq!(eval_error(source), expected, "{source}");
    }
}

#[test]
fn non_callable_and_property_errors_name_the_cause() {
    for (source, expected) in [
        ("undefined()", "TypeError: not a function"),
        ("null()", "TypeError: not a function"),
        ("(42)()", "TypeError: not a function"),
        ("({}.missing)()", "TypeError: not a function"),
        ("const obj = {}; obj.missing()", "TypeError: not a function"),
        ("const obj = { f: 1 }; obj.f()", "TypeError: not a function"),
        ("[1, 2][0]()", "TypeError: not a function"),
        (
            "undefined.foo",
            "TypeError: cannot read property 'foo' of undefined",
        ),
        ("null.foo", "TypeError: cannot read property 'foo' of null"),
        (
            "undefined.foo = 1",
            "TypeError: cannot set property 'foo' of undefined",
        ),
        (
            "null.foo = 1",
            "TypeError: cannot set property 'foo' of null",
        ),
        ("null[0] = 1", "TypeError: cannot set property '0' of null"),
        (
            "delete undefined.foo",
            "TypeError: Cannot convert undefined or null to object",
        ),
        (
            "delete null.foo",
            "TypeError: Cannot convert undefined or null to object",
        ),
        (
            "const { a } = null;",
            "TypeError: Cannot convert undefined or null to object",
        ),
        (
            "Object.keys(undefined)",
            "TypeError: Cannot convert undefined or null to object",
        ),
        (
            "'use strict'; const o = Object.freeze({ a: 1 }); o.a = 2;",
            "TypeError: 'a' is read-only",
        ),
        (
            "'use strict'; const o = Object.preventExtensions({}); o.a = 1;",
            "TypeError: object is not extensible",
        ),
        (
            "'use strict'; const o = { get a() { return 1; } }; o.a = 2;",
            "TypeError: no setter for property",
        ),
        (
            "'use strict'; const o = Object.freeze({ a: 1 }); delete o.a;",
            "TypeError: could not delete property",
        ),
        ("'use strict'; (1).a = 2;", "TypeError: not an object"),
        (
            "'use strict'; const p = new Proxy({}, { set() { return false; } }); p.a = 1;",
            "TypeError: proxy: cannot set property",
        ),
        ("'a' in null", "TypeError: invalid 'in' operand"),
        (
            "({}) instanceof 1",
            "TypeError: invalid 'instanceof' right operand",
        ),
        (
            "({}) instanceof {}",
            "TypeError: invalid 'instanceof' right operand",
        ),
        (
            "class C extends 1 {}",
            "TypeError: parent class must be constructor",
        ),
        (
            "class C { #x = 1; static get(o) { return o.#x; } } C.get({});",
            "TypeError: private class field '#x' does not exist",
        ),
    ] {
        assert_eq!(eval_error(source), expected, "{source}");
    }
}

#[test]
fn json_parse_errors_report_reason_and_position() {
    for (source, expected) in [
        (
            "JSON.parse('')",
            "SyntaxError: Unexpected end of JSON input",
        ),
        (
            "JSON.parse('\"abc')",
            "SyntaxError: Unexpected end of JSON input",
        ),
        (
            "JSON.parse('{bad')",
            "SyntaxError: Expected property name or '}' in JSON at position 1 (line 1 column 2)",
        ),
        (
            "JSON.parse('{\"a\": 1,}')",
            "SyntaxError: Expected property name or '}' in JSON at position 8 (line 1 column 9)",
        ),
        (
            "JSON.parse('{\"a\" 1}')",
            "SyntaxError: Expected ':' after property name in JSON at position 5 (line 1 column 6)",
        ),
        (
            "JSON.parse('{\"a\": 1 \"b\": 2}')",
            "SyntaxError: Expected ',' or '}' after property value in JSON at position 8 (line 1 column 9)",
        ),
        (
            "JSON.parse('[1, 2 3]')",
            "SyntaxError: Expected ',' or ']' after array element in JSON at position 6 (line 1 column 7)",
        ),
        (
            "JSON.parse('[1, 2')",
            "SyntaxError: Expected ',' or ']' after array element in JSON at position 5 (line 1 column 6)",
        ),
        ("JSON.parse('[1,]')", "SyntaxError: unexpected token: ']'"),
        (
            "JSON.parse('{\"a\":}')",
            "SyntaxError: unexpected token: '}'",
        ),
        (
            "JSON.parse('1 2')",
            "SyntaxError: unexpected data at the end",
        ),
        (
            "JSON.parse(undefined)",
            "SyntaxError: unexpected token: 'undefined'",
        ),
        (
            r#"JSON.parse('"\\x"')"#,
            "SyntaxError: Bad escaped character in JSON at position 2 (line 1 column 3)",
        ),
        (
            r#"JSON.parse('"\\u123g"')"#,
            "SyntaxError: Bad Unicode escape in JSON at position 6 (line 1 column 7)",
        ),
        (
            r#"JSON.parse('"\\u12"')"#,
            "SyntaxError: Bad Unicode escape in JSON at position 5 (line 1 column 6)",
        ),
        (
            r#"JSON.parse('"a\nb"')"#,
            "SyntaxError: Bad control character in string literal in JSON at position 2 (line 1 column 3)",
        ),
        (
            "JSON.parse('01')",
            "SyntaxError: Unexpected number in JSON at position 0 (line 1 column 1)",
        ),
        (
            "JSON.parse('-')",
            "SyntaxError: No number after minus sign in JSON at position 0 (line 1 column 1)",
        ),
        (
            "JSON.parse('1.')",
            "SyntaxError: Unterminated fractional number in JSON at position 2 (line 1 column 3)",
        ),
        (
            "JSON.parse('1e+')",
            "SyntaxError: Exponent part is missing a number in JSON at position 3 (line 1 column 4)",
        ),
        (
            "JSON.parse('{\\n  \"a\": 1,\\n  \"b\": bad\\n}')",
            "SyntaxError: unexpected token: 'bad'",
        ),
        (
            "JSON.parse('{\\n  \"a\": 1,\\n  bad: 2\\n}')",
            "SyntaxError: Expected property name or '}' in JSON at position 14 (line 3 column 3)",
        ),
        (
            "JSON.rawJSON('{bad}')",
            "SyntaxError: invalid rawJSON string",
        ),
    ] {
        assert_eq!(eval_error(source), expected, "{source}");
    }
}

#[test]
fn syntax_and_validation_errors_name_the_offending_token_or_binding() {
    for (source, expected) in [
        (
            "1 + * 2",
            "SyntaxError: unexpected token in expression: '*'",
        ),
        ("(1 + 2", "SyntaxError: Unexpected end of input"),
        ("[1, ;]", "SyntaxError: unexpected token in expression: ';'"),
        (
            "({ a: })",
            "SyntaxError: unexpected token in expression: '}'",
        ),
        ("function () {}", "SyntaxError: function name expected"),
        ("class {}", "SyntaxError: class statement requires a name"),
        ("return 1;", "SyntaxError: return not in a function"),
        ("break;", "SyntaxError: break must be inside loop or switch"),
        ("continue;", "SyntaxError: continue must be inside loop"),
        ("const x;", "SyntaxError: missing initializer for variable"),
        ("let x = 1; let x = 2;", "SyntaxError: redeclaration of 'x'"),
        ("1 = 2", "SyntaxError: invalid assignment left-hand side"),
        (
            "'use strict'; function f(a, a) {}",
            "SyntaxError: duplicate parameter 'a'",
        ),
        ("const x: number = 1;", "SyntaxError: Unexpected token ':'"),
        (
            "function f(x: number) {}",
            "SyntaxError: Unexpected token ':'",
        ),
        (
            "interface Foo { a: number }",
            "SyntaxError: Unexpected identifier 'Foo'",
        ),
        (
            "const x = a as number;",
            "SyntaxError: Unexpected identifier 'as'",
        ),
        ("await 1;", "SyntaxError: Unexpected number"),
        (
            "function f() { await 1; }",
            "SyntaxError: Unexpected number",
        ),
        (
            "function f() { const x = await Promise.resolve(1); }",
            "SyntaxError: Unexpected identifier 'Promise'",
        ),
        ("const x = @;", "SyntaxError: unexpected token: '@'"),
        (
            "function f() { f(); } f();",
            "RangeError: Maximum call stack size exceeded",
        ),
    ] {
        assert_eq!(eval_error(source), expected, "{source}");
    }
}
