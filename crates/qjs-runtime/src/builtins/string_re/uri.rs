use crate::object::ErrorKind;
use crate::root::{Args, ExecCtx, Handle};
use crate::string::JsString;
use crate::value::{JsResult, StrId, Value};

use super::{
    Builder, MAX_STRING_LEN, STRING_RE_BASE, arg, code_point, global, make_native_fn, string_error,
    string_id, units,
};
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
const URI_ALLOWED: u8 = 1;
const URI_FULL: u8 = 2;
const URI_FLAGS: [u8; 256] = uri_flags();
const UNHEX: [u8; 256] = unhex_table();

const fn uri_flags() -> [u8; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 128 {
        if (i as u8).is_ascii_alphanumeric() {
            table[i] = URI_ALLOWED | URI_FULL;
        }
        i += 1;
    }
    i = 0;
    while i < URI_MARK.len() {
        table[URI_MARK[i] as usize] = URI_ALLOWED | URI_FULL;
        i += 1;
    }
    i = 0;
    while i < URI_RESERVED.len() {
        table[URI_RESERVED[i] as usize] = URI_FULL;
        i += 1;
    }
    table
}

const fn unhex_table() -> [u8; 256] {
    let mut table = [u8::MAX; 256];
    let mut i = 0;
    while i < 16 {
        table[HEX[i] as usize] = i as u8;
        if i >= 10 {
            table[HEX[i] as usize + (b'a' - b'A') as usize] = i as u8;
        }
        i += 1;
    }
    table
}

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
    if op <= 3 && ctx.runtime.vm.interrupt_handler.is_none() && !ctx.heap().string(id).is_wide() {
        ctx.flatten_string(input)?;
        if op <= 1 {
            return encode_latin1(ctx, input, op == 0);
        }
        let bytes = ctx.heap().string(id).as_latin1().unwrap();
        if !bytes.contains(&b'%') {
            return Ok(value);
        }
        if bytes.len() <= 32 {
            return decode_latin1_short(ctx, id, op == 2);
        }
        return decode_latin1_long(ctx, id, op == 2);
    }
    let len = ctx.heap().string(id).len() as usize;
    let mut short = [0; 32];
    let long;
    let input: &[u16] = if let Some(short) = short.get_mut(..len) {
        if let Some(wide) = ctx.heap().string(id).as_utf16() {
            short.copy_from_slice(wide);
        } else {
            for (slot, unit) in short.iter_mut().zip(ctx.heap().string_units(id)) {
                *slot = unit;
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

fn encode_latin1<'s>(ctx: &mut ExecCtx<'_, 's>, source: Handle<'s>, full: bool) -> JsResult<Value> {
    let id = string_id(ctx.get(source));
    let mask = if full { URI_FULL } else { URI_ALLOWED };
    let allowed = |b: u8| URI_FLAGS[usize::from(b)] & mask != 0;
    let bytes = ctx.heap().string(id).as_latin1().unwrap();
    if bytes.iter().copied().all(allowed) {
        return Ok(ctx.get(source));
    }
    let len = bytes.iter().try_fold(0usize, |len, &b| {
        len.checked_add(if allowed(b) {
            1
        } else if b < 128 {
            3
        } else {
            6
        })
    });
    let Some(len) = len.filter(|&len| len <= MAX_STRING_LEN) else {
        return Err(string_error(ctx));
    };
    let result = ctx.make_string(|heap| {
        let bytes = heap.string(id).as_latin1().unwrap();
        JsString::from_latin1_iter(
            &heap.ledger,
            len,
            bytes.iter().flat_map(|&b| {
                let mut out = [0; 6];
                let n = if allowed(b) {
                    out[0] = b;
                    1
                } else if b < 128 {
                    let [a, b] = hex_byte(b);
                    out[..3].copy_from_slice(&[b'%', a, b]);
                    3
                } else {
                    let [a, b1] = hex_byte(0xc0 | (b >> 6));
                    let [c, d] = hex_byte(0x80 | (b & 63));
                    out = [b'%', a, b1, b'%', c, d];
                    6
                };
                out.into_iter().take(n)
            }),
        )
    })?;
    Ok(ctx.get(result))
}

fn unhex(unit: u16) -> Option<u8> {
    let digit = *UNHEX.get(usize::from(unit))?;
    (digit != u8::MAX).then_some(digit)
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

enum Decoded {
    Byte(u8),
    Reserved([u8; 3]),
    Wide(char),
}

impl Decoded {
    fn latin1(self) -> Option<([u8; 3], usize)> {
        match self {
            Self::Byte(b) => Some(([b, 0, 0], 1)),
            Self::Reserved(bytes) => Some((bytes, 3)),
            Self::Wide(_) => None,
        }
    }

    fn utf16(self) -> ([u16; 3], usize) {
        if let Self::Wide(c) = self {
            let mut out = [0; 3];
            let len = c.encode_utf16(&mut out).len();
            return (out, len);
        }
        let (bytes, len) = self.latin1().unwrap();
        (bytes.map(u16::from), len)
    }
}

fn decode_parts(mut bytes: &[u8], full: bool) -> impl Iterator<Item = Option<Decoded>> + '_ {
    fn escaped(input: &[u8]) -> Option<u8> {
        let &[b'%', hi, lo] = input.get(..3)? else {
            return None;
        };
        let hi = UNHEX[usize::from(hi)];
        let lo = UNHEX[usize::from(lo)];
        (hi | lo != u8::MAX).then_some((hi << 4) | lo)
    }
    fn next(bytes: &mut &[u8], full: bool) -> Option<Decoded> {
        let (&first, rest) = bytes.split_first()?;
        if first != b'%' {
            *bytes = rest;
            return Some(Decoded::Byte(first));
        }
        let byte = escaped(bytes)?;
        if byte < 128 {
            let reserved = [first, bytes[1], bytes[2]];
            *bytes = &bytes[3..];
            return Some(if full && URI_FLAGS[usize::from(byte)] == URI_FULL {
                Decoded::Reserved(reserved)
            } else {
                Decoded::Byte(byte)
            });
        }
        let (n, min, mask) = match byte {
            0xc2..=0xdf => (2, 0x80, 0x1f),
            0xe0..=0xef => (3, 0x800, 0xf),
            0xf0..=0xf4 => (4, 0x10000, 7),
            _ => return None,
        };
        let mut point = u32::from(byte & mask);
        let encoded = bytes.get(..3 * n)?;
        for escape in encoded[3..].chunks_exact(3) {
            let byte = escaped(escape)?;
            if byte >> 6 != 2 {
                return None;
            }
            point = (point << 6) | u32::from(byte & 63);
        }
        if point < min {
            return None;
        }
        *bytes = &bytes[3 * n..];
        if point <= 255 {
            Some(Decoded::Byte(point as u8))
        } else {
            char::from_u32(point).map(Decoded::Wide)
        }
    }
    std::iter::from_fn(move || (!bytes.is_empty()).then(|| next(&mut bytes, full)))
}

fn decode_latin1_short(ctx: &mut ExecCtx<'_, '_>, id: StrId, full: bool) -> JsResult<Value> {
    let mut latin = [0; 32];
    let mut utf16 = [0; 32];
    let mut wide = false;
    let mut len = 0;
    let complete =
        decode_parts(ctx.heap().string(id).as_latin1().unwrap(), full).try_for_each(|part| {
            let part = part?;
            if !wide && !matches!(part, Decoded::Wide(_)) {
                let (data, n) = part.latin1().unwrap();
                latin[len..len + n].copy_from_slice(&data[..n]);
                len += n;
            } else {
                if !wide {
                    for (dest, &b) in utf16.iter_mut().zip(&latin[..len]) {
                        *dest = u16::from(b);
                    }
                    wide = true;
                }
                let (data, n) = part.utf16();
                utf16[len..len + n].copy_from_slice(&data[..n]);
                len += n;
            }
            Some(())
        });
    if complete.is_none() {
        return Err(uri_error(ctx));
    }
    let out = if wide {
        ctx.new_utf16(&utf16[..len])?
    } else {
        ctx.new_latin1(&latin[..len])?
    };
    Ok(ctx.get(out))
}

fn decode_latin1_long(ctx: &mut ExecCtx<'_, '_>, id: StrId, full: bool) -> JsResult<Value> {
    let mut wide = false;
    let len =
        decode_parts(ctx.heap().string(id).as_latin1().unwrap(), full).try_fold(0, |len, part| {
            Some(
                len + match part? {
                    Decoded::Byte(_) => 1,
                    Decoded::Reserved(_) => 3,
                    Decoded::Wide(c) => {
                        wide = true;
                        c.len_utf16()
                    }
                },
            )
        });
    let Some(len) = len else {
        return Err(uri_error(ctx));
    };
    let out = ctx.make_string(|heap| {
        let bytes = heap.string(id).as_latin1().unwrap();
        let parts = decode_parts(bytes, full).map(Option::unwrap);
        if wide {
            JsString::from_units(
                &heap.ledger,
                len,
                true,
                parts.flat_map(|p| {
                    let (data, n) = p.utf16();
                    data.into_iter().take(n)
                }),
            )
        } else {
            JsString::from_latin1_iter(
                &heap.ledger,
                len,
                parts.flat_map(|p| {
                    let (data, n) = p.latin1().unwrap();
                    data.into_iter().take(n)
                }),
            )
        }
    })?;
    Ok(ctx.get(out))
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

#[cfg(test)]
mod tests {
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

    #[test]
    fn uri_paths_preserve_reserved_escapes_and_validate_utf8() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let result = engine
            .eval_to_string(
                r#"
            const prefix = 'x'.repeat(40);
            const encoded = prefix + '%2f%C3%A9%F0%9F%98%80';
            let errors = 0;
            for (const input of ['%', '%FC', '%C0%AF', '%ED%A0%80', '%F4%90%80%80', '%0q']) {
                for (const lead of ['', prefix, 'λ']) {
                    try { decodeURI(lead + input); }
                    catch (e) { if (e instanceof URIError) ++errors; }
                }
            }
            [decodeURI('%2f%C3%A9'), decodeURIComponent('%2f%C3%A9'),
             decodeURI(encoded).slice(40), decodeURIComponent(prefix + '%00').charCodeAt(40),
             encodeURI('/é ?'), encodeURIComponent('/é ?'),
             decodeURI('λ%2f%C3%A9'), encodeURIComponent('\u0000'), errors].join('|');
        "#,
                EvalOptions::default(),
            )
            .unwrap();
        assert_eq!(
            result,
            "%2fé|/é|%2fé😀|0|/%C3%A9%20?|%2F%C3%A9%20%3F|λ%2fé|%00|18"
        );
    }
}
