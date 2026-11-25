//! Operations emitted before binding and label resolution.

use crate::{Instruction, UnitAtom};

/// An unresolved name at a lexical scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedName {
    pub name: UnitAtom,
    pub scope: u16,
}

/// The compiler lowers names to frame, closure or global operations before
/// replacing label IDs with relative bytes and selecting short forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrOp {
    /// An instruction whose operands need no name or label resolution.
    Resolved(Instruction),
    EnterScope(u16),
    LeaveScope(u16),
    DisposeScope(u16),
    ScopeGetVar(ScopedName),
    /// Like `ScopeGetVar`, but an unbound name yields `undefined` for `typeof`.
    ScopeGetVarUndef(ScopedName),
    ScopePutVar(ScopedName),
    ScopePutVarInit(ScopedName),
    ScopeDeleteVar(ScopedName),
    ScopeMakeRef {
        binding: ScopedName,
        store_label: u32,
    },
    ScopeGetRef(ScopedName),
    ScopeGetPrivateField(ScopedName),
    ScopeGetPrivateFieldKeepReceiver(ScopedName),
    ScopePutPrivateField(ScopedName),
    ScopeInPrivateField(ScopedName),
    GetFieldOptionalChain(UnitAtom),
    GetArrayElementOptionalChain,
    /// Name inference points to the definition of the class in this IR.
    SetClassName {
        class_label: u32,
    },
    DirectEval {
        arguments: u16,
        scope: u16,
    },
    ApplyEval {
        scope: u16,
    },
    Label(u32),
    Jump(u32),
    JumpIfFalse(u32),
    JumpIfTrue(u32),
    Catch(u32),
    Gosub(u32),
    /// Byte offset in the source text.
    SourceLoc(u32),
}
