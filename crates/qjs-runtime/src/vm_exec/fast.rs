use std::cell::Cell;

use qjs_bytecode::{BuiltinAtom, Instruction, Opcode, Operands, UnitAtom};

use crate::ledger::{AllocResult, HeapLedger, TrackedVec};
use crate::vm::{CompiledFunc, InlineCache};

/// Compact 8-byte pre-decoded instruction for the fast VM dispatch loop.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FastOp {
    Nop,
    PushI32(i32),
    PushBigintI32(i32),
    Undefined,
    Null,
    PushFalse,
    PushTrue,
    PushEmptyString,
    PushThis,
    SpecialObject(u8),
    PushConst(u32),
    PushRegExpConst(u32),
    PushConstRepeat { index: u32, count: u16 },
    PushConstSeq { start: u32, count: u16 },
    Drop,
    Dup,
    Dup1,
    Swap,
    Nip,
    Insert2,
    Insert3,
    Perm3,
    Perm4,
    Rot3l,
    Rot3r,
    GetLoc(u16),
    PutLoc(u16),
    SetLoc(u16),
    GetLocPair(u16, u16),
    GetLocPutLoc(u16, u16),
    GetArgPutLoc(u16, u16),
    PushI16PutLoc(u16, i16),
    GetArg(u16),
    PutArg(u16),
    SetArg(u16),
    GetVarRef(u16),
    PutVarRef(u16),
    SetVarRef(u16),
    IncLoc(u16),
    DecLoc(u16),
    IncArg(u16),
    DecArg(u16),
    IncVarRef(u16),
    DecVarRef(u16),
    AddLoc(u16),
    AddPutLoc(u16),
    SubPutLoc(u16),
    GetLoc2Mul(u16, u16),
    GetLoc2MulAddLoc { dst: u8, a: u8, b: u8 },
    GetLoc2Add(u16, u16),
    GetLocAddLoc { dst: u16, src: u16 },
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Sar,
    Shr,
    And,
    Or,
    Xor,
    Pow,
    Neg,
    Plus,
    Not,
    Inc,
    Dec,
    PostInc,
    PostDec,
    Lnot,
    Eq,
    Neq,
    StrictEq,
    StrictNeq,
    Lt,
    Lte,
    Gt,
    Gte,
    IsUndefined,
    IsNull,
    IsUndefinedOrNull,
    TypeofIsUndefined,
    TypeofIsFunction,
    Goto(u32),
    IfFalse(u32),
    IfTrue(u32),
    LtIfFalse(u32),
    LtIfTrue(u32),
    LteIfFalse(u32),
    LteIfTrue(u32),
    GtIfFalse(u32),
    GtIfTrue(u32),
    GteIfFalse(u32),
    GteIfTrue(u32),
    EqIfFalse(u32),
    EqIfTrue(u32),
    NeqIfFalse(u32),
    NeqIfTrue(u32),
    StrictEqIfFalse(u32),
    StrictEqIfTrue(u32),
    StrictNeqIfFalse(u32),
    StrictNeqIfTrue(u32),
    IncLocGoto { loc: u16, target: u32 },
    DecLocGoto { loc: u16, target: u32 },
    GetLocLtArgIfFalse { loc: u8, arg: u8, target: u32 },
    GetLocLtLocIfFalse { a: u8, b: u8, target: u32 },
    GetLocLtImmIfFalse { loc: u8, imm: u16, target: u32 },
    GetLocLteArgIfFalse { loc: u8, arg: u8, target: u32 },
    GetLocLteLocIfFalse { a: u8, b: u8, target: u32 },
    GetLocLteImmIfFalse { loc: u8, imm: u16, target: u32 },
    GetField(u16),
    GetField2(u16),
    PutField(u16),
    GetLocGetField { loc: u16, ic: u16 },
    GetLocGetFieldAddLoc { dst: u8, obj: u8, ic: u16 },
    PutLocField { obj: u8, val: u8, ic: u16 },
    GetLocGetVarAddLoc { dst: u8, ic: u16 },
    GetVar(u16),
    GetVarUndef(u16),
    PutVar(u16),
    MakeVarRef(u16),
    GetRefValue(u16),
    PutRefValue(u16),
    GetLength,
    GetArrayEl,
    GetArrayEl2,
    PutArrayEl,
    GetLoc2ArrayEl { arr: u16, idx: u16 },
    GetLoc2ArrayElAddLoc { dst: u8, arr: u8, idx: u8 },
    GetLocImmArrayEl { arr: u8, idx: u16 },
    GetLocImmArrayElAddLoc { dst: u8, arr: u8, idx: u16 },
    PutLocImmArrayEl { arr: u8, idx: u16, val: u8 },
    Delete,
    ForOfStart,
    ForOfNext(u16),
    IteratorClose,
    ForInNext,
    NewObject,
    NewNullProtoObject,
    NewArrayEmpty,
    Call(u16),
    CallMethod(u16),
    CallConstructor(u16),
    GetField2CallMethod0(u16),
    Return,
    ReturnUndef,
    ReturnAsync,
    Slow,
}

const _: () = assert!(size_of::<FastOp>() == 8);

#[derive(Debug)]
pub(crate) struct FastProgram {
    pub ops: TrackedVec<FastOp>,
    /// Byte PC for each `FastOp`, plus `code.len() as u32` as a final sentinel.
    pub orig_pcs: TrackedVec<u32>,
    /// Atom associated with each direct-indexed inline cache slot.
    pub ic_atoms: TrackedVec<UnitAtom>,
    /// Direct-indexed inline cache table (no PC hash collisions).
    pub caches: TrackedVec<Cell<InlineCache>>,
}

#[derive(Clone, Copy)]
struct MidInsn {
    pc: u32,
    op: MidOp,
    target: bool,
}

#[derive(Clone, Copy)]
enum MidOp {
    Inst(Instruction),
    IncLoc(u16),
    DecLoc(u16),
    IncArg(u16),
    DecArg(u16),
    IncVarRef(u16),
    DecVarRef(u16),
    PutLoc(u16),
    PutArg(u16),
    PutVarRef(u16),
}

fn as_get_loc(op: MidOp) -> Option<u16> {
    match op {
        MidOp::Inst(Instruction {
            opcode:
                Opcode::GetLoc
                | Opcode::GetLoc8
                | Opcode::GetLoc0
                | Opcode::GetLoc1
                | Opcode::GetLoc2
                | Opcode::GetLoc3,
            operands: Operands::Local(n),
        }) => Some(n),
        _ => None,
    }
}

fn as_put_loc(op: MidOp) -> Option<u16> {
    match op {
        MidOp::PutLoc(n) => Some(n),
        MidOp::Inst(Instruction {
            opcode:
                Opcode::PutLoc
                | Opcode::PutLoc8
                | Opcode::PutLoc0
                | Opcode::PutLoc1
                | Opcode::PutLoc2
                | Opcode::PutLoc3,
            operands: Operands::Local(n),
        }) => Some(n),
        _ => None,
    }
}

fn as_get_arg(op: MidOp) -> Option<u16> {
    match op {
        MidOp::Inst(Instruction {
            opcode:
                Opcode::GetArg
                | Opcode::GetArg0
                | Opcode::GetArg1
                | Opcode::GetArg2
                | Opcode::GetArg3,
            operands: Operands::Argument(n),
        }) => Some(n),
        _ => None,
    }
}

fn as_push_i32(op: MidOp) -> Option<i32> {
    match op {
        MidOp::Inst(Instruction {
            opcode:
                Opcode::PushI32
                | Opcode::PushI16
                | Opcode::PushI8
                | Opcode::PushMinus1
                | Opcode::Push0
                | Opcode::Push1
                | Opcode::Push2
                | Opcode::Push3
                | Opcode::Push4
                | Opcode::Push5
                | Opcode::Push6
                | Opcode::Push7,
            operands: Operands::Signed(n),
        }) => Some(n),
        _ => None,
    }
}

fn as_plain_const(op: MidOp, regex_constants: &[u32]) -> Option<u32> {
    match op {
        MidOp::Inst(Instruction {
            opcode: Opcode::PushConst | Opcode::PushConst8,
            operands: Operands::Constant(idx),
        }) if regex_constants.is_empty() || regex_constants.binary_search(&idx).is_err() => {
            Some(idx)
        }
        _ => None,
    }
}

fn as_jump(insn: MidInsn) -> Option<(Opcode, u32)> {
    let MidOp::Inst(inst) = insn.op else {
        return None;
    };
    let target = inst.branch_target(insn.pc)? as u32;
    Some((inst.opcode, target))
}

fn as_plain_opcode(op: MidOp) -> Option<Opcode> {
    match op {
        MidOp::Inst(inst) => Some(inst.opcode),
        _ => None,
    }
}

impl FastProgram {
    pub(crate) fn compile(ledger: &HeapLedger, function: &CompiledFunc) -> AllocResult<Self> {
        use Opcode::*;

        let bytes = &function.code;
        let mut raw: Vec<(u32, Instruction)> = Vec::new();
        let mut pc = 0usize;
        while pc < bytes.len() {
            let inst = Instruction::decode(bytes, pc).expect("verified instruction");
            raw.push((pc as u32, inst));
            pc += usize::from(inst.opcode.info().size);
        }

        // Mark all control-flow targets and exception-handler boundaries.
        let mut is_target = vec![false; raw.len()];
        if !is_target.is_empty() {
            is_target[0] = true;
        }
        let mark_pc = |target_pc: u32, is_target: &mut [bool]| {
            if let Ok(idx) = raw.binary_search_by_key(&target_pc, |&(p, _)| p) {
                is_target[idx] = true;
            }
        };
        for (i, &(at, inst)) in raw.iter().enumerate() {
            if let Some(target_pc) = inst.branch_target(at) {
                mark_pc(target_pc as u32, &mut is_target);
            }
            if inst.opcode == Gosub && i + 1 < raw.len() {
                is_target[i + 1] = true;
            }
        }
        for handler in function.handlers.iter() {
            mark_pc(handler.start_pc, &mut is_target);
            mark_pc(handler.end_pc, &mut is_target);
            mark_pc(handler.handler_pc, &mut is_target);
        }

        // Count local references to prove temporary slots from `x++`/`x--` are dead.
        let mut local_refs = vec![0u32; usize::from(function.var_count)];
        for &(_, inst) in &raw {
            match inst.operands {
                Operands::Local(n) => {
                    if let Some(c) = local_refs.get_mut(usize::from(n)) {
                        *c = c.saturating_add(1);
                    }
                    if matches!(inst.opcode, UsingDispose | UsingDisposeAsync)
                        && let Some(c) = local_refs.get_mut(usize::from(n) + 1)
                    {
                        *c = c.saturating_add(1);
                    }
                }
                Operands::Locals(a, b) => {
                    for n in [a, b] {
                        if let Some(c) = local_refs.get_mut(usize::from(n)) {
                            *c = c.saturating_add(1);
                        }
                    }
                }
                Operands::AtomU16(_, n) if inst.opcode == MakeLocRef => {
                    if let Some(c) = local_refs.get_mut(usize::from(n)) {
                        *c = c.saturating_add(1);
                    }
                }
                _ => {}
            }
        }
        let is_dead_temp = |slot: u16| -> bool {
            if function.eval.is_some() {
                return false;
            }
            if local_refs.get(usize::from(slot)).copied() != Some(2) {
                return false;
            }
            let var_idx = usize::from(function.arg_count) + usize::from(slot);
            function
                .vars
                .get(var_idx)
                .is_some_and(|v| v.name == UnitAtom::NONE && !v.captured)
        };

        // Pass 2: normalize Set*+Drop -> Put* and dead-temp PostInc/PostDec -> Inc*/Dec*.
        let mut mid: Vec<MidInsn> = Vec::with_capacity(raw.len());
        let mut i = 0;
        let can_fuse_raw = |start: usize, len: usize| -> bool {
            start + len <= raw.len() && (start + 1..start + len).all(|k| !is_target[k])
        };
        while i < raw.len() {
            let (at, inst) = raw[i];
            let target = is_target[i];

            // Pattern: Get{Loc,Arg,VarRef}(v), PostInc/PostDec, Swap, PutLoc(tmp), Put{Loc,Arg,VarRef}(v), GetLoc(tmp), Drop
            if can_fuse_raw(i, 7)
                && matches!(raw[i + 1].1.opcode, PostInc | PostDec)
                && raw[i + 2].1.opcode == Swap
                && raw[i + 6].1.opcode == Drop
                && let (
                    Instruction {
                        opcode: PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3,
                        operands: Operands::Local(tmp1),
                    },
                    Instruction {
                        opcode: GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3,
                        operands: Operands::Local(tmp2),
                    },
                ) = (raw[i + 3].1, raw[i + 5].1)
                && tmp1 == tmp2
                && is_dead_temp(tmp1)
            {
                let inc = raw[i + 1].1.opcode == PostInc;
                let fused = match (inst, raw[i + 4].1) {
                    (
                        Instruction {
                            opcode: GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3,
                            operands: Operands::Local(v1),
                        },
                        Instruction {
                            opcode: PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3,
                            operands: Operands::Local(v2),
                        },
                    ) if v1 == v2 && v1 != tmp1 => Some(if inc {
                        MidOp::IncLoc(v1)
                    } else {
                        MidOp::DecLoc(v1)
                    }),
                    (
                        Instruction {
                            opcode: GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3,
                            operands: Operands::Argument(a1),
                        },
                        Instruction {
                            opcode: PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3,
                            operands: Operands::Argument(a2),
                        },
                    ) if a1 == a2 => Some(if inc {
                        MidOp::IncArg(a1)
                    } else {
                        MidOp::DecArg(a1)
                    }),
                    (
                        Instruction {
                            opcode:
                                GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3,
                            operands: Operands::VarRef(r1),
                        },
                        Instruction {
                            opcode:
                                PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3,
                            operands: Operands::VarRef(r2),
                        },
                    ) if r1 == r2 => Some(if inc {
                        MidOp::IncVarRef(r1)
                    } else {
                        MidOp::DecVarRef(r1)
                    }),
                    _ => None,
                };
                if let Some(op) = fused {
                    mid.push(MidInsn {
                        pc: at,
                        op,
                        target,
                    });
                    i += 7;
                    continue;
                }
            }

            // Pattern: Get{Loc,Arg,VarRef}(v), Inc/Dec, Set{Loc,Arg,VarRef}(v), Drop
            if can_fuse_raw(i, 4)
                && matches!(raw[i + 1].1.opcode, Inc | Dec)
                && raw[i + 3].1.opcode == Drop
            {
                let inc = raw[i + 1].1.opcode == Inc;
                let fused = match (inst, raw[i + 2].1) {
                    (
                        Instruction {
                            opcode: GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3,
                            operands: Operands::Local(v1),
                        },
                        Instruction {
                            opcode: SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3,
                            operands: Operands::Local(v2),
                        },
                    ) if v1 == v2 => Some(if inc {
                        MidOp::IncLoc(v1)
                    } else {
                        MidOp::DecLoc(v1)
                    }),
                    (
                        Instruction {
                            opcode: GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3,
                            operands: Operands::Argument(a1),
                        },
                        Instruction {
                            opcode: SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3,
                            operands: Operands::Argument(a2),
                        },
                    ) if a1 == a2 => Some(if inc {
                        MidOp::IncArg(a1)
                    } else {
                        MidOp::DecArg(a1)
                    }),
                    (
                        Instruction {
                            opcode:
                                GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3,
                            operands: Operands::VarRef(r1),
                        },
                        Instruction {
                            opcode:
                                SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3,
                            operands: Operands::VarRef(r2),
                        },
                    ) if r1 == r2 => Some(if inc {
                        MidOp::IncVarRef(r1)
                    } else {
                        MidOp::DecVarRef(r1)
                    }),
                    _ => None,
                };
                if let Some(op) = fused {
                    mid.push(MidInsn {
                        pc: at,
                        op,
                        target,
                    });
                    i += 4;
                    continue;
                }
            }

            // Pattern: Insert2, PutField(atom), Drop -> PutField(atom)
            //          Insert3, PutArrayEl, Drop -> PutArrayEl
            //          Dup, PutVar(atom), Drop -> PutVar(atom)
            //          Dup, PutRefValue, Drop -> PutRefValue
            if can_fuse_raw(i, 3) && raw[i + 2].1.opcode == Drop {
                let collapsed = match (inst.opcode, raw[i + 1].1) {
                    (
                        Insert2,
                        put @ Instruction {
                            opcode: PutField,
                            ..
                        },
                    ) => Some(MidOp::Inst(put)),
                    (
                        Insert3,
                        put @ Instruction {
                            opcode: PutArrayEl,
                            ..
                        },
                    ) => Some(MidOp::Inst(put)),
                    (
                        Dup,
                        put @ Instruction {
                            opcode: PutVar | PutRefValue,
                            ..
                        },
                    ) => Some(MidOp::Inst(put)),
                    (
                        Dup,
                        Instruction {
                            opcode: PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3,
                            operands: Operands::Local(n),
                        },
                    ) => Some(MidOp::PutLoc(n)),
                    (
                        Dup,
                        Instruction {
                            opcode: PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3,
                            operands: Operands::Argument(n),
                        },
                    ) => Some(MidOp::PutArg(n)),
                    (
                        Dup,
                        Instruction {
                            opcode: PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3,
                            operands: Operands::VarRef(n),
                        },
                    ) => Some(MidOp::PutVarRef(n)),
                    _ => None,
                };
                if let Some(op) = collapsed {
                    mid.push(MidInsn {
                        pc: at,
                        op,
                        target,
                    });
                    i += 3;
                    continue;
                }
            }

            // Pattern: Set{Loc,Arg,VarRef}(v), Drop -> Put{Loc,Arg,VarRef}(v)
            if can_fuse_raw(i, 2) && raw[i + 1].1.opcode == Drop {
                let collapsed = match inst {
                    Instruction {
                        opcode: SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3,
                        operands: Operands::Local(n),
                    } => Some(MidOp::PutLoc(n)),
                    Instruction {
                        opcode: SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3,
                        operands: Operands::Argument(n),
                    } => Some(MidOp::PutArg(n)),
                    Instruction {
                        opcode: SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3,
                        operands: Operands::VarRef(n),
                    } => Some(MidOp::PutVarRef(n)),
                    _ => None,
                };
                if let Some(op) = collapsed {
                    mid.push(MidInsn {
                        pc: at,
                        op,
                        target,
                    });
                    i += 2;
                    continue;
                }
            }

            mid.push(MidInsn {
                pc: at,
                op: MidOp::Inst(inst),
                target,
            });
            i += 1;
        }

        // Pass 3: superinstruction fusion + direct IC slot assignment.
        let mut ops: Vec<FastOp> = Vec::with_capacity(mid.len());
        let mut orig_pcs: Vec<u32> = Vec::with_capacity(mid.len() + 1);
        let mut ic_atoms: Vec<UnitAtom> = Vec::new();
        let mut jump_fixups: Vec<(usize, u32)> = Vec::new();

        let alloc_ic = |ic_atoms: &mut Vec<UnitAtom>, atom: UnitAtom| -> Option<u16> {
            let slot = u16::try_from(ic_atoms.len()).ok()?;
            ic_atoms.push(atom);
            Some(slot)
        };

        let can_fuse_mid = |start: usize, len: usize| -> bool {
            start + len <= mid.len() && (start + 1..start + len).all(|k| !mid[k].target)
        };

        let mut m = 0;
        while m < mid.len() {
            let cur = mid[m];
            let emit_pc = cur.pc;

            // 1. PushConstRepeat (identical) or PushConstSeq (consecutive indices) for non-regex constants.
            if let Some(idx) = as_plain_const(cur.op, &function.regex_constants) {
                let mut rep = 1usize;
                while m + rep < mid.len()
                    && rep < usize::from(u16::MAX)
                    && !mid[m + rep].target
                    && as_plain_const(mid[m + rep].op, &function.regex_constants) == Some(idx)
                {
                    rep += 1;
                }
                if rep >= 2 {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::PushConstRepeat {
                        index: idx,
                        count: rep as u16,
                    });
                    m += rep;
                    continue;
                }
                let mut seq = 1usize;
                while m + seq < mid.len()
                    && seq < usize::from(u16::MAX)
                    && !mid[m + seq].target
                    && as_plain_const(mid[m + seq].op, &function.regex_constants)
                        == Some(idx + seq as u32)
                {
                    seq += 1;
                }
                if seq >= 2 {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::PushConstSeq {
                        start: idx,
                        count: seq as u16,
                    });
                    m += seq;
                    continue;
                }
            }

            // 2. Object, Null, SetProto -> NewNullProtoObject
            if can_fuse_mid(m, 3)
                && as_plain_opcode(cur.op) == Some(Object)
                && as_plain_opcode(mid[m + 1].op) == Some(Null)
                && as_plain_opcode(mid[m + 2].op) == Some(SetProto)
            {
                orig_pcs.push(emit_pc);
                ops.push(FastOp::NewNullProtoObject);
                m += 3;
                continue;
            }

            // 2b. 6-op compound-add fusions:
            //     GetLoc(dst), GetLoc(arr), PushI32(imm), GetArrayEl, Add, PutLoc(dst)
            //     GetLoc(dst), GetLoc(arr), GetLoc(idx), GetArrayEl, Add, PutLoc(dst)
            //     GetLoc(dst), GetLoc(a), GetLoc(b), Mul, Add, PutLoc(dst)
            if can_fuse_mid(m, 6)
                && let (Some(dst1), Some(a), Some(dst2)) = (
                    as_get_loc(cur.op),
                    as_get_loc(mid[m + 1].op),
                    as_put_loc(mid[m + 5].op),
                )
                && dst1 == dst2
                && as_plain_opcode(mid[m + 4].op) == Some(Add)
                && let (Ok(dst_u8), Ok(a_u8)) = (u8::try_from(dst1), u8::try_from(a))
            {
                let fused = match as_plain_opcode(mid[m + 3].op) {
                    Some(GetArrayEl) => {
                        if let Some(imm) = as_push_i32(mid[m + 2].op)
                            && let Ok(idx_u16) = u16::try_from(imm)
                        {
                            Some(FastOp::GetLocImmArrayElAddLoc {
                                dst: dst_u8,
                                arr: a_u8,
                                idx: idx_u16,
                            })
                        } else if let Some(idx) = as_get_loc(mid[m + 2].op)
                            && let Ok(idx_u8) = u8::try_from(idx)
                        {
                            Some(FastOp::GetLoc2ArrayElAddLoc {
                                dst: dst_u8,
                                arr: a_u8,
                                idx: idx_u8,
                            })
                        } else {
                            None
                        }
                    }
                    Some(Mul) => as_get_loc(mid[m + 2].op)
                        .and_then(|b| u8::try_from(b).ok())
                        .map(|b_u8| FastOp::GetLoc2MulAddLoc {
                            dst: dst_u8,
                            a: a_u8,
                            b: b_u8,
                        }),
                    _ => None,
                };
                if let Some(op) = fused {
                    orig_pcs.push(emit_pc);
                    ops.push(op);
                    m += 6;
                    continue;
                }
            }

            // 3. GetLoc(dst), GetLoc(obj), GetField(atom), Add, PutLoc(dst) -> GetLocGetFieldAddLoc
            if can_fuse_mid(m, 5)
                && let (Some(dst1), Some(obj), Some(dst2)) = (
                    as_get_loc(cur.op),
                    as_get_loc(mid[m + 1].op),
                    as_put_loc(mid[m + 4].op),
                )
                && dst1 == dst2
                && let (Ok(dst_u8), Ok(obj_u8)) = (u8::try_from(dst1), u8::try_from(obj))
                && as_plain_opcode(mid[m + 3].op) == Some(Add)
                && let MidOp::Inst(Instruction {
                    opcode: GetField,
                    operands: Operands::Atom(atom),
                }) = mid[m + 2].op
                && let Some(ic) = alloc_ic(&mut ic_atoms, atom)
            {
                orig_pcs.push(emit_pc);
                ops.push(FastOp::GetLocGetFieldAddLoc {
                    dst: dst_u8,
                    obj: obj_u8,
                    ic,
                });
                m += 5;
                continue;
            }

            // 4. Loop condition fusion:
            //    GetLoc(loc), GetArg(arg) / GetLoc(b) / PushI32(imm), Lt/Lte, IfFalse(target)
            if can_fuse_mid(m, 4)
                && let Some(loc) = as_get_loc(cur.op)
                && let Ok(loc_u8) = u8::try_from(loc)
                && let Some(cmp_op @ (Lt | Lte)) = as_plain_opcode(mid[m + 2].op)
                && let Some((IfFalse | IfFalse8, target_pc)) = as_jump(mid[m + 3])
            {
                let fused = if let Some(arg) = as_get_arg(mid[m + 1].op)
                    && let Ok(arg_u8) = u8::try_from(arg)
                {
                    Some(if cmp_op == Lt {
                        FastOp::GetLocLtArgIfFalse {
                            loc: loc_u8,
                            arg: arg_u8,
                            target: 0,
                        }
                    } else {
                        FastOp::GetLocLteArgIfFalse {
                            loc: loc_u8,
                            arg: arg_u8,
                            target: 0,
                        }
                    })
                } else if let Some(b) = as_get_loc(mid[m + 1].op)
                    && let Ok(b_u8) = u8::try_from(b)
                {
                    Some(if cmp_op == Lt {
                        FastOp::GetLocLtLocIfFalse {
                            a: loc_u8,
                            b: b_u8,
                            target: 0,
                        }
                    } else {
                        FastOp::GetLocLteLocIfFalse {
                            a: loc_u8,
                            b: b_u8,
                            target: 0,
                        }
                    })
                } else if let Some(imm) = as_push_i32(mid[m + 1].op)
                    && let Ok(imm_u16) = u16::try_from(imm)
                {
                    Some(if cmp_op == Lt {
                        FastOp::GetLocLtImmIfFalse {
                            loc: loc_u8,
                            imm: imm_u16,
                            target: 0,
                        }
                    } else {
                        FastOp::GetLocLteImmIfFalse {
                            loc: loc_u8,
                            imm: imm_u16,
                            target: 0,
                        }
                    })
                } else {
                    None
                };
                if let Some(op) = fused {
                    let ip = ops.len();
                    orig_pcs.push(emit_pc);
                    ops.push(op);
                    jump_fixups.push((ip, target_pc));
                    m += 4;
                    continue;
                }
            }

            // 5. GetLoc(dst), GetLoc(src), Add, PutLoc(dst) -> GetLocAddLoc
            //    GetLoc(dst), GetVar(atom), Add, PutLoc(dst) -> GetLocGetVarAddLoc
            if can_fuse_mid(m, 4)
                && let (Some(dst1), Some(dst2)) = (as_get_loc(cur.op), as_put_loc(mid[m + 3].op))
                && dst1 == dst2
                && as_plain_opcode(mid[m + 2].op) == Some(Add)
            {
                if let Some(src) = as_get_loc(mid[m + 1].op) {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::GetLocAddLoc { dst: dst1, src });
                    m += 4;
                    continue;
                }
                if function.eval.is_none()
                    && let Ok(dst_u8) = u8::try_from(dst1)
                    && let MidOp::Inst(Instruction {
                        opcode: GetVar,
                        operands: Operands::Atom(atom),
                    }) = mid[m + 1].op
                    && let Some(ic) = alloc_ic(&mut ic_atoms, atom)
                {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::GetLocGetVarAddLoc { dst: dst_u8, ic });
                    m += 4;
                    continue;
                }
            }

            // 5b. GetLoc(arr), PushI32(imm), GetLoc(val), PutArrayEl -> PutLocImmArrayEl
            if can_fuse_mid(m, 4)
                && let (Some(arr), Some(imm), Some(val)) = (
                    as_get_loc(cur.op),
                    as_push_i32(mid[m + 1].op),
                    as_get_loc(mid[m + 2].op),
                )
                && as_plain_opcode(mid[m + 3].op) == Some(PutArrayEl)
                && let (Ok(arr_u8), Ok(idx_u16), Ok(val_u8)) =
                    (u8::try_from(arr), u16::try_from(imm), u8::try_from(val))
            {
                orig_pcs.push(emit_pc);
                ops.push(FastOp::PutLocImmArrayEl {
                    arr: arr_u8,
                    idx: idx_u16,
                    val: val_u8,
                });
                m += 4;
                continue;
            }

            // 6. GetLoc(obj), GetLoc(val), PutField(atom) -> PutLocField
            if can_fuse_mid(m, 3)
                && let (Some(obj), Some(val)) = (as_get_loc(cur.op), as_get_loc(mid[m + 1].op))
                && let (Ok(obj_u8), Ok(val_u8)) = (u8::try_from(obj), u8::try_from(val))
                && let MidOp::Inst(Instruction {
                    opcode: PutField,
                    operands: Operands::Atom(atom),
                }) = mid[m + 2].op
                && let Some(ic) = alloc_ic(&mut ic_atoms, atom)
            {
                orig_pcs.push(emit_pc);
                ops.push(FastOp::PutLocField {
                    obj: obj_u8,
                    val: val_u8,
                    ic,
                });
                m += 3;
                continue;
            }

            // 6b. GetLoc(arr), PushI32(imm), GetArrayEl -> GetLocImmArrayEl
            if can_fuse_mid(m, 3)
                && let (Some(arr), Some(imm)) = (as_get_loc(cur.op), as_push_i32(mid[m + 1].op))
                && as_plain_opcode(mid[m + 2].op) == Some(GetArrayEl)
                && let (Ok(arr_u8), Ok(idx_u16)) = (u8::try_from(arr), u16::try_from(imm))
            {
                orig_pcs.push(emit_pc);
                ops.push(FastOp::GetLocImmArrayEl {
                    arr: arr_u8,
                    idx: idx_u16,
                });
                m += 3;
                continue;
            }

            // 6c. GetLoc(a), GetLoc(b), Mul / Add / GetArrayEl
            if can_fuse_mid(m, 3)
                && let (Some(a), Some(b)) = (as_get_loc(cur.op), as_get_loc(mid[m + 1].op))
            {
                let fused = match as_plain_opcode(mid[m + 2].op) {
                    Some(Mul) => Some(FastOp::GetLoc2Mul(a, b)),
                    Some(Add) => Some(FastOp::GetLoc2Add(a, b)),
                    Some(GetArrayEl) => Some(FastOp::GetLoc2ArrayEl { arr: a, idx: b }),
                    _ => None,
                };
                if let Some(op) = fused {
                    orig_pcs.push(emit_pc);
                    ops.push(op);
                    m += 3;
                    continue;
                }
            }

            // 7. IncLoc/DecLoc + Goto -> IncLocGoto/DecLocGoto
            if can_fuse_mid(m, 2)
                && let Some((Goto | Goto8 | Goto16, target_pc)) = as_jump(mid[m + 1])
            {
                let fused = match cur.op {
                    MidOp::IncLoc(loc) => Some(FastOp::IncLocGoto { loc, target: 0 }),
                    MidOp::DecLoc(loc) => Some(FastOp::DecLocGoto { loc, target: 0 }),
                    MidOp::Inst(Instruction {
                        opcode: IncLoc,
                        operands: Operands::Local(loc),
                    }) => Some(FastOp::IncLocGoto { loc, target: 0 }),
                    MidOp::Inst(Instruction {
                        opcode: DecLoc,
                        operands: Operands::Local(loc),
                    }) => Some(FastOp::DecLocGoto { loc, target: 0 }),
                    _ => None,
                };
                if let Some(op) = fused {
                    let ip = ops.len();
                    orig_pcs.push(emit_pc);
                    ops.push(op);
                    jump_fixups.push((ip, target_pc));
                    m += 2;
                    continue;
                }
            }

            // 8. Compare + IfFalse/IfTrue
            if can_fuse_mid(m, 2)
                && let Some((br @ (IfFalse | IfFalse8 | IfTrue | IfTrue8), target_pc)) =
                    as_jump(mid[m + 1])
            {
                let is_true = matches!(br, IfTrue | IfTrue8);
                let fused = match (as_plain_opcode(cur.op), is_true) {
                    (Some(Lt), false) => Some(FastOp::LtIfFalse(0)),
                    (Some(Lt), true) => Some(FastOp::LtIfTrue(0)),
                    (Some(Lte), false) => Some(FastOp::LteIfFalse(0)),
                    (Some(Lte), true) => Some(FastOp::LteIfTrue(0)),
                    (Some(Gt), false) => Some(FastOp::GtIfFalse(0)),
                    (Some(Gt), true) => Some(FastOp::GtIfTrue(0)),
                    (Some(Gte), false) => Some(FastOp::GteIfFalse(0)),
                    (Some(Gte), true) => Some(FastOp::GteIfTrue(0)),
                    (Some(Eq), false) => Some(FastOp::EqIfFalse(0)),
                    (Some(Eq), true) => Some(FastOp::EqIfTrue(0)),
                    (Some(Neq), false) => Some(FastOp::NeqIfFalse(0)),
                    (Some(Neq), true) => Some(FastOp::NeqIfTrue(0)),
                    (Some(StrictEq), false) => Some(FastOp::StrictEqIfFalse(0)),
                    (Some(StrictEq), true) => Some(FastOp::StrictEqIfTrue(0)),
                    (Some(StrictNeq), false) => Some(FastOp::StrictNeqIfFalse(0)),
                    (Some(StrictNeq), true) => Some(FastOp::StrictNeqIfTrue(0)),
                    _ => None,
                };
                if let Some(op) = fused {
                    let ip = ops.len();
                    orig_pcs.push(emit_pc);
                    ops.push(op);
                    jump_fixups.push((ip, target_pc));
                    m += 2;
                    continue;
                }
            }

            // 9. GetField2(atom), CallMethod(0) -> GetField2CallMethod0
            if can_fuse_mid(m, 2)
                && let (
                    MidOp::Inst(Instruction {
                        opcode: GetField2,
                        operands: Operands::Atom(atom),
                    }),
                    MidOp::Inst(Instruction {
                        opcode: CallMethod,
                        operands: Operands::Call(0),
                    }),
                ) = (cur.op, mid[m + 1].op)
                && let Some(ic) = alloc_ic(&mut ic_atoms, atom)
            {
                orig_pcs.push(emit_pc);
                ops.push(FastOp::GetField2CallMethod0(ic));
                m += 2;
                continue;
            }

            // 10. 2-op local/arithmetic/field fusions
            if can_fuse_mid(m, 2) {
                if let (Some(loc), MidOp::Inst(Instruction {
                    opcode: GetField,
                    operands: Operands::Atom(atom),
                })) = (as_get_loc(cur.op), mid[m + 1].op)
                    && let Some(ic) = alloc_ic(&mut ic_atoms, atom)
                {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::GetLocGetField { loc, ic });
                    m += 2;
                    continue;
                }
                if let (Some(a), Some(b)) = (as_get_loc(cur.op), as_get_loc(mid[m + 1].op)) {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::GetLocPair(a, b));
                    m += 2;
                    continue;
                }
                if let (Some(src), Some(dst)) = (as_get_loc(cur.op), as_put_loc(mid[m + 1].op)) {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::GetLocPutLoc(src, dst));
                    m += 2;
                    continue;
                }
                if let (Some(src), Some(dst)) = (as_get_arg(cur.op), as_put_loc(mid[m + 1].op)) {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::GetArgPutLoc(src, dst));
                    m += 2;
                    continue;
                }
                if let (Some(imm), Some(dst)) = (as_push_i32(cur.op), as_put_loc(mid[m + 1].op))
                    && let Ok(imm16) = i16::try_from(imm)
                {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::PushI16PutLoc(dst, imm16));
                    m += 2;
                    continue;
                }
                if let (Some(Add), Some(dst)) =
                    (as_plain_opcode(cur.op), as_put_loc(mid[m + 1].op))
                {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::AddPutLoc(dst));
                    m += 2;
                    continue;
                }
                if let (Some(Sub), Some(dst)) =
                    (as_plain_opcode(cur.op), as_put_loc(mid[m + 1].op))
                {
                    orig_pcs.push(emit_pc);
                    ops.push(FastOp::SubPutLoc(dst));
                    m += 2;
                    continue;
                }
            }

            // Single instruction translation
            orig_pcs.push(emit_pc);
            let fast_op = match cur.op {
                MidOp::IncLoc(n) => FastOp::IncLoc(n),
                MidOp::DecLoc(n) => FastOp::DecLoc(n),
                MidOp::IncArg(n) => FastOp::IncArg(n),
                MidOp::DecArg(n) => FastOp::DecArg(n),
                MidOp::IncVarRef(n) => FastOp::IncVarRef(n),
                MidOp::DecVarRef(n) => FastOp::DecVarRef(n),
                MidOp::PutLoc(n) => FastOp::PutLoc(n),
                MidOp::PutArg(n) => FastOp::PutArg(n),
                MidOp::PutVarRef(n) => FastOp::PutVarRef(n),
                MidOp::Inst(inst) => match (inst.opcode, inst.operands) {
                    (Nop, _) => FastOp::Nop,
                    (
                        PushI32 | PushI16 | PushI8 | PushMinus1 | Push0 | Push1 | Push2 | Push3
                        | Push4 | Push5 | Push6 | Push7,
                        Operands::Signed(n),
                    ) => FastOp::PushI32(n),
                    (PushBigintI32, Operands::Signed(n)) => FastOp::PushBigintI32(n),
                    (Undefined, _) => FastOp::Undefined,
                    (Null, _) => FastOp::Null,
                    (PushFalse, _) => FastOp::PushFalse,
                    (PushTrue, _) => FastOp::PushTrue,
                    (PushEmptyString, _) => FastOp::PushEmptyString,
                    (PushThis, _) => FastOp::PushThis,
                    (SpecialObject, Operands::Unsigned(sel @ (2 | 3)))
                        if function.eval.is_none() =>
                    {
                        FastOp::SpecialObject(sel as u8)
                    }
                    (PushConst | PushConst8, Operands::Constant(idx)) => {
                        if !function.regex_constants.is_empty()
                            && function.regex_constants.binary_search(&idx).is_ok()
                        {
                            FastOp::PushRegExpConst(idx)
                        } else {
                            FastOp::PushConst(idx)
                        }
                    }
                    (Drop, _) => FastOp::Drop,
                    (Dup, _) => FastOp::Dup,
                    (Dup1, _) => FastOp::Dup1,
                    (Swap, _) => FastOp::Swap,
                    (Nip, _) => FastOp::Nip,
                    (Insert2, _) => FastOp::Insert2,
                    (Insert3, _) => FastOp::Insert3,
                    (Perm3, _) => FastOp::Perm3,
                    (Perm4, _) => FastOp::Perm4,
                    (Rot3l, _) => FastOp::Rot3l,
                    (Rot3r, _) => FastOp::Rot3r,
                    (GetLoc0Loc1, _) => FastOp::GetLocPair(0, 1),
                    (
                        GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3,
                        Operands::Local(n),
                    ) => FastOp::GetLoc(n),
                    (
                        PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3,
                        Operands::Local(n),
                    ) => FastOp::PutLoc(n),
                    (
                        SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3,
                        Operands::Local(n),
                    ) => FastOp::SetLoc(n),
                    (GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3, Operands::Argument(n)) => {
                        FastOp::GetArg(n)
                    }
                    (PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3, Operands::Argument(n)) => {
                        FastOp::PutArg(n)
                    }
                    (SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3, Operands::Argument(n)) => {
                        FastOp::SetArg(n)
                    }
                    (
                        GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3,
                        Operands::VarRef(n),
                    ) => FastOp::GetVarRef(n),
                    (
                        PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3,
                        Operands::VarRef(n),
                    ) => FastOp::PutVarRef(n),
                    (
                        SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3,
                        Operands::VarRef(n),
                    ) => FastOp::SetVarRef(n),
                    (IncLoc, Operands::Local(n)) => FastOp::IncLoc(n),
                    (DecLoc, Operands::Local(n)) => FastOp::DecLoc(n),
                    (AddLoc, Operands::Local(n)) => FastOp::AddLoc(n),
                    (Add, _) => FastOp::Add,
                    (Sub, _) => FastOp::Sub,
                    (Mul, _) => FastOp::Mul,
                    (Div, _) => FastOp::Div,
                    (Mod, _) => FastOp::Mod,
                    (Shl, _) => FastOp::Shl,
                    (Sar, _) => FastOp::Sar,
                    (Shr, _) => FastOp::Shr,
                    (And, _) => FastOp::And,
                    (Or, _) => FastOp::Or,
                    (Xor, _) => FastOp::Xor,
                    (Pow, _) => FastOp::Pow,
                    (Neg, _) => FastOp::Neg,
                    (Plus, _) => FastOp::Plus,
                    (Not, _) => FastOp::Not,
                    (Inc, _) => FastOp::Inc,
                    (Dec, _) => FastOp::Dec,
                    (PostInc, _) => FastOp::PostInc,
                    (PostDec, _) => FastOp::PostDec,
                    (Lnot, _) => FastOp::Lnot,
                    (Eq, _) => FastOp::Eq,
                    (Neq, _) => FastOp::Neq,
                    (StrictEq, _) => FastOp::StrictEq,
                    (StrictNeq, _) => FastOp::StrictNeq,
                    (Lt, _) => FastOp::Lt,
                    (Lte, _) => FastOp::Lte,
                    (Gt, _) => FastOp::Gt,
                    (Gte, _) => FastOp::Gte,
                    (IsUndefined, _) => FastOp::IsUndefined,
                    (IsNull, _) => FastOp::IsNull,
                    (IsUndefinedOrNull, _) => FastOp::IsUndefinedOrNull,
                    (TypeofIsUndefined, _) => FastOp::TypeofIsUndefined,
                    (TypeofIsFunction, _) => FastOp::TypeofIsFunction,
                    (Goto | Goto8 | Goto16, _) => {
                        let target_pc = inst.branch_target(emit_pc).unwrap() as u32;
                        jump_fixups.push((ops.len(), target_pc));
                        FastOp::Goto(0)
                    }
                    (IfFalse | IfFalse8, _) => {
                        let target_pc = inst.branch_target(emit_pc).unwrap() as u32;
                        jump_fixups.push((ops.len(), target_pc));
                        FastOp::IfFalse(0)
                    }
                    (IfTrue | IfTrue8, _) => {
                        let target_pc = inst.branch_target(emit_pc).unwrap() as u32;
                        jump_fixups.push((ops.len(), target_pc));
                        FastOp::IfTrue(0)
                    }
                    (GetField, Operands::Atom(atom)) => {
                        if atom.raw() == BuiltinAtom::Length as u32 {
                            // Still allocate IC so plain objects with own `.length` also cache cleanly.
                            alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::GetField)
                        } else {
                            alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::GetField)
                        }
                    }
                    (GetField2, Operands::Atom(atom)) => {
                        alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::GetField2)
                    }
                    (PutField, Operands::Atom(atom)) => {
                        alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::PutField)
                    }
                    (GetVar, Operands::Atom(atom)) if function.eval.is_none() => {
                        alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::GetVar)
                    }
                    (GetVarUndef, Operands::Atom(atom)) if function.eval.is_none() => {
                        alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::GetVarUndef)
                    }
                    (PutVar, Operands::Atom(atom)) if function.eval.is_none() => {
                        alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::PutVar)
                    }
                    (MakeVarRef, Operands::Atom(atom)) if function.eval.is_none() => {
                        alloc_ic(&mut ic_atoms, atom).map_or(FastOp::Slow, FastOp::MakeVarRef)
                    }
                    (GetRefValue, _) => alloc_ic(&mut ic_atoms, UnitAtom::NONE)
                        .map_or(FastOp::Slow, FastOp::GetRefValue),
                    (PutRefValue, _) => alloc_ic(&mut ic_atoms, UnitAtom::NONE)
                        .map_or(FastOp::Slow, FastOp::PutRefValue),
                    (GetLength, _) => FastOp::GetLength,
                    (GetArrayEl, _) => FastOp::GetArrayEl,
                    (GetArrayEl2, _) => FastOp::GetArrayEl2,
                    (PutArrayEl, _) => FastOp::PutArrayEl,
                    (Delete, _) => FastOp::Delete,
                    (ForOfStart, _) => FastOp::ForOfStart,
                    (ForOfNext, Operands::Unsigned(skip)) => FastOp::ForOfNext(skip as u16),
                    (IteratorClose, _) => FastOp::IteratorClose,
                    (ForInNext, _) => FastOp::ForInNext,
                    (Object, _) => FastOp::NewObject,
                    (ArrayFrom, Operands::Call(0)) => FastOp::NewArrayEmpty,
                    (Call | Call0 | Call1 | Call2 | Call3, Operands::Call(n)) => FastOp::Call(n),
                    (CallMethod, Operands::Call(n)) => FastOp::CallMethod(n),
                    (CallConstructor, Operands::Call(n)) => FastOp::CallConstructor(n),
                    (Return, _) => FastOp::Return,
                    (ReturnUndef, _) => FastOp::ReturnUndef,
                    (ReturnAsync, _) => FastOp::ReturnAsync,
                    _ => FastOp::Slow,
                },
            };
            ops.push(fast_op);
            m += 1;
        }
        orig_pcs.push(bytes.len() as u32);

        // Pass 4: patch all branch targets from byte PC -> FastOp instruction index.
        for (ip, target_pc) in jump_fixups {
            let target_ip = orig_pcs
                .binary_search(&target_pc)
                .expect("jump target must be a FastOp boundary") as u32;
            match &mut ops[ip] {
                FastOp::Goto(t)
                | FastOp::IfFalse(t)
                | FastOp::IfTrue(t)
                | FastOp::LtIfFalse(t)
                | FastOp::LtIfTrue(t)
                | FastOp::LteIfFalse(t)
                | FastOp::LteIfTrue(t)
                | FastOp::GtIfFalse(t)
                | FastOp::GtIfTrue(t)
                | FastOp::GteIfFalse(t)
                | FastOp::GteIfTrue(t)
                | FastOp::EqIfFalse(t)
                | FastOp::EqIfTrue(t)
                | FastOp::NeqIfFalse(t)
                | FastOp::NeqIfTrue(t)
                | FastOp::StrictEqIfFalse(t)
                | FastOp::StrictEqIfTrue(t)
                | FastOp::StrictNeqIfFalse(t)
                | FastOp::StrictNeqIfTrue(t)
                | FastOp::IncLocGoto { target: t, .. }
                | FastOp::DecLocGoto { target: t, .. }
                | FastOp::GetLocLtArgIfFalse { target: t, .. }
                | FastOp::GetLocLtLocIfFalse { target: t, .. }
                | FastOp::GetLocLtImmIfFalse { target: t, .. }
                | FastOp::GetLocLteArgIfFalse { target: t, .. }
                | FastOp::GetLocLteLocIfFalse { target: t, .. }
                | FastOp::GetLocLteImmIfFalse { target: t, .. } => *t = target_ip,
                _ => unreachable!(),
            }
        }

        // Charge and move into TrackedVecs.
        let tracked_ops = TrackedVec::from_slice(ledger, &ops)?;
        let tracked_pcs = TrackedVec::from_slice(ledger, &orig_pcs)?;
        let tracked_atoms = TrackedVec::from_slice(ledger, &ic_atoms)?;
        let mut caches = TrackedVec::new();
        if !ic_atoms.is_empty() {
            caches.try_reserve_exact(ledger, ic_atoms.len())?;
            for _ in 0..ic_atoms.len() {
                caches.push_reserved(Cell::new(InlineCache::Empty));
            }
        }

        Ok(Self {
            ops: tracked_ops,
            orig_pcs: tracked_pcs,
            ic_atoms: tracked_atoms,
            caches,
        })
    }
}
