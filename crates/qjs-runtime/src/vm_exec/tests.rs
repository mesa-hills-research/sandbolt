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
                "#),
        "true;true;true;true;true;true;true;length,name;true;true"
    );
}

#[test]
fn call_and_construct_choose_the_error_realm() {
    assert_eq!(
        run_with_other_realm(r#"
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
        "#),
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
                ("data", Some("json")) => {
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
        import text from 'text' with { type: 'text' };
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
