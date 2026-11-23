use qjs_bytecode::{
    ConstEntry, FunctionKind, Instruction, Opcode, Operands, ProgramImage, verify_image,
};
use qjs_compiler::{CompileError, compile_source};
use qjs_syntax::{ParseGoal, ParseOptions, SourceText};

fn script(text: &str) -> ProgramImage {
    let source = SourceText::from_utf8("example.js", text);
    let image = compile_source(&source, ParseOptions::default())
        .unwrap_or_else(|error| panic!("{error}\n{text}"));
    verify_image(&image).unwrap();
    image
}

fn instructions(image: &ProgramImage, function: usize) -> Vec<Instruction> {
    let mut pc = 0;
    let code = &image.functions[function].code;
    let mut instructions = Vec::new();
    while pc < code.len() {
        let insn = Instruction::decode(code, pc).unwrap();
        instructions.push(insn);
        pc += usize::from(insn.opcode.info().size);
    }
    instructions
}

fn has_op(insns: &[Instruction], expected: Opcode) -> bool {
    insns.iter().any(|insn| insn.opcode == expected)
}

#[test]
fn computed_reference_order() {
    for (js, before_value, after_value) in [
        ("obj[key()] = rhs()", Opcode::ToPropkey, Opcode::PutArrayEl),
        ("obj[key()] += rhs()", Opcode::ToPropkey, Opcode::PutArrayEl),
        (
            "({ [sourceKey()]: obj[targetKey()] = rhs() } = source)",
            Opcode::GetArrayEl,
            Opcode::PutArrayEl,
        ),
        (
            "[obj[targetKey()] = rhs()] = source",
            Opcode::ForOfNext,
            Opcode::PutArrayEl,
        ),
        (
            "({ [key()]: rhs() })",
            Opcode::ToPropkey,
            Opcode::DefineArrayEl,
        ),
    ] {
        let image = script(js);
        let insns = instructions(&image, 0);
        let rhs = insns
            .iter()
            .enumerate()
            .filter(|(_, i)| i.opcode == Opcode::CallMethod)
            .map(|(idx, _)| idx)
            .next_back()
            .unwrap();
        assert!(has_op(&insns[..rhs], before_value), "{js}");
        assert!(has_op(&insns[rhs + 1..], after_value), "{js}");
        if js.contains("targetKey") {
            assert!(has_op(&insns[rhs + 1..], Opcode::ToPropkey), "{js}");
        }
    }
}

#[test]
fn sloppy_call_target_throws_before_rhs() {
    let image = script("left() = rhs();");
    let insns = instructions(&image, 0);
    let error = insns
        .iter()
        .position(|i| {
            matches!(
                i,
                Instruction {
                    opcode: Opcode::ThrowError,
                    operands: Operands::AtomU8(_, 2)
                }
            )
        })
        .unwrap();
    assert_eq!(
        insns[..error]
            .iter()
            .filter(|i| i.opcode == Opcode::CallMethod)
            .count(),
        1
    );
    assert_eq!(
        insns[error + 1..]
            .iter()
            .filter(|i| i.opcode == Opcode::CallMethod)
            .count(),
        1
    );
}

#[test]
fn closures_eval_with_and_per_iteration_cells() {
    let image = script(
        "function outer(a) { let x; { const y = 1; return () => () => [a, x, y, arguments, eval('x')]; } } for (let i = 0; i < 3; i++) { (() => i); } with (obj) { p += val; p(); }",
    );
    assert!(image.functions.iter().any(|f| f.closure_vars.len() > 2));
    let ops = instructions(&image, 0);
    assert!(has_op(&ops, Opcode::CloseLoc));
    assert!(has_op(&ops, Opcode::WithMakeRef));
    assert!(has_op(&ops, Opcode::WithGetRef));
    assert!(
        image
            .functions
            .iter()
            .enumerate()
            .any(|(i, _)| has_op(&instructions(&image, i), Opcode::Eval))
    );
}

#[test]
fn eval_created_variables_participate_in_outer_name_lookup() {
    let image = script(
        "function outer() { let x = 1; return function inner() { eval('var x = 2'); return x; }; }",
    );
    let inner = instructions(&image, 2);
    assert!(inner.iter().any(|i| matches!(
        i,
        Instruction {
            opcode: Opcode::SpecialObject,
            operands: Operands::Unsigned(5)
        }
    )));
    assert!(inner.iter().any(|i| matches!(
        i,
        Instruction {
            opcode: Opcode::WithGetVar,
            operands: Operands::AtomJumpU8(_, _, 0)
        }
    )));
    assert!(has_op(&inner, Opcode::GetVarRefCheck));
}

#[test]
fn with_assignment_rechecks_the_chosen_environment() {
    let image = script("with (proxy) { p += f(); }");
    let entry = instructions(&image, 0);
    let call = entry
        .iter()
        .position(|i| i.opcode == Opcode::CallMethod)
        .unwrap();
    assert!(has_op(&entry[..call], Opcode::WithMakeRef));
    assert!(has_op(&entry[..call], Opcode::In));
    assert!(has_op(&entry[call + 1..], Opcode::In));
    assert!(has_op(&entry[call + 1..], Opcode::PutRefValue));
}

#[test]
fn references_and_optional_calls_keep_receivers() {
    let image = script(
        "a?.b(); a.b?.(); (a?.b)(); delete a?.[key()]; x &&= f(); y ||= z; x ??= g(); x ?? y;",
    );
    let ops = instructions(&image, 0);
    assert!(
        ops.iter()
            .filter(|i| i.opcode == Opcode::CallMethod)
            .count()
            >= 3
    );
    assert!(has_op(&ops, Opcode::IsUndefinedOrNull));
    assert!(has_op(&ops, Opcode::Delete));
}

#[test]
fn loop_and_finally_control_stack() {
    let image = script(
        "function f() { outer: for (const x of values) { try { for (const y of x) { if (y) continue outer; return y; } } catch ({message}) { use(message); } finally { clean(); } } } 1; try { 2; } finally { 3; }",
    );
    let ops = instructions(&image, 1);
    for op in [
        Opcode::Catch,
        Opcode::Gosub,
        Opcode::Ret,
        Opcode::IteratorClose,
        Opcode::ForOfNext,
    ] {
        assert!(has_op(&ops, op), "{op:?}");
    }
}

#[test]
fn using_and_async_using_control_stack() {
    let image = script(
        "function f() { { using a = make(), b = make(); if (flag) return a; throw b; } } async function g() { await using a = await make(); for await (await using x of stream) { if (x) continue; return a; } }",
    );
    assert!(has_op(&instructions(&image, 1), Opcode::UsingDispose));
    assert!(has_op(&instructions(&image, 2), Opcode::UsingDisposeAsync));
    assert!(has_op(&instructions(&image, 2), Opcode::UsingDisposeMerge));
}

#[test]
fn classes_have_constructor_source_and_initializers() {
    let text = "let T = class extends Base { [field()] = 1; #x; get #y() { return this.#x; } set #y(v) { this.#x = v; } constructor(v) { super(v); this.#y = v; } m() { return super[method()]() ?? (#x in this); } static { this.t = 1; } };";
    let image = script(text);
    assert!(has_op(&instructions(&image, 0), Opcode::DefineClass));
    let ctor = image
        .functions
        .iter()
        .find(|f| f.flags.is_derived_class_constructor)
        .unwrap();
    assert_eq!(
        &text[ctor.debug.source_start as usize..ctor.debug.source_end as usize],
        &text[8..text.len() - 1]
    );
    assert_eq!(ctor.defined_arg_count, 1);
    assert!(
        image
            .functions
            .iter()
            .enumerate()
            .any(|(i, _)| has_op(&instructions(&image, i), Opcode::DefinePrivateField))
    );
}

#[test]
fn templates_generators_and_names() {
    let image = script(
        "let fn = (a, {b}, c = 1, ...d) => a; function* gen() { yield 1; yield* items; } async function* agen() { yield await p; yield* stream; } obj.tag`a${f()}b`; `${first()}${second()}`;",
    );
    assert_eq!(image.functions[2].kind, Some(FunctionKind::AsyncGenerator));
    let arrow = image.functions.iter().find(|f| f.arg_count == 4).unwrap();
    assert_eq!(arrow.defined_arg_count, 2);
    let entry = &image.functions[0];
    assert!(
        entry
            .constants
            .iter()
            .any(|c| matches!(c, ConstEntry::TemplateObject(t) if t.raw.len() == 2))
    );
    assert!(
        image
            .functions
            .iter()
            .enumerate()
            .any(|(i, _)| has_op(&instructions(&image, i), Opcode::YieldStar))
    );
}

#[test]
fn module_imports_exports_and_top_level_await() {
    let source = SourceText::from_utf8(
        "example.mjs",
        "import data from './data' with { type: 'json' }; export {data}; export * from './more'; export * as ns from './ns'; export default () => data; export const settled = await import('./later'); await using value = acquire(); import.meta;",
    );
    let image = compile_source(
        &source,
        ParseOptions {
            goal: ParseGoal::Module,
            ..ParseOptions::default()
        },
    )
    .unwrap();
    verify_image(&image).unwrap();
    let module = &image.modules[0];
    assert_eq!(module.exports.len(), 5);
    assert_eq!(module.imports.len(), 1);
    assert_eq!(module.requested_modules.len(), 3);
    assert_eq!(module.requested_modules[0].attributes.len(), 1);
    assert!(module.has_top_level_await);
    assert!(has_op(&instructions(&image, 0), Opcode::Await));
}

#[test]
fn suspended_expression_operands_are_unwound_before_cleanup() {
    let image = script(
        "function* g() { try { obj.foo = yield; } finally { return 1; } } async function* f() { for await ([[x[yield]]] of list) {} }",
    );
    assert!(has_op(&instructions(&image, 1), Opcode::NipCatch));
    assert!(has_op(&instructions(&image, 2), Opcode::IteratorClose));
}

#[test]
fn sloppy_block_function_has_a_variable_environment_mirror() {
    let image = script("if (go) { function f() {} } f(); { let h; { function h() {} } }");
    let entry = instructions(&image, 0);
    assert!(entry.iter().any(|i| matches!(
        i,
        Instruction {
            opcode: Opcode::PutVar,
            ..
        }
    )));
    assert_eq!(
        entry
            .iter()
            .filter(|i| i.opcode == Opcode::DefineVar)
            .count(),
        1
    );
}

#[test]
fn source_map_and_compilation_limits() {
    let text = "// α\nlet f = function /* name */ (a, b = 2) { return a; };\nf(1);";
    let image = script(text);
    let nested = &image.functions[1];
    assert_eq!(nested.defined_arg_count, 1);
    let source = SourceText::from_utf8("example.js", text);
    assert_eq!(source.line_col(nested.debug.source_start), (2, 9));
    assert_eq!(
        source.slice(qjs_syntax::Span::new(
            nested.debug.source_start,
            nested.debug.source_end
        )),
        b"function /* name */ (a, b = 2) { return a; }"
    );
    assert!(matches!(
        compile_source(
            &source,
            ParseOptions {
                max_bytes: 1,
                ..ParseOptions::default()
            }
        ),
        Err(CompileError::OutOfMemory)
    ));
    assert!(matches!(
        compile_source(
            &source,
            ParseOptions {
                max_depth: 1,
                ..ParseOptions::default()
            }
        ),
        Err(CompileError::StackOverflow)
    ));
}

#[test]
fn short_forms_encode_and_verify() {
    let image = script("function f(a) { if (a) return 24; return 0; }");
    let ops = instructions(&image, 1);
    for op in [
        Opcode::GetArg0,
        Opcode::PushI8,
        Opcode::Push0,
        Opcode::IfFalse8,
    ] {
        assert!(has_op(&ops, op), "{op:?}");
    }
    for f in &image.functions {
        assert_eq!(f.stack_size, qjs_bytecode::compute_stack_size(f).unwrap());
    }
}
