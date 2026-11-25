use std::cell::Cell;

use super::*;
use crate::atom::KeyKind;
use crate::ledger::TrackedBox;
use crate::object::ErrorKind;
use crate::ops::compare::same_value_raw;
use crate::ops::convert::{key_value, to_length, to_number, to_string};
use crate::ops::property::{delete_property, get_own_property_descriptor, own_property_keys};
use crate::value::{Abrupt, ObjId};

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
    let input = units(ctx, ctx.get(text))?;
    let reviver = args
        .get(1)
        .filter(|&value| !raw && callable(ctx, ctx.get(value)));
    let mut parser = Parser {
        input: &input,
        pos: 0,
        record_source: reviver.is_some(),
    };
    if raw && matches!(parser.peek(), None | Some(9 | 10 | 13 | 32 | 91 | 123)) {
        return Err(invalid(ctx));
    }
    let (value, meta) = parser.value(ctx)?;
    if !raw {
        parser.whitespace(ctx)?;
    }
    if parser.peek().is_some() {
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
        input: &input,
        callback: ctx.get(reviver),
    }
    .visit(ctx, root, empty, meta)
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

struct Parser<'a> {
    input: &'a [u16],
    pos: usize,
    record_source: bool,
}

type Parsed<'s> = (Handle<'s>, Option<Handle<'s>>);

impl Parser<'_> {
    fn peek(&self) -> Option<u16> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        self.pos += 1;
        if self.pos.is_multiple_of(4096) {
            ctx.check_interrupt()?;
        }
        Ok(())
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() != Some(u16::from(byte)) {
            return false;
        }
        self.pos += 1;
        true
    }

    fn expect(&mut self, ctx: &mut ExecCtx<'_, '_>, byte: u8) -> JsResult<()> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(invalid(ctx))
        }
    }

    fn whitespace(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        while matches!(self.peek(), Some(9 | 10 | 13 | 32)) {
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
        let value = match self.peek() {
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
        let array = self.eat(b'[');
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
        if self.eat(close) {
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
            if self.eat(close) {
                break;
            }
            self.expect(ctx, b',')?;
        }
        Ok((object, meta))
    }

    fn string<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        self.expect(ctx, b'"')?;
        let mut out = TrackedVec::new();
        loop {
            let unit = self.peek().ok_or_else(|| invalid(ctx))?;
            self.advance(ctx)?;
            let unit = match unit {
                34 => break,
                0..=31 => return Err(invalid(ctx)),
                92 => self.escape(ctx)?,
                _ => unit,
            };
            push(ctx, &mut out, unit)?;
        }
        ctx.new_utf16(&out)
    }

    fn escape(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<u16> {
        let unit = self.peek().ok_or_else(|| invalid(ctx))?;
        self.advance(ctx)?;
        Ok(match unit {
            34 | 47 | 92 => unit,
            98 => 8,
            102 => 12,
            110 => 10,
            114 => 13,
            116 => 9,
            117 => {
                let mut char = 0;
                for _ in 0..4 {
                    let digit = match self.peek() {
                        Some(n @ 48..=57) => n - 48,
                        Some(n @ 65..=70) => n - 65 + 10,
                        Some(n @ 97..=102) => n - 97 + 10,
                        _ => return Err(invalid(ctx)),
                    };
                    self.advance(ctx)?;
                    char = (char << 4) | digit;
                }
                char
            }
            _ => return Err(invalid(ctx)),
        })
    }

    fn digits(&mut self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
        if !matches!(self.peek(), Some(48..=57)) {
            return Err(invalid(ctx));
        }
        while matches!(self.peek(), Some(48..=57)) {
            self.advance(ctx)?;
        }
        Ok(())
    }

    fn number<'s>(&mut self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        let start = self.pos;
        self.eat(b'-');
        if !self.eat(b'0') {
            self.digits(ctx)?;
        }
        if self.eat(b'.') {
            self.digits(ctx)?;
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            self.digits(ctx)?;
        }
        let units = &self.input[start..self.pos];
        let mut bytes = TrackedVec::new();
        ctx.allocate(|runtime| bytes.try_reserve_exact(&runtime.heap.ledger, units.len()))?;
        for (index, &unit) in units.iter().enumerate() {
            if index % 4096 == 0 {
                ctx.check_interrupt()?;
            }
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

struct Reviver<'a> {
    input: &'a [u16],
    callback: Value,
}

impl Reviver<'_> {
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
            source = Some(ctx.new_utf16(&self.input[start..end])?);
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
        output: TrackedVec::new(),
        parents: TrackedVec::new(),
    };
    let Some(value) = writer.prepare(ctx, holder, empty)? else {
        return Ok(Value::UNDEFINED);
    };
    writer.emit(ctx, value, 0)?;
    let text = ctx.new_utf16(&writer.output)?;
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
    output: TrackedVec<u16>,
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
        push(ctx, &mut self.output, u16::from(open))?;
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
        push(ctx, &mut self.output, u16::from(close))
    }

    fn ascii(&mut self, ctx: &mut ExecCtx<'_, '_>, text: &[u8]) -> JsResult<()> {
        for &byte in text {
            push(ctx, &mut self.output, u16::from(byte))?;
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
                    push(ctx, &mut self.output, unit)?;
                }
            }
        }
        Ok(())
    }

    fn text(&mut self, ctx: &mut ExecCtx<'_, '_>, value: Value, quote: bool) -> JsResult<()> {
        let text = units(ctx, value)?;
        let mut marks = text.iter().copied().enumerate().peekable();
        if quote {
            self.ascii(ctx, b"\"")?;
        }
        while let Some((index, unit)) = marks.next() {
            if index % 4096 == 0 {
                ctx.check_interrupt()?;
            }
            if !quote {
                push(ctx, &mut self.output, unit)?;
                continue;
            }
            match unit {
                8 => self.ascii(ctx, b"\\b")?,
                9 => self.ascii(ctx, b"\\t")?,
                10 => self.ascii(ctx, b"\\n")?,
                12 => self.ascii(ctx, b"\\f")?,
                13 => self.ascii(ctx, b"\\r")?,
                34 => self.ascii(ctx, b"\\\"")?,
                92 => self.ascii(ctx, b"\\\\")?,
                0xd800..=0xdbff if matches!(marks.peek(), Some((_, 0xdc00..=0xdfff))) => {
                    push(ctx, &mut self.output, unit)?;
                    push(ctx, &mut self.output, marks.next().unwrap().1)?;
                }
                0..=31 | 0xd800..=0xdfff => {
                    let hex = b"0123456789abcdef";
                    self.ascii(
                        ctx,
                        &[
                            b'\\',
                            b'u',
                            hex[usize::from(unit >> 12)],
                            hex[usize::from((unit >> 8) & 15)],
                            hex[usize::from((unit >> 4) & 15)],
                            hex[usize::from(unit & 15)],
                        ],
                    )?;
                }
                _ => push(ctx, &mut self.output, unit)?,
            }
        }
        if quote {
            self.ascii(ctx, b"\"")?;
        }
        Ok(())
    }
}
