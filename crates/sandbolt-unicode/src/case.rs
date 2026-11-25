use std::cmp::Ordering;

use crate::props::in_table;
use crate::tables::{
    CASE_CONV_EXT, CASE_CONV_TABLE1, CASE_CONV_TABLE2, UNICODE_PROP_CASED1_INDEX,
    UNICODE_PROP_CASED1_TABLE,
};
use crate::{CASE_CONV_MAX_LEN, CaseConv, CharRange, UnicodeError};

const U: u32 = 0;
const L: u32 = 1;
const UF: u32 = 2;
const LF: u32 = 3;
const UL: u32 = 4;
const LSU: u32 = 5;
const U2L_399_EXT2: u32 = 6;
const UF_D20: u32 = 7;
const UF_D1_EXT: u32 = 8;
const U_EXT: u32 = 9;
const LF_EXT: u32 = 10;
const UF_EXT2: u32 = 11;
const LF_EXT2: u32 = 12;
const UF_EXT3: u32 = 13;

pub(crate) const CASE_U: u32 = 1 << CaseConv::Upper as u32;
pub(crate) const CASE_L: u32 = 1 << CaseConv::Lower as u32;
pub(crate) const CASE_F: u32 = 1 << CaseConv::CaseFold as u32;

#[derive(Clone, Copy)]
struct Run(u32);

impl Run {
    fn start(self) -> u32 {
        self.0 >> 15
    }

    fn len(self) -> u32 {
        (self.0 >> 8) & 0x7f
    }

    fn kind(self) -> u32 {
        (self.0 >> 4) & 0xf
    }

    fn data(self, idx: usize) -> usize {
        (((self.0 & 0xf) as usize) << 8) | usize::from(CASE_CONV_TABLE2[idx])
    }
}

fn find_run(c: u32) -> Option<usize> {
    CASE_CONV_TABLE1
        .binary_search_by(|&entry| {
            let run = Run(entry);
            if c < run.start() {
                Ordering::Greater
            } else if c >= run.start() + run.len() {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .ok()
}

fn ext(idx: usize) -> u32 {
    u32::from(CASE_CONV_EXT[idx])
}

fn convert_entry(mut c: u32, conv: CaseConv, idx: usize) -> ([u32; CASE_CONV_MAX_LEN], usize) {
    let run = Run(CASE_CONV_TABLE1[idx]);
    let offset = c - run.start();
    let kind = run.kind();
    let data = run.data(idx);
    let is_lower = conv != CaseConv::Upper;
    let fold = conv == CaseConv::CaseFold;
    match kind {
        U | L | UF | LF => {
            if conv as u32 == kind & 1 || (kind >= UF && fold) {
                c = offset + Run(CASE_CONV_TABLE1[data]).start();
            }
        }
        UL => {
            if offset & 1 == u32::from(!is_lower) {
                c = run.start() + (offset ^ 1);
            }
        }
        LSU => {
            let step = if offset == 1 {
                1
            } else if offset == 2 * u32::from(!is_lower) {
                2
            } else {
                0
            };
            c = if is_lower { c + step } else { c - step };
        }
        U2L_399_EXT2 => {
            if !is_lower {
                return ([offset + ext(data >> 6), 0x399, 0], 2);
            }
            c = offset + ext(data & 0x3f);
        }
        UF_D20 if conv != CaseConv::Lower => c = data as u32 + u32::from(fold) * 0x20,
        UF_D1_EXT if conv != CaseConv::Lower => c = ext(data) + u32::from(fold),
        U_EXT | LF_EXT if u32::from(is_lower) == kind - U_EXT => c = ext(data),
        LF_EXT2 if is_lower => return ([offset + ext(data >> 6), ext(data & 0x3f), 0], 2),
        UF_EXT2 | UF_EXT3 if conv != CaseConv::Lower => {
            let (mut result, len) = if kind == UF_EXT2 {
                ([offset + ext(data >> 6), ext(data & 0x3f), 0], 2)
            } else {
                ([ext(data >> 8), ext((data >> 4) & 0xf), ext(data & 0xf)], 3)
            };
            if fold {
                for cp in &mut result[..len] {
                    *cp = case_conv(*cp, CaseConv::Lower).0[0];
                }
            }
            return (result, len);
        }
        _ => {}
    }
    ([c, 0, 0], 1)
}

/// Convert one code point. The returned length selects the mapping prefix.
pub fn case_conv(c: u32, conv: CaseConv) -> ([u32; CASE_CONV_MAX_LEN], usize) {
    if c < 128 {
        let byte = c as u8;
        let result = match conv {
            CaseConv::Upper => byte.to_ascii_uppercase(),
            CaseConv::Lower | CaseConv::CaseFold => byte.to_ascii_lowercase(),
        };
        ([result.into(), 0, 0], 1)
    } else if let Some(idx) = find_run(c) {
        convert_entry(c, conv, idx)
    } else {
        ([c, 0, 0], 1)
    }
}

/// Map one code point according to ECMAScript RegExp's ignore-case rules.
pub fn canonicalize(c: u32, is_unicode: bool) -> i32 {
    let conv = if is_unicode {
        CaseConv::CaseFold
    } else {
        CaseConv::Upper
    };
    let (result, len) = case_conv(c, conv);
    if len == 1 && (is_unicode || c < 128 || result[0] >= 128) {
        result[0] as i32
    } else if is_unicode {
        // These full folds also have a distinct single-code-point fold.
        match c {
            0xfb06 => 0xfb05,
            0x1fd3 => 0x390,
            0x1fe3 => 0x3b0,
            _ => c as i32,
        }
    } else {
        c as i32
    }
}

pub fn is_cased(c: u32) -> bool {
    find_run(c).is_some() || in_table(c, &UNICODE_PROP_CASED1_TABLE, &UNICODE_PROP_CASED1_INDEX)
}

const fn kinds(types: &[u32]) -> u32 {
    let mut mask = 0;
    let mut idx = 0;
    while idx < types.len() {
        mask |= 1 << types[idx];
        idx += 1;
    }
    mask
}

pub(crate) fn case_ranges(mask: u32) -> Result<CharRange, UnicodeError> {
    const MASKS: [u32; 3] = [
        kinds(&[
            U,
            UF,
            UL,
            LSU,
            U2L_399_EXT2,
            UF_D20,
            UF_D1_EXT,
            U_EXT,
            UF_EXT2,
            UF_EXT3,
        ]),
        kinds(&[L, LF, UL, LSU, U2L_399_EXT2, LF_EXT, LF_EXT2]),
        kinds(&[
            UF,
            LF,
            UL,
            LSU,
            U2L_399_EXT2,
            LF_EXT,
            LF_EXT2,
            UF_D20,
            UF_D1_EXT,
            UF_EXT2,
            UF_EXT3,
        ]),
    ];
    let mut selected = 0;
    for (i, run_mask) in MASKS.into_iter().enumerate() {
        if mask & (1 << i) != 0 {
            selected |= run_mask;
        }
    }
    let mut range = CharRange::new();
    let upper = mask & CASE_U != 0;
    let lower = mask & (CASE_L | CASE_F) != 0;
    for entry in CASE_CONV_TABLE1 {
        let run = Run(entry);
        let start = run.start();
        let len = run.len();
        let kind = run.kind();
        if selected & (1 << kind) == 0 {
            continue;
        }
        match kind {
            UL if !(upper && lower) => {
                let first = start + u32::from(upper);
                for offset in (0..len).step_by(2) {
                    range.try_add_interval(first + offset, first + offset + 1)?;
                }
            }
            LSU if !(upper && lower) => {
                if !upper {
                    range.try_add_interval(start, start + 1)?;
                }
                range.try_add_interval(start + 1, start + 2)?;
                if upper {
                    range.try_add_interval(start + 2, start + 3)?;
                }
            }
            _ => range.try_add_interval(start, start + len)?,
        }
    }
    Ok(range)
}
