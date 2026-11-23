use crate::object::ErrorKind;
use crate::root::{Args, ExecCtx};
use crate::value::{JsResult, Value};

use super::{Builder, STRING_RE_BASE, arg, code_point, global, make_native_fn, string_id, units};
use crate::ops::convert::to_string;
use crate::realm::NativeFnId;

const NAMES: [&str; 6] = [
    "encodeURI",
    "encodeURIComponent",
    "decodeURI",
    "decodeURIComponent",
    "escape",
    "unescape",
];
const URI_MARK: &[u8] = b"-_.!~*'()";
const URI_RESERVED: &[u8] = b";/?:@&=+$,#";
const ESCAPE_SAFE: &[u8] = b"@*_+-./";
const HEX: &[u8] = b"0123456789ABCDEF";

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    for (offset, name) in NAMES.into_iter().enumerate() {
        ctx.with_scope(|ctx| {
            let f = make_native_fn(
                ctx,
                NativeFnId(STRING_RE_BASE + 0x100 + offset as u16),
                name,
                1,
                false,
            )?;
            global(ctx, name, f)
        })?;
    }
    Ok(())
}

pub(super) fn dispatch<'s>(op: u16, ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let input = arg(ctx, args, 0)?;
    let input = to_string(ctx, input)?;
    let value = ctx.get(input);
    let id = string_id(value);
    let len = ctx.heap().string(id).len() as usize;
    let mut short = [0; 32];
    let long;
    let input: &[u16] = if let Some(short) = short.get_mut(..len) {
        match ctx.heap().string(id) {
            crate::string::JsString::Latin1 { data, .. } => {
                for (slot, &byte) in short.iter_mut().zip(data.iter()) {
                    *slot = u16::from(byte);
                }
            }
            crate::string::JsString::Utf16(data) => short.copy_from_slice(data),
            crate::string::JsString::Rope { .. } => {
                for (slot, unit) in short.iter_mut().zip(ctx.heap().string_units(id)) {
                    *slot = unit;
                }
            }
        }
        short
    } else {
        long = units(ctx, value)?;
        &long
    };
    match op {
        0 | 1 => encode(ctx, input, op == 0),
        2 | 3 => decode(ctx, input, op == 2),
        4 => escape(ctx, input),
        _ => unescape(ctx, input),
    }
}

fn allowed(cp: u32, marks: &[u8]) -> bool {
    u8::try_from(cp).is_ok_and(|b| b.is_ascii_alphanumeric() || marks.contains(&b))
}

fn uri_error(ctx: &mut ExecCtx<'_, '_>) -> crate::Abrupt {
    ctx.throw_error(ErrorKind::Uri, "malformed URI")
}

fn hex_byte(byte: u8) -> [u8; 2] {
    [HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]
}

fn encode(ctx: &mut ExecCtx<'_, '_>, units: &[u16], full: bool) -> JsResult<Value> {
    let mut out = Builder::new();
    let mut i = 0;
    while i < units.len() {
        if i > 0 && (i & 2047) < 2 {
            ctx.check_interrupt()?;
        }
        let (cp, end) = code_point(units, i);
        i = end;
        if allowed(cp, URI_MARK) || (full && cp < 128 && URI_RESERVED.contains(&(cp as u8))) {
            out.unit(ctx, cp as u16)?;
        } else {
            let c = char::from_u32(cp).ok_or_else(|| uri_error(ctx))?;
            for &b in c.encode_utf8(&mut [0; 4]).as_bytes() {
                let [a, b] = hex_byte(b);
                out.ascii(ctx, &[b'%', a, b])?;
            }
        }
    }
    out.value(ctx)
}

fn unhex(unit: u16) -> Option<u8> {
    match unit {
        0x30..=0x39 => Some(unit as u8 - b'0'),
        0x41..=0x46 => Some(unit as u8 - b'A' + 10),
        0x61..=0x66 => Some(unit as u8 - b'a' + 10),
        _ => None,
    }
}

fn hex_value(units: &[u16]) -> Option<u16> {
    units
        .iter()
        .try_fold(0, |val, &unit| Some((val << 4) | u16::from(unhex(unit)?)))
}

fn percent_byte(input: &[u16]) -> Option<u8> {
    if input.first() == Some(&u16::from(b'%')) {
        Some(hex_value(input.get(1..3)?)? as u8)
    } else {
        None
    }
}

fn decode(ctx: &mut ExecCtx<'_, '_>, units: &[u16], full: bool) -> JsResult<Value> {
    if units.len() <= 32 {
        return decode_short(ctx, units, full);
    }
    let mut out = Builder::new();
    let mut i = 0;
    while i < units.len() {
        if i > 0 && (i & 2047) < 12 {
            ctx.check_interrupt()?;
        }
        if units[i] != u16::from(b'%') {
            out.unit(ctx, units[i])?;
            i += 1;
            continue;
        }
        let first = percent_byte(&units[i..]).ok_or_else(|| uri_error(ctx))?;
        let n = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(uri_error(ctx)),
        };
        if n == 1 && full && URI_RESERVED.contains(&first) {
            out.extend(ctx, &units[i..i + 3])?;
        } else {
            let mut bytes = [first; 4];
            for (j, byte) in bytes.iter_mut().enumerate().take(n).skip(1) {
                let rest = units.get(i + 3 * j..).ok_or_else(|| uri_error(ctx))?;
                *byte = percent_byte(rest).ok_or_else(|| uri_error(ctx))?;
            }
            let decoded = std::str::from_utf8(&bytes[..n]).map_err(|_| uri_error(ctx))?;
            for c in decoded.chars() {
                out.code_point(ctx, c as u32)?;
            }
        }
        i += 3 * n;
    }
    out.value(ctx)
}

fn decode_short(ctx: &mut ExecCtx<'_, '_>, units: &[u16], full: bool) -> JsResult<Value> {
    let mut buf = [0u16; 32];
    let mut out_len = 0usize;
    let mut i = 0;
    while i < units.len() {
        if units[i] != u16::from(b'%') {
            buf[out_len] = units[i];
            out_len += 1;
            i += 1;
            continue;
        }
        let first = percent_byte(&units[i..]).ok_or_else(|| uri_error(ctx))?;
        let n = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(uri_error(ctx)),
        };
        if n == 1 && full && URI_RESERVED.contains(&first) {
            buf[out_len..out_len + 3].copy_from_slice(&units[i..i + 3]);
            out_len += 3;
        } else {
            let mut bytes = [first; 4];
            for (j, byte) in bytes.iter_mut().enumerate().take(n).skip(1) {
                let rest = units.get(i + 3 * j..).ok_or_else(|| uri_error(ctx))?;
                *byte = percent_byte(rest).ok_or_else(|| uri_error(ctx))?;
            }
            let decoded = std::str::from_utf8(&bytes[..n]).map_err(|_| uri_error(ctx))?;
            for c in decoded.chars() {
                let mut enc = [0u16; 2];
                let slice = c.encode_utf16(&mut enc);
                buf[out_len..out_len + slice.len()].copy_from_slice(slice);
                out_len += slice.len();
            }
        }
        i += 3 * n;
    }
    ctx.new_utf16(&buf[..out_len]).map(|s| ctx.get(s))
}

fn escape(ctx: &mut ExecCtx<'_, '_>, units: &[u16]) -> JsResult<Value> {
    let mut out = Builder::new();
    for (i, &unit) in units.iter().enumerate() {
        if i & 2047 == 0 {
            ctx.check_interrupt()?;
        }
        if allowed(u32::from(unit), ESCAPE_SAFE) {
            out.unit(ctx, unit)?;
        } else {
            let [a, b] = hex_byte(unit as u8);
            if unit < 256 {
                out.ascii(ctx, &[b'%', a, b])?;
            } else {
                let [c, d] = hex_byte((unit >> 8) as u8);
                out.ascii(ctx, &[b'%', b'u', c, d, a, b])?;
            }
        }
    }
    out.value(ctx)
}

fn unescape(ctx: &mut ExecCtx<'_, '_>, units: &[u16]) -> JsResult<Value> {
    let mut out = Builder::new();
    let mut i = 0;
    while i < units.len() {
        if i & 2047 < 6 {
            ctx.check_interrupt()?;
        }
        if units[i] == u16::from(b'%') {
            if units.get(i + 1) == Some(&u16::from(b'u'))
                && let Some(cp) = units.get(i + 2..i + 6).and_then(hex_value)
            {
                out.unit(ctx, cp)?;
                i += 6;
                continue;
            }
            if let Some(byte) = percent_byte(&units[i..]) {
                out.unit(ctx, u16::from(byte))?;
                i += 3;
                continue;
            }
        }
        out.unit(ctx, units[i])?;
        i += 1;
    }
    out.value(ctx)
}
