//! Unicode utilities from QuickJS-ng v0.17.0.

#![deny(unsafe_code)]

mod case;
mod normalize;
mod props;
mod ranges;
mod sequence;
mod tables;

pub use case::{canonicalize, case_conv, is_cased};
pub use normalize::normalize;
pub use props::{
    is_case_ignorable, is_id_continue, is_id_start, is_js_ident_continue, is_js_ident_start,
    is_space, is_white_space, unicode_general_category, unicode_prop, unicode_script,
};
pub use ranges::{CharRange, cr_op};
pub use sequence::unicode_sequence_prop;

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

impl From<std::collections::TryReserveError> for UnicodeError {
    fn from(_: std::collections::TryReserveError) -> Self {
        Self::OutOfMemory
    }
}

#[cfg(test)]
mod tests;
