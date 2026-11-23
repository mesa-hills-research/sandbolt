use qjs_bytecode::{
    DebugInfo, FunctionBytecode, Instruction, Opcode, Operands, PcSourceMap, ProgramImage,
};
use qjs_syntax::SourceText;

use crate::api::{EvalOptions, JsEngine};
use crate::{EngineError, EngineLimits};

fn run(script: &str) -> String {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    engine
        .eval_to_string(script, EvalOptions::default())
        .unwrap()
}

#[test]
fn bytecode_uses_utf8_columns() {
    let code = [
        Instruction {
            opcode: Opcode::Push1,
            operands: Operands::Signed(1),
        },
        Instruction {
            opcode: Opcode::Throw,
            operands: Operands::None,
        },
    ]
    .into_iter()
    .flat_map(|op| op.encode().unwrap().as_bytes().to_vec())
    .collect();
    let image = ProgramImage {
        functions: vec![FunctionBytecode {
            code,
            stack_size: 1,
            debug: DebugInfo {
                filename: "example.js".into(),
                source_start: 0,
                source_end: 6,
                pc_map: vec![PcSourceMap {
                    pc: 1,
                    byte_offset: 4,
                }],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let source = SourceText::from_utf8("example.js", "éé!!");
    let Err(EngineError::Thrown { message, frames }) = engine.execute_image(&image, Some(&source))
    else {
        panic!("expected a throw");
    };
    assert_eq!(message, "1");
    assert_eq!(frames[0].file, "example.js");
    assert_eq!((frames[0].line, frames[0].column), (1, 5));
}

#[test]
fn prepares_stack_with_native_and_constructor_sites() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let options = EvalOptions {
        filename: "example.js".into(),
        ..Default::default()
    };
    engine
        .eval(
            r#"
        Error.prepareStackTrace = function (error, frames) {
            if (this !== Error || error.stack !== undefined) throw 'bad formatter receiver';
            return frames;
        };
        var kept = [0].map(function build() {
            function Example() { return new Error(); }
            return new Example();
        })[0].stack;
        delete Error.prepareStackTrace;
        "#,
            options.clone(),
        )
        .unwrap();
    engine.collect_garbage();
    let text = engine
        .eval_to_string(
            r#"
        let ctor = kept[0], caller = kept[1], native = kept[2];
        [ctor.getFunctionName(), ctor.getFunction().name, ctor.getFileName(),
         ctor.isConstructor(), caller.getFunctionName(), caller.isConstructor(),
         native.getFunction() === Array.prototype.map, native.isNative(),
         native.getFileName() === null, native.getLineNumber() === null,
         native.getColumnNumber() === null, Object.prototype.toString.call(native),
         typeof CallSite].join('|');
        "#,
            options,
        )
        .unwrap();
    assert_eq!(
        text,
        "Example|Example|example.js|true|build|false|true|true|true|true|true|[object CallSite]|undefined"
    );
}

#[test]
fn forwarded_calls_keep_native_frames_without_rust_reentry() {
    assert_eq!(
        run(r#"
        Error.prepareStackTrace = (_, frames) => frames;
        function capture() { return new Error().stack; }
        let results = [capture.call(null), capture.apply(null, []),
                       Reflect.apply(capture, null, []), Reflect.construct(capture, [])];
        results.map(frames => [frames[0].isConstructor(), frames[1].getFunctionName(),
                               frames[1].isNative(), frames[2].getFunctionName() === null].join(':')).join('|');
        "#),
        "false:call:true:true|false:apply:true:true|false:apply:true:true|true:construct:true:true"
    );
}

#[test]
fn stack_hooks_keep_the_original_error_and_respect_the_limit() {
    assert_eq!(
        run(r#"
        let result = [Error.stackTraceLimit];
        let limit = {valueOf() { throw new Error('conversion'); }};
        Error.stackTraceLimit = limit;
        Error.prepareStackTrace = (_, frames) => frames.length;
        result.push(Error.stackTraceLimit === limit, new Error('original').stack);
        Error.stackTraceLimit = Infinity;
        function depth(n) { return n ? depth(n - 1) : new Error().stack; }
        result.push(depth(70));
        Error.stackTraceLimit = 1.8;
        Error.prepareStackTrace = (error, frames) => {
            result.push(frames.length, new Error().stack === undefined);
            throw new Error('formatting');
        };
        try { noSuchGlobal; } catch (error) {
            result.push(error instanceof ReferenceError, error.stack === null);
        }
        result.join('|');
        "#),
        "10|true|0|64|1|true|true|true"
    );
}

#[test]
fn interrupt_during_stack_preparation_escapes_javascript_handlers() {
    use std::{cell::Cell, rc::Rc};

    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let interrupt = Rc::new(Cell::new(false));
    engine.set_print_callback(Box::new({
        let interrupt = interrupt.clone();
        move |_| interrupt.set(true)
    }));
    engine.set_interrupt_handler(Some(Box::new(move || interrupt.replace(false))));
    let result = engine.eval(
        "Error.prepareStackTrace = () => { print('interrupt'); while (true) {} };
         try { noSuchGlobal; } catch {}",
        EvalOptions::default(),
    );
    assert_eq!(result, Err(EngineError::Interrupted));
    assert!(engine.runtime_mut().pending_exception().is_none());
}

#[test]
fn captured_stack_filters_frames_and_honors_own_stack() {
    assert_eq!(
        run(r#"
        const target = {};
        Error.prepareStackTrace = (_, frames) => frames;
        function frame() { Error.captureStackTrace(target, frame); }
        function parent() { frame(); }
        parent();
        let d = Object.getOwnPropertyDescriptor(target, 'stack');
        Error.prepareStackTrace = e => { e.stack = 'own'; return 'hidden'; };
        const e = new Error();
        delete e.stack;
        [d.value[0].getFunctionName(), d.enumerable, d.configurable, d.writable,
         e.stack === undefined].join('|');
        "#),
        "parent|false|true|true|true"
    );
}

#[test]
fn syntax_and_iterator_frames_keep_the_source_location() {
    assert_eq!(
        run(r#"
        let positions = [];
        Error.prepareStackTrace = (_, frames) => frames;
        function add(site) {
            positions.push(site.getLineNumber() + ':' + site.getColumnNumber());
        }
        for (const source of ['let n = 123bcd', '`xyz\nabc', '`xyz\nabc`; let n = @', "  'ab\\\ncd"]) {
            try { eval(source); } catch (e) {
                if (source.includes('123bcd')) positions.push(e.message);
                positions.push(e.stack[0].getFileName());
                add(e.stack[0]);
            }
        }
        var iterable = {
            [Symbol.iterator]() { return {next: () => ({value: 1, done: false}),
                return() { add(new Error().stack[1]); return {}; }}; }
        };
        eval('for (const x of iterable) {\n  break;\n}\nfor (const [x] of [iterable]) break;');
        Error.prepareStackTrace = undefined;
        try { (0, eval)('   missingName'); }
        catch (e) { positions.push(e.stack.includes('<input>:1:4')); }
        try { (0, eval)('  (missingName)()'); }
        catch (e) { positions.push(e.stack.includes('<input>:1:4')); }
        positions.push(eval('  var sample = new Error("sample"); sample.stack.includes("<input>:1:20")'));
        positions.join('|');
        "#),
        "invalid number literal|<input>|1:9|<input>|1:1|<input>|2:15|<input>|1:3|1:1|4:12|true|true|true"
    );
}

#[test]
fn type_errors_describe_the_key_or_the_declared_function() {
    assert_eq!(
        run(r#"
        let result = [];
        let calls = 0;
        let key = { [Symbol.toPrimitive]() { calls++; } };
        for (const k of ['x', 42, Symbol('local'), Symbol.iterator, key, '\ud800']) {
            try { null[k]; } catch (e) { result.push(e.message); }
        }
        try { undefined.x; } catch (e) { result.push(e.message); }
        function* Declared() {}
        Object.defineProperty(Declared, 'name', { value: 'changed' });
        let inferred = function* () {};
        for (const f of [Declared, inferred]) {
            try { new f(); } catch (e) { result.push(e.message); }
        }
        result[5] = result[5].charCodeAt(22);
        result.push(calls);
        result.join('|');
        "#),
        "cannot read property 'x' of null|cannot read property '42' of null|cannot read property 'local' of null|cannot read property 'Symbol.iterator' of null|cannot read property '{}' of null|55296|cannot read property 'x' of undefined|Declared is not a constructor|not a constructor|0"
    );
}

#[test]
fn executes_compiled_closures_and_finally() {
    assert_eq!(
        run(
            "function f(n) { let y = n; return () => ++y; } let g = f(40); try { g(); } finally { g(); } g()"
        ),
        "43"
    );
}

#[test]
fn handles_number_and_bigint_conversions() {
    assert_eq!(run("+\"0x3a67a9c3787c65c1e5\""), "1.0773808726543752e+21");
    assert_eq!(run("-(2n ** 89n) / 4n"), "-154742504910672534362390528");
    assert_eq!(
        run("9007199254740993n == ' 9007199254740993\\ufeff'"),
        "true"
    );
}

#[test]
fn short_bigint_arithmetic_spills_at_the_signed_payload_limits() {
    assert_eq!(
        run(r#"
        let min = -(2n ** 46n), max = 2n ** 46n - 1n;
        let results = [max + 1n, min - 1n, min * -1n, min / -1n, -min];
        let p = max++, q = min--;
        results.push(p, max, q, min, min % -2n);
        results.join('|');
        "#),
        "70368744177664|-70368744177665|70368744177664|70368744177664|70368744177664|70368744177663|70368744177664|-70368744177664|-70368744177665|-1"
    );
}

#[test]
fn short_bigint_arithmetic_keeps_js_signs_and_range_errors() {
    assert_eq!(
        run(r#"
        let x = -37n, y = 2n, n = -2n;
        let result = [x + y, x - y, x * y, x / y, x % y, -x / n, -x % n,
            x & y, x | y, x ^ y, ~x, x << n, x >> n, x >> 64n, 3n >> -64n,
            (-1n) ** 70368744177663n, y ** 45n, y ** 46n, 0n << 1048576n];
        for (const op of [() => x / 0n, () => x % 0n, () => y ** -1n,
            () => x >>> 1n, () => 1n << 1048576n]) {
            try { op(); } catch (e) { result.push(e.name); }
        }
        result.join('|');
        "#),
        "-35|-39|-74|-18|-1|-18|1|2|-37|-39|36|-10|-148|-1|55340232221128654848|-1|35184372088832|70368744177664|0|RangeError|RangeError|RangeError|TypeError|RangeError"
    );
}

#[test]
fn short_bigint_multiply_divide_remainder_and_power_keep_interrupt_checks() {
    use std::{cell::Cell, rc::Rc};

    for op in ["*", "/", "%", "**"] {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        engine
            .eval("var a = 3n, b = 2n", EvalOptions::default())
            .unwrap();
        let polls = Rc::new(Cell::new(0));
        engine.set_interrupt_handler(Some(Box::new({
            let polls = polls.clone();
            move || {
                polls.set(polls.get() + 1);
                polls.get() == 2
            }
        })));
        assert_eq!(
            engine.eval(
                &format!("try {{ a {op} b }} catch {{}}"),
                EvalOptions::default()
            ),
            Err(EngineError::Interrupted),
            "{op}"
        );
        assert_eq!(polls.get(), 2, "{op}");
    }
}

#[test]
fn primitive_property_reads_keep_the_receiver_and_string_indices() {
    assert_eq!(
        run(r#"
        Object.defineProperty(String.prototype, '0', {value: 'shadowed'});
        Object.defineProperty(String.prototype, '1', {get() {'use strict'; return this;}});
        Object.defineProperty(Number.prototype, 'key', {get() {'use strict'; return this;}});
        let pair = '\uD83D' + '\uDE00';
        ['x'[0], 'x'[1], pair.length, pair[1].charCodeAt(0).toString(16), (7).key === 7].join('|')
        "#),
        "x|x|2|de00|true"
    );
}

#[test]
fn numeric_property_keys_preserve_proxy_key_strings() {
    assert_eq!(
        run(r#"
        let seen = [];
        let key = { [Symbol.toPrimitive](hint) { seen.push(hint); return -0; } };
        let proxy = new Proxy({}, {get(_, key) { return key; }});
        [proxy[key], proxy[3], proxy[1073741823], proxy[1073741824], proxy[-1],
         proxy[1.25], proxy[NaN], seen.join(',')].join('|')
        "#),
        "0|3|1073741823|1073741824|-1|1.25|NaN|string"
    );
}

#[test]
fn number_operations_preserve_signed_zero_and_unordered_comparisons() {
    assert_eq!(
        run(r#"
        let z = -0, n = NaN, inf = Infinity;
        [Object.is(z / 3, -0), Object.is(z * -1, 0), z == 0, n == n,
         n <= 1, 1 >= n, 1 ** inf, 0xffffffff >>> 0, ~4294967295,
         -1.9 >>> 0, 0xfffffffe + 2].join('|')
        "#),
        "true|true|true|false|false|false|NaN|4294967295|0|4294967295|4294967296"
    );
}

#[test]
fn numeric_binary_operators_coerce_the_left_operand_first() {
    assert_eq!(
        run(r#"
        let log = [];
        function left() {
            log.push('left');
            return { valueOf() { log.push('left numeric'); return Symbol(); } };
        }
        function right() {
            log.push('right');
            return { valueOf() { log.push('right numeric'); throw new Error(); } };
        }
        try { left() & right(); } catch (e) { log.push(e.name); }
        log.join(',')
    "#),
        "left,right,left numeric,TypeError"
    );
}

#[test]
fn computed_references_check_the_base_and_reuse_the_key() {
    assert_eq!(
        run(r#"
        let log = [];
        let key = { toString() { log.push('key'); return 'p'; } };
        function property() { log.push('property'); return key; }
        function rhs() { log.push('rhs'); return 4; }
        try { null[property()]; } catch (e) { log.push(e.name); }
        try { null[property()] *= rhs(); } catch (e) { log.push(e.name); }
        let obj = { p: 2 };
        obj[property()] *= rhs();
        log.push(obj[property()]++);
        log.push(obj.p);
        log.join(',')
    "#),
        "property,TypeError,property,TypeError,property,key,rhs,property,key,8,9"
    );
}

#[test]
fn global_data_reads_and_writes_respect_binding_changes() {
    assert_eq!(
        run(r#"
        'use strict';
        let log = [];
        Object.defineProperty(globalThis, 'count', {value: 1, writable: true, configurable: true});
        count = 2;
        log.push(count);
        function shadow() { let count = 3; return eval('count = 4; count'); }
        log.push(shadow(), count);
        Object.defineProperty(globalThis, 'count', {
          get() { log.push('get'); return 5; },
          set(value) { log.push('set', value); }
        });
        count = 6;
        log.push(count);
        Object.defineProperty(globalThis, 'count', {value: 7, writable: false});
        try { count = 8; } catch (error) { log.push(error.name); }
        log.push(count);
        delete globalThis.count;
        try { count; } catch (error) { log.push(error.name); }
        Object.setPrototypeOf(globalThis, new Proxy(Object.getPrototypeOf(globalThis), {
            has(target, key) { return key === 'count' || Reflect.has(target, key); },
            get(target, key, receiver) {
                return key === 'count' ? receiver === globalThis : Reflect.get(target, key, receiver);
            }
        }));
        log.push(count);
        log.join('|')
        "#),
        "2|4|2|set|6|get|5|TypeError|7|ReferenceError|true"
    );
}

#[test]
fn strict_assignment_rechecks_the_resolved_object_binding() {
    assert_eq!(
        run(r#"
        Object.defineProperty(globalThis, 'x', { value: 1, configurable: true });
        Object.defineProperty(globalThis, 'y', {
            configurable: true,
            get() { delete globalThis.y; return 1; }
        });
        (function () {
            'use strict';
            let log = [];
            try { x = (delete globalThis.x, 2); } catch (e) { log.push(e.name); }
            try { ++y; } catch (e) { log.push(e.name); }
            log.push('x' in globalThis, 'y' in globalThis);
            return log.join(',');
        })()
    "#),
        "ReferenceError,ReferenceError,false,false"
    );
}

#[test]
fn applies_and_constructs_proxies() {
    assert_eq!(
        run(
            "let f = new Proxy(() => 1, { apply(t,r,a) { return r.x + a[0] } }); Reflect.apply(f, {x: 19}, [23])"
        ),
        "42"
    );
    assert_eq!(
        run(
            "let f = new Proxy(function(){}, { construct(t,a,n) { return { answer: a[0] } } }); new f(42).answer"
        ),
        "42"
    );
    assert_eq!(
        run(
            "let count = 0; let target = new Proxy(function(){}, {get(t,k,r){ if (k === 'prototype') count++; return Reflect.get(t,k,r); }}); Reflect.construct(new Proxy(Map, {}), [], target); count"
        ),
        "1"
    );
}

fn run_with_other_realm(script: &str) -> String {
    use crate::Value;
    use crate::builtins::define_data;
    use crate::shape::PropFlags;

    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let realm = engine.default_realm();
    engine
        .runtime_mut()
        .with_scope(realm, |ctx| {
            let other = ctx.create_realm_with_builtins()?;
            let other = ctx.root(Value::object(ctx.heap().realm(other).global_object))?;
            let global = ctx.root(Value::object(ctx.realm().global_object))?;
            let key = ctx.intern_str("other")?;
            define_data(ctx, global, key, other, PropFlags::DEFAULT_DATA)?;
            Ok::<_, crate::Abrupt>(())
        })
        .unwrap();
    engine
        .eval_to_string(script, EvalOptions::default())
        .unwrap()
}

#[test]
fn shares_a_frozen_throw_type_error_within_each_realm() {
    assert_eq!(
        run_with_other_realm(
            r#"
                const caller = Object.getOwnPropertyDescriptor(Function.prototype, 'caller');
                const args = Object.getOwnPropertyDescriptor(Function.prototype, 'arguments');
                const thrower = caller.get;
                const otherThrower = Object.getOwnPropertyDescriptor(other.Function.prototype, 'caller').get;
                function strict() { 'use strict'; return arguments; }
                function nonSimple(x = 0) { return arguments; }
                function sloppy() {}
                const callee = o => Object.getOwnPropertyDescriptor(o, 'callee');
                delete Function.prototype.caller;
                delete Function.prototype.arguments;
                [caller.set === thrower, args.get === thrower, args.set === thrower,
                 callee(strict()).get === thrower, callee(strict()).set === thrower,
                 callee(nonSimple()).get === thrower, thrower !== otherThrower,
                 Object.getOwnPropertyNames(thrower).join(','), Object.isFrozen(thrower),
                 thrower.call(sloppy) === undefined].join(';');
                "#
        ),
        "true;true;true;true;true;true;true;length,name;true;true"
    );
}

#[test]
fn call_and_construct_choose_the_error_realm() {
    assert_eq!(
        run_with_other_realm(
            r#"
            function throws(ctor, run) {
                try { run(); } catch (e) { return e.constructor === ctor; }
                return false;
            }
            const base = other.eval('(class {})');
            const derived = other.eval('(class extends Object { constructor() { return null; } })');
            const uninitialized = other.eval('(class extends Object { constructor() {} })');
            const tdz = other.eval('(function () { v = 1; let v; })');
            function revokeDuringPrototypeLookup(ctor) {
                const handle = Proxy.revocable(ctor, { get() { handle.revoke(); } });
                return throws(TypeError, () => new handle.proxy());
            }
            [throws(other.TypeError, () => base()), throws(TypeError, () => new derived()),
             throws(ReferenceError, () => new uninitialized()), throws(ReferenceError, () => tdz()),
             throws(other.TypeError, () => other.Function.prototype.apply.call(() => {}, null, 1)),
             throws(other.TypeError, () => other.Function.prototype.call.call(0)),
             throws(other.TypeError, () => other.Reflect.apply(() => {}, null, 1)),
             revokeDuringPrototypeLookup(function () {}), revokeDuringPrototypeLookup(Function)]
                 .join(',');
        "#
        ),
        "true,true,true,true,true,true,true,true,true"
    );
}

#[test]
fn array_length_coerces_before_rejecting_descriptor_flags() {
    assert_eq!(
        run(r#"
            let calls = 0;
            const array = [];
            Object.defineProperty(array, 'length', { writable: false });
            const value = { valueOf() { calls++; return -1; } };
            let rangeError = false;
            try {
                Reflect.defineProperty(array, 'length', { value, configurable: true });
            } catch (e) {
                rangeError = e instanceof RangeError;
            }
            [calls, rangeError, array.length, Array.of.length].join(',');
        "#),
        "2,true,0,0"
    );
}

#[test]
fn small_array_and_typed_array_sorts_compare_identical_values() {
    assert_eq!(
        run(r#"
            const o = {};
            function calls(list, method) {
                let n = 0;
                list[method]((a, b) => { if (a === b) n++; return 0; });
                return n;
            }
            [calls([o, o, o], 'sort'), calls([o, o, o], 'toSorted'),
             calls(new Int8Array([1, 1, 1]), 'sort'),
             calls(new BigInt64Array([1n, 1n, 1n]), 'toSorted')].join(',');
        "#),
        "2,2,2,2"
    );
}

#[test]
fn interrupts_backward_branches_without_running_finally() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let mut remaining = 20;
    engine.set_interrupt_handler(Some(Box::new(move || {
        remaining -= 1;
        remaining <= 0
    })));
    assert_eq!(
        engine.eval(
            "var ran = false; try { for (;;) {} } finally { ran = true; }",
            EvalOptions::default()
        ),
        Err(EngineError::Interrupted)
    );
    engine.set_interrupt_handler(None);
    assert_eq!(
        engine
            .eval_to_string("ran", EvalOptions::default())
            .unwrap(),
        "false"
    );
}

#[test]
fn dispatches_property_proxy_traps_and_preserves_invariants() {
    assert_eq!(
        run(r#"
        let seen = [];
        let t = {};
        let p = new Proxy(t, {
            get(t,k,r) { seen.push('get'); return Reflect.get(t,k,r) },
            set(t,k,v,r) { seen.push('set'); return Reflect.set(t,k,v,r) },
            has(t,k) { seen.push('has'); return Reflect.has(t,k) },
            deleteProperty(t,k) { seen.push('delete'); return Reflect.deleteProperty(t,k) },
            getOwnPropertyDescriptor(t,k) { seen.push('descriptor'); return Reflect.getOwnPropertyDescriptor(t,k) },
            defineProperty(t,k,d) { seen.push('define'); return Reflect.defineProperty(t,k,d) },
            ownKeys(t) { seen.push('keys'); return ['z', 'x']; },
            getPrototypeOf(t) { seen.push('getProto'); return Reflect.getPrototypeOf(t) },
            setPrototypeOf(t,v) { seen.push('setProto'); return Reflect.setPrototypeOf(t,v) },
            isExtensible(t) { seen.push('extensible'); return Reflect.isExtensible(t) },
            preventExtensions(t) { seen.push('prevent'); return Reflect.preventExtensions(t) },
        });
        p.x = 1; p.x; 'x' in p; Reflect.ownKeys(p); delete p.x;
        Reflect.getPrototypeOf(p); Reflect.setPrototypeOf(p,null);
        Reflect.isExtensible(p); Reflect.preventExtensions(p);
        seen.join(',')
    "#),
        "set,descriptor,define,get,has,keys,delete,getProto,setProto,extensible,prevent"
    );
    assert_eq!(
        run(r#"
        const frozen = Object.freeze({x: 1});
        const attempts = [
            () => new Proxy(frozen, {get(){return 2}}).x,
            () => Reflect.set(new Proxy(frozen, {set(){return true}}), 'x', 2),
            () => Reflect.has(new Proxy(frozen, {has(){return false}}), 'x'),
            () => Reflect.deleteProperty(new Proxy(frozen, {deleteProperty(){return true}}), 'x'),
            () => Reflect.ownKeys(new Proxy(frozen, {ownKeys(){return []}})),
            () => Reflect.ownKeys(new Proxy({}, {ownKeys(){return ['x','x']}})),
            () => Reflect.defineProperty(new Proxy(frozen, {defineProperty(){return true}}), 'x', {value:2}),
            () => Reflect.getOwnPropertyDescriptor(new Proxy(frozen, {getOwnPropertyDescriptor(){}}), 'x'),
            () => Reflect.isExtensible(new Proxy(frozen, {isExtensible(){return true}})),
            () => Reflect.preventExtensions(new Proxy({}, {preventExtensions(){return true}})),
            () => Reflect.getPrototypeOf(new Proxy(frozen, {getPrototypeOf(){return null}})),
            () => Reflect.setPrototypeOf(new Proxy(frozen, {setPrototypeOf(){return true}}), null),
        ];
        attempts.map(f => { try { f(); return 'no'; } catch(e) { return e instanceof TypeError; } }).every(v => v === true)
    "#),
        "true"
    );
}

#[test]
fn string_typed_array_and_arguments_exotic_properties() {
    assert_eq!(
        run(r#"
        const s = new String('áb');
        const d = Object.getOwnPropertyDescriptor(s, '0');
        [s[0], s.length, Reflect.ownKeys(s).join(','), d.writable, Reflect.deleteProperty(s,'0')].join('|')
    "#),
        "á|2|0,1,length|false|false"
    );
    assert_eq!(
        run(r#"
        const a = new Uint8Array([1,2]);
        a[0] = 9;
        Object.setPrototypeOf(a, {NaN: 18});
        [a[0], a[1], a.NaN, Reflect.ownKeys(a), Reflect.deleteProperty(a,'0')].join('|')
    "#),
        "9|2||0,1|false"
    );
    assert_eq!(
        run(r#"
        function f(a) {
            a = 4; let x = arguments[0];
            arguments[0] = 7; let y = a;
            Object.defineProperty(arguments, '0', {writable:false});
            a = 12; let z = arguments[0];
            return [x,y,z].join(',');
        }
        f(2)
    "#),
        "4,7,7"
    );
}

#[test]
fn generator_captures_live_locals_across_suspension() {
    assert_eq!(
        run(r#"
        function* g(a) {
            let x = 2;
            yield () => ++x;
            try { yield arguments[0] + x; }
            finally { yield x; }
            return 42;
        }
        const i = g(10);
        const f = i.next().value;
        const first = f();
        const second = i.next().value;
        const third = i.return(99).value;
        const last = i.next();
        [first, second, third, last.value, last.done, f()].join(',')
    "#),
        "3,13,3,99,true,4"
    );
}

#[test]
fn initial_yield_requires_a_generator_frame() {
    let image = ProgramImage {
        functions: vec![FunctionBytecode {
            code: vec![Opcode::InitialYield as u8, Opcode::ReturnUndef as u8],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    assert!(matches!(
        engine.execute_image(&image, None),
        Err(EngineError::Thrown { message, .. }) if message.contains("function cannot suspend")
    ));
}

#[test]
fn generators_read_the_prototype_after_parameter_initialization() {
    assert_eq!(
        run(r#"
        let log = [];
        function* g(a = (log.push('parameter'), g.prototype = null)) {
            log.push('body');
        }
        async function* ag(a = (log.push('async parameter'), ag.prototype = null)) {}
        let proto = g.prototype;
        let asyncProto = ag.prototype;
        let it = g();
        let asyncIt = ag();
        log.push(Object.getPrototypeOf(it) === proto);
        log.push(Object.getPrototypeOf(asyncIt) === asyncProto);
        it.next();
        log.join(',')
    "#),
        "parameter,async parameter,false,false,body"
    );
}

#[test]
fn async_generator_bare_return_does_not_await() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let opts = EvalOptions::default();
    engine
        .eval(
            r#"
        var log = [];
        async function* fallthrough() {}
        async function* bareReturn() { return; }
        async function* valueReturn() { return undefined; }
        Promise.resolve().then(() => log.push('tick 1')).then(() => log.push('tick 2'));
        fallthrough().next().then(() => log.push('fallthrough'));
        bareReturn().next().then(() => log.push('bare'));
        valueReturn().next().then(() => log.push('value'));
    "#,
            opts.clone(),
        )
        .unwrap();
    engine.run_jobs().unwrap();
    assert_eq!(
        engine.eval_to_string("log.join(',')", opts).unwrap(),
        "tick 1,fallthrough,bare,tick 2,value"
    );
}

#[test]
fn generator_delegation_and_throw_reenter_the_suspended_frame() {
    assert_eq!(
        run(r#"
        function* g() {
           try { yield* [1,2]; yield 3; }
           catch(x) { yield x + 1; }
        }
        const i = g();
        [i.next().value, i.next().value, i.next().value, i.throw(9).value, i.next().done].join(',')
    "#),
        "1,2,3,10,true"
    );
}

#[test]
fn promise_jobs_resume_async_functions_and_generators() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let opts = EvalOptions::default();
    engine
        .eval(
            r#"
        var log = [];
        (async () => {
            log.push('start');
            try { await Promise.reject(1); }
            catch(e) { log.push(e); }
            log.push(await 2);
            return 3;
        })().then(v => log.push(v));
        log.push('sync');
    "#,
            opts.clone(),
        )
        .unwrap();
    assert_eq!(
        engine
            .eval_to_string("log.join(',')", opts.clone())
            .unwrap(),
        "start,sync"
    );
    engine.run_jobs().unwrap();
    assert_eq!(
        engine
            .eval_to_string("log.join(',')", opts.clone())
            .unwrap(),
        "start,sync,1,2,3"
    );
    engine
        .eval(
            r#"
        log = [];
        async function* g() { yield Promise.resolve(1); yield 2; }
        let i = g();
        i.next().then(r => log.push(r.value));
        i.next().then(r => log.push(r.value));
        i.return(Promise.resolve(3)).then(r => log.push(r.value, r.done));
    "#,
            opts.clone(),
        )
        .unwrap();
    engine.run_jobs().unwrap();
    assert_eq!(
        engine.eval_to_string("log.join(',')", opts).unwrap(),
        "1,2,3,true"
    );
}

#[test]
fn module_graph_links_live_bindings_and_waits_for_cycle_roots() {
    use crate::api::{ModuleLoader, ResolvedModule};
    struct Loader;
    impl ModuleLoader for Loader {
        fn resolve(
            &mut self,
            _referrer: &str,
            specifier: &str,
            module_type: Option<&str>,
        ) -> Result<ResolvedModule, String> {
            let source = match (specifier, module_type) {
                ("a", None) => {
                    "import 'b'; export function f() { return 7 } globalThis.events.push('a'); await 1; globalThis.events.push('a end')"
                }
                ("b", None) => {
                    "import {f} from 'a'; globalThis.events.push(f()); await 1; globalThis.events.push('b')"
                }
                ("c", None) => "import 'b'; globalThis.events.push('c')",
                ("data" | "./data", None | Some("json")) => {
                    return Ok(ResolvedModule::Json {
                        name: "data".into(),
                        source: "{\"n\":42}".into(),
                    });
                }
                ("text", Some("text")) => {
                    return Ok(ResolvedModule::Text {
                        name: "text".into(),
                        text: "text".into(),
                    });
                }
                ("bytes", Some("bytes")) => {
                    return Ok(ResolvedModule::Bytes {
                        name: "bytes".into(),
                        bytes: vec![255],
                    });
                }
                _ => return Err(format!("unknown module {specifier}")),
            };
            Ok(ResolvedModule::Source {
                name: specifier.into(),
                source: source.into(),
            })
        }
    }
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let script = EvalOptions::default();
    engine.eval("var events = []", script.clone()).unwrap();
    engine.set_module_loader(Box::new(Loader));
    let module = EvalOptions {
        filename: "main.js".into(),
        module: true,
        ..Default::default()
    };
    engine
        .eval(
            r#"
        import 'a'; import 'c';
        import data from 'data' with { type: 'json' };
        import plain from 'data';
        import text from 'text' with { type: 'text' };
        if (data === plain) throw new Error('different attributes shared one module');
        if ((await import('./data', { with: { type: 'json' } })).default !== data) {
            throw new Error('equal attributes loaded a second module');
        }
        if ((await import('./data', { with: {} })).default !== plain) {
            throw new Error('empty attributes loaded a second module');
        }
        import bytes from 'bytes' with { type: 'bytes' };
        events.push(data.n, text, bytes[0]);
        (await import('a')).f && events.push(import.meta.url);
    "#,
            module,
        )
        .unwrap();
    engine.run_jobs().unwrap();
    assert_eq!(
        engine.eval_to_string("events.join(',')", script).unwrap(),
        "7,b,a,a end,c,42,text,255,main.js"
    );
}

#[test]
fn direct_eval_shares_lexical_bindings_and_sloppy_variables() {
    assert_eq!(
        run(r#"
        var marker = 'global';
        function f(x) {
            let y = 1;
            eval('x += 2; y += 1; var z = 6; function p(t) { return x + y + t }');
            const read = eval('() => y');
            y++;
            return [p(z), z, eval(...['x']), (0, eval)('marker'), read()].join(',');
        }
        f(7)
    "#),
        "18,6,9,global,3"
    );
    assert_eq!(
        run(r#"
        let outer = 42;
        function f() {
            'use strict';
            eval('var temp = outer');
            return [eval('outer'), typeof temp].join(',');
        }
        f()
    "#),
        "42,undefined"
    );
}

#[test]
fn direct_eval_distinguishes_parameter_and_body_environments() {
    assert_eq!(
        run(r#"
        function body(p = 3) { var p = 4; return eval('p'); }
        function strictBody() {
            'use strict';
            return (function(p = 5) { var local = p + 1; return eval('local'); })();
        }
        function introduced(p = 3, read = () => p) {
            return [eval('var p = 4; p'), p, read()].join(':');
        }
        function parameters(p = eval('var x = 7; x'), read = (a = eval('x')) => a) {
            var x = 8;
            return [p, eval('x'), read()].join(':');
        }
        let outer = 'outside';
        function afterDefault(p = outer, read = () => outer) {
            eval('var outer = 9');
            return [outer, p, read()].join(':');
        }
        function readDefault(value = eval('typeof onlyInBody')) { var onlyInBody; return value; }
        function destructured([value]) { eval('var value = 2'); return value; }
        const named = function self(self) { eval('var self = 6'); return [self, arguments[0]].join(':'); };
        [body(), strictBody(), introduced(), parameters(), afterDefault(), readDefault(),
         destructured([1]), named(5)].join(',')
        "#),
        "4,6,4:4:3,7:8:7,9:outside:outside,undefined,2,6:6"
    );
}

#[test]
fn direct_eval_checks_redeclarations_only_through_its_variable_environment() {
    assert_eq!(
        run(r#"
        function attempt(callback) {
            try { return callback(); }
            catch (error) { return error.name; }
        }
        function outer() {
            let x = 1;
            const y = 3;
            return (() => { eval('var x = 2, y = 4'); return [x, y].join(':'); })();
        }
        function arrowInBlock() {
            { const value = 1; return (() => { eval('var value = 2'); return value; })(); }
        }
        function localLet() { { let x = 1; return eval('var x = 2'); } }
        function localConst() { const x = 1; return eval('var x = 2'); }
        function nestedLocal() { return eval('let x = 1; eval("var x = 2")'); }
        function nestedVar() { return eval('eval("var x = 2"); x'); }
        function nestedFunction() {
            return eval('let x = 1; (() => { eval("var x = 2"); return x; })()');
        }
        function strictEval() {
            let x = 1;
            return [eval('"use strict"; var x = 2; x'), x, typeof local,
                    eval('"use strict"; var local = 3; eval("local")')].join(':');
        }
        let globalBlock;
        { const x = 1; try { eval('var x = 2'); } catch (error) { globalBlock = error.name; } }
        [outer(), arrowInBlock(), attempt(localLet), attempt(localConst), attempt(nestedLocal),
         nestedVar(), nestedFunction(), strictEval(), globalBlock].join(',')
        "#),
        "2:4,2,SyntaxError,SyntaxError,SyntaxError,2,2,2:1:undefined:3,SyntaxError"
    );
}

#[test]
fn direct_eval_respects_catch_and_with_scopes() {
    assert_eq!(
        run(r#"
        function inCatch() {
            try { throw 1; }
            catch (x) {
                eval('var x = 2');
                eval('function x() { return 3; }');
                return [x, eval('x')].join(':');
            }
        }
        function inDestructuredCatch() {
            try { throw [1]; }
            catch ([x]) {
                try { eval('var x = 2'); }
                catch (error) { return error.name; }
            }
        }
        function withObject() {
            var x = 1;
            const object = { x: 2, y: 3, [Symbol.unscopables]: { y: true } };
            const y = 4;
            let read;
            with (object) {
                eval('var x = 5');
                read = (() => [eval('x'), eval('y')].join(':'))();
            }
            return [x, object.x, read].join(':');
        }
        [inCatch(), inDestructuredCatch(), withObject()].join(',')
        "#),
        "2:2,SyntaxError,1:5:5:4"
    );
}

#[test]
fn direct_eval_sees_private_names_in_fields_methods_and_nested_eval() {
    assert_eq!(
        run(r#"
        class Outer {
            #value = 1;
            #outer = this.#value;
            read() {
                const outer = this;
                class Inner {
                    #state = eval('outer.#outer');
                    get #value() { return this.#state; }
                    set #value(value) { this.#state = value; }
                    #add(n) { return this.#value + n; }
                    read() {
                        eval('this.#value = 4');
                        return eval('eval("this.#add(3)")');
                    }
                }
                return new Inner().read();
            }
        }
        new Outer().read()
        "#),
        "7"
    );
    assert_eq!(
        run(r#"
        class Example {
            #value = 1;
            read() {
                try { return (0, eval)('this.#value'); }
                catch (error) { return error.name; }
            }
        }
        new Example().read()
        "#),
        "SyntaxError"
    );
}

#[test]
fn private_fields_on_proxies_do_not_query_the_target() {
    assert_eq!(
        run(r#"
        class Base { constructor(object) { return object; } }
        class Child extends Base {
            #field = 7;
            #method() { return this.#field; }
            static read(object) { return object.#method(); }
        }
        const target = Object.preventExtensions({});
        const proxy = new Proxy(target, {
            isExtensible() { throw new Error('isExtensible'); },
            defineProperty() { throw new Error('defineProperty'); },
        });
        let failure;
        try { new Child(target); } catch (error) { failure = error.name; }
        new Child(proxy);
        [failure, Child.read(proxy)].join(',')
        "#),
        "TypeError,7"
    );
}

#[test]
fn direct_eval_respects_parameter_and_binding_lifetimes() {
    assert_eq!(
        run(r#"
        function invalid(p = eval('var arguments')) {}
        let failure;
        try { invalid(); } catch (error) { failure = error.name; }
        function readArgument(p = eval('arguments[1]')) { return p; }
        const keepName = function self() { return eval('self = null; self') === keepName; };
        const removeLocals = () => eval(`
            var x;
            function f() {}
            [delete x, delete f, typeof x, typeof f].join(',');
        `);
        [failure, readArgument(undefined, 42), keepName(), removeLocals()].join(',')
        "#),
        "SyntaxError,42,true,true,true,undefined,undefined"
    );
}

#[test]
fn eval_accepts_initial_hashbang_comments() {
    assert_eq!(
        run(r#"
        let failure;
        try { Function('#!example\nreturn 9')(); } catch (error) { failure = error.name; }
        [eval('#!example\n7'), (0, eval)('#!example\n8'), eval('#!example'), failure].join(',')
        "#),
        "7,8,,SyntaxError"
    );
}

#[test]
fn constructs_functions_and_captures_error_stacks() {
    assert_eq!(
        run(r#"
        const ctor = (function*(){}).constructor;
        const gen = ctor('x', 'yield x + 1');
        [ctor.name, gen(41).next().value, Function('x','return ++x')(41), (async () => {}).constructor.name].join(',')
    "#),
        "GeneratorFunction,42,42,AsyncFunction"
    );
    assert_eq!(
        run(r#"
        function f() { return new Error('message').stack; }
        function g() { try { btoa('⚡'); } catch (e) { return e.stack; } }
        [f().includes('at f ('), g().includes('at btoa (native)')].join(',')
    "#),
        "true,true"
    );
}

#[test]
fn classes_use_super_and_private_brands() {
    assert_eq!(
        run(r#"
        class Parent { constructor(n) {this.n = n} get value() { return this.n } }
        class Child extends Parent {
          #x = 2;
          constructor(n) { super(n); }
          get value() { return super.value + this.#x; }
          static has(o) { return #x in o; }
        }
        const obj = new Child(40);
        [obj.value, Child.has(obj), Child.has(new Parent(1)), Reflect.ownKeys(obj)].join(',')
    "#),
        "42,true,false,n"
    );
}

#[test]
fn rethrow_keeps_error_creation_frames() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    engine
        .eval(
            "function create() { return new Error('origin'); }\nvar saved = create();",
            EvalOptions {
                filename: "user.js".into(),
                ..Default::default()
            },
        )
        .unwrap();
    engine
        .eval(
            "function rethrow() { throw saved; }",
            EvalOptions {
                filename: "adapter.js".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let Err(EngineError::Thrown { message, frames }) = engine.call_global("rethrow", "") else {
        panic!("expected a throw");
    };
    assert_eq!(message, "Error: origin");
    assert!(
        frames
            .iter()
            .any(|frame| frame.file == "user.js" && frame.function == "create" && frame.line == 1)
    );
    assert!(!frames.iter().any(|frame| frame.file == "adapter.js"));
    let Err(EngineError::Thrown { message, .. }) =
        engine.eval("throw { n: 1 }", EvalOptions::default())
    else {
        panic!("expected a throw");
    };
    assert_eq!(message, "{\"n\":1}");
}

#[test]
fn trampoline_calls_do_not_consume_native_reentry_budget() {
    assert_eq!(
        run("function f(n) { return n ? Reflect.apply(f, null, [n-1]) : 42; } f(500)"),
        "42"
    );
    assert_eq!(
        run("function f(n) { return n ? f.apply(null,[n-1]) : 42; } f(500)"),
        "42"
    );
    assert_eq!(
        run("function f(n) { return n ? f.call(null,n-1) : 42; } f(500)"),
        "42"
    );
    let mut engine = JsEngine::new(EngineLimits {
        stack_bytes: 64 * 1024,
        ..Default::default()
    })
    .unwrap();
    let Err(EngineError::Thrown { message, frames }) = engine.eval(
        "function f() { return f.call(null); } f()",
        EvalOptions::default(),
    ) else {
        panic!("expected stack overflow");
    };
    assert_eq!(message, "RangeError: Maximum call stack size exceeded");
    assert_eq!(frames[0].function, "f");
}

#[test]
fn bytecode_oom_is_catchable_but_host_classifies_uncaught_oom() {
    let mut engine = JsEngine::new(EngineLimits {
        heap_bytes: 8 * 1024 * 1024,
        ..Default::default()
    })
    .unwrap();
    let options = EvalOptions::default();
    assert_eq!(engine.eval_to_string("var text; try { new Array(1000000000).fill(1); } catch (e) { text = e.message; } text", options.clone()).unwrap(), "out of memory");
    assert_eq!(
        engine.eval("new Array(1000000000).fill(1);", options),
        Err(EngineError::OutOfMemory)
    );
}

#[test]
fn interrupt_during_async_resume_does_not_run_handlers() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let options = EvalOptions::default();
    engine.eval("var handled = false; (async () => { try { await 1; for (;;) {} } finally { handled = true; } })().catch(() => { handled = true; });", options.clone()).unwrap();
    let mut remaining = 30;
    engine.set_interrupt_handler(Some(Box::new(move || {
        remaining -= 1;
        remaining <= 0
    })));
    assert_eq!(engine.run_jobs(), Err(EngineError::Interrupted));
    engine.set_interrupt_handler(None);
    engine.run_jobs().unwrap();
    assert_eq!(engine.eval_to_string("handled", options).unwrap(), "false");
}

#[test]
fn regexp_literals_share_code_but_not_last_index() {
    assert_eq!(
        run(
            "function f() { return /a/g; } const a = f(), b = f(); a.test('a'); [a === b, a.lastIndex, b.lastIndex].join(',')"
        ),
        "false,1,0"
    );
    let code = [
        Instruction {
            opcode: Opcode::PushConst8,
            operands: Operands::Constant(0),
        },
        Instruction {
            opcode: Opcode::PushConst8,
            operands: Operands::Constant(1),
        },
        Instruction {
            opcode: Opcode::Regexp,
            operands: Operands::None,
        },
        Instruction {
            opcode: Opcode::Return,
            operands: Operands::None,
        },
    ]
    .into_iter()
    .flat_map(|op| op.encode().unwrap().as_bytes().to_vec())
    .collect();
    let compiled = qjs_regexp::compile(
        b"a",
        qjs_regexp::RegExpFlags(qjs_regexp::RegExpFlags::IGNORECASE),
        &mut qjs_regexp::UnlimitedRegExpHost,
    )
    .unwrap();
    let text = |bytes| qjs_bytecode::ConstEntry::String(qjs_bytecode::ConstString::Latin1(bytes));
    let image = ProgramImage {
        functions: vec![FunctionBytecode {
            code,
            stack_size: 2,
            constants: vec![text(vec![b'a']), text(compiled.bytecode)],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let realm = engine.default_realm();
    engine.runtime_mut().with_scope(realm, |ctx| {
        let object = super::execute_image(ctx, &image, None)
            .unwrap()
            .as_object()
            .unwrap();
        let crate::object::ObjectData::RegExp(data) = &ctx.heap().object(object).data else {
            panic!("expected a RegExp");
        };
        assert!(
            data.program
                .compiled
                .flags
                .contains(qjs_regexp::RegExpFlags::IGNORECASE)
        );
    });
}

#[test]
fn host_bridge_and_print_are_opt_in() {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    let opts = EvalOptions::default();
    assert_eq!(
        engine
            .eval_to_string("typeof print + ':' + typeof __host", opts.clone())
            .unwrap(),
        "undefined:undefined"
    );
    engine
        .install_host_bridge(Box::new(|text| format!("echo {text}")))
        .unwrap();
    let lines = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let copy = lines.clone();
    engine.set_print_callback(Box::new(move |s| copy.borrow_mut().push(s.to_owned())));
    engine.eval("print(__host('text'), 42)", opts).unwrap();
    assert_eq!(&*lines.borrow(), &["echo text 42"]);
}
