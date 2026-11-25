use std::cell::Cell;

use super::*;
use crate::atom::{AtomTable, KeyKind};
use crate::heap::Heap;
use crate::ledger::TrackedBox;
use crate::object::ErrorKind;
use crate::ops::compare::same_value_raw;
use crate::ops::convert::{key_value, to_length, to_number, to_string};
use crate::ops::property::{delete_property, get_own_property_descriptor, own_property_keys};
use crate::string::{JsString, UnitBuffer};
use crate::value::{Abrupt, ObjId, RealmId, ShapeId, StrId};

const PARSE: NativeFnId = NativeFnId(BASE + 0x80);
const STRINGIFY: NativeFnId = NativeFnId(BASE + 0x81);
const RAW_JSON: NativeFnId = NativeFnId(BASE + 0x82);
const IS_RAW_JSON: NativeFnId = NativeFnId(BASE + 0x83);
const FRAME_COST: usize = 4096;
const MAX_DEPTH: usize = 256;

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let json = ctx.new_object()?;
    for (name, id, length) in [
        ("isRawJSON", IS_RAW_JSON, 1),
        ("parse", PARSE, 2),
        ("rawJSON", RAW_JSON, 1),
        ("stringify", STRINGIFY, 3),
    ] {
        define_method(ctx, json, name, id, length)?;
    }
    tag(ctx, json, "JSON")?;
    let global = global(ctx)?;
    data(ctx, global, "JSON", json, mutable())
}

pub(super) fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        PARSE | RAW_JSON => parse(ctx, args, id == RAW_JSON),
        IS_RAW_JSON => Ok(Value::bool(
            args.get(0)
                .and_then(|value| raw_text(ctx, ctx.get(value)))
                .is_some(),
        )),
        STRINGIFY => stringify(ctx, args),
        _ => Err(ctx.throw_type_error("unknown JSON builtin function")),
    }
}

fn raw_text(ctx: &ExecCtx<'_, '_>, value: Value) -> Option<Value> {
    if let ObjectData::HostObject(data) = &ctx.heap().object(value.as_object()?).data
        && let HostObjectData::RawJson(text) = **data
    {
        return Some(text);
    }
    None
}

fn parse<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, raw: bool) -> JsResult<Value> {
    let input = arg(ctx, args, 0)?;
    let text = to_string(ctx, input)?;
    // Read where it is, so the text takes no copy.
    ctx.flatten_string(text)?;
    let ValueKind::String(id) = ctx.get(text).decode() else {
        unreachable!()
    };
    let reviver = args
        .get(1)
        .filter(|&value| !raw && callable(ctx, ctx.get(value)));
    let gc_epoch = ctx.heap().collection_count();
    let mut parser = Parser {
        text: id,
        pos: 0,
        record_source: reviver.is_some(),
        raw,
        nodes: 0,
        key_cache_epoch: gc_epoch,
        key_cache: [KeyCacheEntry::EMPTY; KEY_CACHE_SIZE],
    };
    if !raw && reviver.is_none() {
        ctx.check_interrupt()?;
        let value = parser.value_raw(ctx)?;
        parser.skip_ws(ctx)?;
        if parser.peek(ctx).is_some() {
            return Err(parser.syntax_error(ctx, "unexpected data at the end"));
        }
        return Ok(value);
    }
    if raw && matches!(parser.peek(ctx), None | Some(9 | 10 | 13 | 32 | 91 | 123)) {
        return Err(parser.raw_invalid(ctx));
    }
    let (value, meta) = parser.value(ctx)?;
    if !raw {
        parser.skip_ws(ctx)?;
    }
    if parser.peek(ctx).is_some() {
        return Err(parser.syntax_error(ctx, "unexpected data at the end"));
    }
    if raw {
        let raw = ctx.get(text);
        let record = ctx.make_object(None, ObjectFlags::EXTENSIBLE, |ledger| {
            Ok(ObjectData::HostObject(TrackedBox::new(
                ledger,
                HostObjectData::RawJson(raw),
            )?))
        })?;
        data(ctx, record, "rawJSON", text, PropFlags::ENUMERABLE)?;
        crate::ops::property::prevent_extensions(ctx, record)?;
        return Ok(ctx.get(record));
    }
    let Some(reviver) = reviver else {
        return Ok(ctx.get(value));
    };
    let root = ctx.new_object()?;
    let empty = ctx.intern_str("")?;
    super::super::define_data(ctx, root, empty, value, DATA)?;
    Reviver {
        text: id,
        callback: ctx.get(reviver),
    }
    .visit(ctx, root, empty, meta)
}

/// The units of a flat string.
#[derive(Clone, Copy)]
enum Flat<'h> {
    Latin1(&'h [u8]),
    Utf16(&'h [u16]),
}

impl<'h> Flat<'h> {
    fn of(heap: &'h Heap, id: StrId) -> Self {
        let string = heap.string(id);
        if let Some(bytes) = string.as_latin1() {
            Self::Latin1(bytes)
        } else if let Some(units) = string.as_utf16() {
            Self::Utf16(units)
        } else {
            unreachable!("JSON.parse reads a flat string")
        }
    }

    fn get(self, index: usize) -> Option<u16> {
        match self {
            Self::Latin1(bytes) => bytes.get(index).map(|&byte| u16::from(byte)),
            Self::Utf16(units) => units.get(index).copied(),
        }
    }

    fn units(self, start: usize, end: usize) -> impl Iterator<Item = u16> + 'h {
        let (bytes, units): (&[u8], &[u16]) = match self {
            Self::Latin1(bytes) => (&bytes[start..end], &[]),
            Self::Utf16(units) => (&[], &units[start..end]),
        };
        bytes
            .iter()
            .map(|&byte| u16::from(byte))
            .chain(units.iter().copied())
    }
}

/// A unit of JSON text, a byte of Latin-1 text or a UTF-16 code unit.
trait Unit: Copy + Into<u16> {}

impl Unit for u8 {}

impl Unit for u16 {}

/// Why reading or writing JSON text stopped.
enum Fault {
    UnexpectedEof,
    BadControl(usize),
    BadEscape(usize),
    BadUnicodeEscape(usize),
    Interrupted,
}

impl Fault {
    fn throw(self, parser: &Parser, ctx: &mut ExecCtx<'_, '_>) -> Abrupt {
        match self {
            Self::UnexpectedEof => parser.unexpected_eof(ctx),
            Self::BadControl(pos) => {
                parser.error_at(ctx, pos, "Bad control character in string literal")
            }
            Self::BadEscape(pos) => parser.error_at(ctx, pos, "Bad escaped character"),
            Self::BadUnicodeEscape(pos) => parser.error_at(ctx, pos, "Bad Unicode escape"),
            Self::Interrupted => {
                ctx.take_exception();
                Abrupt::Interrupted
            }
        }
    }
}

/// Polls the interrupt handler, which the heap can stay borrowed beside.
fn interrupted(stop: &mut Option<Box<dyn FnMut() -> bool>>) -> bool {
    stop.as_mut().is_some_and(|stop| stop())
}

fn hex_digit(unit: u16) -> Option<u16> {
    match unit {
        48..=57 => Some(unit - 48),
        65..=70 => Some(unit - 65 + 10),
        97..=102 => Some(unit - 97 + 10),
        _ => None,
    }
}

/// Scans a Latin-1 JSON string starting after its opening quote. Returns
/// `(end, wide, escaped)`.
fn scan_latin1(
    bytes: &[u8],
    start: usize,
    stop: &mut Option<Box<dyn FnMut() -> bool>>,
) -> Result<(usize, bool, bool), Fault> {
    let mut end = start;
    let mut wide = false;
    let mut escaped = false;
    let mut next_poll = if stop.is_some() {
        start.saturating_add(4096)
    } else {
        usize::MAX
    };
    while let Some(&b) = bytes.get(end) {
        if end >= next_poll {
            if interrupted(stop) {
                return Err(Fault::Interrupted);
            }
            next_poll = end.saturating_add(4096);
        }
        if b >= 0x20 && b != b'"' && b != b'\\' {
            end += 1;
            continue;
        }
        match b {
            b'"' => return Ok((end, wide, escaped)),
            0..=31 => return Err(Fault::BadControl(end)),
            b'\\' => {
                escaped = true;
                let Some(&escape) = bytes.get(end + 1) else {
                    return Err(Fault::UnexpectedEof);
                };
                match escape {
                    b'"' | b'/' | b'\\' | b'b' | b'f' | b'n' | b'r' | b't' => end += 2,
                    b'u' => {
                        let mut code = 0u16;
                        for offset in 0..4 {
                            let pos = end + 2 + offset;
                            let Some(&digit) = bytes.get(pos) else {
                                return Err(Fault::BadUnicodeEscape(pos));
                            };
                            let Some(nibble) = hex_digit(u16::from(digit)) else {
                                return Err(Fault::BadUnicodeEscape(pos));
                            };
                            code = (code << 4) | nibble;
                        }
                        wide |= code > 0xff;
                        end += 6;
                    }
                    _ => return Err(Fault::BadEscape(end + 1)),
                }
            }
            _ => unreachable!(),
        }
    }
    Err(Fault::UnexpectedEof)
}

/// Decodes an already-validated, non-wide escaped Latin-1 string token that
/// fits in [`crate::string::INLINE_LATIN1_CAPACITY`] bytes.
fn decode_latin1_inline(
    bytes: &[u8],
    out: &mut [u8; crate::string::INLINE_LATIN1_CAPACITY],
) -> (usize, bool) {
    let mut i = 0;
    let mut len = 0;
    let mut is_ascii = true;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            out[len] = b;
            is_ascii &= b < 0x80;
            len += 1;
            i += 1;
            continue;
        }
        let decoded = match bytes[i + 1] {
            b'"' => b'"',
            b'/' => b'/',
            b'\\' => b'\\',
            b'b' => 8,
            b'f' => 12,
            b'n' => 10,
            b'r' => 13,
            b't' => 9,
            b'u' => {
                let unit = hex(bytes.get(i + 2..i + 6)).unwrap();
                i += 4;
                unit as u8
            }
            _ => unreachable!(),
        };
        out[len] = decoded;
        is_ascii &= decoded < 0x80;
        len += 1;
        i += 2;
    }
    (len, is_ascii)
}

/// Decodes a Latin-1 JSON string token into `out`, copying unescaped runs in
/// bulk to keep Latin-1 strings at 1 byte per unit.
fn decode_latin1(
    bytes: &[u8],
    out: &mut UnitBuffer,
    stop: &mut Option<Box<dyn FnMut() -> bool>>,
) -> Result<(), Fault> {
    let mut index = 0;
    let mut next_poll = if stop.is_some() { 4096 } else { usize::MAX };
    while index < bytes.len() {
        let run_start = index;
        while index < bytes.len() && bytes[index] != b'\\' {
            index += 1;
            if index >= next_poll {
                if interrupted(stop) {
                    return Err(Fault::Interrupted);
                }
                next_poll = index.saturating_add(4096);
            }
        }
        if index > run_start {
            out.push_latin1_reserved(&bytes[run_start..index]);
        }
        if index >= bytes.len() {
            break;
        }
        if index >= next_poll {
            if interrupted(stop) {
                return Err(Fault::Interrupted);
            }
            next_poll = index.saturating_add(4096);
        }
        let escape = *bytes.get(index + 1).ok_or(Fault::UnexpectedEof)?;
        let unit = match escape {
            b'"' | b'/' | b'\\' => u16::from(escape),
            b'b' => 8,
            b'f' => 12,
            b'n' => 10,
            b'r' => 13,
            b't' => 9,
            b'u' => {
                let unit = hex(bytes.get(index + 2..index + 6))
                    .ok_or(Fault::BadUnicodeEscape(index + 2))?;
                index += 4;
                unit
            }
            _ => return Err(Fault::BadEscape(index + 1)),
        };
        out.push_reserved(unit);
        index += 2;
    }
    Ok(())
}

enum ScannedNumber {
    Int(Value, usize),
    Float { end: usize, has_exp: bool },
}

enum NumberFault {
    UnexpectedNumber(usize),
    NoNumberAfterMinus(usize),
    UnexpectedToken(usize),
    UnterminatedFraction(usize),
    MissingExponent(usize),
    Interrupted,
}

fn scan_number_latin1(
    bytes: &[u8],
    start: usize,
    stop: &mut Option<Box<dyn FnMut() -> bool>>,
) -> Result<ScannedNumber, NumberFault> {
    let mut p = start;
    let neg = bytes.get(p) == Some(&b'-');
    if neg {
        p += 1;
    }
    let int_start = p;
    let mut mantissa: u64 = 0;
    let mut digits: usize = 0;
    let mut next_poll = if stop.is_some() {
        start.saturating_add(4096)
    } else {
        usize::MAX
    };
    if bytes.get(p) == Some(&b'0') {
        p += 1;
        digits = 1;
        if matches!(bytes.get(p), Some(b'0'..=b'9')) {
            return Err(NumberFault::UnexpectedNumber(int_start));
        }
    } else if matches!(bytes.get(p), Some(b'1'..=b'9')) {
        while let Some(&b @ b'0'..=b'9') = bytes.get(p) {
            if digits < 15 {
                mantissa = mantissa * 10 + u64::from(b - b'0');
            }
            digits += 1;
            p += 1;
            if p >= next_poll {
                if interrupted(stop) {
                    return Err(NumberFault::Interrupted);
                }
                next_poll = p.saturating_add(4096);
            }
        }
    } else if neg {
        return Err(NumberFault::NoNumberAfterMinus(start));
    } else {
        return Err(NumberFault::UnexpectedToken(start));
    }

    if !matches!(bytes.get(p), Some(b'.' | b'e' | b'E')) {
        if digits <= 15 {
            let value = if neg {
                if mantissa == 0 {
                    Value::number(-0.0)
                } else if mantissa <= 0x8000_0000 {
                    Value::int32(mantissa.wrapping_neg() as i32)
                } else {
                    Value::number(-(mantissa as f64))
                }
            } else if mantissa <= i32::MAX as u64 {
                Value::int32(mantissa as i32)
            } else {
                Value::number(mantissa as f64)
            };
            return Ok(ScannedNumber::Int(value, p));
        }
        return Ok(ScannedNumber::Float {
            end: p,
            has_exp: false,
        });
    }

    if bytes.get(p) == Some(&b'.') {
        p += 1;
        if !matches!(bytes.get(p), Some(b'0'..=b'9')) {
            return Err(NumberFault::UnterminatedFraction(p));
        }
        while matches!(bytes.get(p), Some(b'0'..=b'9')) {
            p += 1;
            if p >= next_poll {
                if interrupted(stop) {
                    return Err(NumberFault::Interrupted);
                }
                next_poll = p.saturating_add(4096);
            }
        }
    }

    let mut has_exp = false;
    if matches!(bytes.get(p), Some(b'e' | b'E')) {
        has_exp = true;
        p += 1;
        if matches!(bytes.get(p), Some(b'+' | b'-')) {
            p += 1;
        }
        if !matches!(bytes.get(p), Some(b'0'..=b'9')) {
            return Err(NumberFault::MissingExponent(p));
        }
        while matches!(bytes.get(p), Some(b'0'..=b'9')) {
            p += 1;
            if p >= next_poll {
                if interrupted(stop) {
                    return Err(NumberFault::Interrupted);
                }
                next_poll = p.saturating_add(4096);
            }
        }
    }

    Ok(ScannedNumber::Float { end: p, has_exp })
}

#[inline]
fn hash_bytes(bytes: &[u8]) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h as usize
}

fn insert_units_raw(ctx: &mut ExecCtx<'_, '_>, units: UnitBuffer) -> JsResult<Value> {
    match units.len() {
        0 => {
            if let Some(empty) = ctx.heap().atoms.ascii_string(128) {
                return Ok(Value::string(empty));
            }
        }
        1 => {
            if let Some(unit) = units.get(0)
                && unit < 128
                && let Some(ch) = ctx.heap().atoms.ascii_string(unit as usize)
            {
                return Ok(Value::string(ch));
            }
        }
        _ => (),
    }
    let mut string = Some(units.into_string(&ctx.heap().ledger));
    let id = ctx.allocate(|runtime| {
        runtime
            .heap
            .strings
            .try_reserve_slot(&runtime.heap.ledger)?;
        Ok(runtime
            .heap
            .strings
            .insert_reserved(string.take().expect("the string goes in once")))
    })?;
    Ok(Value::string(id))
}

/// The end of the string token whose units start at `start` (after its
/// opening quote), and whether a unit of the string is above 0xFF.
fn scan<U: Unit>(
    units: &[U],
    start: usize,
    stop: &mut Option<Box<dyn FnMut() -> bool>>,
) -> Result<(usize, bool), Fault> {
    let mut end = start;
    let mut wide = false;
    for step in 1usize.. {
        if step.is_multiple_of(4096) && interrupted(stop) {
            return Err(Fault::Interrupted);
        }
        let unit: u16 = (*units.get(end).ok_or(Fault::UnexpectedEof)?).into();
        match unit {
            34 => break,
            0..=31 => return Err(Fault::BadControl(end)),
            92 => {
                let escape: u16 = (*units.get(end + 1).ok_or(Fault::UnexpectedEof)?).into();
                match escape {
                    34 | 47 | 92 | 98 | 102 | 110 | 114 | 116 => end += 2,
                    117 => {
                        let mut code = 0u16;
                        for offset in 0..4 {
                            let pos = end + 2 + offset;
                            let Some(&digit) = units.get(pos) else {
                                return Err(Fault::BadUnicodeEscape(pos));
                            };
                            let Some(nibble) = hex_digit(digit.into()) else {
                                return Err(Fault::BadUnicodeEscape(pos));
                            };
                            code = (code << 4) | nibble;
                        }
                        wide |= code > 0xff;
                        end += 6;
                    }
                    _ => return Err(Fault::BadEscape(end + 1)),
                }
            }
            _ => {
                wide |= unit > 0xff;
                end += 1;
            }
        }
    }
    Ok((end, wide))
}

/// The four hex digits of a `\u` escape as the unit they write.
fn hex<U: Unit>(digits: Option<&[U]>) -> Option<u16> {
    digits?.iter().try_fold(0, |unit, &digit| {
        Some((unit << 4) | hex_digit(digit.into())?)
    })
}

/// Writes the string that the raw units of a string token stand for into
/// `out`, which has room for as many units as `units` has, wide ones too
/// where [`scan`] found them.
fn decode<U: Unit>(
    units: &[U],
    out: &mut UnitBuffer,
    stop: &mut Option<Box<dyn FnMut() -> bool>>,
) -> Result<(), Fault> {
    let mut index = 0;
    for step in 1usize.. {
        let Some(&unit) = units.get(index) else {
            break;
        };
        if step.is_multiple_of(4096) && interrupted(stop) {
            return Err(Fault::Interrupted);
        }
        let unit: u16 = unit.into();
        if unit != 92 {
            out.push_reserved(unit);
            index += 1;
            continue;
        }
        let escape: u16 = (*units.get(index + 1).ok_or(Fault::UnexpectedEof)?).into();
        let unit = match escape {
            34 | 47 | 92 => escape,
            98 => 8,
            102 => 12,
            110 => 10,
            114 => 13,
            116 => 9,
            117 => {
                let unit = hex(units.get(index + 2..index + 6))
                    .ok_or(Fault::BadUnicodeEscape(index + 2))?;
                index += 4;
                unit
            }
            _ => return Err(Fault::BadEscape(index + 1)),
        };
        out.push_reserved(unit);
        index += 2;
    }
    Ok(())
}

/// The string of units `start..end` of flat string `text`.
fn slice<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    text: StrId,
    start: usize,
    end: usize,
) -> JsResult<Handle<'s>> {
    let wide = Flat::of(ctx.heap(), text)
        .units(start, end)
        .any(|unit| unit > 0xff);
    let mut out = UnitBuffer::default();
    reserve(ctx, &mut out, end - start, wide)?;
    for unit in Flat::of(ctx.heap(), text).units(start, end) {
        out.push_reserved(unit);
    }
    ctx.new_string_from_units(out)
}

fn line_col(flat: Flat<'_>, pos: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i < pos {
        match flat.get(i) {
            Some(13) => {
                i += 1 + usize::from(flat.get(i + 1) == Some(10));
                line += 1;
                line_start = i;
            }
            Some(10) => {
                i += 1;
                line += 1;
                line_start = i;
            }
            _ => i += 1,
        }
    }
    (line, pos.saturating_sub(line_start) + 1)
}

fn is_ident_start(unit: u16) -> bool {
    matches!(unit, 65..=90 | 97..=122 | 95 | 36)
}

fn is_ident_continue(unit: u16) -> bool {
    is_ident_start(unit) || matches!(unit, 48..=57)
}

thread_local! {
    static TRAVERSAL_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct TraversalFrame;

impl TraversalFrame {
    #[inline]
    fn enter_checked(ctx: &mut ExecCtx<'_, '_>, poll_interrupt: bool) -> JsResult<Self> {
        // A reviver, replacer or getter can start another JSON traversal on this stack.
        let depth = TRAVERSAL_DEPTH.get();
        if depth >= MAX_DEPTH
            || !ctx
                .runtime
                .vm
                .check_budget(depth * (FRAME_COST / size_of::<Value>()), 0, 0)
        {
            return Err(ctx.stack_overflow());
        }
        if poll_interrupt {
            ctx.check_interrupt()?;
        }
        TRAVERSAL_DEPTH.set(depth + 1);
        Ok(Self)
    }

    fn enter(ctx: &mut ExecCtx<'_, '_>) -> JsResult<Self> {
        Self::enter_checked(ctx, true)
    }
}

impl Drop for TraversalFrame {
    fn drop(&mut self) {
        TRAVERSAL_DEPTH.set(TRAVERSAL_DEPTH.get() - 1);
    }
}

const KEY_CACHE_SIZE: usize = 64;

#[derive(Clone, Copy)]
struct KeyCacheEntry {
    start: u32,
    len: u32,
    key: PropKey,
    backing: Value,
}

impl KeyCacheEntry {
    const EMPTY: Self = Self {
        start: 0,
        len: u32::MAX,
        key: PropKey::atom(Atom::LENGTH),
        backing: Value::UNDEFINED,
    };
}

struct Parser {
    /// The text, a flat string.
    text: StrId,
    pos: usize,
    record_source: bool,
    raw: bool,
    nodes: u32,
    key_cache_epoch: usize,
    key_cache: [KeyCacheEntry; KEY_CACHE_SIZE],
}

type Parsed<'s> = (Handle<'s>, Option<Handle<'s>>);

impl Parser {
    fn raw_invalid(&self, ctx: &mut ExecCtx<'_, '_>) -> Abrupt {
        ctx.throw_error(ErrorKind::Syntax, "invalid rawJSON string")
    }

    fn syntax_error(&self, ctx: &mut ExecCtx<'_, '_>, message: &str) -> Abrupt {
        if self.raw {
            return self.raw_invalid(ctx);
        }
        ctx.throw_error(ErrorKind::Syntax, message)
    }

    fn error_at(&self, ctx: &mut ExecCtx<'_, '_>, pos: usize, reason: &str) -> Abrupt {
        if self.raw {
            return self.raw_invalid(ctx);
        }
        let (line, col) = line_col(Flat::of(ctx.heap(), self.text), pos);
        ctx.throw_error(
            ErrorKind::Syntax,
            &format!("{reason} in JSON at position {pos} (line {line} column {col})"),
        )
    }

    fn unexpected_eof(&self, ctx: &mut ExecCtx<'_, '_>) -> Abrupt {
        self.syntax_error(ctx, "Unexpected end of JSON input")
    }

    fn unexpected_token(&self, ctx: &mut ExecCtx<'_, '_>, start: usize) -> Abrupt {
        if self.raw {
            return self.raw_invalid(ctx);
        }
        let flat = Flat::of(ctx.heap(), self.text);
        let Some(first) = flat.get(start) else {
            return self.unexpected_eof(ctx);
        };
        if is_ident_start(first) {
            let mut end = start + 1;
            while flat.get(end).is_some_and(is_ident_continue) {
                end += 1;
            }
            let mut token = String::with_capacity(end - start);
            for unit in flat.units(start, end) {
                token.push(char::from(unit as u8));
            }
            return ctx.throw_error(ErrorKind::Syntax, &format!("unexpected token: '{token}'"));
        }
        if (0x20..=0x7e).contains(&first) {
            let ch = char::from(first as u8);
            return ctx.throw_error(ErrorKind::Syntax, &format!("unexpected token: '{ch}'"));
        }
        self.error_at(ctx, start, &format!("Unexpected token '\\u{first:04x}'"))
    }

    fn peek(&self, ctx: &ExecCtx<'_, '_>) -> Option<u16> {
        Flat::of(ctx.heap(), self.text).get(self.pos)
    }

    fn advance(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        self.pos += 1;
        if self.pos.is_multiple_of(4096) {
            ctx.check_interrupt()?;
        }
        Ok(())
    }

    fn eat(&mut self, ctx: &ExecCtx<'_, '_>, byte: u8) -> bool {
        if self.peek(ctx) != Some(u16::from(byte)) {
            return false;
        }
        self.pos += 1;
        true
    }

    fn whitespace(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        while matches!(self.peek(ctx), Some(9 | 10 | 13 | 32)) {
            self.advance(ctx)?;
        }
        Ok(())
    }

    #[inline]
    fn skip_ws(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        let runtime = &mut *ctx.runtime;
        let stop = &mut runtime.vm.interrupt_handler;
        match Flat::of(&runtime.heap, self.text) {
            Flat::Latin1(bytes) => {
                let mut pos = self.pos;
                let mut next_poll = if stop.is_some() {
                    pos.saturating_add(4096)
                } else {
                    usize::MAX
                };
                while let Some(&b) = bytes.get(pos) {
                    if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    pos += 1;
                    if pos >= next_poll {
                        if interrupted(stop) {
                            self.pos = pos;
                            ctx.take_exception();
                            return Err(Abrupt::Interrupted);
                        }
                        next_poll = pos.saturating_add(4096);
                    }
                }
                self.pos = pos;
                Ok(())
            }
            Flat::Utf16(_) => self.whitespace(ctx),
        }
    }

    #[inline]
    fn eat_fast(&mut self, ctx: &ExecCtx<'_, '_>, byte: u8) -> bool {
        match Flat::of(ctx.heap(), self.text) {
            Flat::Latin1(bytes) => {
                if bytes.get(self.pos) == Some(&byte) {
                    self.pos += 1;
                    true
                } else {
                    false
                }
            }
            Flat::Utf16(units) => {
                if units.get(self.pos) == Some(&u16::from(byte)) {
                    self.pos += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn literal_raw(
        &mut self,
        ctx: &mut ExecCtx<'_, '_>,
        word: &[u8],
        value: Value,
    ) -> JsResult<Value> {
        let start = self.pos;
        if let Flat::Latin1(bytes) = Flat::of(ctx.heap(), self.text) {
            let end = start + word.len();
            if bytes.get(start..end) == Some(word)
                && !bytes
                    .get(end)
                    .is_some_and(|&b| is_ident_continue(u16::from(b)))
            {
                self.pos = end;
                return Ok(value);
            }
            return Err(self.unexpected_token(ctx, start));
        }
        for &byte in word {
            if !self.eat(ctx, byte) {
                return Err(self.unexpected_token(ctx, start));
            }
        }
        if self.peek(ctx).is_some_and(is_ident_continue) {
            return Err(self.unexpected_token(ctx, start));
        }
        Ok(value)
    }

    fn literal<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        word: &[u8],
        value: Value,
    ) -> JsResult<Handle<'s>> {
        let value = self.literal_raw(ctx, word, value)?;
        ctx.root(value)
    }

    fn store_key_cache(
        &mut self,
        heap: &Heap,
        slot: usize,
        start: u32,
        len: u32,
        key: PropKey,
        backing: Value,
    ) {
        let epoch = heap.collection_count();
        if epoch != self.key_cache_epoch {
            for entry in &mut self.key_cache {
                entry.len = u32::MAX;
            }
            self.key_cache_epoch = epoch;
        }
        self.key_cache[slot] = KeyCacheEntry {
            start,
            len,
            key,
            backing,
        };
    }

    fn parse_key_raw<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        key_root: &crate::root::MutRoot<'s>,
    ) -> JsResult<(PropKey, Value)> {
        if matches!(Flat::of(ctx.heap(), self.text), Flat::Latin1(_)) {
            let eaten = self.eat_fast(ctx, b'"');
            debug_assert!(eaten);
            let start = self.pos;
            let scanned = {
                let runtime = &mut *ctx.runtime;
                let stop = &mut runtime.vm.interrupt_handler;
                let Flat::Latin1(bytes) = Flat::of(&runtime.heap, self.text) else {
                    unreachable!()
                };
                scan_latin1(bytes, start, stop)
            };
            let (end, wide, escaped) = scanned.map_err(|fault| fault.throw(self, ctx))?;
            self.pos = end + 1;
            if !escaped {
                let len = end - start;
                let (slot, cached) = {
                    let heap = ctx.heap();
                    let gc_epoch = heap.collection_count();
                    if gc_epoch != self.key_cache_epoch {
                        for entry in &mut self.key_cache {
                            entry.len = u32::MAX;
                        }
                        self.key_cache_epoch = gc_epoch;
                    }
                    let Flat::Latin1(bytes) = Flat::of(heap, self.text) else {
                        unreachable!()
                    };
                    let slice = &bytes[start..end];
                    let slot = hash_bytes(slice) & (KEY_CACHE_SIZE - 1);
                    let entry = self.key_cache[slot];
                    let hit = if entry.len == len as u32 {
                        let e_start = entry.start as usize;
                        bytes[e_start..e_start + len] == *slice
                    } else {
                        false
                    };
                    if hit {
                        (slot, Some((entry.key, entry.backing, true)))
                    } else if slice.is_ascii()
                        && let Ok(text) = std::str::from_utf8(slice)
                        && let Some(key) = heap.atoms.find_str(&heap.strings, text)
                    {
                        let backing = heap.dynamic_key_value(key).unwrap_or(Value::UNDEFINED);
                        (slot, Some((key, backing, false)))
                    } else {
                        (slot, None)
                    }
                };
                if let Some((key, backing, from_cache)) = cached {
                    if !from_cache {
                        self.store_key_cache(
                            ctx.heap(),
                            slot,
                            start as u32,
                            len as u32,
                            key,
                            backing,
                        );
                    }
                    return Ok((key, backing));
                }
                let id = ctx.allocate(|runtime| {
                    let Flat::Latin1(bytes) = Flat::of(&runtime.heap, self.text) else {
                        unreachable!()
                    };
                    let s = JsString::from_latin1(&runtime.heap.ledger, &bytes[start..end])?;
                    runtime.heap.strings.insert(&runtime.heap.ledger, s)
                })?;
                ctx.set(key_root, Value::string(id));
                let key = ctx.intern(key_root.handle())?;
                let backing = ctx
                    .heap()
                    .dynamic_key_value(key)
                    .unwrap_or(Value::UNDEFINED);
                self.store_key_cache(ctx.heap(), slot, start as u32, len as u32, key, backing);
                return Ok((key, backing));
            }
            let val = self.decode_scanned_latin1(ctx, start, end, wide)?;
            ctx.set(key_root, val);
            let key = ctx.intern(key_root.handle())?;
            let backing = ctx
                .heap()
                .dynamic_key_value(key)
                .unwrap_or(Value::UNDEFINED);
            return Ok((key, backing));
        }
        let val = self.string_raw(ctx)?;
        ctx.set(key_root, val);
        let key = ctx.intern(key_root.handle())?;
        let backing = ctx
            .heap()
            .dynamic_key_value(key)
            .unwrap_or(Value::UNDEFINED);
        Ok((key, backing))
    }

    fn value_raw(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
        self.skip_ws(ctx)?;
        let start = self.pos;
        match self.peek(ctx) {
            None => Err(self.unexpected_eof(ctx)),
            Some(34) => self.string_raw(ctx),
            Some(110) => self.literal_raw(ctx, b"null", Value::NULL),
            Some(116) => self.literal_raw(ctx, b"true", Value::bool(true)),
            Some(102) => self.literal_raw(ctx, b"false", Value::bool(false)),
            Some(45 | 48..=57) => self.number_raw(ctx),
            Some(91) => self.parse_array_raw(ctx),
            Some(123) => self.parse_object_raw(ctx),
            Some(_) => Err(self.unexpected_token(ctx, start)),
        }
    }

    fn parse_array_raw(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
        self.nodes = self.nodes.wrapping_add(1);
        let _frame = TraversalFrame::enter_checked(ctx, self.nodes & 1023 == 0)?;
        let eaten = self.eat_fast(ctx, b'[');
        debug_assert!(eaten);
        ctx.with_scope(|ctx| {
            let arr = ctx.new_array()?;
            let arr_id = ctx.get(arr).as_object().unwrap();
            self.skip_ws(ctx)?;
            if self.eat_fast(ctx, b']') {
                return Ok(Value::object(arr_id));
            }
            let elem_root = ctx.mut_root(Value::UNDEFINED)?;
            let mut index: u32 = 0;
            loop {
                if (index & 1023) == 1023 {
                    ctx.check_interrupt()?;
                }
                let val = self.value_raw(ctx)?;
                let pushed = {
                    let ObjectData::Array(array) =
                        &mut ctx.runtime.heap.objects.get_mut(arr_id).unwrap().data
                    else {
                        unreachable!()
                    };
                    if array.elements.len() < array.elements.capacity() {
                        array.elements.push_reserved(val);
                        array.length += 1;
                        true
                    } else {
                        false
                    }
                };
                if !pushed {
                    ctx.set(&elem_root, val);
                    ctx.allocate(|runtime| {
                        let ObjectData::Array(array) =
                            &mut runtime.heap.objects.get_mut(arr_id).unwrap().data
                        else {
                            unreachable!()
                        };
                        array.elements.push(&runtime.heap.ledger, val)?;
                        array.length += 1;
                        Ok(())
                    })?;
                    ctx.set(&elem_root, Value::UNDEFINED);
                }
                index = index.wrapping_add(1);
                self.skip_ws(ctx)?;
                if self.eat_fast(ctx, b']') {
                    break;
                }
                if !self.eat_fast(ctx, b',') {
                    return Err(self.error_at(
                        ctx,
                        self.pos,
                        "Expected ',' or ']' after array element",
                    ));
                }
            }
            Ok(Value::object(arr_id))
        })
    }

    fn parse_object_raw(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
        self.nodes = self.nodes.wrapping_add(1);
        let _frame = TraversalFrame::enter_checked(ctx, self.nodes & 1023 == 0)?;
        let eaten = self.eat_fast(ctx, b'{');
        debug_assert!(eaten);
        ctx.with_scope(|ctx| {
            let obj = ctx.new_object()?;
            let obj_id = ctx.get(obj).as_object().unwrap();
            self.skip_ws(ctx)?;
            if self.eat_fast(ctx, b'}') {
                return Ok(Value::object(obj_id));
            }
            let key_root = ctx.mut_root(Value::UNDEFINED)?;
            let val_root = ctx.mut_root(Value::UNDEFINED)?;
            let mut count: u32 = 0;
            loop {
                if (count & 1023) == 1023 {
                    ctx.check_interrupt()?;
                }
                self.skip_ws(ctx)?;
                if self.peek(ctx) != Some(34) {
                    return Err(self.error_at(ctx, self.pos, "Expected property name or '}'"));
                }
                let (key, backing) = self.parse_key_raw(ctx, &key_root)?;
                ctx.set(&key_root, backing);
                self.skip_ws(ctx)?;
                if !self.eat_fast(ctx, b':') {
                    return Err(self.error_at(ctx, self.pos, "Expected ':' after property name"));
                }
                let val = self.value_raw(ctx)?;
                let change = crate::shape::ShapeProp {
                    key,
                    flags: PropFlags::DEFAULT_DATA,
                };
                let done = {
                    let heap = ctx.heap();
                    let object = heap.object(obj_id);
                    let shape = heap.shape(object.shape);
                    if !shape.is_mutable()
                        && !heap.building_shapes
                        && object.props.len() < object.props.capacity()
                        && shape.props.len() < crate::shape::MAX_SHARED_SHAPE_PROPS
                        && let Some(next) = shape.find_transition(change, |target| {
                            heap.shapes.get_weak(target).is_some()
                        })
                    {
                        let object = ctx.runtime.heap.objects.get_mut(obj_id).unwrap();
                        object.props.push_reserved(val);
                        object.shape = next;
                        true
                    } else if let Some((slot, flags)) = shape.find(key)
                        && flags == PropFlags::DEFAULT_DATA
                    {
                        ctx.runtime.heap.objects.get_mut(obj_id).unwrap().props[slot] = val;
                        true
                    } else {
                        false
                    }
                };
                if !done {
                    ctx.set(&val_root, val);
                    crate::ops::property::define_plain_data(ctx, obj_id, key, val)?;
                    ctx.set(&val_root, Value::UNDEFINED);
                }
                ctx.set(&key_root, Value::UNDEFINED);
                count = count.wrapping_add(1);
                self.skip_ws(ctx)?;
                if self.eat_fast(ctx, b'}') {
                    break;
                }
                if !self.eat_fast(ctx, b',') {
                    return Err(self.error_at(
                        ctx,
                        self.pos,
                        "Expected ',' or '}' after property value",
                    ));
                }
            }
            Ok(Value::object(obj_id))
        })
    }

    fn value<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Parsed<'s>> {
        let _frame = TraversalFrame::enter(ctx)?;
        self.whitespace(ctx)?;
        let start = self.pos;
        let value = match self.peek(ctx) {
            None => return Err(self.unexpected_eof(ctx)),
            Some(34) => self.string(ctx),
            Some(110) => self.literal(ctx, b"null", Value::NULL),
            Some(116) => self.literal(ctx, b"true", Value::bool(true)),
            Some(102) => self.literal(ctx, b"false", Value::bool(false)),
            Some(45 | 48..=57) => self.number(ctx),
            Some(91 | 123) => return self.compound(ctx),
            Some(_) => return Err(self.unexpected_token(ctx, start)),
        }?;
        let meta = self
            .record_source
            .then(|| {
                host(
                    ctx,
                    HostObjectData::JsonParseNode {
                        value: ctx.get(value),
                        start,
                        end: self.pos,
                    },
                )
            })
            .transpose()?;
        Ok((value, meta))
    }

    fn compound<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Parsed<'s>> {
        let array = self.eat(ctx, b'[');
        let close = if array {
            b']'
        } else {
            let eaten = self.eat(ctx, b'{');
            debug_assert!(eaten);
            b'}'
        };
        let object = if array {
            ctx.new_array()?
        } else {
            ctx.new_object()?
        };
        let meta = if self.record_source {
            let data = HostObjectData::JsonParseNode {
                value: ctx.get(object),
                start: self.pos,
                end: self.pos,
            };
            Some(ctx.make_object(None, ObjectFlags::EXTENSIBLE, |ledger| {
                Ok(ObjectData::HostObject(TrackedBox::new(ledger, data)?))
            })?)
        } else {
            None
        };
        self.whitespace(ctx)?;
        if self.eat(ctx, close) {
            return Ok((object, meta));
        }
        for index in 0.. {
            let object = ctx.get(object);
            let meta = meta.map(|meta| ctx.get(meta));
            ctx.with_scope(|ctx| {
                let object = ctx.root(object)?;
                let key = if array {
                    index_key(ctx, index)?
                } else {
                    self.whitespace(ctx)?;
                    if self.peek(ctx) != Some(34) {
                        return Err(self.error_at(ctx, self.pos, "Expected property name or '}'"));
                    }
                    let key = self.string(ctx)?;
                    let key = ctx.intern(key)?;
                    self.whitespace(ctx)?;
                    if !self.eat(ctx, b':') {
                        return Err(self.error_at(
                            ctx,
                            self.pos,
                            "Expected ':' after property name",
                        ));
                    }
                    key
                };
                let (value, child) = self.value(ctx)?;
                super::super::define_data(ctx, object, key, value, DATA)?;
                if let Some((meta, child)) = meta.zip(child) {
                    let meta = ctx.root(meta)?;
                    super::super::define_data(ctx, meta, key, child, DATA)?;
                }
                Ok(())
            })?;
            self.whitespace(ctx)?;
            if self.eat(ctx, close) {
                break;
            }
            if !self.eat(ctx, b',') {
                let reason = if array {
                    "Expected ',' or ']' after array element"
                } else {
                    "Expected ',' or '}' after property value"
                };
                return Err(self.error_at(ctx, self.pos, reason));
            }
        }
        Ok((object, meta))
    }

    fn decode_scanned_latin1(
        &self,
        ctx: &mut ExecCtx<'_, '_>,
        start: usize,
        end: usize,
        wide: bool,
    ) -> JsResult<Value> {
        if !wide && end - start <= crate::string::INLINE_LATIN1_CAPACITY {
            let mut data = [0u8; crate::string::INLINE_LATIN1_CAPACITY];
            let (len, is_ascii) = {
                let Flat::Latin1(bytes) = Flat::of(ctx.heap(), self.text) else {
                    unreachable!()
                };
                decode_latin1_inline(&bytes[start..end], &mut data)
            };
            if len == 0
                && let Some(empty) = ctx.heap().atoms.ascii_string(128)
            {
                return Ok(Value::string(empty));
            }
            if len == 1
                && data[0] < 128
                && let Some(ch) = ctx.heap().atoms.ascii_string(data[0] as usize)
            {
                return Ok(Value::string(ch));
            }
            let id = ctx.allocate(|runtime| {
                let s = JsString::InlineLatin1 {
                    len: len as u8,
                    is_ascii,
                    data,
                };
                runtime.heap.strings.insert(&runtime.heap.ledger, s)
            })?;
            return Ok(Value::string(id));
        }
        let mut out = UnitBuffer::default();
        reserve(ctx, &mut out, end - start, wide)?;
        let decoded = {
            let runtime = &mut *ctx.runtime;
            let stop = &mut runtime.vm.interrupt_handler;
            let Flat::Latin1(bytes) = Flat::of(&runtime.heap, self.text) else {
                unreachable!()
            };
            decode_latin1(&bytes[start..end], &mut out, stop)
        };
        decoded.map_err(|fault| fault.throw(self, ctx))?;
        insert_units_raw(ctx, out)
    }

    fn string_raw(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
        let eaten = self.eat_fast(ctx, b'"');
        debug_assert!(eaten);
        let start = self.pos;
        if matches!(Flat::of(ctx.heap(), self.text), Flat::Latin1(_)) {
            let scanned = {
                let runtime = &mut *ctx.runtime;
                let stop = &mut runtime.vm.interrupt_handler;
                let Flat::Latin1(bytes) = Flat::of(&runtime.heap, self.text) else {
                    unreachable!()
                };
                scan_latin1(bytes, start, stop)
            };
            let (end, wide, escaped) = scanned.map_err(|fault| fault.throw(self, ctx))?;
            self.pos = end + 1;
            if !escaped {
                let len = end - start;
                if len == 0
                    && let Some(empty) = ctx.heap().atoms.ascii_string(128)
                {
                    return Ok(Value::string(empty));
                }
                if len == 1 {
                    let b = {
                        let Flat::Latin1(bytes) = Flat::of(ctx.heap(), self.text) else {
                            unreachable!()
                        };
                        bytes[start]
                    };
                    if b < 128
                        && let Some(ch) = ctx.heap().atoms.ascii_string(b as usize)
                    {
                        return Ok(Value::string(ch));
                    }
                }
                let id = ctx.allocate(|runtime| {
                    let Flat::Latin1(bytes) = Flat::of(&runtime.heap, self.text) else {
                        unreachable!()
                    };
                    let s = JsString::from_latin1(&runtime.heap.ledger, &bytes[start..end])?;
                    runtime.heap.strings.insert(&runtime.heap.ledger, s)
                })?;
                return Ok(Value::string(id));
            }
            return self.decode_scanned_latin1(ctx, start, end, wide);
        }
        let runtime = &mut *ctx.runtime;
        let stop = &mut runtime.vm.interrupt_handler;
        let Flat::Utf16(units) = Flat::of(&runtime.heap, self.text) else {
            unreachable!()
        };
        let scanned = scan(units, start, stop);
        let (end, wide) = scanned.map_err(|fault| fault.throw(self, ctx))?;
        // Escapes only shorten it, so the string fits in the token's length.
        let mut out = UnitBuffer::default();
        reserve(ctx, &mut out, end - start, wide)?;
        let runtime = &mut *ctx.runtime;
        let stop = &mut runtime.vm.interrupt_handler;
        let Flat::Utf16(units) = Flat::of(&runtime.heap, self.text) else {
            unreachable!()
        };
        let decoded = decode(&units[start..end], &mut out, stop);
        decoded.map_err(|fault| fault.throw(self, ctx))?;
        self.pos = end + 1;
        insert_units_raw(ctx, out)
    }

    fn string<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        let value = self.string_raw(ctx)?;
        ctx.root(value)
    }

    fn digits(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        while matches!(self.peek(ctx), Some(48..=57)) {
            self.advance(ctx)?;
        }
        Ok(())
    }

    fn number_raw(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
        let start = self.pos;
        if matches!(Flat::of(ctx.heap(), self.text), Flat::Latin1(_)) {
            let scanned = {
                let runtime = &mut *ctx.runtime;
                let stop = &mut runtime.vm.interrupt_handler;
                let Flat::Latin1(bytes) = Flat::of(&runtime.heap, self.text) else {
                    unreachable!()
                };
                scan_number_latin1(bytes, start, stop)
            };
            match scanned {
                Ok(ScannedNumber::Int(val, end)) => {
                    self.pos = end;
                    return Ok(val);
                }
                Ok(ScannedNumber::Float { end, has_exp }) => {
                    self.pos = end;
                    let Flat::Latin1(bytes) = Flat::of(ctx.heap(), self.text) else {
                        unreachable!()
                    };
                    let slice = &bytes[start..end];
                    if !has_exp && let Some(num) = sandbolt_dtoa::parse_simple_decimal(slice) {
                        return Ok(Value::number(num));
                    }
                    let text = std::str::from_utf8(slice).unwrap();
                    let (num, _) =
                        sandbolt_dtoa::atod(text, 10, sandbolt_dtoa::AtodFlags::default());
                    return Ok(Value::number(num));
                }
                Err(NumberFault::UnexpectedNumber(pos)) => {
                    return Err(self.error_at(ctx, pos, "Unexpected number"));
                }
                Err(NumberFault::NoNumberAfterMinus(pos)) => {
                    return Err(self.error_at(ctx, pos, "No number after minus sign"));
                }
                Err(NumberFault::UnexpectedToken(pos)) => {
                    return Err(self.unexpected_token(ctx, pos));
                }
                Err(NumberFault::UnterminatedFraction(pos)) => {
                    return Err(self.error_at(ctx, pos, "Unterminated fractional number"));
                }
                Err(NumberFault::MissingExponent(pos)) => {
                    return Err(self.error_at(ctx, pos, "Exponent part is missing a number"));
                }
                Err(NumberFault::Interrupted) => {
                    ctx.take_exception();
                    return Err(Abrupt::Interrupted);
                }
            }
        }
        let neg = self.eat(ctx, b'-');
        let int_start = self.pos;
        if self.eat(ctx, b'0') {
            if matches!(self.peek(ctx), Some(48..=57)) {
                return Err(self.error_at(ctx, int_start, "Unexpected number"));
            }
        } else if matches!(self.peek(ctx), Some(48..=57)) {
            self.digits(ctx)?;
        } else if neg {
            return Err(self.error_at(ctx, start, "No number after minus sign"));
        } else {
            return Err(self.unexpected_token(ctx, start));
        }
        if self.eat(ctx, b'.') {
            if !matches!(self.peek(ctx), Some(48..=57)) {
                return Err(self.error_at(ctx, self.pos, "Unterminated fractional number"));
            }
            self.digits(ctx)?;
        }
        if self.eat(ctx, b'e') || self.eat(ctx, b'E') {
            if !self.eat(ctx, b'+') {
                self.eat(ctx, b'-');
            }
            if !matches!(self.peek(ctx), Some(48..=57)) {
                return Err(self.error_at(ctx, self.pos, "Exponent part is missing a number"));
            }
            self.digits(ctx)?;
        }
        let mut bytes = TrackedVec::new();
        ctx.allocate(|runtime| bytes.try_reserve_exact(&runtime.heap.ledger, self.pos - start))?;
        for unit in Flat::of(ctx.heap(), self.text).units(start, self.pos) {
            bytes.push_reserved(unit as u8);
        }
        let text = std::str::from_utf8(&bytes).unwrap();
        let (num, _) = sandbolt_dtoa::atod(text, 10, sandbolt_dtoa::AtodFlags::default());
        Ok(Value::number(num))
    }

    fn number<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        let value = self.number_raw(ctx)?;
        ctx.root(value)
    }
}

fn push<T: Copy>(ctx: &mut ExecCtx<'_, '_>, buffer: &mut TrackedVec<T>, value: T) -> JsResult<()> {
    if buffer.len() < buffer.capacity() {
        buffer.push_reserved(value);
        Ok(())
    } else {
        ctx.allocate(|runtime| buffer.push(&runtime.heap.ledger, value))
    }
}

/// Makes room in `buffer` for `additional` more units, units above 0xFF
/// among them when `wide` says.
fn reserve(
    ctx: &mut ExecCtx<'_, '_>,
    buffer: &mut UnitBuffer,
    additional: usize,
    wide: bool,
) -> JsResult<()> {
    if buffer.has_room(additional, wide) {
        return Ok(());
    }
    ctx.allocate(|runtime| buffer.reserve(&runtime.heap.ledger, additional, wide))
}

struct Reviver {
    /// The text, a flat string.
    text: StrId,
    callback: Value,
}

impl Reviver {
    fn visit<'s>(
        &self,
        ctx: &mut ExecCtx<'_, 's>,
        holder: Handle<'s>,
        key: PropKey,
        meta: Option<Handle<'s>>,
    ) -> JsResult<Value> {
        let _frame = TraversalFrame::enter(ctx)?;
        let value = get(ctx, holder, key)?;
        let record = meta.and_then(|meta| {
            let ObjectData::HostObject(meta) =
                &ctx.heap().object(ctx.get(meta).as_object().unwrap()).data
            else {
                unreachable!()
            };
            let HostObjectData::JsonParseNode {
                value: original,
                start,
                end,
            } = **meta
            else {
                unreachable!()
            };
            same_value_raw(ctx.heap(), original, ctx.get(value)).then_some((start, end))
        });
        let mut source = None;
        if ctx.get(value).as_object().is_some() {
            let meta = meta.filter(|_| record.is_some());
            if is_array(ctx, ctx.get(value))? {
                let length = get(ctx, value, LENGTH)?;
                let length = to_length(ctx, length)?;
                for index in 0..length {
                    let value = ctx.get(value);
                    let meta = meta.map(|meta| ctx.get(meta));
                    ctx.with_scope(|ctx| {
                        let key = index_key(ctx, index)?;
                        self.update(ctx, value, key, meta)
                    })?;
                }
            } else {
                let keys = enumerable_keys(ctx, value)?;
                for &key in keys.iter() {
                    let value = ctx.get(value);
                    let meta = meta.map(|meta| ctx.get(meta));
                    ctx.with_scope(|ctx| self.update(ctx, value, key, meta))?;
                }
            }
        } else if let Some((start, end)) = record {
            source = Some(slice(ctx, self.text, start, end)?);
        }
        let context = ctx.new_object()?;
        if let Some(source) = source {
            data(ctx, context, "source", source, DATA)?;
        }
        let key = key_value(ctx, key)?;
        let callback = ctx.root(self.callback)?;
        call_with(ctx, callback, holder, &[key, value, context])
    }

    fn update<'s>(
        &self,
        ctx: &mut ExecCtx<'_, 's>,
        holder: Value,
        key: PropKey,
        meta: Option<Value>,
    ) -> JsResult<()> {
        let holder = ctx.root(holder)?;
        let meta = if let Some(meta) = meta {
            let meta = ctx.root(meta)?;
            let child = get(ctx, meta, key)?;
            ctx.get(child).as_object().map(|_| child)
        } else {
            None
        };
        let value = self.visit(ctx, holder, key, meta)?;
        if value == Value::UNDEFINED {
            delete_property(ctx, holder, key)?;
        } else {
            let value = ctx.root(value)?;
            crate::ops::property::define_own_property(
                ctx,
                holder,
                key,
                crate::ops::property::PropertyDescriptor::data(value),
            )?;
        }
        Ok(())
    }
}

fn enumerable_keys<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
) -> JsResult<TrackedVec<PropKey>> {
    let mut keys = own_property_keys(ctx, object)?;
    for &key in keys.iter() {
        if let Some(value) = ctx.heap().dynamic_key_value(key) {
            ctx.root(value)?;
        }
    }
    let mut i = 0;
    for j in 0..keys.len() {
        let key = keys[j];
        if !matches!(key.decode(), KeyKind::Symbol(_))
            && get_own_property_descriptor(ctx, object, key)?
                .is_some_and(|desc| desc.enumerable == Some(true))
        {
            keys[i] = key;
            i += 1;
        }
    }
    keys.truncate(i);
    Ok(keys)
}

fn stringify<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let value = arg(ctx, args, 0)?;
    let replacer = arg(ctx, args, 1)?;
    let replacer = ctx.get(replacer);
    let (replacer, keys) = if callable(ctx, replacer) {
        (Some(replacer), None)
    } else if is_array(ctx, replacer)? {
        let replacer = ctx.root(replacer)?;
        (None, Some(replacer_keys(ctx, replacer)?))
    } else {
        (None, None)
    };
    let space = arg(ctx, args, 2)?;
    let (gap, size) = gap(ctx, space)?;
    let holder = ctx.new_object()?;
    let empty = ctx.intern_str("")?;
    super::super::define_data(ctx, holder, empty, value, DATA)?;
    let to_json = ctx.intern_str("toJSON")?;
    if let Some(root) = ctx.heap().dynamic_key_value(to_json) {
        ctx.root(root)?;
    }
    let gap_slice = &gap[..size];
    let gap_wide = gap_slice.iter().any(|&u| u > 0xff);
    let mut writer = Writer::new(ctx, keys.as_deref(), replacer, gap_slice, gap_wide, to_json);
    let Some(value) = writer.prepare(ctx, holder, empty)? else {
        return Ok(Value::UNDEFINED);
    };
    writer.emit_top(ctx, value)?;
    let text = ctx.new_string_from_units(std::mem::take(&mut writer.output))?;
    Ok(ctx.get(text))
}

fn replacer_keys<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    replacer: Handle<'s>,
) -> JsResult<TrackedVec<PropKey>> {
    let length = get(ctx, replacer, LENGTH)?;
    let length = to_length(ctx, length)?;
    let mut keys = TrackedVec::new();
    for index in 0..length {
        ctx.check_interrupt()?;
        let replacer = ctx.get(replacer);
        let key = ctx.with_scope(|ctx| {
            let replacer = ctx.root(replacer)?;
            let value = get_index(ctx, replacer, index)?;
            let is_name = match ctx.get(value).decode() {
                ValueKind::String(_) | ValueKind::Int32(_) | ValueKind::Float64(_) => true,
                ValueKind::Object(id) => matches!(
                    ctx.heap().object(id).data,
                    ObjectData::StringObject(_) | ObjectData::NumberObject(_)
                ),
                _ => false,
            };
            if is_name {
                let text = to_string(ctx, value)?;
                Ok(Some(ctx.intern(text)?))
            } else {
                Ok(None)
            }
        })?;
        if let Some(key) = key
            && !keys.contains(&key)
        {
            if let Some(root) = ctx.heap().dynamic_key_value(key) {
                ctx.root(root)?;
            }
            push(ctx, &mut keys, key)?;
        }
    }
    Ok(keys)
}

fn gap<'s>(ctx: &mut ExecCtx<'_, 's>, space: Handle<'s>) -> JsResult<([u16; 10], usize)> {
    let space = match ctx.get(space).decode() {
        ValueKind::Object(id) => match ctx.heap().object(id).data {
            ObjectData::NumberObject(_) => Value::number(to_number(ctx, space)?),
            ObjectData::StringObject(_) => {
                let string = to_string(ctx, space)?;
                ctx.get(string)
            }
            _ => Value::UNDEFINED,
        },
        _ => ctx.get(space),
    };
    let mut gap = [32; 10];
    let size = match space.decode() {
        ValueKind::String(id) => {
            for (slot, unit) in gap.iter_mut().zip(ctx.heap().string_units(id)) {
                *slot = unit;
            }
            ctx.heap().string(id).len().min(10) as usize
        }
        _ => space.as_number().unwrap_or_default().clamp(0.0, 10.0) as usize,
    };
    Ok((gap, size))
}

#[inline]
fn is_json_plain_primitive(value: Value) -> bool {
    matches!(
        value.decode(),
        ValueKind::Null
            | ValueKind::Bool(_)
            | ValueKind::Int32(_)
            | ValueKind::Float64(_)
            | ValueKind::String(_)
            | ValueKind::Undefined
            | ValueKind::Symbol(_)
    )
}

#[inline]
fn is_plain_object_or_array(heap: &Heap, id: ObjId) -> bool {
    matches!(
        heap.object(id).data,
        ObjectData::Ordinary | ObjectData::Array(_)
    )
}

#[inline]
fn shape_has_array_index(heap: &Heap, shape: &crate::shape::Shape) -> bool {
    shape.has_index_key()
        || shape
            .props
            .iter()
            .any(|prop| heap.array_index(prop.key).is_some())
}

#[inline]
fn flat_latin1(heap: &Heap, id: StrId) -> Option<&[u8]> {
    match heap.string(id) {
        JsString::Latin1 { data, .. } => Some(data),
        JsString::InlineLatin1 { len, data, .. } => Some(&data[..usize::from(*len)]),
        JsString::Slice {
            base,
            start,
            len,
            is_wide: false,
        } => {
            let start = *start as usize;
            let end = start + *len as usize;
            heap.string(*base)
                .as_latin1()
                .map(|bytes| &bytes[start..end])
        }
        _ => None,
    }
}

#[inline]
fn needs_json_escape(byte: u8) -> bool {
    byte < 0x20 || byte == b'"' || byte == b'\\'
}

fn measure_latin1_quoted(
    bytes: &[u8],
    stop: &mut Option<Box<dyn FnMut() -> bool>>,
) -> Result<(usize, Option<usize>), ()> {
    let mut first_esc: Option<usize> = None;
    let mut extra = 0usize;
    let check_stop = stop.is_some() && bytes.len() >= 4096;
    if !check_stop {
        if let Some(pos) = bytes.iter().position(|&b| needs_json_escape(b)) {
            first_esc = Some(pos);
            for &b in &bytes[pos..] {
                if needs_json_escape(b) {
                    extra += match b {
                        8 | 9 | 10 | 12 | 13 | b'"' | b'\\' => 1,
                        _ => 5,
                    };
                }
            }
        }
    } else {
        let mut offset = 0usize;
        while offset < bytes.len() {
            if interrupted(stop) {
                return Err(());
            }
            let end = (offset + 4096).min(bytes.len());
            let chunk = &bytes[offset..end];
            if first_esc.is_none() {
                if let Some(pos) = chunk.iter().position(|&b| needs_json_escape(b)) {
                    let abs = offset + pos;
                    first_esc = Some(abs);
                    for &b in &chunk[pos..] {
                        if needs_json_escape(b) {
                            extra += match b {
                                8 | 9 | 10 | 12 | 13 | b'"' | b'\\' => 1,
                                _ => 5,
                            };
                        }
                    }
                }
            } else {
                for &b in chunk {
                    if needs_json_escape(b) {
                        extra += match b {
                            8 | 9 | 10 | 12 | 13 | b'"' | b'\\' => 1,
                            _ => 5,
                        };
                    }
                }
            }
            offset = end;
        }
    }
    Ok((bytes.len() + 2 + extra, first_esc))
}

fn write_escaped_latin1_reserved(output: &mut UnitBuffer, bytes: &[u8], first_esc: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut run_start = 0usize;
    let mut i = first_esc;
    while i < bytes.len() {
        let byte = bytes[i];
        if needs_json_escape(byte) {
            if i > run_start {
                output.push_latin1_reserved(&bytes[run_start..i]);
            }
            match byte {
                8 => output.push_latin1_reserved(b"\\b"),
                9 => output.push_latin1_reserved(b"\\t"),
                10 => output.push_latin1_reserved(b"\\n"),
                12 => output.push_latin1_reserved(b"\\f"),
                13 => output.push_latin1_reserved(b"\\r"),
                b'"' => output.push_latin1_reserved(b"\\\""),
                b'\\' => output.push_latin1_reserved(b"\\\\"),
                _ => {
                    let esc = [
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[usize::from(byte >> 4)],
                        HEX[usize::from(byte & 15)],
                    ];
                    output.push_latin1_reserved(&esc);
                }
            }
            run_start = i + 1;
        }
        i += 1;
    }
    if run_start < bytes.len() {
        output.push_latin1_reserved(&bytes[run_start..]);
    }
}

struct Writer<'a> {
    keys: Option<&'a [PropKey]>,
    replacer: Option<Value>,
    gap: &'a [u16],
    gap_wide: bool,
    output: UnitBuffer,
    parents: TrackedVec<ObjId>,
    to_json: PropKey,
    realm: RealmId,
    cache_epoch: usize,
    obj_proto: Option<(ObjId, ShapeId)>,
    arr_proto: Option<(ObjId, ShapeId)>,
    plain_obj_shape: Option<ShapeId>,
    plain_obj_has_proto: bool,
}

impl<'a> Writer<'a> {
    fn new(
        ctx: &ExecCtx<'_, '_>,
        keys: Option<&'a [PropKey]>,
        replacer: Option<Value>,
        gap: &'a [u16],
        gap_wide: bool,
        to_json: PropKey,
    ) -> Self {
        let mut writer = Self {
            keys,
            replacer,
            gap,
            gap_wide,
            output: UnitBuffer::default(),
            parents: TrackedVec::new(),
            to_json,
            realm: ctx.realm_id(),
            cache_epoch: usize::MAX,
            obj_proto: None,
            arr_proto: None,
            plain_obj_shape: None,
            plain_obj_has_proto: false,
        };
        writer.refresh_proto_cache(ctx.heap());
        writer
    }

    fn refresh_proto_cache(&mut self, heap: &Heap) {
        self.cache_epoch = heap.collection_count();
        self.plain_obj_shape = None;
        let realm = heap.realm(self.realm);
        let obj_proto = realm.object_proto;
        let obj_proto_obj = heap.object(obj_proto);
        let obj_proto_shape_id = obj_proto_obj.shape;
        let obj_proto_shape = heap.shape(obj_proto_shape_id);
        self.obj_proto = if !matches!(
            obj_proto_obj.data,
            ObjectData::Proxy(_) | ObjectData::ModuleNamespace(_)
        ) && !obj_proto_shape.is_mutable()
            && obj_proto_shape.proto.is_none()
            && obj_proto_shape.find(self.to_json).is_none()
        {
            Some((obj_proto, obj_proto_shape_id))
        } else {
            None
        };
        let arr_proto = realm.array_proto;
        let arr_proto_obj = heap.object(arr_proto);
        let arr_proto_shape_id = arr_proto_obj.shape;
        let arr_proto_shape = heap.shape(arr_proto_shape_id);
        self.arr_proto = if self.obj_proto.is_some()
            && !matches!(
                arr_proto_obj.data,
                ObjectData::Proxy(_) | ObjectData::ModuleNamespace(_)
            )
            && !arr_proto_shape.is_mutable()
            && arr_proto_shape.proto == Some(obj_proto)
            && arr_proto_shape.find(self.to_json).is_none()
        {
            Some((arr_proto, arr_proto_shape_id))
        } else {
            None
        };
    }

    #[inline]
    fn ensure_cache(&mut self, heap: &Heap) {
        if heap.collection_count() != self.cache_epoch {
            self.refresh_proto_cache(heap);
        }
    }

    #[inline]
    fn obj_proto_intact(&self, heap: &Heap) -> bool {
        self.obj_proto
            .is_some_and(|(id, shape)| heap.object(id).shape == shape)
    }

    #[inline]
    fn has_no_to_json(&mut self, heap: &Heap, id: ObjId) -> bool {
        self.ensure_cache(heap);
        let obj = heap.object(id);
        if self.plain_obj_shape == Some(obj.shape)
            && (!self.plain_obj_has_proto || self.obj_proto_intact(heap))
        {
            return true;
        }
        if matches!(
            obj.data,
            ObjectData::Proxy(_) | ObjectData::ModuleNamespace(_)
        ) {
            return false;
        }
        let shape = heap.shape(obj.shape);
        if shape.find(self.to_json).is_some() {
            return false;
        }
        let Some(mut cur) = shape.proto else {
            return true;
        };
        if let Some((op, ops)) = self.obj_proto {
            if cur == op && heap.object(op).shape == ops {
                return true;
            }
            if let Some((ap, aps)) = self.arr_proto
                && cur == ap
                && heap.object(ap).shape == aps
                && heap.object(op).shape == ops
            {
                return true;
            }
        }
        for _ in 0..16 {
            let cur_obj = heap.object(cur);
            if matches!(
                cur_obj.data,
                ObjectData::Proxy(_) | ObjectData::ModuleNamespace(_)
            ) {
                return false;
            }
            let cur_shape = heap.shape(cur_obj.shape);
            if cur_shape.find(self.to_json).is_some() {
                return false;
            }
            let Some(next) = cur_shape.proto else {
                return true;
            };
            if let Some((op, ops)) = self.obj_proto
                && next == op
                && heap.object(op).shape == ops
            {
                return true;
            }
            cur = next;
        }
        false
    }

    fn prepare<'s>(
        &self,
        ctx: &mut ExecCtx<'_, 's>,
        holder: Handle<'s>,
        key: PropKey,
    ) -> JsResult<Option<Handle<'s>>> {
        let value = get(ctx, holder, key)?;
        let value = ctx.mut_root(ctx.get(value))?;
        if matches!(
            ctx.get(value.handle()).decode(),
            ValueKind::Object(_) | ValueKind::BigInt(_) | ValueKind::ShortBigInt(_)
        ) {
            let to_json = get(ctx, value.handle(), self.to_json)?;
            if callable(ctx, ctx.get(to_json)) {
                let name = key_value(ctx, key)?;
                let output = call_with(ctx, to_json, value.handle(), &[name])?;
                ctx.set(&value, output);
            }
        }
        if let Some(replacer) = self.replacer {
            let replacer = ctx.root(replacer)?;
            let name = key_value(ctx, key)?;
            let output = call_with(ctx, replacer, holder, &[name, value.handle()])?;
            ctx.set(&value, output);
        }
        if let Some(id) = ctx.get(value.handle()).as_object() {
            let primitive = match &ctx.heap().object(id).data {
                ObjectData::NumberObject(_) => Some(Value::number(to_number(ctx, value.handle())?)),
                ObjectData::StringObject(_) => {
                    let text = to_string(ctx, value.handle())?;
                    Some(ctx.get(text))
                }
                ObjectData::BooleanObject(data) => Some(data.value),
                ObjectData::BigIntObject(data) => Some(data.value),
                _ => None,
            };
            if let Some(primitive) = primitive {
                ctx.set(&value, primitive);
            }
        }
        match ctx.get(value.handle()).decode() {
            ValueKind::Undefined | ValueKind::Symbol(_) => Ok(None),
            ValueKind::BigInt(_) | ValueKind::ShortBigInt(_) => {
                Err(ctx.throw_type_error("cannot serialize BigInt"))
            }
            v if matches!(v, ValueKind::Object(_)) && callable(ctx, ctx.get(value.handle())) => {
                Ok(None)
            }
            _ => Ok(Some(value.handle())),
        }
    }

    fn emit_top<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<()> {
        let raw = ctx.get(value);
        match raw.decode() {
            ValueKind::Object(id) => {
                if let Some(raw_str) = raw_text(ctx, raw) {
                    ctx.check_interrupt()?;
                    let ValueKind::String(str_id) = raw_str.decode() else {
                        unreachable!()
                    };
                    return self.write_string(ctx, str_id, false);
                }
                self.emit_object(ctx, value, id, 0)
            }
            _ => {
                ctx.check_interrupt()?;
                self.write_primitive(ctx, raw)
            }
        }
    }

    fn emit<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Handle<'s>,
        depth: usize,
    ) -> JsResult<()> {
        let raw = ctx.get(value);
        match raw.decode() {
            ValueKind::Object(id) => {
                if let Some(raw_str) = raw_text(ctx, raw) {
                    let ValueKind::String(str_id) = raw_str.decode() else {
                        unreachable!()
                    };
                    return self.write_string(ctx, str_id, false);
                }
                self.emit_object(ctx, value, id, depth)
            }
            _ => self.write_primitive(ctx, raw),
        }
    }

    #[inline]
    fn write_primitive(&mut self, ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
        match value.decode() {
            ValueKind::Null => self.ascii(ctx, b"null"),
            ValueKind::Bool(true) => self.ascii(ctx, b"true"),
            ValueKind::Bool(false) => self.ascii(ctx, b"false"),
            ValueKind::Int32(_) | ValueKind::Float64(_) => self.write_number(ctx, value),
            ValueKind::String(id) => self.write_string(ctx, id, true),
            _ => unreachable!(),
        }
    }

    fn emit_object<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Handle<'s>,
        id: ObjId,
        depth: usize,
    ) -> JsResult<()> {
        if self.parents.contains(&id) {
            return Err(ctx.throw_type_error("cyclic JSON value"));
        }
        // Fast leaf path for plain immutable Ordinary objects whose enumerable
        // properties are all non-BigInt primitives, avoiding recursion,
        // temporary vectors and handle scopes.
        if self.replacer.is_none()
            && self.keys.is_none()
            && self.try_emit_plain_leaf_object(ctx, id, depth)?
        {
            return Ok(());
        }
        let _frame = TraversalFrame::enter(ctx)?;
        push(ctx, &mut self.parents, id)?;
        let result = self.compound(ctx, value, id, depth);
        self.parents.pop();
        result
    }

    fn try_emit_plain_leaf_object(
        &mut self,
        ctx: &mut ExecCtx<'_, '_>,
        id: ObjId,
        depth: usize,
    ) -> JsResult<bool> {
        let (shape_id, prop_count, cached_all_plain) = {
            let heap = ctx.heap();
            let obj = heap.object(id);
            if !matches!(obj.data, ObjectData::Ordinary)
                || obj.flags.contains(ObjectFlags::IS_EXOTIC)
            {
                return Ok(false);
            }
            self.ensure_cache(heap);
            let shape_id = obj.shape;
            if self.plain_obj_shape == Some(shape_id) {
                if !obj
                    .props
                    .iter()
                    .all(|&val| !val.is_auto_native() && is_json_plain_primitive(val))
                {
                    return Ok(false);
                }
                (shape_id, obj.props.len(), true)
            } else {
                let shape = heap.shape(shape_id);
                if shape.is_mutable() || shape.has_index_key() {
                    return Ok(false);
                }
                let mut can_cache = shape.proto.is_none()
                    || self
                        .obj_proto
                        .is_some_and(|(op, _)| shape.proto == Some(op));
                for (slot, prop) in shape.props.iter().enumerate() {
                    if heap.array_index(prop.key).is_some() {
                        return Ok(false);
                    }
                    if !prop.flags.contains(PropFlags::ENUMERABLE)
                        || matches!(prop.key.decode(), KeyKind::Symbol(_))
                    {
                        can_cache = false;
                        continue;
                    }
                    if prop.flags.contains(PropFlags::ACCESSOR) {
                        return Ok(false);
                    }
                    if prop.key == self.to_json {
                        can_cache = false;
                    }
                    let val = obj.props[slot];
                    if val.is_auto_native() || !is_json_plain_primitive(val) {
                        return Ok(false);
                    }
                }
                if can_cache {
                    self.plain_obj_shape = Some(shape_id);
                    self.plain_obj_has_proto = shape.proto.is_some();
                }
                (shape_id, shape.props.len(), can_cache)
            }
        };
        let traversal_depth = TRAVERSAL_DEPTH.get();
        if traversal_depth >= MAX_DEPTH
            || !ctx.runtime.vm.check_budget(
                traversal_depth * (FRAME_COST / size_of::<Value>()),
                0,
                0,
            )
        {
            return Err(ctx.stack_overflow());
        }
        if depth == 0 {
            ctx.check_interrupt()?;
        }
        self.byte(ctx, b'{')?;
        let mut written = false;
        for slot in 0..prop_count {
            let (key, val) = {
                let heap = ctx.heap();
                let prop = heap.shape(shape_id).props[slot];
                if !cached_all_plain
                    && (!prop.flags.contains(PropFlags::ENUMERABLE)
                        || matches!(prop.key.decode(), KeyKind::Symbol(_)))
                {
                    continue;
                }
                (prop.key, heap.object(id).props[slot])
            };
            if matches!(val.decode(), ValueKind::Undefined | ValueKind::Symbol(_)) {
                continue;
            }
            self.separator(ctx, written, depth + 1)?;
            self.quote_key(ctx, key)?;
            self.colon(ctx)?;
            self.write_primitive(ctx, val)?;
            written = true;
        }
        if written {
            self.indent(ctx, depth)?;
        }
        self.byte(ctx, b'}')?;
        Ok(true)
    }

    fn emit_array_element_slow<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Value,
        index: u64,
        written: bool,
        depth: usize,
    ) -> JsResult<()> {
        ctx.with_scope(|ctx| {
            let holder = ctx.root(value)?;
            let key = index_key(ctx, index)?;
            let item = self.prepare(ctx, holder, key)?;
            self.separator(ctx, written, depth + 1)?;
            if let Some(item) = item {
                self.emit(ctx, item, depth + 1)
            } else {
                self.ascii(ctx, b"null")
            }
        })
    }

    fn emit_object_prop_slow<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Value,
        key: PropKey,
        written: bool,
        depth: usize,
    ) -> JsResult<bool> {
        ctx.with_scope(|ctx| {
            let holder = ctx.root(value)?;
            let Some(item) = self.prepare(ctx, holder, key)? else {
                return Ok(false);
            };
            self.separator(ctx, written, depth + 1)?;
            self.quote_key(ctx, key)?;
            self.colon(ctx)?;
            self.emit(ctx, item, depth + 1)?;
            Ok(true)
        })
    }

    fn compound<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Handle<'s>,
        id: ObjId,
        depth: usize,
    ) -> JsResult<()> {
        let is_array = match &ctx.heap().object(id).data {
            ObjectData::Array(_) => true,
            ObjectData::Ordinary => false,
            ObjectData::Proxy(_) => is_array(ctx, Value::object(id))?,
            _ => false,
        };
        let (open, close) = if is_array { (b'[', b']') } else { (b'{', b'}') };
        self.byte(ctx, open)?;
        let mut written = false;
        if is_array {
            if self.replacer.is_none()
                && ctx
                    .heap()
                    .object(id)
                    .flags
                    .contains(ObjectFlags::FAST_ARRAY)
            {
                let length = match &ctx.heap().object(id).data {
                    ObjectData::Array(array) => array.length,
                    _ => unreachable!(),
                };
                let elem_slot = ctx.mut_root(Value::UNDEFINED)?;
                for index in 0..length {
                    if index & 1023 == 0 {
                        ctx.check_interrupt()?;
                    }
                    let elem = {
                        let obj = ctx.heap().object(id);
                        if obj.flags.contains(ObjectFlags::FAST_ARRAY) {
                            let ObjectData::Array(array) = &obj.data else {
                                unreachable!()
                            };
                            array
                                .elements
                                .get(index as usize)
                                .copied()
                                .unwrap_or(Value::EMPTY)
                        } else {
                            Value::EMPTY
                        }
                    };
                    if elem == Value::EMPTY {
                        self.emit_array_element_slow(
                            ctx,
                            ctx.get(value),
                            u64::from(index),
                            written,
                            depth,
                        )?;
                        written = true;
                        continue;
                    }
                    match elem.decode() {
                        ValueKind::Undefined | ValueKind::Symbol(_) | ValueKind::Null => {
                            self.separator(ctx, written, depth + 1)?;
                            self.ascii(ctx, b"null")?;
                        }
                        ValueKind::Bool(true) => {
                            self.separator(ctx, written, depth + 1)?;
                            self.ascii(ctx, b"true")?;
                        }
                        ValueKind::Bool(false) => {
                            self.separator(ctx, written, depth + 1)?;
                            self.ascii(ctx, b"false")?;
                        }
                        ValueKind::Int32(_) | ValueKind::Float64(_) => {
                            self.separator(ctx, written, depth + 1)?;
                            self.write_number(ctx, elem)?;
                        }
                        ValueKind::String(str_id) => {
                            self.separator(ctx, written, depth + 1)?;
                            self.write_string(ctx, str_id, true)?;
                        }
                        ValueKind::Object(child_id)
                            if is_plain_object_or_array(ctx.heap(), child_id)
                                && self.has_no_to_json(ctx.heap(), child_id) =>
                        {
                            self.separator(ctx, written, depth + 1)?;
                            ctx.set(&elem_slot, elem);
                            self.emit_object(ctx, elem_slot.handle(), child_id, depth + 1)?;
                        }
                        _ => {
                            self.emit_array_element_slow(
                                ctx,
                                ctx.get(value),
                                u64::from(index),
                                written,
                                depth,
                            )?;
                        }
                    }
                    written = true;
                }
            } else {
                let length = get(ctx, value, LENGTH)?;
                let length = to_length(ctx, length)?;
                for index in 0..length {
                    if index & 1023 == 0 {
                        ctx.check_interrupt()?;
                    }
                    self.emit_array_element_slow(ctx, ctx.get(value), index, written, depth)?;
                    written = true;
                }
            }
        } else if self.replacer.is_none()
            && self.keys.is_none()
            && matches!(ctx.heap().object(id).data, ObjectData::Ordinary)
            && !ctx.heap().object(id).flags.contains(ObjectFlags::IS_EXOTIC)
            && !ctx.heap().shape(ctx.heap().object(id).shape).is_mutable()
            && !shape_has_array_index(ctx.heap(), ctx.heap().shape(ctx.heap().object(id).shape))
        {
            let initial_shape = ctx.heap().object(id).shape;
            let prop_count = ctx.heap().shape(initial_shape).props.len();
            let mut entries = TrackedVec::<(PropKey, u32)>::new();
            ctx.allocate(|runtime| entries.try_reserve(&runtime.heap.ledger, prop_count))?;
            for slot in 0..prop_count {
                let prop = ctx.heap().shape(initial_shape).props[slot];
                if prop.flags.contains(PropFlags::ENUMERABLE)
                    && !matches!(prop.key.decode(), KeyKind::Symbol(_))
                {
                    if let Some(root) = ctx.heap().dynamic_key_value(prop.key) {
                        ctx.root(root)?;
                    }
                    entries.push_reserved((prop.key, slot as u32));
                }
            }
            let child_slot = ctx.mut_root(Value::UNDEFINED)?;
            for (idx, &(key, slot)) in entries.iter().enumerate() {
                if idx & 1023 == 0 {
                    ctx.check_interrupt()?;
                }
                let direct = {
                    let heap = ctx.heap();
                    let obj = heap.object(id);
                    if obj.shape == initial_shape {
                        let shape = heap.shape(initial_shape);
                        if !shape.is_mutable()
                            && let Some(prop) = shape.props.get(slot as usize)
                            && prop.key == key
                            && !prop.flags.contains(PropFlags::ACCESSOR)
                        {
                            let val = obj.props[slot as usize];
                            (!val.is_auto_native()).then_some(val)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                if let Some(val) = direct {
                    match val.decode() {
                        ValueKind::Undefined | ValueKind::Symbol(_) => continue,
                        ValueKind::Null
                        | ValueKind::Bool(_)
                        | ValueKind::Int32(_)
                        | ValueKind::Float64(_)
                        | ValueKind::String(_) => {
                            self.separator(ctx, written, depth + 1)?;
                            self.quote_key(ctx, key)?;
                            self.colon(ctx)?;
                            self.write_primitive(ctx, val)?;
                            written = true;
                            continue;
                        }
                        ValueKind::Object(child_id)
                            if is_plain_object_or_array(ctx.heap(), child_id)
                                && self.has_no_to_json(ctx.heap(), child_id) =>
                        {
                            self.separator(ctx, written, depth + 1)?;
                            self.quote_key(ctx, key)?;
                            self.colon(ctx)?;
                            ctx.set(&child_slot, val);
                            self.emit_object(ctx, child_slot.handle(), child_id, depth + 1)?;
                            written = true;
                            continue;
                        }
                        _ => {}
                    }
                }
                written |= self.emit_object_prop_slow(ctx, ctx.get(value), key, written, depth)?;
            }
        } else {
            let keys;
            let property_list = if let Some(list) = self.keys {
                list
            } else {
                keys = enumerable_keys(ctx, value)?;
                &keys
            };
            for (idx, &key) in property_list.iter().enumerate() {
                if idx & 1023 == 0 {
                    ctx.check_interrupt()?;
                }
                written |= self.emit_object_prop_slow(ctx, ctx.get(value), key, written, depth)?;
            }
        }
        if written {
            self.indent(ctx, depth)?;
        }
        self.byte(ctx, close)
    }

    #[inline]
    fn byte(&mut self, ctx: &mut ExecCtx<'_, '_>, byte: u8) -> JsResult<()> {
        if !self.output.has_room(1, false) {
            reserve(ctx, &mut self.output, 1, false)?;
        }
        self.output.push_byte_reserved(byte);
        Ok(())
    }

    #[inline]
    fn ascii(&mut self, ctx: &mut ExecCtx<'_, '_>, text: &[u8]) -> JsResult<()> {
        if !self.output.has_room(text.len(), false) {
            reserve(ctx, &mut self.output, text.len(), false)?;
        }
        self.output.push_latin1_reserved(text);
        Ok(())
    }

    #[inline]
    fn colon(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        if self.gap.is_empty() {
            self.byte(ctx, b':')
        } else {
            self.ascii(ctx, b": ")
        }
    }

    #[inline]
    fn separator(
        &mut self,
        ctx: &mut ExecCtx<'_, '_>,
        written: bool,
        depth: usize,
    ) -> JsResult<()> {
        if written {
            self.byte(ctx, b',')?;
        }
        if !self.gap.is_empty() {
            self.indent(ctx, depth)?;
        }
        Ok(())
    }

    fn indent(&mut self, ctx: &mut ExecCtx<'_, '_>, depth: usize) -> JsResult<()> {
        if !self.gap.is_empty() {
            let total = 1usize.saturating_add(depth.saturating_mul(self.gap.len()));
            if !self.output.has_room(total, self.gap_wide) {
                reserve(ctx, &mut self.output, total, self.gap_wide)?;
            }
            self.output.push_byte_reserved(b'\n');
            for _ in 0..depth {
                self.output.push_utf16_reserved(self.gap);
            }
        }
        Ok(())
    }

    fn write_number(&mut self, ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
        let mut buf = [0u8; 64];
        match value.decode() {
            ValueKind::Int32(n) => {
                let len = sandbolt_dtoa::i32toa(&mut buf, n);
                self.ascii(ctx, &buf[..len])
            }
            ValueKind::Float64(n) => {
                if !n.is_finite() {
                    return self.ascii(ctx, b"null");
                }
                if n == 0.0 {
                    return self.byte(ctx, b'0');
                }
                let int = n as i32;
                if f64::from(int) == n {
                    let len = sandbolt_dtoa::i32toa(&mut buf, int);
                    return self.ascii(ctx, &buf[..len]);
                }
                let opts = sandbolt_dtoa::DtoaOptions::default();
                let max_len = sandbolt_dtoa::dtoa_max_len(n, 10, 0, opts);
                if max_len <= buf.len() {
                    let len = sandbolt_dtoa::dtoa_into(&mut buf, n, 10, 0, opts);
                    self.ascii(ctx, &buf[..len])
                } else {
                    let text = sandbolt_dtoa::dtoa(n, 10, 0, opts);
                    self.ascii(ctx, text.as_bytes())
                }
            }
            _ => unreachable!(),
        }
    }

    fn quote_key(&mut self, ctx: &mut ExecCtx<'_, '_>, key: PropKey) -> JsResult<()> {
        match key.decode() {
            KeyKind::Index(index) => {
                let mut buf = [0u8; 16];
                buf[0] = b'"';
                let len = sandbolt_dtoa::u64toa(&mut buf[1..], u64::from(index));
                buf[1 + len] = b'"';
                self.ascii(ctx, &buf[..len + 2])
            }
            KeyKind::String(atom) => {
                if let Some(text) = AtomTable::builtin(atom) {
                    let bytes = text.as_bytes();
                    let (out_len, first_esc) = {
                        let stop = &mut ctx.runtime.vm.interrupt_handler;
                        match measure_latin1_quoted(bytes, stop) {
                            Ok(res) => res,
                            Err(()) => {
                                ctx.take_exception();
                                return Err(Abrupt::Interrupted);
                            }
                        }
                    };
                    if !self.output.has_room(out_len, false) {
                        reserve(ctx, &mut self.output, out_len, false)?;
                    }
                    self.output.push_byte_reserved(b'"');
                    if let Some(first_esc) = first_esc {
                        write_escaped_latin1_reserved(&mut self.output, bytes, first_esc);
                    } else {
                        self.output.push_latin1_reserved(bytes);
                    }
                    self.output.push_byte_reserved(b'"');
                    Ok(())
                } else {
                    let id = ctx.heap().atoms.dynamic_string(atom).unwrap();
                    self.write_string(ctx, id, true)
                }
            }
            KeyKind::Symbol(_) => unreachable!("JSON.stringify skips symbol keys"),
        }
    }

    fn write_string(&mut self, ctx: &mut ExecCtx<'_, '_>, id: StrId, quote: bool) -> JsResult<()> {
        if flat_latin1(ctx.heap(), id).is_some() {
            return self.write_flat_latin1(ctx, id, quote);
        }
        // Measured first, so the text goes in without a copy of its own.
        let mut len = 0;
        let mut wide = false;
        escape(ctx, id, quote, |unit| {
            len += 1;
            wide |= unit > 0xff;
        })?;
        reserve(ctx, &mut self.output, len, wide)?;
        let output = &mut self.output;
        escape(ctx, id, quote, |unit| output.push_reserved(unit))
    }

    fn write_flat_latin1(
        &mut self,
        ctx: &mut ExecCtx<'_, '_>,
        id: StrId,
        quote: bool,
    ) -> JsResult<()> {
        if !quote {
            let len = {
                let runtime = &mut *ctx.runtime;
                let stop = &mut runtime.vm.interrupt_handler;
                let bytes = flat_latin1(&runtime.heap, id).unwrap();
                if stop.is_some() && bytes.len() >= 4096 {
                    for _ in (0..bytes.len()).step_by(4096) {
                        if interrupted(stop) {
                            ctx.take_exception();
                            return Err(Abrupt::Interrupted);
                        }
                    }
                }
                bytes.len()
            };
            if !self.output.has_room(len, false) {
                reserve(ctx, &mut self.output, len, false)?;
            }
            let bytes = flat_latin1(ctx.heap(), id).unwrap();
            self.output.push_latin1_reserved(bytes);
            return Ok(());
        }
        let (out_len, first_esc) = {
            let runtime = &mut *ctx.runtime;
            let stop = &mut runtime.vm.interrupt_handler;
            let bytes = flat_latin1(&runtime.heap, id).unwrap();
            match measure_latin1_quoted(bytes, stop) {
                Ok(res) => res,
                Err(()) => {
                    ctx.take_exception();
                    return Err(Abrupt::Interrupted);
                }
            }
        };
        if !self.output.has_room(out_len, false) {
            reserve(ctx, &mut self.output, out_len, false)?;
        }
        let bytes = flat_latin1(ctx.heap(), id).unwrap();
        self.output.push_byte_reserved(b'"');
        if let Some(first_esc) = first_esc {
            write_escaped_latin1_reserved(&mut self.output, bytes, first_esc);
        } else {
            self.output.push_latin1_reserved(bytes);
        }
        self.output.push_byte_reserved(b'"');
        Ok(())
    }
}

/// Gives `emit` the units JSON.stringify writes for string `id`: its units
/// as they are, or when `quote` says, in quotes with JSON's escapes. It
/// polls the interrupt handler every 4096 units.
fn escape(
    ctx: &mut ExecCtx<'_, '_>,
    id: StrId,
    quote: bool,
    mut emit: impl FnMut(u16),
) -> JsResult<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let runtime = &mut *ctx.runtime;
    let stop = &mut runtime.vm.interrupt_handler;
    let mut units = runtime.heap.string_units(id).peekable();
    if quote {
        emit(u16::from(b'"'));
    }
    let mut index = 0usize;
    while let Some(unit) = units.next() {
        if index.is_multiple_of(4096) && interrupted(stop) {
            ctx.take_exception();
            return Err(Abrupt::Interrupted);
        }
        index += 1;
        if !quote {
            emit(unit);
            continue;
        }
        let letter = match unit {
            8 => b'b',
            9 => b't',
            10 => b'n',
            12 => b'f',
            13 => b'r',
            34 => b'"',
            92 => b'\\',
            0xd800..=0xdbff if matches!(units.peek(), Some(0xdc00..=0xdfff)) => {
                emit(unit);
                if let Some(low) = units.next() {
                    index += 1;
                    emit(low);
                }
                continue;
            }
            0..=31 | 0xd800..=0xdfff => {
                emit(u16::from(b'\\'));
                emit(u16::from(b'u'));
                for shift in [12, 8, 4, 0] {
                    emit(u16::from(HEX[usize::from((unit >> shift) & 15)]));
                }
                continue;
            }
            _ => {
                emit(unit);
                continue;
            }
        };
        emit(u16::from(b'\\'));
        emit(u16::from(letter));
    }
    if quote {
        emit(u16::from(b'"'));
    }
    Ok(())
}
