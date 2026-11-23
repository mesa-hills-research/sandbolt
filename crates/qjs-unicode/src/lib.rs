//! Pure-Rust port of QuickJS-ng v0.17.0 `libunicode` (Unicode 16 tables,
//! case conversion, canonicalization, normalization, and character ranges).

/// Maximum number of code points emitted by a single case conversion.
pub const CASE_CONV_MAX_LEN: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseConv {
    Upper = 0,
    Lower = 1,
    CaseFold = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnicodeNormalization {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharRangeOp {
    Union,
    Inter,
    Xor,
    Sub,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnicodeError {
    OutOfMemory,
    InvalidProperty,
}

/// Sorted half-open code-point intervals `[start, end)` stored flat as pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CharRange {
    pub points: Vec<u32>,
}

impl CharRange {
    pub fn new() -> Self {
        Self { points: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }

    pub fn add_interval(&mut self, start: u32, end: u32) {
        self.points.push(start);
        self.points.push(end);
    }

    pub fn union_interval(&mut self, c1: u32, c2: u32) {
        let other = [c1, c2.saturating_add(1)];
        self.op_in_place(&other, CharRangeOp::Union);
    }

    pub fn op_in_place(&mut self, other: &[u32], op: CharRangeOp) {
        let mut out = CharRange::new();
        cr_op(&mut out, &self.points, other, op);
        self.points = out.points;
    }

    pub fn invert(&mut self) {
        let _ = self;
        todo!("implemented by qjs-unicode worker")
    }

    pub fn regexp_canonicalize(&mut self, _is_unicode: bool) {
        todo!("implemented by qjs-unicode worker")
    }
}

pub fn cr_op(_out: &mut CharRange, _a: &[u32], _b: &[u32], _op: CharRangeOp) {
    todo!("implemented by qjs-unicode worker")
}

pub fn case_conv(_c: u32, _conv: CaseConv) -> ([u32; CASE_CONV_MAX_LEN], usize) {
    todo!("implemented by qjs-unicode worker")
}

pub fn canonicalize(_c: u32, _is_unicode: bool) -> i32 {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_cased(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_case_ignorable(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_id_start(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_id_continue(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_white_space(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_space(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_js_ident_start(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn is_js_ident_continue(_c: u32) -> bool {
    todo!("implemented by qjs-unicode worker")
}

pub fn normalize(
    _src: &[u32],
    _form: UnicodeNormalization,
) -> Result<Vec<u32>, UnicodeError> {
    todo!("implemented by qjs-unicode worker")
}

pub fn unicode_script(_cr: &mut CharRange, _name: &str, _is_ext: bool) -> Result<(), UnicodeError> {
    todo!("implemented by qjs-unicode worker")
}

pub fn unicode_general_category(_cr: &mut CharRange, _name: &str) -> Result<(), UnicodeError> {
    todo!("implemented by qjs-unicode worker")
}

pub fn unicode_prop(_cr: &mut CharRange, _name: &str) -> Result<(), UnicodeError> {
    todo!("implemented by qjs-unicode worker")
}

pub fn unicode_sequence_prop<F>(
    _name: &str,
    _cb: &mut F,
    _cr: &mut CharRange,
) -> Result<(), UnicodeError>
where
    F: FnMut(&[u32]),
{
    todo!("implemented by qjs-unicode worker")
}
