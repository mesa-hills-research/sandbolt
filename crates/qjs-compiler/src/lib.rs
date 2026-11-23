//! AST -> scoped QuickJS-style IR -> variable/closure resolution ->
//! label resolution, peephole optimization, and verified bytecode emission.

use qjs_bytecode::ProgramImage;
use qjs_syntax::{ParseOptions, SourceText};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    Syntax { message: String, line: u32, col: u32 },
    OutOfMemory,
    StackOverflow,
}

pub fn compile_source(
    _source: &SourceText,
    _opts: ParseOptions,
) -> Result<ProgramImage, CompileError> {
    todo!("implemented in Phase 3 by qjs-compiler worker")
}
