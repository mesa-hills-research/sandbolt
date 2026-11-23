mod regexp;
mod strings;
mod uri;

pub(crate) use regexp::create as regexp_literal;

#[cfg(test)]
mod tests;

use crate::atom::{Atom, PropKey};
use crate::ledger::{HeapLedger, TrackedVec};
use crate::object::ObjectData;
use crate::ops::call::{call, is_callable};
use crate::ops::convert::{to_object, to_string};
use crate::ops::property::get_property;
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::string::JsString;
use crate::symbol::WellKnown;
use crate::value::{JsResult, ObjId, StrId, Value, ValueKind};

use super::{STRING_RE_BASE, define_data, install_method, make_native_fn};

const MAX_STRING_LEN: usize = (1 << 30) - 1;
const LENGTH: PropKey = PropKey::atom(Atom::LENGTH);
fn method_flags() -> PropFlags {
    PropFlags::WRITABLE | PropFlags::CONFIGURABLE
}
const STRING: NativeFnId = NativeFnId(STRING_RE_BASE);
const REGEXP: NativeFnId = NativeFnId(STRING_RE_BASE + 0x80);

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    strings::install(ctx)?;
    regexp::install(ctx)?;
    uri::install(ctx)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id.0 - STRING_RE_BASE {
        0..=0x7f => strings::dispatch(id.0 - STRING_RE_BASE, ctx, this, args),
        0x80..=0xff => regexp::dispatch(id.0 - REGEXP.0, ctx, this, args),
        0x100..=0x105 => uri::dispatch(id.0 - (STRING_RE_BASE + 0x100), ctx, args),
        _ => Err(ctx.throw_type_error("unknown text builtin")),
    }
}

fn sym(ctx: &ExecCtx<'_, '_>, name: WellKnown) -> PropKey {
    let heap = ctx.heap();
    heap.symbols
        .get(heap.symbol_registry.well_known(name))
        .unwrap()
        .key()
}

fn arg<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, index: usize) -> JsResult<Handle<'s>> {
    args.get(index)
        .map_or_else(|| ctx.root(Value::UNDEFINED), Ok)
}

fn nullish(value: Value) -> bool {
    matches!(value, Value::NULL | Value::UNDEFINED)
}

fn require_object_coercible(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
    if nullish(value) {
        Err(ctx.throw_type_error("cannot convert null or undefined"))
    } else {
        Ok(())
    }
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, key: PropKey) -> JsResult<Handle<'s>> {
    let value = get_property(ctx, object, key)?;
    ctx.root(value)
}

fn index_key<'s>(ctx: &mut ExecCtx<'_, 's>, index: usize) -> JsResult<PropKey> {
    match u32::try_from(index).ok().and_then(PropKey::index) {
        Some(key) => Ok(key),
        None => ctx.intern_str(&index.to_string()),
    }
}

fn get_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    index: usize,
) -> JsResult<Handle<'s>> {
    let key = index_key(ctx, index)?;
    get(ctx, object, key)
}

fn put_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    index: usize,
    value: Handle<'s>,
) -> JsResult<()> {
    let key = index_key(ctx, index)?;
    define_data(ctx, object, key, value, PropFlags::DEFAULT_DATA)
}

fn callable(ctx: &ExecCtx<'_, '_>, value: Value) -> bool {
    value
        .as_object()
        .is_some_and(|id| is_callable(ctx.heap(), id))
}

fn get_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<Option<Handle<'s>>> {
    let boxed = to_object(ctx, object)?;
    let value = crate::ops::property::get_property_with_receiver(ctx, boxed, key, object)?;
    if nullish(value) {
        Ok(None)
    } else if callable(ctx, value) {
        Ok(Some(ctx.root(value)?))
    } else {
        Err(ctx.throw_type_error("expected a callable method"))
    }
}

fn call_with<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    method: Handle<'s>,
    receiver: Handle<'s>,
    args: &[Handle<'s>],
) -> JsResult<Value> {
    let args = ctx.args(args)?;
    call(ctx, method, receiver, args)
}

fn global<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str, value: Handle<'s>) -> JsResult<()> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let key = ctx.intern_str(name)?;
    define_data(ctx, global, key, value, method_flags())
}

fn methods<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    base: u16,
    list: &[(u16, &str, u32)],
) -> JsResult<()> {
    let target = ctx.get(target);
    for &(offset, name, length) in list {
        ctx.with_scope(|ctx| {
            let target = ctx.root(target)?;
            let key = ctx.intern_str(name)?;
            install_method(ctx, target, key, NativeFnId(base + offset), name, length)
        })?;
    }
    Ok(())
}

fn symbol_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    symbol: WellKnown,
    id: NativeFnId,
    name: &str,
    length: u32,
) -> JsResult<()> {
    install_method(ctx, target, sym(ctx, symbol), id, name, length)
}

fn iterator_proto<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: ObjId,
    id: NativeFnId,
    tag: &str,
) -> JsResult<()> {
    let proto = ctx.root(Value::object(proto))?;
    let next = make_native_fn(ctx, id, "next", 0, false)?;
    let key = ctx.intern_str("next")?;
    define_data(ctx, proto, key, next, method_flags())?;
    let tag = ctx.new_string(tag)?;
    define_data(
        ctx,
        proto,
        sym(ctx, WellKnown::ToStringTag),
        tag,
        PropFlags::CONFIGURABLE,
    )
}

fn iterator_result<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    value: Handle<'s>,
    done: bool,
) -> JsResult<Value> {
    let result = ctx.new_object()?;
    let key = ctx.intern_str("value")?;
    define_data(ctx, result, key, value, PropFlags::DEFAULT_DATA)?;
    let done = ctx.root(Value::bool(done))?;
    let key = ctx.intern_str("done")?;
    define_data(ctx, result, key, done, PropFlags::DEFAULT_DATA)?;
    Ok(ctx.get(result))
}

fn as_string(value: Value) -> Option<StrId> {
    if let ValueKind::String(id) = value.decode() {
        Some(id)
    } else {
        None
    }
}

fn string_id(value: Value) -> StrId {
    as_string(value).expect("expected a string")
}

fn units(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<TrackedVec<u16>> {
    let id = string_id(value);
    let len = ctx.heap().string(id).len() as usize;
    let mut out = TrackedVec::new();
    ctx.allocate(|rt| out.try_reserve_exact(&rt.heap.ledger, len))?;
    for unit in ctx.heap().string_units(id) {
        out.push_reserved(unit);
    }
    ctx.check_interrupt()?;
    Ok(out)
}

fn text<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<TrackedVec<u16>> {
    let str = to_string(ctx, value)?;
    units(ctx, ctx.get(str))
}

fn is_high(u: u16) -> bool {
    (0xd800..=0xdbff).contains(&u)
}
fn is_low(u: u16) -> bool {
    (0xdc00..=0xdfff).contains(&u)
}
fn is_surrogate(u: u16) -> bool {
    (0xd800..=0xdfff).contains(&u)
}

fn advance(units: &[u16], pos: usize, unicode: bool) -> usize {
    if unicode
        && pos < units.len().saturating_sub(1)
        && is_high(units[pos])
        && is_low(units[pos + 1])
    {
        pos + 2
    } else {
        pos + 1
    }
}

fn code_point(units: &[u16], pos: usize) -> (u32, usize) {
    let end = advance(units, pos, true);
    let high = u32::from(units[pos]);
    if end == pos + 1 {
        (high, end)
    } else {
        (
            0x10000 + ((high - 0xd800) << 10) + (u32::from(units[pos + 1]) - 0xdc00),
            end,
        )
    }
}

fn string_error(ctx: &mut ExecCtx<'_, '_>) -> crate::Abrupt {
    ctx.throw_range_error("invalid string length")
}

struct Builder(TrackedVec<u16>);

impl Builder {
    fn new() -> Self {
        Self(TrackedVec::new())
    }

    fn reserve(&mut self, ctx: &mut ExecCtx<'_, '_>, n: usize, exact: bool) -> JsResult<()> {
        if n > MAX_STRING_LEN.saturating_sub(self.0.len()) {
            return Err(string_error(ctx));
        }
        if n > self.0.capacity() - self.0.len() {
            ctx.allocate(|rt| {
                if exact {
                    self.0.try_reserve_exact(&rt.heap.ledger, n)
                } else {
                    self.0.try_reserve(&rt.heap.ledger, n)
                }
            })?;
        }
        Ok(())
    }

    fn unit(&mut self, ctx: &mut ExecCtx<'_, '_>, u: u16) -> JsResult<()> {
        self.reserve(ctx, 1, false)?;
        self.0.push_reserved(u);
        Ok(())
    }

    fn code_point(&mut self, ctx: &mut ExecCtx<'_, '_>, c: u32) -> JsResult<()> {
        if c < 0x10000 {
            self.unit(ctx, c as u16)
        } else {
            let c = c - 0x10000;
            self.extend(
                ctx,
                &[0xd800 | (c >> 10) as u16, 0xdc00 | (c & 0x3ff) as u16],
            )
        }
    }

    fn extend(&mut self, ctx: &mut ExecCtx<'_, '_>, units: &[u16]) -> JsResult<()> {
        self.reserve(ctx, units.len(), false)?;
        for (chunk_idx, chunk) in units.chunks(4096).enumerate() {
            if chunk_idx > 0 {
                ctx.check_interrupt()?;
            }
            for &unit in chunk {
                self.0.push_reserved(unit);
            }
        }
        Ok(())
    }

    fn ascii(&mut self, ctx: &mut ExecCtx<'_, '_>, ascii: &[u8]) -> JsResult<()> {
        self.reserve(ctx, ascii.len(), false)?;
        for &byte in ascii {
            self.0.push_reserved(u16::from(byte));
        }
        Ok(())
    }

    fn string(&mut self, ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
        let id = string_id(value);
        self.reserve(ctx, ctx.heap().string(id).len() as usize, false)?;
        for unit in ctx.heap().string_units(id) {
            self.0.push_reserved(unit);
        }
        ctx.check_interrupt()
    }

    fn finish<'s>(self, ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
        let mut data = Some(self.0);
        let id = ctx.allocate(|rt| {
            rt.heap.strings.try_reserve_slot(&rt.heap.ledger)?;
            Ok(rt
                .heap
                .strings
                .insert_reserved(JsString::Utf16(data.take().unwrap())))
        })?;
        ctx.root(Value::string(id))
    }

    fn value(self, ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
        let value = self.finish(ctx)?;
        Ok(ctx.get(value))
    }
}

struct Charge {
    ledger: HeapLedger,
    bytes: usize,
}

impl Charge {
    fn new(ctx: &mut ExecCtx<'_, '_>, bytes: usize) -> JsResult<Self> {
        ctx.allocate(|rt| rt.heap.ledger.charge_bytes(bytes))?;
        Ok(Self {
            ledger: ctx.heap().ledger.clone(),
            bytes,
        })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.ledger.release_bytes(self.bytes);
    }
}

struct Substitution<'a, 's> {
    input: &'a [u16],
    matched: &'a [u16],
    position: usize,
    captures: &'a [Handle<'s>],
    groups: Option<Handle<'s>>,
}

impl<'s> Substitution<'_, 's> {
    fn append(
        &self,
        ctx: &mut ExecCtx<'_, 's>,
        out: &mut Builder,
        replacement: &[u16],
    ) -> JsResult<()> {
        let mut pos = 0;
        while pos < replacement.len() {
            ctx.check_interrupt()?;
            let tail = &replacement[pos..];
            let prefix = tail.iter().position(|&u| u == 36).unwrap_or(tail.len());
            out.extend(ctx, &tail[..prefix])?;
            pos += prefix;
            let tail = &tail[prefix..];
            if tail.is_empty() {
                break;
            }
            match tail.get(1).copied() {
                Some(36) => {
                    out.unit(ctx, 36)?;
                    pos += 2;
                }
                Some(38) => {
                    out.extend(ctx, self.matched)?;
                    pos += 2;
                }
                Some(96) => {
                    out.extend(ctx, &self.input[..self.position])?;
                    pos += 2;
                }
                Some(39) => {
                    let end = (self.position + self.matched.len()).min(self.input.len());
                    out.extend(ctx, &self.input[end..])?;
                    pos += 2;
                }
                Some(60) if self.groups.is_some() && tail[2..].contains(&62) => {
                    let end = tail[2..].iter().position(|&c| c == 62).unwrap() + 2;
                    let groups = ctx.get(self.groups.unwrap());
                    ctx.with_scope(|ctx| {
                        let groups = ctx.root(groups)?;
                        let name = ctx.new_utf16(&tail[2..end])?;
                        let key = ctx.intern(name)?;
                        let value = get(ctx, groups, key)?;
                        if ctx.get(value) != Value::UNDEFINED {
                            let value = to_string(ctx, value)?;
                            out.string(ctx, ctx.get(value))?;
                        }
                        Ok(())
                    })?;
                    pos += end + 1;
                }
                Some(first @ 48..=57) => {
                    let one = usize::from(first - 48);
                    let (index, consumed) = match tail.get(2).copied() {
                        Some(second @ 48..=57)
                            if one * 10 + usize::from(second - 48) <= self.captures.len() =>
                        {
                            (one * 10 + usize::from(second - 48), 3)
                        }
                        _ => (one, 2),
                    };
                    if index != 0
                        && let Some(&capture) = self.captures.get(index - 1)
                    {
                        let value = ctx.get(capture);
                        if value != Value::UNDEFINED {
                            out.string(ctx, value)?;
                        }
                        pos += consumed;
                    } else {
                        out.unit(ctx, 36)?;
                        pos += 1;
                    }
                }
                _ => {
                    out.unit(ctx, 36)?;
                    pos += 1;
                }
            }
        }
        Ok(())
    }
}
