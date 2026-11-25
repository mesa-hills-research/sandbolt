use Opcode::*;
use sandbolt_bytecode::*;

fn function(code: &[u8], stack_size: u16) -> FunctionBytecode {
    FunctionBytecode {
        code: code.into(),
        stack_size,
        ..FunctionBytecode::default()
    }
}

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn accepts_straight_line_code_and_balanced_backward_branch() {
    let mut func = function(&[Push0 as u8, Push1 as u8, Add as u8, Return as u8], 2);
    assert_eq!(verify(&func), Ok(()));
    assert_eq!(compute_stack_size(&func), Ok(2));
    func.stack_size = 7;
    assert_eq!(verify(&func), Ok(()));
    let infinite = function(&[Goto8 as u8, 0xff], 0);
    assert_eq!(verify(&infinite), Ok(()));
}

#[test]
fn function_length_is_independent_of_argument_slots() {
    let mut func = function(&hex("0d01007e81000ecf83d7cb26020028"), 5);
    func.arg_count = 1;
    func.var_count = 1;
    // In this QuickJS-ng function with a destructured rest parameter, length is 2.
    func.defined_arg_count = 2;
    assert_eq!(verify(&func), Ok(()));
}

#[test]
fn rejects_invalid_and_truncated_unreachable_instructions() {
    for byte in [0, 252, 253, 254, 255] {
        let error = verify(&function(&[ReturnUndef as u8, byte], 0)).unwrap_err();
        assert_eq!(error.pc, 1);
        assert!(error.message.contains("invalid opcode"));
    }
    let error = verify(&function(&[ReturnUndef as u8, PushI32 as u8, 1, 2], 0)).unwrap_err();
    assert_eq!(error.pc, 1);
    assert!(error.message.contains("truncated push_i32"));
}

#[test]
fn rejects_targets_inside_operands_outside_code_and_at_eof() {
    for op in [Goto, IfFalse, Catch, Gosub] {
        let func = function(&[op as u8, 1, 0, 0, 0, ReturnUndef as u8], 1);
        assert!(
            verify(&func)
                .unwrap_err()
                .message
                .contains("instruction boundary")
        );
    }
    for offset in [0x80, 1, 2, 127] {
        assert!(
            verify(&function(&[Goto8 as u8, offset], 0))
                .unwrap_err()
                .message
                .contains("instruction boundary")
        );
    }
    let code = [
        WithGetVar as u8,
        BuiltinAtom::Name as u8,
        0,
        0,
        0,
        2,
        0,
        0,
        0,
        1,
        ReturnUndef as u8,
    ];
    assert!(
        verify(&function(&code, 0))
            .unwrap_err()
            .message
            .contains("instruction boundary")
    );
}

#[test]
fn rejects_stack_underflow_overflow_and_differing_join_heights() {
    let underflow = function(&[Drop as u8, ReturnUndef as u8], 0);
    assert!(
        verify(&underflow)
            .unwrap_err()
            .message
            .contains("stack underflow")
    );
    let overflow = function(&[Push0 as u8, Dup as u8, Add as u8, Return as u8], 1);
    assert!(
        verify(&overflow)
            .unwrap_err()
            .message
            .contains("stack overflow")
    );
    let join = function(
        &[
            Push0 as u8,
            IfTrue8 as u8,
            2,
            Push1 as u8,
            ReturnUndef as u8,
        ],
        1,
    );
    assert_eq!(verify(&join).unwrap_err().pc, 4);
    assert!(
        verify(&join)
            .unwrap_err()
            .message
            .contains("inconsistent stack heights")
    );
    assert!(
        verify(&function(&[Nop as u8], 0))
            .unwrap_err()
            .message
            .contains("falls off")
    );
    assert!(
        verify(&FunctionBytecode::default())
            .unwrap_err()
            .message
            .contains("empty bytecode")
    );
}

#[test]
fn counts_variable_call_pops_including_short_calls_and_eval() {
    for call in [
        &[Call as u8, 2, 0][..],
        &[Call2 as u8][..],
        &[Eval as u8, 2, 0, 0, 0][..],
    ] {
        let mut func = function(&[Undefined as u8, Push1 as u8, Push2 as u8], 3);
        func.code.extend_from_slice(call);
        func.code.push(Return as u8);
        assert_eq!(compute_stack_size(&func), Ok(3));
        func.code.remove(2);
        assert!(
            verify(&func)
                .unwrap_err()
                .message
                .contains("stack underflow")
        );
    }
}

#[test]
fn checks_constant_local_argument_varref_and_atom_indices() {
    let mut func = function(&[PushConst8 as u8, 0, Return as u8], 1);
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("constant index")
    );
    func.constants.push(ConstEntry::Int(1));
    assert_eq!(verify(&func), Ok(()));
    func.code[0] = Fclosure8 as u8;
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("function constant")
    );
    func.code = vec![GetLoc0Loc1 as u8, Add as u8, Return as u8];
    func.stack_size = 2;
    func.var_count = 1;
    assert!(verify(&func).unwrap_err().message.contains("local index 1"));
    func.var_count += 1;
    assert_eq!(verify(&func), Ok(()));
    func.code = vec![GetArg0 as u8, Return as u8];
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("argument index 0")
    );
    func.code[0] = GetVarRef0 as u8;
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("varref index 0")
    );
    func.closure_vars.push(ClosureVarDef::default());
    assert_eq!(verify(&func), Ok(()));
    func.code = vec![PushAtomValue as u8];
    func.code.extend(FIRST_DYNAMIC_ATOM.to_le_bytes());
    func.code.push(Return as u8);
    assert!(verify(&func).unwrap_err().message.contains("atom index"));
    func.atom_count = 1;
    assert_eq!(verify(&func), Ok(()));
    func.code[1..5].fill(0);
    assert!(verify(&func).unwrap_err().message.contains("atom index 0"));
}

#[test]
fn checks_additional_slots_read_by_disposal_and_iterator_ops() {
    let mut func = function(&[UsingDisposeAsync as u8, 0, 0, Return as u8], 1);
    func.var_count = 1;
    assert!(verify(&func).unwrap_err().message.contains("local index 1"));
    func.var_count = 2;
    assert_eq!(verify(&func), Ok(()));
    let code = [
        Object as u8,
        ForOfStart as u8,
        ForOfNext as u8,
        3,
        Return as u8,
    ];
    assert!(
        verify(&function(&code, 6))
            .unwrap_err()
            .message
            .contains("stack underflow")
    );
    let code = [Object as u8, IteratorGetValueDone as u8, Return as u8];
    assert!(
        verify(&function(&code, 6))
            .unwrap_err()
            .message
            .contains("stack underflow")
    );
    let code = [
        Object as u8,
        Object as u8,
        Null as u8,
        CopyDataProperties as u8,
        0xe0,
        Return as u8,
    ];
    assert!(
        verify(&function(&code, 8))
            .unwrap_err()
            .message
            .contains("stack underflow")
    );
}

#[test]
fn iterator_result_cannot_replace_an_exception_marker() {
    let code = [
        Catch as u8,
        9,
        0,
        0,
        0,
        Undefined as u8,
        Undefined as u8,
        Object as u8,
        IteratorGetValueDone as u8,
        Return as u8,
        Return as u8,
    ];
    assert!(
        verify(&function(&code, 5))
            .unwrap_err()
            .message
            .contains("control marker")
    );
}

#[test]
fn gosub_checks_real_returns_and_catch_checks_marker_removal() {
    let code = [Gosub as u8, 5, 0, 0, 0, ReturnUndef as u8, Ret as u8];
    assert_eq!(compute_stack_size(&function(&code, 0)), Ok(1));
    assert!(
        verify(&function(&code, 0))
            .unwrap_err()
            .message
            .contains("stack overflow")
    );
    let code = [Push0 as u8, Ret as u8];
    assert!(
        verify(&function(&code, 1))
            .unwrap_err()
            .message
            .contains("return address")
    );
    let code = [Push0 as u8, Push1 as u8, NipCatch as u8, Return as u8];
    assert!(
        verify(&function(&code, 2))
            .unwrap_err()
            .message
            .contains("no active catch")
    );
}

#[test]
fn exception_table_adds_handler_edges_with_a_preserved_prefix() {
    let mut func = function(
        &[
            Push0 as u8,
            Undefined as u8,
            Return as u8,
            Add as u8,
            Return as u8,
        ],
        2,
    );
    func.exception_handlers.push(ExceptionHandler {
        start_pc: 1,
        end_pc: 2,
        handler_pc: 3,
        stack_depth: 1,
    });
    assert_eq!(verify(&func), Ok(()));
    func.exception_handlers[0].stack_depth = 2;
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("preserves more slots")
    );
    func.exception_handlers[0].stack_depth = 0;
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("stack underflow")
    );
    func.exception_handlers[0].handler_pc = 5;
    assert!(
        verify(&func)
            .unwrap_err()
            .message
            .contains("invalid exception handler")
    );
}

#[test]
fn quickjs_stack_sizes_match_representative_compiled_functions() {
    // QuickJS-ng v0.17.0. Each row has the function body, arg and local counts,
    // then the emitted bytecode and stack size.
    let cases = [
        (
            "try { with(o) { if (x) return x; } for (const v of o) if (v) return v; } catch (e) { return e; } finally { o.y = 1; }",
            1,
            3,
            "6b5c000000d760000070d373500200000a000000013850020000f018cb73500200000a0000000138500200006e6c4300000028600100d77ef215d0610100f00f6101006e1e06836e6c28000000288100f0e90e830e066c1a0000000e29d16b0c000000cd6e6c0b000000286c0500000030d7bb42530200006d29",
            6,
        ),
        (
            "try { return 1; } finally { return 2; }",
            0,
            0,
            "6b0c000000bb6e6c0b000000286c0500000030bc28",
            3,
        ),
        (
            "return 1 + (yield* a);",
            1,
            0,
            "86bbd77e0e0606846f4171000000f1388811f1040ef2f1bcadf1168500f10e6f4171000000f0ea40440000000f0f0f2f8501f10b6f4171000000f0d5f20a85020e31000000000440440000000f0f0f9c2f",
            7,
        ),
        (
            "for await (const x of a) if (x) return x;",
            1,
            1,
            "600000d77ff224cf610000f01e6100006e0f1b11f8f112410600000011aff1082400008a6ff2020e0e2f140e2400008a82f0d50e0e0e11f8f112410600000011aff1082400008a6ff2020e0e062f",
            6,
        ),
        (
            "const [x, y] = a; return y;",
            1,
            2,
            "6001006000000611f8f10d7e81000ecf81000ed083f2050ed7f2f161010028",
            5,
        ),
        ("return ({...a});", 1, 0, "0bd70752060e0e28", 3),
        (
            "const {x, ...r} = a; return r;",
            1,
            2,
            "6001006000000611f8f118700b074b520200001b4152020000cf0b5244d00e0ef2050ed7f2e661010028",
            3,
        ),
        (
            "try { try { return a } finally { a++ } } finally { a *= 2 }",
            1,
            0,
            "6b210000006b12000000d76e6c110000006e6c15000000286c0500000030d78edb6d6c0500000030d7bc99db6d",
            4,
        ),
        (
            "for (const a of x) { try { if (a) continue } finally { if (x) break } } return x",
            1,
            1,
            "600000d77ef22ecf6b1d000000610000f00b0e066c170000000ef2190e066c0d0000000ef20f6c0500000030d7f0050e0ef2076d8100f0d00e83d728",
            6,
        ),
        (
            "await using r = a; return r",
            1,
            2,
            "6001006000006b1e000000d7b701d0cf6100006eb26b0c000000b400008a0e0ef202b5b62f6b0b000000b400008a0e0e30b530",
            4,
        ),
        (
            "using r = a; return r",
            1,
            2,
            "6001006000006b13000000d7b700d0cf6100006eb2b30000b628b3000030",
            3,
        ),
        (
            "try { return yield* x } finally { await x }",
            1,
            0,
            "866b6b000000d77f0e0606848a6f4171000000f14940440000008911f1040ef2ebbcadf11f8a8500f10f8a6f4171000000f0e340440000000f0f0f8a6e6c350000002f8501f10c8a6f4171000000f0c6f20c8502f1028a31000000000440440000000f0f0f8a6e6c0b0000002f6c0500000030d78a0e6d062f",
            7,
        ),
        (
            "with(a) { x = 5; delete x; return typeof x }",
            1,
            1,
            "d760000070d376500200000a000000017c50020000bf3ccb75500200000a0000000198500200000ecb73500200000a0000000137500200009628",
            3,
        ),
        (
            "var obj = { [a]: class {} }; return obj",
            1,
            2,
            "0bd706600100c40056300000000006d00e670100500ed328",
            5,
        ),
        (
            "return class { [a] = class {}; #p; static {} get p(){return this.#p} }",
            1,
            3,
            "06600200600100600000c401553000000000d771cf0551020000d0c500535202000001c5024fd10e11c5034f2400000e67020067010067000028",
            3,
        ),
        (
            "function f(a = eval(\"1\")) { return a }",
            1,
            7,
            "0c03c80408d20c00c906c8050c05d00c05d1600000d711f8f01a0ecd733e0000000a00000000383e000000c4003201000700dfcf670600670000d728",
            2,
        ),
        (
            "function f(a, ...[b]) { return [a,b] }",
            1,
            1,
            "0d01007e81000ecf83d7cb26020028",
            5,
        ),
    ];
    for (source, arg_count, var_count, code, stack_size) in cases {
        let func = FunctionBytecode {
            arg_count,
            var_count,
            code: hex(code),
            stack_size,
            atom_count: 500,
            constants: vec![ConstEntry::Function(0); 4],
            ..FunctionBytecode::default()
        };
        assert_eq!(
            compute_stack_size(&func),
            Ok(stack_size),
            "{source}\n{}",
            disassemble_function(&func)
        );
        assert_eq!(verify(&func), Ok(()));
    }
}
