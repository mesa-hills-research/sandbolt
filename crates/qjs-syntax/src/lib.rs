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
pub struct ParseOptions {
    pub goal: ParseGoal,
    pub strict: bool,
    pub annex_b: bool,
    pub max_depth: usize,
    pub max_bytes: usize,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            goal: ParseGoal::Script,
            strict: false,
            annex_b: true,
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
    let program = parser::Parser::new(source, opts)?.program()?;
    validate::validate(source, opts, &program)?;
    Ok(program)
}
