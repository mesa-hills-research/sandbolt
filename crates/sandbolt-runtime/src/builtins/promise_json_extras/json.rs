use std::cell::Cell;

use super::*;
use crate::atom::KeyKind;
use crate::heap::Heap;
use crate::ledger::TrackedBox;
use crate::object::ErrorKind;
use crate::ops::compare::same_value_raw;
use crate::ops::convert::{key_value, to_length, to_number, to_string};
use crate::ops::property::{delete_property, get_own_property_descriptor, own_property_keys};
use crate::string::UnitBuffer;
use crate::value::{Abrupt, ObjId, StrId};

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
    let mut parser = Parser {
        text: id,
        pos: 0,
        record_source: reviver.is_some(),
    };
    if raw && matches!(parser.peek(ctx), None | Some(9 | 10 | 13 | 32 | 91 | 123)) {
        return Err(invalid(ctx));
    }
    let (value, meta) = parser.value(ctx)?;
    if !raw {
        parser.whitespace(ctx)?;
    }
    if parser.peek(ctx).is_some() {
        return Err(invalid(ctx));
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
    Invalid,
    Interrupted,
}

impl Fault {
    fn throw(self, ctx: &mut ExecCtx<'_, '_>) -> Abrupt {
        match self {
            Self::Invalid => invalid(ctx),
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
        let unit: u16 = (*units.get(end).ok_or(Fault::Invalid)?).into();
        match unit {
            34 => break,
            0..=31 => return Err(Fault::Invalid),
            92 => {
                if units.get(end + 1).map(|&unit| unit.into()) == Some(117) {
                    wide |= hex(units.get(end + 2..end + 6)).is_some_and(|unit| unit > 0xff);
                }
                end += 2;
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
        let digit = match digit.into() {
            digit @ 48..=57 => digit - 48,
            digit @ 65..=70 => digit - 65 + 10,
            digit @ 97..=102 => digit - 97 + 10,
            _ => return None,
        };
        Some((unit << 4) | digit)
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
        let escape: u16 = (*units.get(index + 1).ok_or(Fault::Invalid)?).into();
        let unit = match escape {
            34 | 47 | 92 => escape,
            98 => 8,
            102 => 12,
            110 => 10,
            114 => 13,
            116 => 9,
            117 => {
                let unit = hex(units.get(index + 2..index + 6)).ok_or(Fault::Invalid)?;
                index += 4;
                unit
            }
            _ => return Err(Fault::Invalid),
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

fn invalid(ctx: &mut ExecCtx<'_, '_>) -> Abrupt {
    ctx.throw_error(ErrorKind::Syntax, "invalid JSON")
}

thread_local! {
    static TRAVERSAL_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct TraversalFrame;

impl TraversalFrame {
    fn enter(ctx: &mut ExecCtx<'_, '_>) -> JsResult<Self> {
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
        ctx.check_interrupt()?;
        TRAVERSAL_DEPTH.set(depth + 1);
        Ok(Self)
    }
}

impl Drop for TraversalFrame {
    fn drop(&mut self) {
        TRAVERSAL_DEPTH.set(TRAVERSAL_DEPTH.get() - 1);
    }
}

struct Parser {
    /// The text, a flat string.
    text: StrId,
    pos: usize,
    record_source: bool,
}

type Parsed<'s> = (Handle<'s>, Option<Handle<'s>>);

impl Parser {
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

    fn expect(&mut self, ctx: &mut ExecCtx<'_, '_>, byte: u8) -> JsResult<()> {
        if self.eat(ctx, byte) {
            Ok(())
        } else {
            Err(invalid(ctx))
        }
    }

    fn whitespace(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        while matches!(self.peek(ctx), Some(9 | 10 | 13 | 32)) {
            self.advance(ctx)?;
        }
        Ok(())
    }

    fn literal<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        word: &[u8],
        value: Value,
    ) -> JsResult<Handle<'s>> {
        for &byte in word {
            self.expect(ctx, byte)?;
        }
        ctx.root(value)
    }

    fn value<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Parsed<'s>> {
        let _frame = TraversalFrame::enter(ctx)?;
        self.whitespace(ctx)?;
        let start = self.pos;
        let value = match self.peek(ctx) {
            Some(34) => self.string(ctx),
            Some(110) => self.literal(ctx, b"null", Value::NULL),
            Some(116) => self.literal(ctx, b"true", Value::bool(true)),
            Some(102) => self.literal(ctx, b"false", Value::bool(false)),
            Some(45 | 48..=57) => self.number(ctx),
            Some(91 | 123) => return self.compound(ctx),
            _ => return Err(invalid(ctx)),
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
            self.expect(ctx, b'{')?;
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
                    let key = self.string(ctx)?;
                    let key = ctx.intern(key)?;
                    self.whitespace(ctx)?;
                    self.expect(ctx, b':')?;
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
            self.expect(ctx, b',')?;
        }
        Ok((object, meta))
    }

    fn string<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        self.expect(ctx, b'"')?;
        let start = self.pos;
        let runtime = &mut *ctx.runtime;
        let stop = &mut runtime.vm.interrupt_handler;
        let scanned = match Flat::of(&runtime.heap, self.text) {
            Flat::Latin1(units) => scan(units, start, stop),
            Flat::Utf16(units) => scan(units, start, stop),
        };
        let (end, wide) = scanned.map_err(|fault| fault.throw(ctx))?;
        // Escapes only shorten it, so the string fits in the token's length.
        let mut out = UnitBuffer::default();
        reserve(ctx, &mut out, end - start, wide)?;
        let runtime = &mut *ctx.runtime;
        let stop = &mut runtime.vm.interrupt_handler;
        let decoded = match Flat::of(&runtime.heap, self.text) {
            Flat::Latin1(units) => decode(&units[start..end], &mut out, stop),
            Flat::Utf16(units) => decode(&units[start..end], &mut out, stop),
        };
        decoded.map_err(|fault| fault.throw(ctx))?;
        self.pos = end + 1;
        ctx.new_string_from_units(out)
    }

    fn digits(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        if !matches!(self.peek(ctx), Some(48..=57)) {
            return Err(invalid(ctx));
        }
        while matches!(self.peek(ctx), Some(48..=57)) {
            self.advance(ctx)?;
        }
        Ok(())
    }

    fn number<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        let start = self.pos;
        self.eat(ctx, b'-');
        if !self.eat(ctx, b'0') {
            self.digits(ctx)?;
        }
        if self.eat(ctx, b'.') {
            self.digits(ctx)?;
        }
        if self.eat(ctx, b'e') || self.eat(ctx, b'E') {
            if !self.eat(ctx, b'+') {
                self.eat(ctx, b'-');
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
        ctx.root(Value::number(num))
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

fn push_unit(ctx: &mut ExecCtx<'_, '_>, buffer: &mut UnitBuffer, unit: u16) -> JsResult<()> {
    reserve(ctx, buffer, 1, unit > 0xff)?;
    buffer.push_reserved(unit);
    Ok(())
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
    let mut writer = Writer {
        keys: keys.as_deref(),
        replacer,
        gap: &gap[..size],
        output: UnitBuffer::default(),
        parents: TrackedVec::new(),
    };
    let Some(value) = writer.prepare(ctx, holder, empty)? else {
        return Ok(Value::UNDEFINED);
    };
    writer.emit(ctx, value, 0)?;
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

struct Writer<'a> {
    keys: Option<&'a [PropKey]>,
    replacer: Option<Value>,
    gap: &'a [u16],
    output: UnitBuffer,
    parents: TrackedVec<ObjId>,
}

impl Writer<'_> {
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
            let to_json = get_named(ctx, value.handle(), "toJSON")?;
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

    fn emit<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Handle<'s>,
        depth: usize,
    ) -> JsResult<()> {
        let _frame = TraversalFrame::enter(ctx)?;
        match ctx.get(value).decode() {
            ValueKind::Null => self.ascii(ctx, b"null"),
            ValueKind::Bool(true) => self.ascii(ctx, b"true"),
            ValueKind::Bool(false) => self.ascii(ctx, b"false"),
            ValueKind::Int32(_) | ValueKind::Float64(_) => {
                if !ctx.get(value).as_number().unwrap().is_finite() {
                    return self.ascii(ctx, b"null");
                }
                let text = to_string(ctx, value)?;
                self.text(ctx, ctx.get(text), false)
            }
            ValueKind::String(_) => self.text(ctx, ctx.get(value), true),
            ValueKind::Object(id) => {
                if let Some(raw) = raw_text(ctx, ctx.get(value)) {
                    return self.text(ctx, raw, false);
                }
                if self.parents.contains(&id) {
                    return Err(ctx.throw_type_error("cyclic JSON value"));
                }
                push(ctx, &mut self.parents, id)?;
                let result = self.compound(ctx, value, depth);
                self.parents.pop();
                result
            }
            _ => unreachable!(),
        }
    }

    fn compound<'s>(
        &mut self,
        ctx: &mut ExecCtx<'_, 's>,
        value: Handle<'s>,
        depth: usize,
    ) -> JsResult<()> {
        let is_array = is_array(ctx, ctx.get(value))?;
        let (open, close) = if is_array { (b'[', b']') } else { (b'{', b'}') };
        self.ascii(ctx, &[open])?;
        let mut written = false;
        if is_array {
            let length = get(ctx, value, LENGTH)?;
            let length = to_length(ctx, length)?;
            for index in 0..length {
                ctx.check_interrupt()?;
                let value = ctx.get(value);
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
                })?;
                written = true;
            }
        } else {
            let keys;
            let property_list = if let Some(list) = self.keys {
                list
            } else {
                keys = enumerable_keys(ctx, value)?;
                &keys
            };
            for &key in property_list {
                ctx.check_interrupt()?;
                let value = ctx.get(value);
                written |= ctx.with_scope(|ctx| {
                    let holder = ctx.root(value)?;
                    let Some(item) = self.prepare(ctx, holder, key)? else {
                        return Ok(false);
                    };
                    self.separator(ctx, written, depth + 1)?;
                    let name = key_value(ctx, key)?;
                    self.text(ctx, ctx.get(name), true)?;
                    self.ascii(ctx, if self.gap.is_empty() { b":" } else { b": " })?;
                    self.emit(ctx, item, depth + 1)?;
                    Ok(true)
                })?;
            }
        }
        if written {
            self.indent(ctx, depth)?;
        }
        self.ascii(ctx, &[close])
    }

    fn ascii(&mut self, ctx: &mut ExecCtx<'_, '_>, text: &[u8]) -> JsResult<()> {
        reserve(ctx, &mut self.output, text.len(), false)?;
        for &byte in text {
            self.output.push_reserved(u16::from(byte));
        }
        Ok(())
    }

    fn separator(
        &mut self,
        ctx: &mut ExecCtx<'_, '_>,
        written: bool,
        depth: usize,
    ) -> JsResult<()> {
        if written {
            self.ascii(ctx, b",")?;
        }
        self.indent(ctx, depth)
    }

    fn indent(&mut self, ctx: &mut ExecCtx<'_, '_>, depth: usize) -> JsResult<()> {
        if !self.gap.is_empty() {
            self.ascii(ctx, b"\n")?;
            for _ in 0..depth {
                for &unit in self.gap {
                    push_unit(ctx, &mut self.output, unit)?;
                }
            }
        }
        Ok(())
    }

    fn text(&mut self, ctx: &mut ExecCtx<'_, '_>, value: Value, quote: bool) -> JsResult<()> {
        let ValueKind::String(id) = value.decode() else {
            unreachable!()
        };
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
            return Err(Fault::Interrupted.throw(ctx));
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
