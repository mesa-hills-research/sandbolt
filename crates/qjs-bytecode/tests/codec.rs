use qjs_bytecode::codec::{IMAGE_MAGIC, IMAGE_VERSION};
use qjs_bytecode::*;

fn unit() -> ProgramImage {
    let dynamic = |index| UnitAtom::from_dynamic_index(index).unwrap();
    let latin = |bytes: &[u8]| ConstString::Latin1(bytes.to_vec());
    let utf16 = ConstString::Utf16(vec![u16::from(b'a'), 0xd800, 0, 0xdfff]);
    let main = FunctionBytecode {
        name: dynamic(0),
        kind: Some(FunctionKind::AsyncGenerator),
        strict: true,
        flags: FunctionFlags {
            has_prototype: true,
            has_simple_parameter_list: true,
            is_derived_class_constructor: true,
            need_home_object: true,
            new_target_allowed: true,
            super_call_allowed: true,
            super_allowed: true,
            arguments_allowed: true,
            backtrace_barrier: true,
        },
        arg_count: 1,
        var_count: 2,
        defined_arg_count: 1,
        stack_size: 1,
        var_ref_count: 1,
        atom_count: 4,
        vars: vec![
            VarDef {
                name: BuiltinAtom::Name.id(),
                ..VarDef::default()
            },
            VarDef {
                name: dynamic(1),
                kind: VarKind::AwaitUsing,
                scope_level: 1,
                scope_next: ScopeLink::Local(1),
                is_const: true,
                lexical_tdz: true,
                captured: true,
                var_ref_index: Some(0),
            },
            VarDef {
                scope_next: ScopeLink::ParameterEnd,
                ..VarDef::default()
            },
        ],
        closure_vars: vec![ClosureVarDef {
            name: BuiltinAtom::Source.id(),
            kind: ClosureKind::ModuleImport,
            index: 0,
            var_kind: VarKind::Const,
            is_const: true,
            lexical_tdz: true,
        }],
        scopes: vec![
            ScopeInfo {
                start_pc: 0,
                end_pc: 3,
                ..ScopeInfo::default()
            },
            ScopeInfo {
                parent: Some(0),
                start_pc: 0,
                end_pc: 2,
                has_using: true,
                has_await_using: true,
            },
        ],
        exception_handlers: vec![ExceptionHandler {
            start_pc: 0,
            end_pc: 1,
            handler_pc: 2,
            stack_depth: 0,
        }],
        code: vec![
            Opcode::GetArg0 as u8,
            Opcode::Return as u8,
            Opcode::Throw as u8,
        ],
        constants: vec![
            ConstEntry::Int(i32::MIN),
            ConstEntry::Int(i32::MAX),
            ConstEntry::Float(-0.0),
            ConstEntry::String(latin(b"\0\xff")),
            ConstEntry::String(utf16.clone()),
            ConstEntry::BigInt("-123456789012345678901234567890".into()),
            ConstEntry::RegExp {
                pattern: utf16.clone(),
                flags: 0x101,
            },
            ConstEntry::Function(1),
            ConstEntry::TemplateObject(TemplateObject {
                cooked: vec![Some(latin(b"x")), None, Some(utf16.clone())],
                raw: vec![latin(b"x"), latin(b"\\u?"), utf16.clone()],
            }),
        ],
        debug: DebugInfo {
            filename: "example-λ.js".into(),
            source_start: 0,
            source_end: u32::MAX,
            pc_map: vec![
                PcSourceMap {
                    pc: 0,
                    byte_offset: u32::MAX,
                },
                PcSourceMap {
                    pc: 1,
                    byte_offset: 0,
                },
            ],
        },
    };
    let child = FunctionBytecode {
        closure_vars: vec![ClosureVarDef {
            name: dynamic(1),
            kind: ClosureKind::Local,
            index: 0,
            var_kind: VarKind::AwaitUsing,
            is_const: true,
            lexical_tdz: true,
        }],
        atom_count: 4,
        code: vec![Opcode::ReturnUndef as u8],
        ..FunctionBytecode::default()
    };
    ProgramImage {
        atoms: vec![
            latin(b"entry.mjs"),
            utf16,
            latin(b"./data.json"),
            latin(b"json"),
        ],
        functions: vec![main, child],
        entry_function: 0,
        modules: vec![ModuleRecord {
            name: dynamic(0),
            requested_modules: vec![RequestedModule {
                name: dynamic(2),
                attributes: vec![ImportAttribute {
                    key: dynamic(1),
                    value: dynamic(3),
                }],
            }],
            imports: vec![ImportEntry {
                requested_module: 0,
                import_name: None,
                closure_var: 0,
            }],
            exports: vec![
                ExportEntry::Local {
                    export_name: BuiltinAtom::Default.id(),
                    closure_var: 0,
                },
                ExportEntry::Indirect {
                    export_name: BuiltinAtom::Name.id(),
                    requested_module: 0,
                    import_name: Some(BuiltinAtom::Star.id()),
                },
                ExportEntry::Indirect {
                    export_name: BuiltinAtom::Values.id(),
                    requested_module: 0,
                    import_name: None,
                },
                ExportEntry::Star {
                    requested_module: 0,
                },
            ],
            has_top_level_await: true,
            function: 0,
        }],
    }
}

#[test]
fn round_trips_every_image_record() {
    let image = unit();
    let encoded = encode_image(&image).unwrap();
    let copy = decode_image(&encoded).unwrap();
    assert_eq!(image, copy);
    assert_eq!(
        image.atom(UnitAtom::from_dynamic_index(1).unwrap()),
        Some(AtomName::Dynamic(&image.atoms[1]))
    );
    assert_eq!(copy.functions[0].debug.pc_to_byte_offset(1), Some(0));
    assert_eq!(encode_image(&copy).unwrap(), encoded);
}

#[test]
fn preserves_exact_float_bits() {
    let mut image = unit();
    let bits = [
        0x7ff8_0000_0000_0347,
        0xfff0_0000_0000_0000,
        0x8000_0000_0000_0000,
    ];
    image.functions[0].constants = bits
        .map(|bits| ConstEntry::Float(f64::from_bits(bits)))
        .into();
    let encoded = encode_image(&image).unwrap();
    let decoded = decode_image(&encoded).unwrap();
    for (value, expected) in decoded.functions[0].constants.iter().zip(bits) {
        let ConstEntry::Float(value) = value else {
            panic!("expected float")
        };
        assert_eq!(value.to_bits(), expected);
    }
    assert_eq!(encode_image(&decoded).unwrap(), encoded);
}

#[test]
fn minimal_image_has_stable_wire_bytes() {
    let image = ProgramImage {
        functions: vec![FunctionBytecode {
            code: vec![Opcode::ReturnUndef as u8],
            ..FunctionBytecode::default()
        }],
        ..ProgramImage::default()
    };
    let mut expected = IMAGE_MAGIC.to_vec();
    expected.extend([1, 0, 0, 1]); // version, entry, atoms, functions
    expected.extend([0; 14]); // function header and empty tables
    expected.extend([1, 0x29]); // code length and return_undef
    expected.extend([0; 5]); // empty debug info and module table
    assert_eq!(encode_image(&image).unwrap(), expected);
    assert_eq!(decode_image(&expected).unwrap(), image);
    assert_eq!(IMAGE_VERSION, 1);
}

#[test]
fn rejects_every_truncated_prefix_and_trailing_bytes() {
    let mut encoded = encode_image(&unit()).unwrap();
    for end in 0..encoded.len() {
        assert!(decode_image(&encoded[..end]).is_err(), "prefix {end}");
    }
    encoded.push(0);
    assert_eq!(
        decode_image(&encoded).unwrap_err().kind,
        CodecErrorKind::TrailingBytes
    );
}

#[test]
fn rejects_invalid_magic_tags_versions_and_leb128() {
    let mut encoded = encode_image(&unit()).unwrap();
    encoded[0] = 0;
    assert_eq!(
        decode_image(&encoded).unwrap_err().kind,
        CodecErrorKind::BadMagic
    );
    let with_header = |tail: &[u8]| [IMAGE_MAGIC.as_slice(), tail].concat();
    for (tail, kind) in [
        (&[2][..], CodecErrorKind::UnsupportedVersion(2)),
        (&[0x81, 0x00], CodecErrorKind::InvalidLeb128),
        (&[0xff; 10], CodecErrorKind::InvalidLeb128),
        (
            &[0x80, 0x80, 0x80, 0x80, 0x10],
            CodecErrorKind::IntegerOverflow,
        ),
        (
            &[1, 0, 0xff, 0xff, 0xff, 0xff, 0x0f],
            CodecErrorKind::Truncated,
        ),
        (
            &[1, 0, 1, 0xff, 0],
            CodecErrorKind::InvalidTag("string", 0xff),
        ),
    ] {
        assert_eq!(decode_image(&with_header(tail)).unwrap_err().kind, kind);
    }
}

#[test]
fn codec_checks_code_and_cross_table_references() {
    let mut image = unit();
    image.entry_function = 3;
    assert!(matches!(
        encode_image(&image).unwrap_err().kind,
        CodecErrorKind::InvalidImage(_)
    ));
    image.entry_function = 0;
    image.modules[0].imports[0].requested_module = 1;
    assert!(
        encode_image(&image)
            .unwrap_err()
            .to_string()
            .contains("module import reference")
    );
    image.modules[0].imports[0].requested_module = 0;
    image.functions[1].closure_vars[0].index = image.functions[0].var_count;
    assert!(
        encode_image(&image)
            .unwrap_err()
            .to_string()
            .contains("enclosing capture index")
    );
    image.functions[1].closure_vars[0].index = 0;
    image.functions[1].atom_count = 5;
    assert!(
        encode_image(&image)
            .unwrap_err()
            .to_string()
            .contains("dynamic atom count")
    );
    image.functions[1].atom_count = 4;
    image.functions[0].vars[2].scope_next = ScopeLink::Local(0);
    assert!(
        encode_image(&image)
            .unwrap_err()
            .to_string()
            .contains("cyclic lexical scope chain")
    );
    image.functions[0].vars[2].scope_next = ScopeLink::ParameterEnd;
    let mut encoded = encode_image(&image).unwrap();
    let child_code = encoded
        .iter()
        .rposition(|&byte| byte == Opcode::ReturnUndef as u8)
        .unwrap();
    encoded[child_code] = Opcode::Invalid as u8;
    assert!(
        decode_image(&encoded)
            .unwrap_err()
            .to_string()
            .contains("invalid opcode")
    );
}

#[test]
fn mutations_and_generated_inputs_never_panic() {
    let baseline = encode_image(&unit()).unwrap();
    let mut mutated = baseline.clone();
    for (i, &byte) in baseline.iter().enumerate() {
        for replacement in [0, 0x7f, 0x80, 0xff, byte ^ 1] {
            mutated[i] = replacement;
            if let Ok(image) = decode_image(&mutated) {
                assert_eq!(encode_image(&image).unwrap(), mutated);
            }
        }
        mutated[i] = byte;
    }
    let mut seed = 0x1234_5678_u32;
    for i in 0..4096 {
        let mut data = baseline[..9].to_vec();
        for _ in 0..i % 129 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((seed >> 21) as u8);
        }
        if let Ok(image) = decode_image(&data) {
            assert_eq!(encode_image(&image).unwrap(), data);
        }
    }
}
