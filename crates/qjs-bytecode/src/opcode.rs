//! Final VM instructions and their little-endian operand layouts.
//!
//! Several language features share instructions. `special_object` loads
//! `import.meta` with selector 6. `import` performs dynamic import. `apply`
//! and `apply_eval` consume argument arrays, including spread calls. Class
//! field initializers and static blocks are closures created by ordinary
//! function and class instructions. Temporary scope and label operations
//! are represented by [`crate::IrOp`] instead of opcode bytes.

use crate::UnitAtom;
use std::fmt;

/// Layout after the opcode byte. `Label` fields are signed offsets from the
/// start of the offset field, not from the opcode or the next instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandFormat {
    None,
    NoneInt,
    NoneLoc,
    NoneArg,
    NoneVarRef,
    U8,
    I8,
    Loc8,
    Const8,
    Label8,
    U16,
    I16,
    Label16,
    Npop,
    Npopx,
    NpopU16,
    Loc,
    Arg,
    VarRef,
    U32,
    U32x2,
    I32,
    Const,
    Label,
    Atom,
    AtomU8,
    AtomU16,
    AtomLabelU8,
    AtomLabelU16,
    LabelU16,
}

impl OperandFormat {
    pub const fn byte_len(self) -> u8 {
        use OperandFormat::*;
        match self {
            None | NoneInt | NoneLoc | NoneArg | NoneVarRef | Npopx => 0,
            U8 | I8 | Loc8 | Const8 | Label8 => 1,
            U16 | I16 | Label16 | Npop | Loc | Arg | VarRef => 2,
            NpopU16 | U32 | I32 | Const | Label | Atom => 4,
            AtomU8 => 5,
            AtomU16 | LabelU16 => 6,
            U32x2 => 8,
            AtomLabelU8 => 9,
            AtomLabelU16 => 10,
        }
    }
}

/// The upstream stack signature counts fixed inputs and outputs. Calls add
/// their argument count to `n_pop`. Branches such as `gosub` and `with_get_var`
/// have a different stack effect on the taken edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpcodeInfo {
    pub name: &'static str,
    /// Encoded bytes, including the opcode itself.
    pub size: u8,
    pub n_pop: u8,
    pub n_push: u8,
    pub operand_fmt: OperandFormat,
}

macro_rules! opcodes {
    ($($op:ident, $name:literal, $size:literal, $pop:literal, $push:literal, $fmt:ident;)+) => {
        /// QuickJS-ng v0.17.0 opcode bytes, including short forms and the
        /// reserved zero sentinel. Unresolved compiler operations use [`crate::IrOp`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum Opcode {
            $(#[doc = concat!("`", $name, "`")] $op,)+
        }

        /// Opcode metadata in byte order, including the reserved zero slot.
        pub const OPCODE_INFO: &[OpcodeInfo] = &[
            $(OpcodeInfo {
                name: $name, size: $size, n_pop: $pop, n_push: $push,
                operand_fmt: OperandFormat::$fmt,
            },)+
        ];

        impl Opcode {
            /// All table slots, including the reserved zero slot.
            pub const ALL: &'static [Self] = &[$(Self::$op,)+];
        }
    };
}

// From quickjs-opcode.h. The reference and its license are in tests/reference.
opcodes! {
    Invalid, "invalid", 1, 0, 0, None;
    PushI32, "push_i32", 5, 0, 1, I32;
    PushConst, "push_const", 5, 0, 1, Const;
    Fclosure, "fclosure", 5, 0, 1, Const;
    PushAtomValue, "push_atom_value", 5, 0, 1, Atom;
    PrivateSymbol, "private_symbol", 5, 0, 1, Atom;
    Undefined, "undefined", 1, 0, 1, None;
    Null, "null", 1, 0, 1, None;
    PushThis, "push_this", 1, 0, 1, None;
    PushFalse, "push_false", 1, 0, 1, None;
    PushTrue, "push_true", 1, 0, 1, None;
    Object, "object", 1, 0, 1, None;
    SpecialObject, "special_object", 2, 0, 1, U8;
    Rest, "rest", 3, 0, 1, U16;
    Drop, "drop", 1, 1, 0, None;
    Nip, "nip", 1, 2, 1, None;
    Nip1, "nip1", 1, 3, 2, None;
    Dup, "dup", 1, 1, 2, None;
    Dup1, "dup1", 1, 2, 3, None;
    Dup2, "dup2", 1, 2, 4, None;
    Dup3, "dup3", 1, 3, 6, None;
    Insert2, "insert2", 1, 2, 3, None;
    Insert3, "insert3", 1, 3, 4, None;
    Insert4, "insert4", 1, 4, 5, None;
    Perm3, "perm3", 1, 3, 3, None;
    Perm4, "perm4", 1, 4, 4, None;
    Perm5, "perm5", 1, 5, 5, None;
    Swap, "swap", 1, 2, 2, None;
    Swap2, "swap2", 1, 4, 4, None;
    Rot3l, "rot3l", 1, 3, 3, None;
    Rot3r, "rot3r", 1, 3, 3, None;
    Rot4l, "rot4l", 1, 4, 4, None;
    Rot5l, "rot5l", 1, 5, 5, None;
    CallConstructor, "call_constructor", 3, 2, 1, Npop;
    Call, "call", 3, 1, 1, Npop;
    TailCall, "tail_call", 3, 1, 0, Npop;
    CallMethod, "call_method", 3, 2, 1, Npop;
    TailCallMethod, "tail_call_method", 3, 2, 0, Npop;
    ArrayFrom, "array_from", 3, 0, 1, Npop;
    Apply, "apply", 3, 3, 1, U16;
    Return, "return", 1, 1, 0, None;
    ReturnUndef, "return_undef", 1, 0, 0, None;
    CheckCtorReturn, "check_ctor_return", 1, 1, 2, None;
    CheckCtor, "check_ctor", 1, 0, 0, None;
    InitCtor, "init_ctor", 1, 0, 1, None;
    CheckBrand, "check_brand", 1, 2, 2, None;
    AddBrand, "add_brand", 1, 2, 0, None;
    ReturnAsync, "return_async", 1, 1, 0, None;
    Throw, "throw", 1, 1, 0, None;
    ThrowError, "throw_error", 6, 0, 0, AtomU8;
    Eval, "eval", 5, 1, 1, NpopU16;
    ApplyEval, "apply_eval", 3, 2, 1, U16;
    Regexp, "regexp", 1, 2, 1, None;
    GetSuper, "get_super", 1, 1, 1, None;
    Import, "import", 1, 2, 1, None;
    GetVarUndef, "get_var_undef", 5, 0, 1, Atom;
    GetVar, "get_var", 5, 0, 1, Atom;
    PutVar, "put_var", 5, 1, 0, Atom;
    PutVarInit, "put_var_init", 5, 1, 0, Atom;
    GetRefValue, "get_ref_value", 1, 2, 3, None;
    PutRefValue, "put_ref_value", 1, 3, 0, None;
    DefineVar, "define_var", 6, 0, 0, AtomU8;
    CheckDefineVar, "check_define_var", 6, 0, 0, AtomU8;
    DefineFunc, "define_func", 6, 1, 0, AtomU8;
    GetField, "get_field", 5, 1, 1, Atom;
    GetField2, "get_field2", 5, 1, 2, Atom;
    PutField, "put_field", 5, 2, 0, Atom;
    GetPrivateField, "get_private_field", 1, 2, 1, None;
    PutPrivateField, "put_private_field", 1, 3, 0, None;
    DefinePrivateField, "define_private_field", 1, 3, 1, None;
    GetArrayEl, "get_array_el", 1, 2, 1, None;
    GetArrayEl2, "get_array_el2", 1, 2, 2, None;
    PutArrayEl, "put_array_el", 1, 3, 0, None;
    GetSuperValue, "get_super_value", 1, 3, 1, None;
    PutSuperValue, "put_super_value", 1, 4, 0, None;
    DefineField, "define_field", 5, 2, 1, Atom;
    SetName, "set_name", 5, 1, 1, Atom;
    SetNameComputed, "set_name_computed", 1, 2, 2, None;
    SetProto, "set_proto", 1, 2, 1, None;
    SetHomeObject, "set_home_object", 1, 2, 2, None;
    DefineArrayEl, "define_array_el", 1, 3, 2, None;
    Append, "append", 1, 3, 2, None;
    CopyDataProperties, "copy_data_properties", 2, 3, 3, U8;
    DefineMethod, "define_method", 6, 2, 1, AtomU8;
    DefineMethodComputed, "define_method_computed", 2, 3, 1, U8;
    DefineClass, "define_class", 6, 2, 2, AtomU8;
    DefineClassComputed, "define_class_computed", 6, 3, 3, AtomU8;
    GetLoc, "get_loc", 3, 0, 1, Loc;
    PutLoc, "put_loc", 3, 1, 0, Loc;
    SetLoc, "set_loc", 3, 1, 1, Loc;
    GetArg, "get_arg", 3, 0, 1, Arg;
    PutArg, "put_arg", 3, 1, 0, Arg;
    SetArg, "set_arg", 3, 1, 1, Arg;
    GetVarRef, "get_var_ref", 3, 0, 1, VarRef;
    PutVarRef, "put_var_ref", 3, 1, 0, VarRef;
    SetVarRef, "set_var_ref", 3, 1, 1, VarRef;
    SetLocUninitialized, "set_loc_uninitialized", 3, 0, 0, Loc;
    GetLocCheck, "get_loc_check", 3, 0, 1, Loc;
    PutLocCheck, "put_loc_check", 3, 1, 0, Loc;
    PutLocCheckInit, "put_loc_check_init", 3, 1, 0, Loc;
    GetVarRefCheck, "get_var_ref_check", 3, 0, 1, VarRef;
    PutVarRefCheck, "put_var_ref_check", 3, 1, 0, VarRef;
    PutVarRefCheckInit, "put_var_ref_check_init", 3, 1, 0, VarRef;
    CloseLoc, "close_loc", 3, 0, 0, Loc;
    IfFalse, "if_false", 5, 1, 0, Label;
    IfTrue, "if_true", 5, 1, 0, Label;
    Goto, "goto", 5, 0, 0, Label;
    Catch, "catch", 5, 0, 1, Label;
    Gosub, "gosub", 5, 0, 0, Label;
    Ret, "ret", 1, 1, 0, None;
    NipCatch, "nip_catch", 1, 2, 1, None;
    CheckObject, "check_object", 1, 1, 1, None;
    ToObject, "to_object", 1, 1, 1, None;
    ToPropkey, "to_propkey", 1, 1, 1, None;
    ToPropkey2, "to_propkey2", 1, 2, 2, None;
    WithGetVar, "with_get_var", 10, 1, 0, AtomLabelU8;
    WithPutVar, "with_put_var", 10, 2, 1, AtomLabelU8;
    WithDeleteVar, "with_delete_var", 10, 1, 0, AtomLabelU8;
    WithMakeRef, "with_make_ref", 10, 1, 0, AtomLabelU8;
    WithGetRef, "with_get_ref", 10, 1, 0, AtomLabelU8;
    WithGetRefUndef, "with_get_ref_undef", 10, 1, 0, AtomLabelU8;
    MakeLocRef, "make_loc_ref", 7, 0, 2, AtomU16;
    MakeArgRef, "make_arg_ref", 7, 0, 2, AtomU16;
    MakeVarRefRef, "make_var_ref_ref", 7, 0, 2, AtomU16;
    MakeVarRef, "make_var_ref", 5, 0, 2, Atom;
    ForInStart, "for_in_start", 1, 1, 1, None;
    ForOfStart, "for_of_start", 1, 1, 3, None;
    ForAwaitOfStart, "for_await_of_start", 1, 1, 3, None;
    ForInNext, "for_in_next", 1, 1, 3, None;
    ForOfNext, "for_of_next", 2, 3, 5, U8;
    IteratorGetValueDone, "iterator_get_value_done", 1, 1, 2, None;
    IteratorClose, "iterator_close", 1, 3, 0, None;
    IteratorNext, "iterator_next", 1, 4, 4, None;
    IteratorCall, "iterator_call", 2, 4, 5, U8;
    InitialYield, "initial_yield", 1, 0, 0, None;
    Yield, "yield", 1, 1, 2, None;
    YieldStar, "yield_star", 1, 1, 2, None;
    AsyncYieldStar, "async_yield_star", 1, 1, 2, None;
    Await, "await", 1, 1, 1, None;
    Neg, "neg", 1, 1, 1, None;
    Plus, "plus", 1, 1, 1, None;
    Dec, "dec", 1, 1, 1, None;
    Inc, "inc", 1, 1, 1, None;
    PostDec, "post_dec", 1, 1, 2, None;
    PostInc, "post_inc", 1, 1, 2, None;
    DecLoc, "dec_loc", 2, 0, 0, Loc8;
    IncLoc, "inc_loc", 2, 0, 0, Loc8;
    AddLoc, "add_loc", 2, 1, 0, Loc8;
    Not, "not", 1, 1, 1, None;
    Lnot, "lnot", 1, 1, 1, None;
    Typeof, "typeof", 1, 1, 1, None;
    Delete, "delete", 1, 2, 1, None;
    DeleteVar, "delete_var", 5, 0, 1, Atom;
    Mul, "mul", 1, 2, 1, None;
    Div, "div", 1, 2, 1, None;
    Mod, "mod", 1, 2, 1, None;
    Add, "add", 1, 2, 1, None;
    Sub, "sub", 1, 2, 1, None;
    Shl, "shl", 1, 2, 1, None;
    Sar, "sar", 1, 2, 1, None;
    Shr, "shr", 1, 2, 1, None;
    And, "and", 1, 2, 1, None;
    Xor, "xor", 1, 2, 1, None;
    Or, "or", 1, 2, 1, None;
    Pow, "pow", 1, 2, 1, None;
    Lt, "lt", 1, 2, 1, None;
    Lte, "lte", 1, 2, 1, None;
    Gt, "gt", 1, 2, 1, None;
    Gte, "gte", 1, 2, 1, None;
    Instanceof, "instanceof", 1, 2, 1, None;
    In, "in", 1, 2, 1, None;
    Eq, "eq", 1, 2, 1, None;
    Neq, "neq", 1, 2, 1, None;
    StrictEq, "strict_eq", 1, 2, 1, None;
    StrictNeq, "strict_neq", 1, 2, 1, None;
    IsUndefinedOrNull, "is_undefined_or_null", 1, 1, 1, None;
    PrivateIn, "private_in", 1, 2, 1, None;
    PushBigintI32, "push_bigint_i32", 5, 0, 1, I32;
    UsingDisposeInit, "using_dispose_init", 1, 0, 1, None;
    UsingDispose, "using_dispose", 3, 1, 1, Loc;
    UsingDisposeAsync, "using_dispose_async", 3, 0, 1, Loc;
    UsingDisposeMerge, "using_dispose_merge", 1, 2, 1, None;
    UsingDisposeEnd, "using_dispose_end", 1, 1, 0, None;
    UsingCheck, "using_check", 2, 1, 2, U8;
    Nop, "nop", 1, 0, 0, None;
    PushMinus1, "push_minus1", 1, 0, 1, NoneInt;
    Push0, "push_0", 1, 0, 1, NoneInt;
    Push1, "push_1", 1, 0, 1, NoneInt;
    Push2, "push_2", 1, 0, 1, NoneInt;
    Push3, "push_3", 1, 0, 1, NoneInt;
    Push4, "push_4", 1, 0, 1, NoneInt;
    Push5, "push_5", 1, 0, 1, NoneInt;
    Push6, "push_6", 1, 0, 1, NoneInt;
    Push7, "push_7", 1, 0, 1, NoneInt;
    PushI8, "push_i8", 2, 0, 1, I8;
    PushI16, "push_i16", 3, 0, 1, I16;
    PushConst8, "push_const8", 2, 0, 1, Const8;
    Fclosure8, "fclosure8", 2, 0, 1, Const8;
    PushEmptyString, "push_empty_string", 1, 0, 1, None;
    GetLoc8, "get_loc8", 2, 0, 1, Loc8;
    PutLoc8, "put_loc8", 2, 1, 0, Loc8;
    SetLoc8, "set_loc8", 2, 1, 1, Loc8;
    GetLoc0Loc1, "get_loc0_loc1", 1, 0, 2, NoneLoc;
    GetLoc0, "get_loc0", 1, 0, 1, NoneLoc;
    GetLoc1, "get_loc1", 1, 0, 1, NoneLoc;
    GetLoc2, "get_loc2", 1, 0, 1, NoneLoc;
    GetLoc3, "get_loc3", 1, 0, 1, NoneLoc;
    PutLoc0, "put_loc0", 1, 1, 0, NoneLoc;
    PutLoc1, "put_loc1", 1, 1, 0, NoneLoc;
    PutLoc2, "put_loc2", 1, 1, 0, NoneLoc;
    PutLoc3, "put_loc3", 1, 1, 0, NoneLoc;
    SetLoc0, "set_loc0", 1, 1, 1, NoneLoc;
    SetLoc1, "set_loc1", 1, 1, 1, NoneLoc;
    SetLoc2, "set_loc2", 1, 1, 1, NoneLoc;
    SetLoc3, "set_loc3", 1, 1, 1, NoneLoc;
    GetArg0, "get_arg0", 1, 0, 1, NoneArg;
    GetArg1, "get_arg1", 1, 0, 1, NoneArg;
    GetArg2, "get_arg2", 1, 0, 1, NoneArg;
    GetArg3, "get_arg3", 1, 0, 1, NoneArg;
    PutArg0, "put_arg0", 1, 1, 0, NoneArg;
    PutArg1, "put_arg1", 1, 1, 0, NoneArg;
    PutArg2, "put_arg2", 1, 1, 0, NoneArg;
    PutArg3, "put_arg3", 1, 1, 0, NoneArg;
    SetArg0, "set_arg0", 1, 1, 1, NoneArg;
    SetArg1, "set_arg1", 1, 1, 1, NoneArg;
    SetArg2, "set_arg2", 1, 1, 1, NoneArg;
    SetArg3, "set_arg3", 1, 1, 1, NoneArg;
    GetVarRef0, "get_var_ref0", 1, 0, 1, NoneVarRef;
    GetVarRef1, "get_var_ref1", 1, 0, 1, NoneVarRef;
    GetVarRef2, "get_var_ref2", 1, 0, 1, NoneVarRef;
    GetVarRef3, "get_var_ref3", 1, 0, 1, NoneVarRef;
    PutVarRef0, "put_var_ref0", 1, 1, 0, NoneVarRef;
    PutVarRef1, "put_var_ref1", 1, 1, 0, NoneVarRef;
    PutVarRef2, "put_var_ref2", 1, 1, 0, NoneVarRef;
    PutVarRef3, "put_var_ref3", 1, 1, 0, NoneVarRef;
    SetVarRef0, "set_var_ref0", 1, 1, 1, NoneVarRef;
    SetVarRef1, "set_var_ref1", 1, 1, 1, NoneVarRef;
    SetVarRef2, "set_var_ref2", 1, 1, 1, NoneVarRef;
    SetVarRef3, "set_var_ref3", 1, 1, 1, NoneVarRef;
    GetLength, "get_length", 1, 1, 1, None;
    IfFalse8, "if_false8", 2, 1, 0, Label8;
    IfTrue8, "if_true8", 2, 1, 0, Label8;
    Goto8, "goto8", 2, 0, 0, Label8;
    Goto16, "goto16", 3, 0, 0, Label16;
    Call0, "call0", 1, 1, 1, Npopx;
    Call1, "call1", 1, 1, 1, Npopx;
    Call2, "call2", 1, 1, 1, Npopx;
    Call3, "call3", 1, 1, 1, Npopx;
    IsUndefined, "is_undefined", 1, 1, 1, None;
    IsNull, "is_null", 1, 1, 1, None;
    TypeofIsUndefined, "typeof_is_undefined", 1, 1, 1, None;
    TypeofIsFunction, "typeof_is_function", 1, 1, 1, None;
}

/// Number of executable opcodes, excluding [`Opcode::Invalid`].
pub const OPCODE_COUNT: usize = Opcode::ALL.len() - 1;

impl Opcode {
    /// Looks up an executable opcode. The zero sentinel is not executable.
    pub const fn from_u8(byte: u8) -> Option<Self> {
        if byte == 0 || byte as usize >= Self::ALL.len() {
            None
        } else {
            Some(Self::ALL[byte as usize])
        }
    }

    pub const fn info(self) -> &'static OpcodeInfo {
        &OPCODE_INFO[self as usize]
    }
}

/// Decoded operands. Short forms expose their implicit operands here, too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operands {
    None,
    Signed(i32),
    Unsigned(u32),
    Pair(u32, u32),
    Call(u16),
    /// Direct-eval argument count and the encoded head of its binding chain.
    Eval {
        arguments: u16,
        scope: u16,
    },
    Constant(u32),
    Local(u16),
    Locals(u16, u16),
    Argument(u16),
    VarRef(u16),
    Atom(UnitAtom),
    AtomU8(UnitAtom, u8),
    AtomU16(UnitAtom, u16),
    AtomJumpU8(UnitAtom, i32, u8),
    AtomJumpU16(UnitAtom, i32, u16),
    Jump(i32),
    JumpU16(i32, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    pub opcode: Opcode,
    pub operands: Operands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionError {
    MissingOpcode,
    InvalidOpcode(u8),
    TruncatedOperand(Opcode),
}

impl fmt::Display for InstructionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOpcode => f.write_str("missing opcode"),
            Self::InvalidOpcode(byte) => write!(f, "invalid opcode 0x{byte:02x}"),
            Self::TruncatedOperand(op) => write!(f, "truncated {} operand", op.info().name),
        }
    }
}

impl std::error::Error for InstructionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeInstructionError {
    InvalidOpcode,
    OperandMismatch(Opcode),
    OperandOutOfRange(Opcode),
}

impl fmt::Display for EncodeInstructionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOpcode => f.write_str("invalid opcode"),
            Self::OperandMismatch(op) => write!(f, "operand does not match {}", op.info().name),
            Self::OperandOutOfRange(op) => {
                write!(f, "operand is out of range for {}", op.info().name)
            }
        }
    }
}

impl std::error::Error for EncodeInstructionError {}

/// One encoded instruction, held inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedInstruction {
    bytes: [u8; 1 + OperandFormat::AtomLabelU16.byte_len() as usize],
    len: u8,
}

impl EncodedInstruction {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl AsRef<[u8]> for EncodedInstruction {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Instruction {
    /// Encodes an instruction once its label offsets and binding indices are known.
    pub fn encode(self) -> Result<EncodedInstruction, EncodeInstructionError> {
        use OperandFormat as F;
        use Operands as O;

        let opcode = self.opcode;
        if opcode == Opcode::Invalid {
            return Err(EncodeInstructionError::InvalidOpcode);
        }
        let info = opcode.info();
        let range = |_| EncodeInstructionError::OperandOutOfRange(opcode);
        let mut encoded = EncodedInstruction {
            bytes: [0; 11],
            len: info.size,
        };
        encoded.bytes[0] = opcode as u8;
        let bytes = &mut encoded.bytes[1..usize::from(info.size)];
        match (info.operand_fmt, self.operands) {
            (F::None | F::NoneInt | F::NoneLoc | F::NoneArg | F::NoneVarRef | F::Npopx, _) => {
                // Short opcodes encode the operand in the opcode byte itself.
                if Self::decode(encoded.as_bytes(), 0) != Ok(self) {
                    return Err(EncodeInstructionError::OperandMismatch(opcode));
                }
            }
            (F::U8, O::Unsigned(value)) | (F::Const8, O::Constant(value)) => {
                bytes[0] = u8::try_from(value).map_err(range)?;
            }
            (F::I8, O::Signed(value)) | (F::Label8, O::Jump(value)) => {
                bytes[0] = i8::try_from(value).map_err(range)? as u8;
            }
            (F::Loc8, O::Local(value)) => bytes[0] = u8::try_from(value).map_err(range)?,
            (F::U16, O::Unsigned(value)) => {
                bytes.copy_from_slice(&u16::try_from(value).map_err(range)?.to_le_bytes());
            }
            (F::I16, O::Signed(value)) | (F::Label16, O::Jump(value)) => {
                bytes.copy_from_slice(&i16::try_from(value).map_err(range)?.to_le_bytes());
            }
            (F::Npop, O::Call(value))
            | (F::Loc, O::Local(value))
            | (F::Arg, O::Argument(value))
            | (F::VarRef, O::VarRef(value)) => {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
            (F::NpopU16, O::Eval { arguments, scope }) => {
                bytes[..2].copy_from_slice(&arguments.to_le_bytes());
                bytes[2..].copy_from_slice(&scope.to_le_bytes());
            }
            (F::U32, O::Unsigned(value))
            | (F::Const, O::Constant(value))
            | (F::Atom, O::Atom(UnitAtom(value))) => {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
            (F::U32x2, O::Pair(first, second)) => {
                bytes[..4].copy_from_slice(&first.to_le_bytes());
                bytes[4..].copy_from_slice(&second.to_le_bytes());
            }
            (F::I32, O::Signed(value)) | (F::Label, O::Jump(value)) => {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
            (F::AtomU8, O::AtomU8(atom, flags)) => {
                bytes[..4].copy_from_slice(&atom.raw().to_le_bytes());
                bytes[4] = flags;
            }
            (F::AtomU16, O::AtomU16(atom, slot)) => {
                bytes[..4].copy_from_slice(&atom.raw().to_le_bytes());
                bytes[4..].copy_from_slice(&slot.to_le_bytes());
            }
            (F::AtomLabelU8, O::AtomJumpU8(atom, offset, flags)) => {
                bytes[..4].copy_from_slice(&atom.raw().to_le_bytes());
                bytes[4..8].copy_from_slice(&offset.to_le_bytes());
                bytes[8] = flags;
            }
            (F::AtomLabelU16, O::AtomJumpU16(atom, offset, slot)) => {
                bytes[..4].copy_from_slice(&atom.raw().to_le_bytes());
                bytes[4..8].copy_from_slice(&offset.to_le_bytes());
                bytes[8..].copy_from_slice(&slot.to_le_bytes());
            }
            (F::LabelU16, O::JumpU16(offset, slot)) => {
                bytes[..4].copy_from_slice(&offset.to_le_bytes());
                bytes[4..].copy_from_slice(&slot.to_le_bytes());
            }
            _ => return Err(EncodeInstructionError::OperandMismatch(opcode)),
        }
        Ok(encoded)
    }

    pub fn decode(code: &[u8], pc: usize) -> Result<Self, InstructionError> {
        let code = code.get(pc..).ok_or(InstructionError::MissingOpcode)?;
        let (&byte, rest) = code.split_first().ok_or(InstructionError::MissingOpcode)?;
        let opcode = Opcode::from_u8(byte).ok_or(InstructionError::InvalidOpcode(byte))?;
        let info = opcode.info();
        let bytes = rest
            .get(..usize::from(info.size - 1))
            .ok_or(InstructionError::TruncatedOperand(opcode))?;
        let u16_at = |i| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at = |i| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let atom = || UnitAtom(u32_at(0));
        let operands = match info.operand_fmt {
            OperandFormat::None => Operands::None,
            OperandFormat::NoneInt => Operands::Signed(i32::from(byte) - Opcode::Push0 as i32),
            OperandFormat::NoneLoc if opcode == Opcode::GetLoc0Loc1 => Operands::Locals(0, 1),
            OperandFormat::NoneLoc => {
                Operands::Local(u16::from((byte - Opcode::GetLoc0 as u8) % 4))
            }
            OperandFormat::NoneArg => {
                Operands::Argument(u16::from((byte - Opcode::GetArg0 as u8) % 4))
            }
            OperandFormat::NoneVarRef => {
                Operands::VarRef(u16::from((byte - Opcode::GetVarRef0 as u8) % 4))
            }
            OperandFormat::U8 => Operands::Unsigned(bytes[0].into()),
            OperandFormat::I8 => Operands::Signed(i32::from(bytes[0] as i8)),
            OperandFormat::Loc8 => Operands::Local(bytes[0].into()),
            OperandFormat::Const8 => Operands::Constant(bytes[0].into()),
            OperandFormat::Label8 => Operands::Jump(i32::from(bytes[0] as i8)),
            OperandFormat::U16 => Operands::Unsigned(u16_at(0).into()),
            OperandFormat::I16 => Operands::Signed(i32::from(u16_at(0) as i16)),
            OperandFormat::Label16 => Operands::Jump(i32::from(u16_at(0) as i16)),
            OperandFormat::Npop => Operands::Call(u16_at(0)),
            OperandFormat::Npopx => Operands::Call(u16::from(byte - Opcode::Call0 as u8)),
            OperandFormat::NpopU16 => Operands::Eval {
                arguments: u16_at(0),
                scope: u16_at(2),
            },
            OperandFormat::Loc => Operands::Local(u16_at(0)),
            OperandFormat::Arg => Operands::Argument(u16_at(0)),
            OperandFormat::VarRef => Operands::VarRef(u16_at(0)),
            OperandFormat::U32 => Operands::Unsigned(u32_at(0)),
            OperandFormat::U32x2 => Operands::Pair(u32_at(0), u32_at(4)),
            OperandFormat::I32 => Operands::Signed(u32_at(0) as i32),
            OperandFormat::Const => Operands::Constant(u32_at(0)),
            OperandFormat::Label => Operands::Jump(u32_at(0) as i32),
            OperandFormat::Atom => Operands::Atom(atom()),
            OperandFormat::AtomU8 => Operands::AtomU8(atom(), bytes[4]),
            OperandFormat::AtomU16 => Operands::AtomU16(atom(), u16_at(4)),
            OperandFormat::AtomLabelU8 => Operands::AtomJumpU8(atom(), u32_at(4) as i32, bytes[8]),
            OperandFormat::AtomLabelU16 => {
                Operands::AtomJumpU16(atom(), u32_at(4) as i32, u16_at(8))
            }
            OperandFormat::LabelU16 => Operands::JumpU16(u32_at(0) as i32, u16_at(4)),
        };
        Ok(Self { opcode, operands })
    }

    /// Computes a relative branch address before checking instruction boundaries.
    pub fn branch_target(self, pc: u32) -> Option<i64> {
        let (base, offset) = match self.operands {
            Operands::Jump(offset) | Operands::JumpU16(offset, _) => (1, offset),
            Operands::AtomJumpU8(_, offset, _) | Operands::AtomJumpU16(_, offset, _) => (5, offset),
            _ => return None,
        };
        Some(i64::from(pc) + base + i64::from(offset))
    }
}
