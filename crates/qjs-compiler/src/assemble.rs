use crate::{
    CompileError,
    emit::{Draft, Result},
};
use qjs_bytecode::*;

fn bad(what: impl ToString) -> CompileError {
    CompileError::Bytecode(what.to_string())
}
fn insn(opcode: Opcode, operands: Operands) -> Instruction {
    Instruction { opcode, operands }
}

fn small(i: Instruction) -> Instruction {
    use Opcode::*;
    use Operands::*;
    const INT: [Opcode; 9] = [
        PushMinus1, Push0, Push1, Push2, Push3, Push4, Push5, Push6, Push7,
    ];
    const SLOT: [[Opcode; 4]; 10] = [
        [GetLoc0, GetLoc1, GetLoc2, GetLoc3],
        [PutLoc0, PutLoc1, PutLoc2, PutLoc3],
        [SetLoc0, SetLoc1, SetLoc2, SetLoc3],
        [GetArg0, GetArg1, GetArg2, GetArg3],
        [PutArg0, PutArg1, PutArg2, PutArg3],
        [SetArg0, SetArg1, SetArg2, SetArg3],
        [GetVarRef0, GetVarRef1, GetVarRef2, GetVarRef3],
        [PutVarRef0, PutVarRef1, PutVarRef2, PutVarRef3],
        [SetVarRef0, SetVarRef1, SetVarRef2, SetVarRef3],
        [Call0, Call1, Call2, Call3],
    ];
    let short = |group: usize, n: u16, fallback| {
        SLOT[group].get(usize::from(n)).copied().unwrap_or(fallback)
    };
    let byte = |n| u8::try_from(n).is_ok();
    let opcode = match (i.opcode, i.operands) {
        (PushI32, Signed(n @ -1..=7)) => INT[(n + 1) as usize],
        (PushI32, Signed(n)) if i8::try_from(n).is_ok() => PushI8,
        (PushI32, Signed(n)) if i16::try_from(n).is_ok() => PushI16,
        (PushConst, Constant(n)) if byte(n) => PushConst8,
        (Fclosure, Constant(n)) if byte(n) => Fclosure8,
        (GetLoc, Local(n)) if byte(u32::from(n)) => short(0, n, GetLoc8),
        (PutLoc, Local(n)) if byte(u32::from(n)) => short(1, n, PutLoc8),
        (SetLoc, Local(n)) if byte(u32::from(n)) => short(2, n, SetLoc8),
        (GetArg, Argument(n)) => short(3, n, GetArg),
        (PutArg, Argument(n)) => short(4, n, PutArg),
        (SetArg, Argument(n)) => short(5, n, SetArg),
        (GetVarRef, VarRef(n)) => short(6, n, GetVarRef),
        (PutVarRef, VarRef(n)) => short(7, n, PutVarRef),
        (SetVarRef, VarRef(n)) => short(8, n, SetVarRef),
        (Opcode::Call, Operands::Call(n)) => short(9, n, Opcode::Call),
        _ => i.opcode,
    };
    insn(opcode, i.operands)
}

fn as_instruction(op: IrOp) -> Result<Option<Instruction>> {
    use IrOp::*;
    let jump = |opcode, label| Some(insn(opcode, Operands::Jump(label as i32)));
    Ok(match op {
        Resolved(i) => Some(small(i)),
        Jump(l) => jump(Opcode::Goto, l),
        JumpIfFalse(l) => jump(Opcode::IfFalse, l),
        JumpIfTrue(l) => jump(Opcode::IfTrue, l),
        Catch(l) => jump(Opcode::Catch, l),
        Gosub(l) => jump(Opcode::Gosub, l),
        EnterScope(_) | LeaveScope(_) | Label(_) | SourceLoc(_) => None,
        _ => return Err(bad(format!("unresolved operation {op:?}"))),
    })
}

fn shorter(op: Opcode, diff: i64) -> Opcode {
    use Opcode::*;
    if i8::try_from(diff).is_ok() {
        match op {
            Goto | Goto16 => Goto8,
            IfFalse => IfFalse8,
            IfTrue => IfTrue8,
            _ => op,
        }
    } else if op == Goto && i16::try_from(diff).is_ok() {
        Goto16
    } else {
        op
    }
}

fn discard_unused_arguments(function: &FunctionBytecode, instructions: &mut [Option<Instruction>]) {
    use Opcode::*;
    use Operands::{Local, Locals, Unsigned};
    let Some((init, slot)) =
        instructions
            .windows(2)
            .enumerate()
            .find_map(|(i, pair)| match *pair {
                [
                    Some(Instruction {
                        opcode: SpecialObject,
                        operands: Unsigned(0 | 1),
                    }),
                    Some(Instruction {
                        opcode: PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3,
                        operands: Local(slot),
                    }),
                ] => Some((i, slot)),
                _ => None,
            })
    else {
        return;
    };
    // Name resolution marks captures, including bindings visible to direct eval.
    if function.vars[usize::from(function.arg_count) + usize::from(slot)].captured
        || instructions.iter().enumerate().any(|(i, insn)| {
            i != init + 1
                && insn.is_some_and(|insn| match insn.operands {
                    Local(n) => n == slot,
                    Locals(a, b) => a == slot || b == slot,
                    _ => false,
                })
        })
    {
        return;
    }
    instructions[init..init + 2].fill(None);
}

pub(crate) fn assemble(mut draft: Draft) -> Result<FunctionBytecode> {
    let ir: Vec<_> = draft.prologue.into_iter().chain(draft.ir).collect();
    let mut instructions = ir
        .iter()
        .copied()
        .map(as_instruction)
        .collect::<Result<Vec<_>>>()?;
    discard_unused_arguments(&draft.bytecode, &mut instructions);
    let mut labels = vec![None; draft.labels as usize];
    let mut positions = vec![0; ir.len()];
    let end = loop {
        let mut pc = 0u32;
        for (i, op) in ir.iter().enumerate() {
            positions[i] = pc;
            if let IrOp::Label(label) = op {
                labels[*label as usize] = Some(pc);
            }
            if let Some(insn) = instructions[i] {
                pc = pc
                    .checked_add(u32::from(insn.opcode.info().size))
                    .ok_or(CompileError::OutOfMemory)?;
            }
        }
        let mut changed = false;
        for (i, insn) in instructions.iter_mut().enumerate() {
            if let Some(Instruction {
                opcode,
                operands: Operands::Jump(label),
            }) = insn
            {
                let target = labels[*label as usize].ok_or_else(|| bad("unbound label"))?;
                let short = shorter(*opcode, i64::from(target) - i64::from(positions[i]) - 1);
                if short != *opcode {
                    *opcode = short;
                    changed = true;
                }
            }
        }
        if !changed {
            break pc;
        }
    };
    let f = &mut draft.bytecode;
    f.code
        .try_reserve_exact(end as usize)
        .map_err(|_| CompileError::OutOfMemory)?;
    let mut source = f.debug.source_start;
    for (i, op) in ir.into_iter().enumerate() {
        let pc = positions[i];
        match op {
            IrOp::SourceLoc(offset) => source = offset,
            IrOp::EnterScope(scope) => draft.scopes[usize::from(scope)].info.start_pc = pc,
            IrOp::LeaveScope(scope) => draft.scopes[usize::from(scope)].info.end_pc = pc,
            _ => (),
        }
        if let Some(mut instruction) = instructions[i] {
            let diff = |label: i32, base: u32| {
                let to = labels[label as usize].ok_or_else(|| bad("unbound label"))?;
                i32::try_from(i64::from(to) - i64::from(pc) - i64::from(base))
                    .map_err(|_| CompileError::OutOfMemory)
            };
            instruction.operands = match instruction.operands {
                Operands::Jump(label) => Operands::Jump(diff(label, 1)?),
                Operands::AtomJumpU8(atom, label, flags) => {
                    Operands::AtomJumpU8(atom, diff(label, 5)?, flags)
                }
                operands => operands,
            };
            if f.debug
                .pc_map
                .last()
                .is_none_or(|last| last.byte_offset != source)
            {
                f.debug.pc_map.push(PcSourceMap {
                    pc,
                    byte_offset: source,
                });
            }
            f.code
                .extend_from_slice(instruction.encode().map_err(bad)?.as_bytes());
        }
    }
    f.scopes = draft.scopes.into_iter().map(|scope| scope.info).collect();
    f.stack_size = compute_stack_size(f).map_err(|err| {
        if err.message.starts_with("stack overflow:") {
            CompileError::StackOverflow
        } else {
            bad(format!("{} in {}", err, f.name.raw()))
        }
    })?;
    Ok(draft.bytecode)
}
