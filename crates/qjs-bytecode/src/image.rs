//! Heap-independent records for a compiled unit.

use crate::{BuiltinAtom, UnitAtom};

/// A lossless JavaScript string. UTF-16 strings may contain lone surrogates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConstString {
    Latin1(Vec<u8>),
    Utf16(Vec<u16>),
}

/// The cooked and raw arrays of one tagged-template site. A missing cooked
/// entry represents `undefined` for an invalid escape sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateObject {
    pub cooked: Vec<Option<ConstString>>,
    pub raw: Vec<ConstString>,
}

/// One entry in a function's constant pool.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstEntry {
    Int(i32),
    Float(f64),
    String(ConstString),
    /// Decimal digits with an optional leading minus sign.
    BigInt(String),
    RegExp {
        pattern: ConstString,
        flags: u16,
    },
    /// Index into [`ProgramImage::functions`].
    Function(u32),
    TemplateObject(TemplateObject),
}

/// At `pc`, the source position changes to `byte_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcSourceMap {
    pub pc: u32,
    pub byte_offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugInfo {
    pub filename: String,
    pub source_start: u32,
    /// Exclusive source byte offset. A location can point at this boundary.
    pub source_end: u32,
    /// Change points in strictly increasing PC order. The codec stores deltas.
    pub pc_map: Vec<PcSourceMap>,
}

impl DebugInfo {
    /// Returns the last recorded source offset at or before `pc`.
    pub fn pc_to_byte_offset(&self, pc: u32) -> Option<u32> {
        let end = self.pc_map.partition_point(|point| point.pc <= pc);
        self.pc_map[..end].last().map(|point| point.byte_offset)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FunctionKind {
    #[default]
    Normal,
    Generator,
    Async,
    AsyncGenerator,
}

/// Function properties used by closure creation, direct eval and backtraces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FunctionFlags {
    pub has_prototype: bool,
    pub has_simple_parameter_list: bool,
    pub is_derived_class_constructor: bool,
    pub need_home_object: bool,
    pub new_target_allowed: bool,
    pub super_call_allowed: bool,
    pub super_allowed: bool,
    pub arguments_allowed: bool,
    pub backtrace_barrier: bool,
    pub name_is_inferred: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VarKind {
    #[default]
    Var,
    Let,
    Const,
    Catch,
    Using,
    AwaitUsing,
    FunctionName,
    FunctionDeclaration,
    AsyncOrGeneratorDeclaration,
    PrivateField,
    PrivateMethod,
    PrivateGetter,
    PrivateSetter,
    PrivateGetterSetter,
    DisposeMethod,
}

/// The next lexical binding visible to direct eval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScopeLink {
    /// Resume lookup in the ordinary argument and var bindings.
    #[default]
    End,
    /// End of the parameter-initializer chain. Body bindings are hidden.
    ParameterEnd,
    Local(u16),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VarDef {
    pub name: UnitAtom,
    pub kind: VarKind,
    /// Index of the defining scope, if this function retains scope metadata.
    pub scope_level: u16,
    pub scope_next: ScopeLink,
    pub is_const: bool,
    pub lexical_tdz: bool,
    pub captured: bool,
    /// Slot in the function's frame-local varref table.
    pub var_ref_index: Option<u16>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClosureKind {
    #[default]
    Local,
    Argument,
    Ref,
    GlobalRef,
    GlobalDeclaration,
    Global,
    ModuleDeclaration,
    ModuleImport,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosureVarDef {
    pub name: UnitAtom,
    pub kind: ClosureKind,
    /// A slot in the enclosing frame for local, argument and ref captures.
    pub index: u16,
    pub var_kind: VarKind,
    pub is_const: bool,
    pub lexical_tdz: bool,
}

/// A table-driven exception edge for instructions in `[start_pc, end_pc)`.
/// The handler receives the preserved stack prefix and then the thrown value.
/// QuickJS-style `catch` instructions can express handlers in the code instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionHandler {
    pub start_pc: u32,
    pub end_pc: u32,
    pub handler_pc: u32,
    pub stack_depth: u16,
}

/// A lexical scope's bytecode range `[start_pc, end_pc)`. Empty ranges are
/// allowed because erasing scope markers can leave a scope with no code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeInfo {
    pub parent: Option<u16>,
    pub start_pc: u32,
    pub end_pc: u32,
    pub has_using: bool,
    pub has_await_using: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FunctionBytecode {
    pub name: UnitAtom,
    /// `None` represents an unspecified kind and executes as normal code.
    pub kind: Option<FunctionKind>,
    pub strict: bool,
    pub flags: FunctionFlags,
    pub arg_count: u16,
    pub var_count: u16,
    /// Number of parameters reflected in the function's `length` property.
    pub defined_arg_count: u16,
    pub stack_size: u16,
    /// Number of frame-local capture slots. Bytecode varref operands index
    /// `closure_vars` instead.
    pub var_ref_count: u16,
    /// Number of dynamic atoms available in the enclosing unit.
    pub atom_count: u32,
    /// Arguments followed by locals. Empty when binding metadata was stripped.
    pub vars: Vec<VarDef>,
    pub closure_vars: Vec<ClosureVarDef>,
    pub scopes: Vec<ScopeInfo>,
    pub exception_handlers: Vec<ExceptionHandler>,
    pub code: Vec<u8>,
    pub constants: Vec<ConstEntry>,
    pub debug: DebugInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportAttribute {
    pub key: UnitAtom,
    pub value: UnitAtom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedModule {
    pub name: UnitAtom,
    pub attributes: Vec<ImportAttribute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportEntry {
    pub requested_module: u32,
    /// `None` imports the module namespace instead of a named export.
    pub import_name: Option<UnitAtom>,
    pub closure_var: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportEntry {
    Local {
        export_name: UnitAtom,
        closure_var: u16,
    },
    Indirect {
        export_name: UnitAtom,
        requested_module: u32,
        import_name: Option<UnitAtom>,
    },
    Star {
        requested_module: u32,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleRecord {
    pub name: UnitAtom,
    pub requested_modules: Vec<RequestedModule>,
    pub imports: Vec<ImportEntry>,
    pub exports: Vec<ExportEntry>,
    pub has_top_level_await: bool,
    /// Index into [`ProgramImage::functions`].
    pub function: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgramImage {
    /// Dynamic atom IDs start at [`crate::FIRST_DYNAMIC_ATOM`].
    pub atoms: Vec<ConstString>,
    pub functions: Vec<FunctionBytecode>,
    pub modules: Vec<ModuleRecord>,
    pub entry_function: u32,
}

/// A name resolved through an image's atom table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomName<'a> {
    Builtin(BuiltinAtom),
    Dynamic(&'a ConstString),
}

impl ProgramImage {
    pub fn atom(&self, id: UnitAtom) -> Option<AtomName<'_>> {
        if let Some(atom) = id.builtin() {
            Some(AtomName::Builtin(atom))
        } else {
            self.atoms
                .get(id.dynamic_index()? as usize)
                .map(AtomName::Dynamic)
        }
    }
}
