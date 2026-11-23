//! Pure-Rust port of QuickJS-ng v0.17.0 `dtoa` (exact ECMAScript float64
//! formatting and parsing, plus fast integer-to-ASCII helpers).

pub const DTOA_MAX_DIGITS: usize = 101;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtoaFormat {
    Free = 0,
    Fixed = 1,
    Frac = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtoaExp {
    Auto = 0,
    Enabled = 1,
    Disabled = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtoaOptions {
    pub format: DtoaFormat,
    pub exp: DtoaExp,
    pub minus_zero: bool,
}

impl Default for DtoaOptions {
    fn default() -> Self {
        Self {
            format: DtoaFormat::Free,
            exp: DtoaExp::Auto,
            minus_zero: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtodFlags {
    pub int_only: bool,
    pub accept_bin_oct: bool,
    pub accept_legacy_octal: bool,
    pub accept_underscores: bool,
}

pub fn dtoa_max_len(_d: f64, _radix: u32, _n_digits: usize, _opts: DtoaOptions) -> usize {
    todo!("implemented by qjs-dtoa worker")
}

pub fn dtoa(_d: f64, _radix: u32, _n_digits: usize, _opts: DtoaOptions) -> String {
    todo!("implemented by qjs-dtoa worker")
}

/// Parses an ECMAScript number from `input`, returning `(value, bytes_consumed)`.
pub fn atod(_input: &str, _radix: u32, _flags: AtodFlags) -> (f64, usize) {
    todo!("implemented by qjs-dtoa worker")
}

pub fn u32toa(buf: &mut [u8], _n: u32) -> usize {
    let _ = buf;
    todo!("implemented by qjs-dtoa worker")
}

pub fn i32toa(buf: &mut [u8], _n: i32) -> usize {
    let _ = buf;
    todo!("implemented by qjs-dtoa worker")
}

pub fn u64toa(buf: &mut [u8], _n: u64) -> usize {
    let _ = buf;
    todo!("implemented by qjs-dtoa worker")
}

pub fn i64toa(buf: &mut [u8], _n: i64) -> usize {
    let _ = buf;
    todo!("implemented by qjs-dtoa worker")
}

pub fn u64toa_radix(buf: &mut [u8], _n: u64, _radix: u32) -> usize {
    let _ = buf;
    todo!("implemented by qjs-dtoa worker")
}

pub fn i64toa_radix(buf: &mut [u8], _n: i64, _radix: u32) -> usize {
    let _ = buf;
    todo!("implemented by qjs-dtoa worker")
}
