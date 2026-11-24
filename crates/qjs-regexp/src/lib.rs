//! ECMAScript regular expression compiler and backtracking VM.
//!
//! The bytecode format follows QuickJS-ng's `libregexp` and uses native byte order.

#![deny(unsafe_code)]

mod alloc;
mod bytecode;
mod class;
mod compiler;
mod escape;
mod vm;

pub use escape::{EscapeError, parse_escape};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RegExpFlags(pub u16);

impl RegExpFlags {
    pub const GLOBAL: u16 = 1 << 0;
    pub const IGNORECASE: u16 = 1 << 1;
    pub const MULTILINE: u16 = 1 << 2;
    pub const DOTALL: u16 = 1 << 3;
    pub const UNICODE: u16 = 1 << 4;
    pub const STICKY: u16 = 1 << 5;
    pub const INDICES: u16 = 1 << 6;
    pub const NAMED_GROUPS: u16 = 1 << 7;
    pub const UNICODE_SETS: u16 = 1 << 8;

    pub fn contains(self, flag: u16) -> bool {
        (self.0 & flag) != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegExpError {
    Syntax(String),
    OutOfMemory,
    Interrupted,
    StackOverflow,
    InvalidBytecode,
}

/// Resource limits for compilation and execution.
///
/// `check_stack` returns `true` if the requested depth is allowed. Each call
/// releases its charges before returning, including those for its output.
/// The caller accounts for storage it retains after the call.
pub trait RegExpHost {
    fn poll_interrupt(&mut self) -> bool;
    fn charge(&mut self, bytes: usize) -> Result<(), RegExpError>;
    fn release(&mut self, bytes: usize);
    fn check_stack(&mut self, depth_bytes: usize) -> bool;
}

/// Default unlimited host for standalone tests.
#[derive(Default)]
pub struct UnlimitedRegExpHost;

impl RegExpHost for UnlimitedRegExpHost {
    fn poll_interrupt(&mut self) -> bool {
        false
    }
    fn charge(&mut self, _bytes: usize) -> Result<(), RegExpError> {
        Ok(())
    }
    fn release(&mut self, _bytes: usize) {}
    fn check_stack(&mut self, _depth_bytes: usize) -> bool {
        true
    }
}

/// Input subject string for RegExp execution (code-unit indexed).
#[derive(Debug, Clone, Copy)]
pub enum RegExpInput<'a> {
    Latin1(&'a [u8]),
    Utf16(&'a [u16]),
}

impl<'a> RegExpInput<'a> {
    pub fn len(&self) -> usize {
        match self {
            Self::Latin1(b) => b.len(),
            Self::Utf16(u) => u.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Compiled RegExp bytecode.
#[derive(Debug, Clone)]
pub struct CompiledRegExp {
    pub bytecode: Vec<u8>,
    pub flags: RegExpFlags,
    /// Number of captures, including capture zero for the whole match.
    pub capture_count: u16,
    /// Names indexed by capture number. Capture zero has no name.
    pub group_names: Vec<Option<String>>,
    pub(crate) plan: bytecode::CompiledPlan,
}

impl PartialEq for CompiledRegExp {
    fn eq(&self, other: &Self) -> bool {
        self.bytecode == other.bytecode
            && self.flags == other.flags
            && self.capture_count == other.capture_count
            && self.group_names == other.group_names
    }
}

impl Eq for CompiledRegExp {}

/// Capture spans `(start, end)` in code-unit indices (`None` if unmatched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub captures: Vec<Option<(usize, usize)>>,
}

pub(crate) const INLINE_CAPTURES: usize = 8;

/// Capture spans stored inline for common patterns with at most 8 captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchSpans {
    Inline {
        len: u8,
        captures: [Option<(usize, usize)>; INLINE_CAPTURES],
    },
    Heap(Vec<Option<(usize, usize)>>),
}

impl MatchSpans {
    pub(crate) fn single(start: usize, end: usize) -> Self {
        let mut captures = [None; INLINE_CAPTURES];
        captures[0] = Some((start, end));
        Self::Inline { len: 1, captures }
    }

    pub fn as_slice(&self) -> &[Option<(usize, usize)>] {
        match self {
            Self::Inline { len, captures } => &captures[..usize::from(*len)],
            Self::Heap(v) => v,
        }
    }

    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(v) => v.capacity() * std::mem::size_of::<Option<(usize, usize)>>(),
        }
    }
}

/// Compile an ECMAScript regular expression pattern.
///
/// Supply UTF-8 in Unicode mode and CESU-8 in legacy mode. Both modes accept
/// encoded lone surrogates. The caller parses and checks the flag string.
pub fn compile(
    pattern: &[u8],
    flags: RegExpFlags,
    host: &mut dyn RegExpHost,
) -> Result<CompiledRegExp, RegExpError> {
    compiler::compile(pattern, flags, host)
}

/// Execute a compiled RegExp starting at a code-unit index.
///
/// The caller manages `lastIndex`, including advancing after an empty match
/// when iterating. With `y`, only the given starting position is tried.
pub fn exec(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start_index: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchResult>, RegExpError> {
    vm::exec(re, input, start_index, host)
}

/// Execute a compiled RegExp and return capture spans inline when possible.
pub fn exec_spans(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start_index: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchSpans>, RegExpError> {
    vm::exec_spans(re, input, start_index, host)
}

/// Execute a compiled RegExp and return only capture 0 `(start, end)`.
pub fn exec_single(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start_index: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<(usize, usize)>, RegExpError> {
    vm::exec_single(re, input, start_index, host)
}

/// Validate the header, instructions, branch targets and group-name records.
pub fn check_bytecode(bytecode: &[u8]) -> Result<(), RegExpError> {
    bytecode::validate(bytecode, &mut alloc::Budget::new(&mut UnlimitedRegExpHost)).map(|_| ())
}

impl CompiledRegExp {
    /// Read the captures and flags from previously compiled bytecode.
    pub fn from_bytecode(
        bytecode: Vec<u8>,
        host: &mut dyn RegExpHost,
    ) -> Result<Self, RegExpError> {
        let mut budget = alloc::Budget::new(host);
        let header = bytecode::validate(&bytecode, &mut budget)?;
        let group_names = compiler::collect_names(&header, &mut budget)?;
        let flags = header.flags;
        let capture_count = header.captures as u16;
        let plan = bytecode::CompiledPlan::from_validated(&bytecode, &header);
        Ok(Self {
            bytecode,
            flags,
            capture_count,
            group_names,
            plan,
        })
    }
}

fn canonicalize(c: u32, unicode: bool) -> u32 {
    if c < 128 {
        let byte = c as u8;
        u32::from(if unicode {
            byte.to_ascii_lowercase()
        } else {
            byte.to_ascii_uppercase()
        })
    } else {
        qjs_unicode::canonicalize(c, unicode) as u32
    }
}
