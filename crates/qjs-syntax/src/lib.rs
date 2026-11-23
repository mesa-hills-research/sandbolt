//! Pure-Rust ES2025 lexer, arena AST, cover-grammar disambiguator, and
//! early-error validator for QuickJS-ng v0.17.0 parity.

/// Byte span into [`SourceText`]'s lossless WTF-8/CESU-8 byte buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
}

/// Lossless ECMAScript source text supporting unpaired UTF-16 surrogates
/// (required for `eval`, `new Function`, and `RegExp` string inputs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceText {
    /// Source filename (for stack frames and diagnostics).
    pub filename: String,
    /// Raw UTF-16 code units (preserves lone surrogates losslessly).
    pub units: Vec<u16>,
    /// Line-start byte offsets matching QuickJS UTF-8/CESU-8 column positions.
    pub line_starts: Vec<u32>,
}

impl SourceText {
    pub fn from_utf8(filename: impl Into<String>, src: &str) -> Self {
        let units: Vec<u16> = src.encode_utf16().collect();
        let line_starts = compute_line_starts_utf8(src.as_bytes());
        Self {
            filename: filename.into(),
            units,
            line_starts,
        }
    }

    pub fn from_utf16(filename: impl Into<String>, units: Vec<u16>) -> Self {
        let mut line_starts = vec![0u32];
        let mut byte_off: u32 = 0;
        let mut i = 0;
        while i < units.len() {
            let u = units[i];
            let next_off = byte_off + cesu8_len(u);
            match u {
                0x000A => line_starts.push(next_off),
                0x000D => {
                    if units.get(i + 1) == Some(&0x000A) {
                        i += 1;
                        byte_off = next_off + 1;
                        line_starts.push(byte_off);
                        i += 1;
                        continue;
                    }
                    line_starts.push(next_off);
                }
                0x2028 | 0x2029 => line_starts.push(next_off),
                _ => {}
            }
            byte_off = next_off;
            i += 1;
        }
        Self {
            filename: filename.into(),
            units,
            line_starts,
        }
    }

    /// 1-based `(line, byte_column)` for a byte offset, matching QuickJS-ng v0.17.0.
    pub fn line_col(&self, byte_offset: u32) -> (u32, u32) {
        let idx = self
            .line_starts
            .partition_point(|&start| start <= byte_offset)
            .saturating_sub(1);
        let line = (idx as u32) + 1;
        let col = byte_offset.saturating_sub(self.line_starts[idx]) + 1;
        (line, col)
    }
}

fn cesu8_len(u: u16) -> u32 {
    if u < 0x80 {
        1
    } else if u < 0x800 {
        2
    } else {
        3
    }
}

fn compute_line_starts_utf8(bytes: &[u8]) -> Vec<u32> {
    let mut starts = vec![0u32];
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                starts.push((i + 1) as u32);
                i += 1;
            }
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    starts.push((i + 2) as u32);
                    i += 2;
                } else {
                    starts.push((i + 1) as u32);
                    i += 1;
                }
            }
            0xE2 if i + 2 < bytes.len()
                && bytes[i + 1] == 0x80
                && (bytes[i + 2] == 0xA8 || bytes[i + 2] == 0xA9) =>
            {
                starts.push((i + 3) as u32);
                i += 3;
            }
            _ => i += 1,
        }
    }
    starts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    Script,
    Module,
    FunctionBody { is_async: bool, is_generator: bool },
    DirectEval { allow_new_target: bool, allow_super_property: bool, allow_super_call: bool, in_class_field_init: bool },
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

/// Parsed program root (expanded by the `qjs-syntax` worker).
#[derive(Debug, Clone, Default)]
pub struct ParsedProgram {
    pub strict: bool,
    pub span: Span,
}

pub fn parse(_source: &SourceText, _opts: ParseOptions) -> Result<ParsedProgram, ParseError> {
    todo!("implemented by qjs-syntax worker")
}
