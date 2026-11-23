//! Lower validated ECMAScript into a verified bytecode image.

#![deny(unsafe_code)]

mod assemble;
mod class;
mod emit;
mod expr;
mod module;
mod pattern;
mod resolve;
mod stmt;

use qjs_bytecode::ProgramImage;
use qjs_syntax::{ParseErrorKind, ParseOptions, ParsedProgram, SourceText};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    Syntax {
        message: String,
        line: u32,
        col: u32,
    },
    OutOfMemory,
    StackOverflow,
    Lowering {
        message: String,
        line: u32,
        col: u32,
    },
    Bytecode(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { message, line, col } | Self::Lowering { message, line, col } => {
                write!(f, "{line}:{col}: {message}")
            }
            Self::OutOfMemory => f.write_str("Compiler memory limit exceeded"),
            Self::StackOverflow => f.write_str("Compiler stack limit exceeded"),
            Self::Bytecode(message) => write!(f, "Invalid compiler output at {message}"),
        }
    }
}

impl std::error::Error for CompileError {}

pub fn compile_source(
    source: &SourceText,
    opts: ParseOptions,
) -> Result<ProgramImage, CompileError> {
    let program = qjs_syntax::parse(source, opts).map_err(|error| match error.kind {
        ParseErrorKind::OutOfMemory => CompileError::OutOfMemory,
        ParseErrorKind::StackOverflow => CompileError::StackOverflow,
        ParseErrorKind::Syntax => CompileError::Syntax {
            message: error.message,
            line: error.line,
            col: error.col,
        },
    })?;
    compile_program(source, &program, opts)
}

/// Compile the validated tree returned by [`qjs_syntax::parse`] with the same source and options.
pub fn compile_program(
    source: &SourceText,
    program: &ParsedProgram,
    opts: ParseOptions,
) -> Result<ProgramImage, CompileError> {
    const INLINE_DEPTH: usize = 32;
    let inline = ParseOptions {
        max_depth: opts.max_depth.min(INLINE_DEPTH),
        ..opts
    };
    match emit::compile(source, program, inline) {
        Err(CompileError::StackOverflow) if opts.max_depth > INLINE_DEPTH => (),
        result => return result,
    }
    let depth = opts
        .max_depth
        .min(source.as_bytes().len().saturating_add(1));
    let stack_size = depth
        .saturating_mul(32 * 1024)
        .saturating_add(2 * 1024 * 1024);
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(stack_size)
            .spawn_scoped(scope, || emit::compile(source, program, opts))
            .map_err(|_| CompileError::OutOfMemory)?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
