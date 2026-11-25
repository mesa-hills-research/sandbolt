use sandbolt_bytecode::*;

#[test]
fn builtin_atoms_match_quickjs_in_order() {
    let atoms: Vec<_> = include_str!("reference/quickjs-atom.h")
        .lines()
        .filter_map(|line| line.strip_prefix("DEF("))
        .map(|line| line.split('"').nth(1).unwrap())
        .collect();
    assert_eq!(atoms.len(), 241);
    assert_eq!(atoms.len(), BUILTIN_ATOM_COUNT as usize);
    for (index, spelling) in atoms.into_iter().enumerate() {
        let id = index as u32 + 1;
        let atom = BuiltinAtom::from_u32(id).unwrap();
        assert_eq!(atom.as_str(), spelling);
        assert_eq!(atom as u32, id);
        assert_eq!(atom.id(), UnitAtom(id));
        if id <= BuiltinAtom::Await as u32 {
            assert_eq!(BuiltinAtom::from_str_keyword(spelling), Some(atom));
        } else {
            assert_eq!(BuiltinAtom::from_str_keyword(spelling), None);
        }
    }
    assert_eq!(
        BuiltinAtom::ALL
            .iter()
            .filter(|atom| atom.as_str().starts_with("Symbol."))
            .count(),
        15
    );
    assert_ne!(BuiltinAtom::Brand, BuiltinAtom::PrivateBrand);
    assert_eq!(
        BuiltinAtom::Brand.as_str(),
        BuiltinAtom::PrivateBrand.as_str()
    );
    assert_ne!(BuiltinAtom::Null.id(), UnitAtom::NONE);
    assert_eq!(BuiltinAtom::from_str_keyword("While"), None);
    assert_eq!(BuiltinAtom::from_str_keyword("using "), None);
}

#[test]
fn unit_atom_ids_separate_builtins_and_dynamic_entries() {
    assert_eq!(UnitAtom::NONE.builtin(), None);
    assert!(!UnitAtom::NONE.is_in_bounds(10));
    assert_eq!(UnitAtom::from_dynamic_index(u32::MAX), None);
    assert_eq!(FIRST_DYNAMIC_ATOM, 242);
    let first = UnitAtom::from_dynamic_index(0).unwrap();
    assert_eq!(first, UnitAtom(FIRST_DYNAMIC_ATOM));
    assert_eq!(first.dynamic_index(), Some(0));
    assert!(!first.is_in_bounds(0));
    assert!(first.is_in_bounds(1));
    assert_eq!(BuiltinAtom::from_u32(FIRST_DYNAMIC_ATOM), None);
    assert_eq!(BuiltinAtom::from_u32(u32::MAX), None);
}

#[test]
fn opcode_info_matches_every_final_quickjs_opcode() {
    let table = include_str!("reference/quickjs-opcode.h")
        .lines()
        .filter_map(|line| line.strip_prefix("DEF("))
        .map(|line| {
            line.split_once(')')
                .unwrap()
                .0
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(table.len(), 252);
    assert_eq!(Opcode::ALL.len(), table.len());
    assert_eq!(OPCODE_INFO.len(), table.len());
    assert_eq!(OPCODE_COUNT, 251);
    for (byte, fields) in table.into_iter().enumerate() {
        let op = Opcode::ALL[byte];
        let info = op.info();
        assert_eq!(op as usize, byte);
        assert_eq!(info.name, fields[0]);
        assert_eq!(info.size, fields[1].parse::<u8>().unwrap());
        assert_eq!(info.n_pop, fields[2].parse::<u8>().unwrap());
        assert_eq!(info.n_push, fields[3].parse::<u8>().unwrap());
        let capitalized = fields[4]
            .split('_')
            .map(|part| part[..1].to_uppercase() + &part[1..])
            .collect::<String>();
        assert_eq!(format!("{:?}", info.operand_fmt), capitalized);
        assert_eq!(info.size, info.operand_fmt.byte_len() + 1);
        if byte != 0 {
            assert_eq!(Opcode::from_u8(byte as u8), Some(op));
        }
    }
    assert_eq!(Opcode::from_u8(0), None);
    for invalid in 252..=255 {
        assert_eq!(Opcode::from_u8(invalid), None);
    }
}

#[test]
fn decoder_checks_each_executable_opcode_and_operand_length() {
    for &opcode in &Opcode::ALL[1..] {
        let mut bytes = [0; 16];
        bytes[0] = opcode as u8;
        let len = usize::from(opcode.info().size);
        assert_eq!(
            Instruction::decode(&bytes[..len], 0).unwrap().opcode,
            opcode
        );
        for end in 1..len {
            assert_eq!(
                Instruction::decode(&bytes[..end], 0),
                Err(InstructionError::TruncatedOperand(opcode))
            );
        }
    }
    assert_eq!(
        Instruction::decode(&[0], 0),
        Err(InstructionError::InvalidOpcode(0))
    );
    assert_eq!(
        Instruction::decode(&[], usize::MAX),
        Err(InstructionError::MissingOpcode)
    );
}

#[test]
fn decoder_exposes_implicit_indices_and_relative_bases() {
    let one = |op| Instruction::decode(&[op as u8], 0).unwrap().operands;
    assert_eq!(one(Opcode::PushMinus1), Operands::Signed(-1));
    assert_eq!(one(Opcode::GetLoc0Loc1), Operands::Locals(0, 1));
    assert_eq!(one(Opcode::SetLoc3), Operands::Local(3));
    assert_eq!(one(Opcode::PutArg2), Operands::Argument(2));
    assert_eq!(one(Opcode::GetVarRef1), Operands::VarRef(1));
    assert_eq!(one(Opcode::Call3), Operands::Call(3));
    let goto = Instruction::decode(&[Opcode::Goto16 as u8, 0xff, 0xff], 0).unwrap();
    assert_eq!(goto.branch_target(12), Some(12));
    let code = [
        Opcode::WithGetVar as u8,
        0x78,
        0x56,
        0x34,
        0x12,
        0xff,
        0xff,
        0xff,
        0xff,
        1,
    ];
    let with = Instruction::decode(&code, 0).unwrap();
    assert_eq!(
        with.operands,
        Operands::AtomJumpU8(UnitAtom(0x1234_5678), -1, 1)
    );
    assert_eq!(with.branch_target(12), Some(16));
}

#[test]
fn encoder_inverts_decoder_for_every_opcode() {
    for &opcode in &Opcode::ALL[1..] {
        let mut bytes = [0; 16];
        bytes[0] = opcode as u8;
        for pattern in [0, 0x12, 0x80, 0xff] {
            for (index, byte) in bytes[1..].iter_mut().enumerate() {
                *byte = (index as u8).wrapping_mul(31).wrapping_add(pattern);
            }
            let expected = &bytes[..usize::from(opcode.info().size)];
            let insn = Instruction::decode(expected, 0).unwrap();
            assert_eq!(insn.encode().unwrap().as_bytes(), expected);
        }
    }
    for (opcode, operands, error) in [
        (
            Opcode::Invalid,
            Operands::None,
            EncodeInstructionError::InvalidOpcode,
        ),
        (
            Opcode::Call1,
            Operands::Call(2),
            EncodeInstructionError::OperandMismatch(Opcode::Call1),
        ),
        (
            Opcode::PushI32,
            Operands::Unsigned(3),
            EncodeInstructionError::OperandMismatch(Opcode::PushI32),
        ),
        (
            Opcode::GetLoc8,
            Operands::Local(256),
            EncodeInstructionError::OperandOutOfRange(Opcode::GetLoc8),
        ),
        (
            Opcode::Goto8,
            Operands::Jump(-129),
            EncodeInstructionError::OperandOutOfRange(Opcode::Goto8),
        ),
        (
            Opcode::Goto16,
            Operands::Jump(32768),
            EncodeInstructionError::OperandOutOfRange(Opcode::Goto16),
        ),
    ] {
        assert_eq!(Instruction { opcode, operands }.encode(), Err(error));
    }
}

#[test]
fn source_mapping_uses_preceding_change_point() {
    let debug = DebugInfo {
        pc_map: vec![
            PcSourceMap {
                pc: 2,
                byte_offset: 90,
            },
            PcSourceMap {
                pc: 7,
                byte_offset: 12,
            },
        ],
        ..DebugInfo::default()
    };
    assert_eq!(debug.pc_to_byte_offset(0), None);
    assert_eq!(debug.pc_to_byte_offset(2), Some(90));
    assert_eq!(debug.pc_to_byte_offset(6), Some(90));
    assert_eq!(debug.pc_to_byte_offset(7), Some(12));
    assert_eq!(debug.pc_to_byte_offset(u32::MAX), Some(12));
}
