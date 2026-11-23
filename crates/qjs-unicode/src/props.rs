use crate::case::{CASE_F, CASE_L, CASE_U, case_ranges};
use crate::ranges::try_cr_op;
use crate::tables::{self, gc, prop, script};
use crate::{CharRange, CharRangeOp, UnicodeError};

pub(crate) fn byte(data: &mut &[u8]) -> u32 {
    let (b, rest) = data.split_first().expect("truncated Unicode table");
    *data = rest;
    u32::from(*b)
}

fn be16(data: &mut &[u8]) -> u32 {
    (byte(data) << 8) | byte(data)
}

fn read_le24(index: &[u8], pos: usize) -> u32 {
    let bytes = &index[3 * pos..][..3];
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

/// Find the run boundary and byte position at the start of an indexed block.
pub(crate) fn indexed_offset(c: u32, index: &[u8]) -> Option<(u32, usize)> {
    const CODE_MASK: u32 = (1 << 21) - 1;
    const BLOCK_LEN: usize = 32;
    let count = index.len() / 3;
    if c < read_le24(index, 0) & CODE_MASK {
        return Some((0, 0));
    }
    let mut low = 0;
    let mut high = count - 1;
    if c >= read_le24(index, high) {
        return None;
    }
    while high - low > 1 {
        let mid = (high + low) / 2;
        if c < read_le24(index, mid) & CODE_MASK {
            high = mid;
        } else {
            low = mid;
        }
    }
    let entry = read_le24(index, low);
    Some((
        entry & CODE_MASK,
        (low + 1) * BLOCK_LEN + (entry >> 21) as usize,
    ))
}

struct BinaryRuns<'a> {
    data: &'a [u8],
    second: Option<u32>,
}

impl<'a> BinaryRuns<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, second: None }
    }
}

impl Iterator for BinaryRuns<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if let Some(n) = self.second.take() {
            return Some(n);
        }
        if self.data.is_empty() {
            return None;
        }
        let n = match byte(&mut self.data) {
            b @ 0..=0x3f => {
                self.second = Some((b & 7) + 1);
                b >> 3
            }
            b @ 0x40..=0x5f => ((b - 0x40) << 8) | byte(&mut self.data),
            b @ 0x60..=0x7f => ((b - 0x60) << 16) | be16(&mut self.data),
            b => b - 0x80,
        };
        Some(n + 1)
    }
}

pub(crate) fn in_table(c: u32, table: &[u8], index: &[u8]) -> bool {
    let Some((mut start, pos)) = indexed_offset(c, index) else {
        return false;
    };
    for (i, len) in BinaryRuns::new(&table[pos..]).enumerate() {
        start += len;
        if c < start {
            return i & 1 != 0;
        }
    }
    false
}

pub fn is_case_ignorable(c: u32) -> bool {
    in_table(
        c,
        &tables::UNICODE_PROP_CASE_IGNORABLE_TABLE,
        &tables::UNICODE_PROP_CASE_IGNORABLE_INDEX,
    )
}

pub fn is_id_start(c: u32) -> bool {
    in_table(
        c,
        &tables::UNICODE_PROP_ID_START_TABLE,
        &tables::UNICODE_PROP_ID_START_INDEX,
    )
}

pub fn is_id_continue(c: u32) -> bool {
    is_id_start(c)
        || in_table(
            c,
            &tables::UNICODE_PROP_ID_CONTINUE1_TABLE,
            &tables::UNICODE_PROP_ID_CONTINUE1_INDEX,
        )
}

/// Test the Unicode `White_Space` property.
pub fn is_white_space(c: u32) -> bool {
    in_table(
        c,
        &tables::UNICODE_PROP_WHITE_SPACE_TABLE,
        &tables::UNICODE_PROP_WHITE_SPACE_INDEX,
    )
}

/// Test ECMAScript white space or a line terminator.
pub fn is_space(c: u32) -> bool {
    matches!(c, 9..=13 | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028..=0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
}

pub fn is_js_ident_start(c: u32) -> bool {
    if c < 128 {
        (c as u8).is_ascii_alphabetic() || matches!(c, 0x24 | 0x5f)
    } else {
        is_id_start(c)
    }
}

pub fn is_js_ident_continue(c: u32) -> bool {
    if c < 128 {
        is_js_ident_start(c) || (c as u8).is_ascii_digit()
    } else {
        is_id_continue(c) || matches!(c, 0x200c | 0x200d)
    }
}

pub(crate) fn find_name(table: &[u8], name: &str) -> Result<usize, UnicodeError> {
    table
        .split(|&b| b == 0)
        .take_while(|group| !group.is_empty())
        .position(|group| {
            group
                .split(|&b| b == b',')
                .any(|alias| alias == name.as_bytes())
        })
        .ok_or(UnicodeError::InvalidProperty)
}

fn run_128(data: &mut &[u8]) -> u32 {
    match byte(data) {
        n @ 0..=127 => n,
        n @ 128..=191 => 128 + (((n - 128) << 8) | byte(data)),
        n => 128 + (1 << 14) + (((n - 192) << 16) | be16(data)),
    }
}

fn script_ranges(out: &mut CharRange, idx: u8) -> Result<(), UnicodeError> {
    let mut data = tables::UNICODE_SCRIPT_TABLE.as_slice();
    let mut start = 0;
    while !data.is_empty() {
        let b = byte(&mut data);
        let len = match b & 0x7f {
            n @ 0..=95 => n,
            n @ 96..=111 => 96 + (((n - 96) << 8) | byte(&mut data)),
            n => 96 + (1 << 12) + (((n - 112) << 16) | be16(&mut data)),
        } + 1;
        let end = start + len;
        if b >> 7 != 0 {
            let value = byte(&mut data) as u8;
            if value == idx || idx == script::UNKNOWN {
                out.try_add_interval(start, end)?;
            }
        }
        start = end;
    }
    if idx == script::UNKNOWN {
        out.try_invert()?;
    }
    Ok(())
}

/// Fill an empty range with the named script or script extensions.
pub fn unicode_script(cr: &mut CharRange, name: &str, is_ext: bool) -> Result<(), UnicodeError> {
    let idx = match name {
        "Unknown" | "Zzzz" => script::UNKNOWN,
        _ => script::UNKNOWN + 1 + find_name(&tables::UNICODE_SCRIPT_NAME_TABLE, name)? as u8,
    };
    if !is_ext {
        return script_ranges(cr, idx);
    }
    let mut base = CharRange::new();
    let mut ext = CharRange::new();
    script_ranges(&mut base, idx)?;
    let common = matches!(idx, script::COMMON | script::INHERITED);
    let mut data = tables::UNICODE_SCRIPT_EXT_TABLE.as_slice();
    let mut start = 0;
    while !data.is_empty() {
        let end = start + run_128(&mut data) + 1;
        let count = byte(&mut data) as usize;
        let scripts;
        (scripts, data) = data.split_at(count);
        if (common && count != 0) || scripts.contains(&idx) {
            ext.try_add_interval(start, end)?;
        }
        start = end;
    }
    // Common and Inherited lose any code point with explicit extensions.
    let op = if common {
        CharRangeOp::Sub
    } else {
        CharRangeOp::Union
    };
    try_cr_op(cr, &base.points, &ext.points, op)
}

macro_rules! mask {
    ($($cat:ident)|+) => { $( (1u32 << gc::$cat) )|+ };
}

const LETTERS: u32 = mask!(LU | LL | LT | LM | LO);
const CASED_LETTERS: u32 = mask!(LU | LL | LT);
const OTHERS: u32 = mask!(CC | CF | CS | CO | CN);
const ID_START: u32 = LETTERS | mask!(NL);
const ID_CONTINUE: u32 = ID_START | mask!(MN | MC | ND | PC);
const GC_MASKS: [u32; 8] = [
    CASED_LETTERS,
    LETTERS,
    mask!(MN | MC | ME),
    mask!(ND | NL | NO),
    mask!(SM | SC | SK | SO),
    mask!(PC | PD | PS | PE | PI | PF | PO),
    mask!(ZS | ZL | ZP),
    OTHERS,
];

fn category_ranges(cr: &mut CharRange, gc_mask: u32) -> Result<(), UnicodeError> {
    const UPPER_LOWER: u32 = mask!(LU | LL);
    let mut data = tables::UNICODE_GC_TABLE.as_slice();
    let mut start = 0;
    while !data.is_empty() {
        let b = byte(&mut data);
        let len = if b >> 5 == 7 {
            7 + run_128(&mut data)
        } else {
            b >> 5
        } + 1;
        let value = b & 0x1f;
        let end = start + len;
        if value == 31 {
            match gc_mask & UPPER_LOWER {
                0 => {}
                UPPER_LOWER => cr.try_add_interval(start, end)?,
                which => {
                    let offset = u32::from(which == mask!(LL));
                    for c in (start + offset..end).step_by(2) {
                        cr.try_add_interval(c, c + 1)?;
                    }
                }
            }
        } else if gc_mask & (1 << value) != 0 {
            cr.try_add_interval(start, end)?;
        }
        start = end;
    }
    Ok(())
}

/// Fill an empty range with the named general category.
pub fn unicode_general_category(cr: &mut CharRange, name: &str) -> Result<(), UnicodeError> {
    let idx = find_name(&tables::UNICODE_GC_NAME_TABLE, name)?;
    let gc_mask = if idx < gc::LC {
        1 << idx
    } else {
        GC_MASKS[idx - gc::LC]
    };
    category_ranges(cr, gc_mask)
}

pub(crate) fn prop_ranges(cr: &mut CharRange, idx: usize) -> Result<(), UnicodeError> {
    let data = tables::UNICODE_PROP_TABLE[idx];
    let len = usize::from(tables::UNICODE_PROP_LEN_TABLE[idx]);
    let mut start = 0;
    for (i, len) in BinaryRuns::new(&data[..len]).enumerate() {
        let end = start + len;
        if i & 1 != 0 {
            cr.try_add_interval(start, end)?;
        }
        start = end;
    }
    Ok(())
}

fn add_props(range: &mut CharRange, props: &[usize]) -> Result<(), UnicodeError> {
    let mut rhs = CharRange::new();
    for &id in props {
        rhs.clear();
        prop_ranges(&mut rhs, id)?;
        range.try_op_in_place(&rhs.points, CharRangeOp::Union)?;
    }
    Ok(())
}

fn category_with_props(gc_mask: u32, props: &[usize]) -> Result<CharRange, UnicodeError> {
    let mut range = CharRange::new();
    category_ranges(&mut range, gc_mask)?;
    add_props(&mut range, props)?;
    Ok(range)
}

fn xid_ranges(is_start: bool) -> Result<CharRange, UnicodeError> {
    let (base, exceptions) = if is_start {
        (
            category_with_props(ID_START, &[prop::OTHER_ID_START])?,
            prop::XID_START1,
        )
    } else {
        (
            category_with_props(
                ID_CONTINUE,
                &[prop::OTHER_ID_START, prop::OTHER_ID_CONTINUE],
            )?,
            prop::XID_CONTINUE1,
        )
    };
    let mut excluded = CharRange::new();
    prop_ranges(&mut excluded, prop::PATTERN_SYNTAX)?;
    add_props(&mut excluded, &[prop::PATTERN_WHITE_SPACE, exceptions])?;
    excluded.try_invert()?;
    let mut result = CharRange::new();
    try_cr_op(
        &mut result,
        &base.points,
        &excluded.points,
        CharRangeOp::Inter,
    )?;
    Ok(result)
}

fn case_change(base: u32, exceptions: usize) -> Result<CharRange, UnicodeError> {
    let mut range = case_ranges(base)?;
    let mut rhs = CharRange::new();
    prop_ranges(&mut rhs, exceptions)?;
    range.try_op_in_place(&rhs.points, CharRangeOp::Xor)?;
    Ok(range)
}

/// Fill an empty range with the named binary property.
pub fn unicode_prop(cr: &mut CharRange, name: &str) -> Result<(), UnicodeError> {
    let idx = prop::ASCII_HEX_DIGIT + find_name(&tables::UNICODE_PROP_NAME_TABLE, name)?;
    *cr = match idx {
        prop::ASCII => return cr.try_add_interval(0, 128),
        prop::ANY => return cr.try_add_interval(0, 0x110000),
        prop::ASSIGNED => {
            let mut range = category_with_props(mask!(CN), &[])?;
            range.try_invert()?;
            range
        }
        prop::MATH => category_with_props(mask!(SM), &[prop::OTHER_MATH])?,
        prop::LOWERCASE => category_with_props(mask!(LL), &[prop::OTHER_LOWERCASE])?,
        prop::UPPERCASE => category_with_props(mask!(LU), &[prop::OTHER_UPPERCASE])?,
        prop::CASED => category_with_props(
            CASED_LETTERS,
            &[prop::OTHER_UPPERCASE, prop::OTHER_LOWERCASE],
        )?,
        prop::ALPHABETIC => category_with_props(
            ID_START,
            &[
                prop::OTHER_UPPERCASE,
                prop::OTHER_LOWERCASE,
                prop::OTHER_ALPHABETIC,
            ],
        )?,
        prop::GRAPHEME_BASE => {
            let mut range = category_with_props(
                OTHERS | mask!(ZL | ZP | ME | MN),
                &[prop::OTHER_GRAPHEME_EXTEND],
            )?;
            range.try_invert()?;
            range
        }
        prop::GRAPHEME_EXTEND => {
            category_with_props(mask!(ME | MN), &[prop::OTHER_GRAPHEME_EXTEND])?
        }
        prop::XID_START => xid_ranges(true)?,
        prop::XID_CONTINUE => xid_ranges(false)?,
        prop::CHANGES_WHEN_UPPERCASED => case_ranges(CASE_U)?,
        prop::CHANGES_WHEN_LOWERCASED => case_ranges(CASE_L)?,
        prop::CHANGES_WHEN_CASEMAPPED => case_ranges(CASE_U | CASE_L | CASE_F)?,
        prop::CHANGES_WHEN_TITLECASED => case_change(CASE_U, prop::CHANGES_WHEN_TITLECASED1)?,
        prop::CHANGES_WHEN_CASEFOLDED => case_change(CASE_F, prop::CHANGES_WHEN_CASEFOLDED1)?,
        prop::CHANGES_WHEN_NFKC_CASEFOLDED => {
            case_change(CASE_F, prop::CHANGES_WHEN_NFKC_CASEFOLDED1)?
        }
        prop::ID_CONTINUE => {
            let mut range = CharRange::new();
            prop_ranges(&mut range, prop::ID_START)?;
            let mut rhs = CharRange::new();
            prop_ranges(&mut rhs, prop::ID_CONTINUE1)?;
            range.try_op_in_place(&rhs.points, CharRangeOp::Xor)?;
            range
        }
        idx if idx < tables::UNICODE_PROP_TABLE.len() => return prop_ranges(cr, idx),
        _ => return Err(UnicodeError::InvalidProperty),
    };
    Ok(())
}
