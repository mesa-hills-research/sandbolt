use super::*;
use crate::ledger::TrackedVec;
use crate::object::ErrorKind;
use crate::string::StringUnits;
use crate::value::{StrId, ValueKind};

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const HEX: &[u8; 16] = b"0123456789abcdef";

pub(super) fn install<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    ctor: Handle<'s>,
    proto: Handle<'s>,
) -> JsResult<()> {
    methods(
        ctx,
        ctor,
        ENCODING_START,
        &[("fromBase64", 1), ("fromHex", 1)],
    )?;
    methods(
        ctx,
        proto,
        ENCODING_START + 2,
        &[
            ("toBase64", 0),
            ("toHex", 0),
            ("setFromBase64", 1),
            ("setFromHex", 1),
        ],
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    Loose,
    Strict,
    Stop,
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let view = if op >= 2 {
        let view = ctx
            .get(this)
            .as_object()
            .and_then(|obj| typed::data(ctx.heap(), obj))
            .filter(|v| v.kind == TypedArrayKind::Uint8);
        Some(view.ok_or_else(|| ctx.throw_type_error("Expected a Uint8Array"))?)
    } else {
        None
    };
    let writing_string = matches!(op, 2 | 3);
    if !writing_string && let Some(view) = view {
        buffer::require_mutable(ctx, view.buffer)?;
    }
    let input = if writing_string {
        None
    } else {
        let arg = item(ctx, args, 0)?;
        let ValueKind::String(input) = ctx.get(arg).decode() else {
            return Err(ctx.throw_type_error("Expected a string"));
        };
        Some(input)
    };
    let base64 = op.is_multiple_of(2);
    let mut last = Last::Loose;
    let mut url = false;
    let mut omit = false;
    if base64 {
        let options = item(ctx, args, usize::from(!writing_string))?;
        if ctx.get(options) != Value::UNDEFINED {
            if ctx.get(options).as_object().is_none() {
                return Err(ctx.throw_type_error("Expected an options object"));
            }
            let alphabet = named(ctx, options, "alphabet")?;
            url = parse_option(ctx, alphabet, &["base64", "base64url"])? != 0;
            if writing_string {
                let padding = named(ctx, options, "omitPadding")?;
                omit = convert::to_boolean_value(ctx.heap(), padding);
            } else {
                let chunk = named(ctx, options, "lastChunkHandling")?;
                last = [Last::Loose, Last::Strict, Last::Stop]
                    [parse_option(ctx, chunk, &["loose", "strict", "stop-before-partial"])?];
            }
        }
    }
    if let Some(view) = view {
        let length = typed::length(ctx.heap(), view)
            .ok_or_else(|| ctx.throw_type_error("Uint8Array is out of bounds"))?;
        if writing_string {
            return encode(ctx, view, length, base64, url, omit);
        }
        let result = decode_into(ctx, input.unwrap(), view, length, base64, url, last);
        let (read, written) = result.ok_or_else(|| syntax(ctx, base64))?;
        let result = ctx.new_object()?;
        put(ctx, result, "read", number(read), PropFlags::DEFAULT_DATA)?;
        put(
            ctx,
            result,
            "written",
            number(written),
            PropFlags::DEFAULT_DATA,
        )?;
        Ok(ctx.get(result))
    } else {
        let input = input.unwrap();
        let len = ctx.heap().string(input).len() as usize;
        let (_, n) = decode(
            ctx.heap().string_units(input),
            len,
            usize::MAX,
            base64,
            url,
            last,
            |_, _| {},
        )
        .ok_or_else(|| syntax(ctx, base64))?;
        let result = typed::intrinsic(ctx, TypedArrayKind::Uint8, n)?;
        let view = typed::data(ctx.heap(), ctx.get(result).as_object().unwrap()).unwrap();
        decode_into(ctx, input, view, n, base64, url, last);
        Ok(ctx.get(result))
    }
}

fn parse_option(ctx: &mut ExecCtx<'_, '_>, value: Value, names: &[&str]) -> JsResult<usize> {
    if value == Value::UNDEFINED {
        return Ok(0);
    }
    if let ValueKind::String(str) = value.decode() {
        for (i, name) in names.iter().enumerate() {
            if ctx.heap().string_units(str).eq(name.encode_utf16()) {
                return Ok(i);
            }
        }
    }
    Err(ctx.throw_type_error("Invalid encoding option"))
}

fn syntax(ctx: &mut ExecCtx<'_, '_>, base64: bool) -> crate::Abrupt {
    ctx.throw_error(
        ErrorKind::Syntax,
        if base64 {
            "Invalid Base64 string"
        } else {
            "Invalid hex string"
        },
    )
}

fn encode(
    ctx: &mut ExecCtx<'_, '_>,
    view: TypedArrayData,
    len: usize,
    base64: bool,
    url: bool,
    omit: bool,
) -> JsResult<Value> {
    let max = if base64 {
        len.div_ceil(3).checked_mul(4)
    } else {
        len.checked_mul(2)
    };
    let max = max.ok_or_else(|| ctx.throw_range_error("Encoded string is too large"))?;
    let mut out = TrackedVec::new();
    ctx.allocate(|rt| out.try_reserve_exact(&rt.heap.ledger, max))?;
    if base64 {
        let alphabet = if url { BASE64URL } else { BASE64 };
        for start in (0..len).step_by(3) {
            let count = (len - start).min(3);
            let mut bytes = [0; 3];
            buffer::read(
                ctx.heap(),
                view.buffer,
                view.byte_offset + start,
                &mut bytes[..count],
            );
            let [a, b, c] = bytes;
            out.push_reserved(alphabet[(a >> 2) as usize]);
            out.push_reserved(alphabet[(((a & 3) << 4) | (b >> 4)) as usize]);
            if count > 1 {
                out.push_reserved(alphabet[(((b & 15) << 2) | (c >> 6)) as usize]);
            } else if !omit {
                out.push_reserved(b'=');
            }
            if count > 2 {
                out.push_reserved(alphabet[(c & 63) as usize]);
            } else if !omit {
                out.push_reserved(b'=');
            }
        }
    } else {
        for i in 0..len {
            let value = typed::raw(ctx.heap(), view, i) as usize;
            out.push_reserved(HEX[value >> 4]);
            out.push_reserved(HEX[value & 15]);
        }
    }
    ctx.new_string(std::str::from_utf8(&out).unwrap())
        .map(|s| ctx.get(s))
}

fn decode_into(
    ctx: &mut ExecCtx<'_, '_>,
    input: StrId,
    view: TypedArrayData,
    len: usize,
    base64: bool,
    url: bool,
    last: Last,
) -> Option<(usize, usize)> {
    let heap = &mut ctx.runtime.heap;
    let str_len = heap.strings.get(input).unwrap().len() as usize;
    let units = StringUnits::new(&heap.strings, input);
    let obj = heap.objects.get_mut(view.buffer).unwrap();
    decode(units, str_len, len, base64, url, last, |offset, bytes| {
        let offset = view.byte_offset + offset;
        match &mut obj.data {
            ObjectData::ArrayBuffer(data) => {
                data.bytes[offset..][..bytes.len()].copy_from_slice(bytes)
            }
            ObjectData::SharedArrayBuffer(data) => data.backing.write(offset, bytes),
            _ => unreachable!(),
        }
    })
}

fn decode(
    mut units: impl Iterator<Item = u16>,
    len: usize,
    cap: usize,
    base64: bool,
    url: bool,
    last: Last,
    mut emit: impl FnMut(usize, &[u8]),
) -> Option<(usize, usize)> {
    if !base64 {
        if !len.is_multiple_of(2) {
            return None;
        }
        let n = (len / 2).min(cap);
        for i in 0..n {
            let hi = nibble(units.next().unwrap())?;
            let lo = nibble(units.next().unwrap())?;
            emit(i, &[(hi << 4) | lo]);
        }
        return Some((n * 2, n));
    }
    let mut chars = units
        .enumerate()
        .filter(|(_, c)| !matches!(c, 9 | 10 | 12 | 13 | 32));
    let mut seen = 0usize;
    let mut bits = 0;
    let mut written = 0;
    let mut read = 0;
    while written < cap {
        match chars.next() {
            None => {
                if seen == 0 {
                    return Some((len, written));
                }
                if last == Last::Stop {
                    return Some((read, written));
                }
                if last == Last::Strict || seen == 1 {
                    return None;
                }
                break;
            }
            Some((_, 61)) => {
                if seen < 2 {
                    return None;
                }
                if seen == 2 {
                    match chars.next() {
                        None if last == Last::Stop => return Some((read, written)),
                        Some((_, 61)) => (),
                        _ => return None,
                    }
                }
                if chars.next().is_some() {
                    return None;
                }
                if last == Last::Strict && bits & (if seen == 2 { 15 } else { 3 }) != 0 {
                    return None;
                }
                break;
            }
            Some((i, ch)) => {
                let val = sextet(ch, url)?;
                if cap - written == seen.saturating_sub(1) && seen >= 2 {
                    return Some((read, written));
                }
                bits = (bits << 6) | u32::from(val);
                seen += 1;
                if seen == 4 {
                    emit(written, &bits.to_be_bytes()[1..]);
                    written += 3;
                    seen = 0;
                    bits = 0;
                    read = i + 1;
                }
            }
        }
    }
    if seen == 2 {
        emit(written, &[(bits >> 4) as u8]);
        Some((len, written + 1))
    } else if seen == 3 {
        let bytes = ((bits >> 2) as u16).to_be_bytes();
        emit(written, &bytes);
        Some((len, written + 2))
    } else {
        Some((read, written))
    }
}

fn sextet(unit: u16, url: bool) -> Option<u8> {
    Some(match u8::try_from(unit).ok()? {
        b'A'..=b'Z' => unit as u8 - b'A',
        b'a'..=b'z' => unit as u8 - b'a' + 26,
        b'0'..=b'9' => unit as u8 - b'0' + 52,
        b'+' if !url => 62,
        b'/' if !url => 63,
        b'-' if url => 62,
        b'_' if url => 63,
        _ => return None,
    })
}

fn nibble(unit: u16) -> Option<u8> {
    Some(match u8::try_from(unit).ok()? {
        c @ b'0'..=b'9' => c - b'0',
        c @ b'a'..=b'f' => c - b'a' + 10,
        c @ b'A'..=b'F' => c - b'A' + 10,
        _ => return None,
    })
}
