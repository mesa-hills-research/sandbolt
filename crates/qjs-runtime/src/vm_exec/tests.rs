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
