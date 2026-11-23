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
