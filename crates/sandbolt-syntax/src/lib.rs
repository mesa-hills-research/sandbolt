//! ECMAScript source text, tokens, and validated syntax trees.

#![deny(unsafe_code)]

mod ast;
mod lexer;
mod parser;
mod source;
mod validate;

#[cfg(test)]
mod tests;

pub use ast::*;
pub use lexer::{Lexer, LexicalGoal, Token, TokenKind};
pub use source::{SourceText, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    Script,
    Module,
    FunctionBody {
        is_async: bool,
        is_generator: bool,
    },
    DirectEval {
        allow_new_target: bool,
        allow_super_property: bool,
        allow_super_call: bool,
        in_class_field_init: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateNameKind {
    Field,
    Method,
    Accessor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOptions<'a> {
    pub goal: ParseGoal,
    pub strict: bool,
    pub annex_b: bool,
    /// Private names visible to direct eval, innermost first, without the `#` prefix.
    pub enclosing_private_names: &'a [(String, PrivateNameKind)],
    pub max_depth: usize,
    pub max_bytes: usize,
}

impl Default for ParseOptions<'_> {
    fn default() -> Self {
        Self {
            goal: ParseGoal::Script,
            strict: false,
            annex_b: true,
            enclosing_private_names: &[],
            max_depth: 512,
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    Syntax,
    OutOfMemory,
    StackOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub message: String,
    pub span: Span,
    pub line: u32,
    pub col: u32,
}

impl ParseError {
    fn new(source: &SourceText, kind: ParseErrorKind, span: Span, message: &str) -> Self {
        let (line, col) = source.line_col(span.start);
        Self {
            kind,
            message: message.into(),
            span,
            line,
            col,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(source: &SourceText, opts: ParseOptions) -> Result<ParsedProgram, ParseError> {
    const INLINE_DEPTH: usize = 48;
    let inline = ParseOptions {
        max_depth: opts.max_depth.min(INLINE_DEPTH),
        ..opts
    };
    match parse_inner(source, inline) {
        Err(e) if e.kind == ParseErrorKind::StackOverflow && opts.max_depth > INLINE_DEPTH => (),
        result => return result,
    }

    // Deep trees need more stack than the caller's thread may have.
    let depth = opts
        .max_depth
        .min(source.as_bytes().len().saturating_add(1));
    let stack_size = depth
        .saturating_mul(32 * 1024)
        .saturating_add(2 * 1024 * 1024);
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .stack_size(stack_size)
            .spawn_scoped(scope, move || parse_inner(source, opts))
            .map_err(|_| {
                ParseError::new(
                    source,
                    ParseErrorKind::OutOfMemory,
                    Span::default(),
                    "Could not allocate the parser stack",
                )
            })?;
        thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

fn parse_inner(source: &SourceText, opts: ParseOptions) -> Result<ParsedProgram, ParseError> {
    let program = parser::Parser::new(source, opts)?.program()?;
    validate::validate(source, opts, &program)?;
    Ok(program)
}
