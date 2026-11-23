//! Pure-Rust bytecode schema, predefined atom table, scoped compiler IR,
//! debug info (`pc2line`), verifier, disassembler, and binary codec.

/// Compile-unit-local atom index (0..BUILTIN_COUNT are predefined atoms
/// matching `quickjs-atom.h`; >= BUILTIN_COUNT index into `ProgramImage::atoms`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UnitAtom(pub u32);

/// A lossless string literal in a compiled unit (Latin-1 or UTF-16 code units).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConstString {
    Latin1(Vec<u8>),
    Utf16(Vec<u16>),
}

/// Constant pool entry in a compiled function image (heap-independent).
#[derive(Debug, Clone, PartialEq)]
pub enum ConstEntry {
    Int(i32),
    Float(f64),
    String(ConstString),
    BigInt(String),
    RegExp { pattern: ConstString, flags: u16 },
    Function(u32),
}

/// Mapping from bytecode PC to byte offset in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcSourceMap {
    pub pc: u32,
    pub byte_offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugInfo {
    pub filename: String,
    pub source_start: u32,
    pub source_end: u32,
    pub pc_map: Vec<PcSourceMap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    Normal,
    Generator,
    Async,
    AsyncGenerator,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FunctionBytecode {
    pub name: UnitAtom,
    pub kind: Option<FunctionKind>,
    pub strict: bool,
    pub arg_count: u16,
    pub var_count: u16,
    pub stack_size: u16,
    pub code: Vec<u8>,
    pub constants: Vec<ConstEntry>,
    pub debug: DebugInfo,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgramImage {
    pub atoms: Vec<ConstString>,
    pub functions: Vec<FunctionBytecode>,
    pub entry_function: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    pub pc: usize,
    pub message: String,
}

pub fn verify(_func: &FunctionBytecode) -> Result<(), VerifyError> {
    todo!("implemented by qjs-bytecode worker")
}
