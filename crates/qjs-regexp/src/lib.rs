//! Pure-Rust port of QuickJS-ng v0.17.0 `libregexp` (full ES2025 RegExp
//! compiler and backtracking bytecode VM with interrupt and limit hooks).

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

/// Host hooks required by `libregexp` (matching `lre_check_timeout`,
/// `lre_realloc`, and `lre_check_stack_overflow`).
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRegExp {
    pub bytecode: Vec<u8>,
    pub flags: RegExpFlags,
    pub capture_count: u16,
    pub group_names: Vec<Option<String>>,
}

/// Capture spans `(start, end)` in code-unit indices (`None` if unmatched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub captures: Vec<Option<(usize, usize)>>,
}

/// Compile an ECMAScript regular expression pattern.
/// Pattern is supplied as UTF-8 or CESU-8/WTF-8 bytes (preserving lone surrogates).
pub fn compile(
    _pattern: &[u8],
    _flags: RegExpFlags,
    _host: &mut dyn RegExpHost,
) -> Result<CompiledRegExp, RegExpError> {
    todo!("implemented by qjs-regexp worker")
}

/// Execute a compiled RegExp starting at code-unit index `start_index`.
pub fn exec(
    _re: &CompiledRegExp,
    _input: RegExpInput<'_>,
    _start_index: usize,
    _host: &mut dyn RegExpHost,
) -> Result<Option<MatchResult>, RegExpError> {
    todo!("implemented by qjs-regexp worker")
}
