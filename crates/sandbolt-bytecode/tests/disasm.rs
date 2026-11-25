use sandbolt_bytecode::*;

#[test]
fn lists_code_constants_source_and_branch_targets() {
    let func = FunctionBytecode {
        name: BuiltinAtom::ValueOf.id(),
        code: vec![
            Opcode::PushConst8 as u8,
            0,
            Opcode::IfTrue8 as u8,
            3,
            Opcode::PushMinus1 as u8,
            Opcode::Return as u8,
            Opcode::ReturnUndef as u8,
        ],
        constants: vec![ConstEntry::String(ConstString::Utf16(vec![
            0xd800, 34, 0xff,
        ]))],
        stack_size: 1,
        debug: DebugInfo {
            filename: "sample.js".into(),
            source_start: 3,
            source_end: 80,
            pc_map: vec![
                PcSourceMap {
                    pc: 0,
                    byte_offset: 21,
                },
                PcSourceMap {
                    pc: 4,
                    byte_offset: 39,
                },
            ],
        },
        ..FunctionBytecode::default()
    };
    assert_eq!(
        disassemble_function(&func),
        concat!(
            "fn \"valueOf\" (args 0, locals 0, closure 0, stack 1)\n",
            "  source \"sample.js\" 3..80\n",
            "  0000  push_const8 const[0] = \"\\ud800\\\"ÿ\"  ; source 21\n",
            "  0002  if_true8 @0006\n",
            "  0004  push_minus1 -1  ; source 39\n",
            "  0005  return\n",
            "  0006  return_undef\n",
        )
    );
}

#[test]
fn image_listing_resolves_dynamic_atoms_and_distinguishes_namespace_imports() {
    let first = UnitAtom(FIRST_DYNAMIC_ATOM);
    let func = FunctionBytecode {
        name: first,
        code: vec![Opcode::ReturnUndef as u8],
        ..FunctionBytecode::default()
    };
    assert!(disassemble_function(&func).starts_with("fn atom[242]"));
    let image = ProgramImage {
        atoms: vec![ConstString::Latin1(b"example.mjs".to_vec())],
        functions: vec![func],
        modules: vec![ModuleRecord {
            name: first,
            imports: vec![ImportEntry {
                requested_module: 0,
                import_name: None,
                closure_var: 0,
            }],
            exports: vec![ExportEntry::Indirect {
                export_name: BuiltinAtom::Default.id(),
                requested_module: 0,
                import_name: Some(BuiltinAtom::Star.id()),
            }],
            has_top_level_await: true,
            ..ModuleRecord::default()
        }],
        entry_function: 0,
    };
    let listing = disassemble_image(&image);
    assert!(listing.starts_with("entry fn[0]\n\nfn[0] \"example.mjs\""));
    assert!(listing.contains("module[0] \"example.mjs\" fn[0] (top-level await)"));
    assert!(listing.contains("import closure[0] request[0] (namespace)"));
    assert!(listing.contains("export \"default\" request[0] \"*\""));
}

#[test]
fn disassembly_reports_a_truncated_operand() {
    let func = FunctionBytecode {
        code: vec![Opcode::Nop as u8, Opcode::PushConst as u8, 0],
        ..FunctionBytecode::default()
    };
    assert!(
        disassemble_function(&func)
            .ends_with("  0000  nop\n  0001  <truncated push_const operand>\n")
    );
}
