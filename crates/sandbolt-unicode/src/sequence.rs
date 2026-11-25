use crate::props::{byte, find_name, prop_ranges};
use crate::tables::{
    UNICODE_RGI_EMOJI_TAG_SEQUENCE, UNICODE_RGI_EMOJI_ZWJ_SEQUENCE,
    UNICODE_SEQUENCE_PROP_NAME_TABLE, prop, sequence_prop as kind,
};
use crate::{CharRange, UnicodeError};

const VS16: u32 = 0xfe0f;
const ZWJ: u32 = 0x200d;
const FIRST_REGION: u32 = 0x1f1e6;
const FIRST_SKIN: u32 = 0x1f3fb;
const FIRST_HAIR: u32 = 0x1f9b0;
const SEQ_MAX_LEN: usize = 16;

fn code_points(cr: &CharRange) -> impl Iterator<Item = u32> + '_ {
    cr.points
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|&[start, end]| start..end)
}

fn emit_zwj(cb: &mut impl FnMut(&[u32])) {
    let mut data = UNICODE_RGI_EMOJI_ZWJ_SEQUENCE.as_slice();
    while !data.is_empty() {
        let len = byte(&mut data);
        let mut seq = [0; SEQ_MAX_LEN];
        let mut used = 0;
        let mut modifier_kind = 0;
        let mut modifier_pos = [0; 2];
        let mut modifier_count = 0;
        let mut hair_pos = None;
        for i in 0..len {
            let encoded = byte(&mut data) | (byte(&mut data) << 8);
            let code = encoded & 0x1fff;
            let c = if code < 0x1000 {
                code + 0x2000
            } else {
                code + 0x1e000
            };
            if c == FIRST_HAIR {
                hair_pos = Some(used);
            }
            seq[used] = c;
            used += 1;
            let modifier = (encoded >> 13) & 3;
            if modifier != 0 {
                modifier_kind = modifier;
                modifier_pos[modifier_count] = used;
                modifier_count += 1;
                used += 1;
            }
            if encoded >> 15 != 0 {
                seq[used] = VS16;
                used += 1;
            }
            if i + 1 < len {
                seq[used] = ZWJ;
                used += 1;
            }
        }
        let modifiers = match modifier_kind {
            1 => 5,
            2 => 25,
            3 => 20,
            _ => 1,
        };
        let hair_variants = if hair_pos.is_some() { 4 } else { 1 };
        for hair in 0..hair_variants {
            if let Some(pos) = hair_pos {
                seq[pos] = FIRST_HAIR + hair;
            }
            for modifier in 0..modifiers {
                match modifier_kind {
                    1 => seq[modifier_pos[0]] = FIRST_SKIN + modifier,
                    2 | 3 => {
                        let mut first = modifier / 5;
                        let second = modifier % 5;
                        if modifier_kind == 3 && first >= second {
                            first += 1;
                        }
                        seq[modifier_pos[0]] = FIRST_SKIN + first;
                        seq[modifier_pos[1]] = FIRST_SKIN + second;
                    }
                    _ => {}
                }
                cb(&seq[..used]);
            }
        }
    }
}

fn emit_tag(cb: &mut impl FnMut(&[u32])) {
    for run in UNICODE_RGI_EMOJI_TAG_SEQUENCE
        .split(|&b| b == 0)
        .take_while(|run| !run.is_empty())
    {
        let mut seq = [0; SEQ_MAX_LEN];
        seq[0] = 0x1f3f4;
        for (pos, &tag) in run.iter().enumerate() {
            seq[pos + 1] = 0xe0000 + u32::from(tag);
        }
        seq[run.len() + 1] = 0xe007f;
        cb(&seq[..run.len() + 2]);
    }
}

fn emit(idx: usize, cb: &mut impl FnMut(&[u32]), cr: &mut CharRange) -> Result<(), UnicodeError> {
    match idx {
        kind::BASIC_EMOJI => {
            prop_ranges(cr, prop::BASIC_EMOJI1)?;
            for c in code_points(cr) {
                cb(&[c]);
            }
            cr.clear();
            prop_ranges(cr, prop::BASIC_EMOJI2)?;
            for c in code_points(cr) {
                cb(&[c, VS16]);
            }
        }
        kind::EMOJI_KEYCAP_SEQUENCE => {
            prop_ranges(cr, prop::EMOJI_KEYCAP_SEQUENCE)?;
            for c in code_points(cr) {
                cb(&[c, VS16, 0x20e3]);
            }
        }
        kind::RGI_EMOJI_MODIFIER_SEQUENCE => {
            prop_ranges(cr, prop::EMOJI_MODIFIER_BASE)?;
            for c in code_points(cr) {
                for skin in FIRST_SKIN..FIRST_SKIN + 5 {
                    cb(&[c, skin]);
                }
            }
        }
        kind::RGI_EMOJI_FLAG_SEQUENCE => {
            prop_ranges(cr, prop::RGI_EMOJI_FLAG_SEQUENCE)?;
            for c in code_points(cr) {
                cb(&[FIRST_REGION + c / 26, FIRST_REGION + c % 26]);
            }
        }
        kind::RGI_EMOJI_TAG_SEQUENCE => emit_tag(cb),
        kind::RGI_EMOJI_ZWJ_SEQUENCE => emit_zwj(cb),
        kind::RGI_EMOJI => {
            for idx in kind::BASIC_EMOJI..=kind::RGI_EMOJI_ZWJ_SEQUENCE {
                emit(idx, cb, cr)?;
                cr.clear();
            }
        }
        _ => return Err(UnicodeError::InvalidProperty),
    }
    Ok(())
}

/// Emit each sequence in the named property. Pass an empty `cr` for scratch space.
pub fn unicode_sequence_prop<F>(
    name: &str,
    cb: &mut F,
    cr: &mut CharRange,
) -> Result<(), UnicodeError>
where
    F: FnMut(&[u32]),
{
    emit(find_name(&UNICODE_SEQUENCE_PROP_NAME_TABLE, name)?, cb, cr)
}
