use std::fmt::{self, Write};

use crate::atom::{Atom, AtomTable, KeyKind, PropKey};
use crate::heap::Heap;
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::*;
use crate::realm::{NativeFn, NativeFnId};
use crate::root::{Args, ExecCtx, Handle};
use crate::symbol::{SymbolDescription, WellKnown};
use crate::value::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreferredType {
    Default,
    String,
    Number,
}

pub fn to_boolean<'s>(ctx: &ExecCtx<'_, 's>, value: Handle<'s>) -> bool {
    to_boolean_value(ctx.heap(), ctx.get(value))
}

pub(crate) fn to_boolean_value(heap: &Heap, value: Value) -> bool {
    match value.decode() {
        ValueKind::Null | ValueKind::Undefined | ValueKind::Uninitialized | ValueKind::Empty => {
            false
        }
        ValueKind::Bool(v) => v,
        ValueKind::Int32(v) => v != 0,
        ValueKind::Float64(v) => v != 0.0 && !v.is_nan(),
        ValueKind::ShortBigInt(v) => v != 0,
        ValueKind::String(id) => !heap.string(id).is_empty(),
        ValueKind::BigInt(id) => heap.bigints.get(id).unwrap().limbs.iter().any(|&n| n != 0),
        ValueKind::Object(id) => !heap.object(id).flags.contains(ObjectFlags::IS_HTML_DDA),
        ValueKind::CatchOffset(_)
        | ValueKind::Symbol(_)
        | ValueKind::Bytecode(_)
        | ValueKind::Module(_)
        | ValueKind::VarRef(_)
        | ValueKind::Accessor(_) => true,
    }
}

pub fn typeof_str<'s>(ctx: &ExecCtx<'_, 's>, value: Handle<'s>) -> &'static str {
    match ctx.get(value).decode() {
        ValueKind::Undefined | ValueKind::Uninitialized | ValueKind::Empty => "undefined",
        ValueKind::Bool(_) => "boolean",
        ValueKind::Int32(_) | ValueKind::Float64(_) => "number",
        ValueKind::ShortBigInt(_) | ValueKind::BigInt(_) => "bigint",
        ValueKind::String(_) => "string",
        ValueKind::Symbol(_) => "symbol",
        ValueKind::Object(id)
            if ctx
                .heap()
                .object(id)
                .flags
                .contains(ObjectFlags::IS_HTML_DDA) =>
        {
            "undefined"
        }
        ValueKind::Object(id) if super::call::is_callable(ctx.heap(), id) => "function",
        ValueKind::Null | ValueKind::Object(_) => "object",
        ValueKind::CatchOffset(_)
        | ValueKind::Bytecode(_)
        | ValueKind::Module(_)
        | ValueKind::VarRef(_)
        | ValueKind::Accessor(_) => "undefined",
    }
}

pub fn to_primitive<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    value: Handle<'s>,
    preferred: PreferredType,
) -> JsResult<Value> {
    let raw = ctx.get(value);
    if raw.as_object().is_none() {
        return Ok(raw);
    }
    ctx.with_scope(|ctx| {
        let value = ctx.root(raw)?;
        let sym = ctx
            .heap()
            .symbol_registry
            .well_known(WellKnown::ToPrimitive);
        let key = ctx.heap().symbols.get(sym).unwrap().key();
        let method = super::property::get_property(ctx, value, key)?;
        if method != Value::UNDEFINED && method != Value::NULL {
            let method = ctx.root(method)?;
            let arg = ctx.new_string(match preferred {
                PreferredType::Default => "default",
                PreferredType::String => "string",
                PreferredType::Number => "number",
            })?;
            let args = ctx.args(&[arg])?;
            let result = super::call::call(ctx, method, value, args)?;
            if result.as_object().is_none() {
                return Ok(result);
            }
            return Err(ctx.throw_type_error("Symbol.toPrimitive returned an object"));
        }
        let methods = if preferred == PreferredType::String {
            [Atom::TO_STRING, Atom::VALUE_OF]
        } else {
            [Atom::VALUE_OF, Atom::TO_STRING]
        };
        for key in methods {
            let method = super::property::get_property(ctx, value, PropKey::atom(key))?;
            if method
                .as_object()
                .is_some_and(|id| super::call::is_callable(ctx.heap(), id))
            {
                let method = ctx.root(method)?;
                let result = super::call::call(ctx, method, value, Args::empty())?;
                if result.as_object().is_none() {
                    return Ok(result);
                }
            }
        }
        Err(ctx.throw_type_error("cannot convert object to primitive"))
    })
}

pub fn to_number<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<f64> {
    let raw = ctx.get(value);
    ctx.with_scope(|ctx| {
        let value = ctx.root(raw)?;
        let raw = to_primitive(ctx, value, PreferredType::Number)?;
        if let Some(n) = raw.as_number() {
            return Ok(n);
        }
        match raw.decode() {
            ValueKind::Null => Ok(0.0),
            ValueKind::Bool(value) => Ok(f64::from(u8::from(value))),
            ValueKind::Symbol(_) => Err(ctx.throw_type_error("cannot convert Symbol to number")),
            ValueKind::ShortBigInt(_) | ValueKind::BigInt(_) => {
                Err(ctx.throw_type_error("cannot convert BigInt to number"))
            }
            ValueKind::String(id) => {
                ctx.root(raw)?;
                string_to_number(ctx, id)
            }
            _ => Ok(f64::NAN),
        }
    })
}

pub(crate) fn space(unit: u16) -> bool {
    qjs_unicode::is_space(u32::from(unit))
}

pub(crate) fn string_to_number(ctx: &mut ExecCtx<'_, '_>, id: StrId) -> JsResult<f64> {
    let mut first = None;
    let mut end = 0;
    for (i, unit) in ctx.heap().string_units(id).enumerate() {
        if !space(unit) {
            if unit > 127 {
                return Ok(f64::NAN);
            }
            first.get_or_insert(i);
            end = i + 1;
        }
    }
    let Some(first) = first else {
        return Ok(0.0);
    };
    let mut short = [0; 128];
    if end - first <= short.len() {
        let bytes = &mut short[..end - first];
        for (byte, unit) in bytes
            .iter_mut()
            .zip(ctx.heap().string_units(id).skip(first))
        {
            if unit > 127 {
                return Ok(f64::NAN);
            }
            *byte = unit as u8;
        }
        return Ok(parse_number(bytes));
    }
    ctx.allocate(|runtime| {
        let heap = &runtime.heap;
        let mut long = TrackedVec::new();
        long.try_reserve_exact(&heap.ledger, end - first)?;
        for unit in heap.string_units(id).skip(first).take(end - first) {
            if unit > 127 {
                return Ok(f64::NAN);
            }
            long.push_reserved(unit as u8);
        }
        Ok(parse_number(&long))
    })
}

fn parse_number(bytes: &[u8]) -> f64 {
    let prefixed = matches!(bytes, [b'0', b'x' | b'X' | b'o' | b'O' | b'b' | b'B', ..]);
    let flags = qjs_dtoa::AtodFlags {
        int_only: prefixed,
        accept_bin_oct: prefixed,
        ..Default::default()
    };
    let (number, consumed) = qjs_dtoa::atod(
        std::str::from_utf8(bytes).unwrap(),
        if prefixed { 0 } else { 10 },
        flags,
    );
    if consumed == bytes.len() {
        number
    } else {
        f64::NAN
    }
}

pub fn to_int32<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<i32> {
    Ok(to_uint32(ctx, value)? as i32)
}
pub fn to_uint32<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<u32> {
    let n = to_number(ctx, value)?;
    if !n.is_finite() {
        return Ok(0);
    }
    // Take the positive remainder before applying the sign so a value close to zero cannot round up to 2^32.
    let magnitude = (n.abs().trunc() % 4_294_967_296.0) as u32;
    Ok(if n.is_sign_negative() {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}
pub fn to_integer_or_infinity<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<f64> {
    let n = to_number(ctx, value)?;
    Ok(if n.is_nan() || n == 0.0 {
        0.0
    } else {
        n.trunc() + 0.0
    })
}
pub fn to_length<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<u64> {
    Ok(to_integer_or_infinity(ctx, value)?.clamp(0.0, 9_007_199_254_740_991.0) as u64)
}

pub fn to_string<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Handle<'s>> {
    let raw = to_primitive(ctx, value, PreferredType::String)?;
    match raw.decode() {
        ValueKind::String(_) => ctx.root(raw),
        ValueKind::Undefined => ctx.new_string("undefined"),
        ValueKind::Null => ctx.new_string("null"),
        ValueKind::Bool(v) => ctx.new_string(if v { "true" } else { "false" }),
        ValueKind::Int32(n) => number_text(ctx, n as f64),
        ValueKind::Float64(n) => number_text(ctx, n),
        ValueKind::ShortBigInt(n) => {
            let mut text = TextBuffer::new();
            write!(text, "{n}").unwrap();
            ctx.new_string(text.as_str())
        }
        ValueKind::BigInt(_) => {
            ctx.root(raw)?;
            let result = crate::vm::exec::numeric::bigint_text(ctx, raw)?;
            ctx.root(result)
        }
        ValueKind::Symbol(_) => Err(ctx.throw_type_error("cannot convert Symbol to string")),
        _ => Err(ctx.throw_type_error("cannot convert value to string")),
    }
}

fn number_text<'s>(ctx: &mut ExecCtx<'_, 's>, n: f64) -> JsResult<Handle<'s>> {
    let options = qjs_dtoa::DtoaOptions::default();
    let bytes = qjs_dtoa::dtoa_max_len(n, 10, 0, options);
    let ledger = ctx.heap().ledger.clone();
    ctx.allocate(|_| ledger.charge_bytes(bytes))?;
    let text = qjs_dtoa::dtoa(n, 10, 0, options);
    let result = ctx.new_string(&text);
    drop(text);
    ledger.release_bytes(bytes);
    result
}

pub fn to_property_key<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<PropKey> {
    let raw = to_primitive(ctx, value, PreferredType::String)?;
    let value = ctx.root(raw)?;
    if let ValueKind::Symbol(id) = raw.decode() {
        return Ok(ctx.heap().symbols.get(id).unwrap().key());
    }
    let string = to_string(ctx, value)?;
    ctx.intern(string)
}

pub(crate) fn key_value<'s>(ctx: &mut ExecCtx<'_, 's>, key: PropKey) -> JsResult<Handle<'s>> {
    match key.decode() {
        KeyKind::Index(index) => number_text(ctx, f64::from(index)),
        KeyKind::String(atom) if AtomTable::builtin(atom).is_some() => {
            ctx.new_string(AtomTable::builtin(atom).unwrap())
        }
        _ => match ctx.heap().dynamic_key_value(key) {
            Some(value) => ctx.root(value),
            None => Err(ctx.throw_type_error("invalid property key")),
        },
    }
}

pub fn to_object<'s>(ctx: &mut ExecCtx<'_, 's>, handle: Handle<'s>) -> JsResult<Handle<'s>> {
    let value = ctx.get(handle);
    macro_rules! boxed {
        ($proto:ident, $variant:ident, $data:ident) => {{
            let proto = ctx.realm().$proto;
            ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
                Ok(ObjectData::$variant(TrackedBox::new(
                    ledger,
                    $data { value },
                )?))
            })
        }};
    }
    match value.decode() {
        ValueKind::Object(_) => Ok(handle),
        ValueKind::Bool(_) => boxed!(boolean_proto, BooleanObject, BooleanObjectData),
        ValueKind::Int32(_) | ValueKind::Float64(_) => {
            boxed!(number_proto, NumberObject, NumberObjectData)
        }
        ValueKind::String(_) => boxed!(string_proto, StringObject, StringObjectData),
        ValueKind::Symbol(_) => boxed!(symbol_proto, SymbolObject, SymbolObjectData),
        ValueKind::BigInt(_) | ValueKind::ShortBigInt(_) => {
            boxed!(bigint_proto, BigIntObject, BigIntObjectData)
        }
        _ => Err(ctx.throw_type_error("cannot convert value to object")),
    }
}

pub(crate) fn builtin_native(id: NativeFnId) -> Option<NativeFn> {
    match id {
        NativeFnId::OBJECT_VALUE_OF => Some(object_value_of),
        NativeFnId::OBJECT_TO_STRING => Some(object_to_string),
        NativeFnId::WRAPPER_VALUE_OF => Some(wrapper_value_of),
        NativeFnId::WRAPPER_TO_STRING => Some(wrapper_to_string),
        NativeFnId::EMPTY => Some(|_, _, _| Ok(Value::UNDEFINED)),
        _ => None,
    }
}

fn object_value_of<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    let value = to_object(ctx, this)?;
    Ok(ctx.get(value))
}

fn object_to_string<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    let value = ctx.get(this);
    let default = match value.decode() {
        ValueKind::Object(id) => match ctx.heap().object(id).data {
            ObjectData::Array(_) => "Array",
            ObjectData::NativeFunction(_)
            | ObjectData::BytecodeFunction(_)
            | ObjectData::BoundFunction(_) => "Function",
            ObjectData::Error(_) => "Error",
            ObjectData::BooleanObject(_) => "Boolean",
            ObjectData::NumberObject(_) => "Number",
            ObjectData::StringObject(_) => "String",
            ObjectData::Arguments(_) | ObjectData::MappedArguments(_) => "Arguments",
            ObjectData::Date(_) => "Date",
            ObjectData::RegExp(_) => "RegExp",
            _ => "Object",
        },
        ValueKind::Null => "Null",
        ValueKind::Undefined => "Undefined",
        ValueKind::String(_) => "String",
        ValueKind::Bool(_) => "Boolean",
        ValueKind::Float64(_) | ValueKind::Int32(_) => "Number",
        ValueKind::Symbol(_) => "Symbol",
        ValueKind::BigInt(_) | ValueKind::ShortBigInt(_) => "BigInt",
        _ => "Object",
    };
    let tag = if value.as_object().is_some() {
        let symbol = ctx
            .heap()
            .symbol_registry
            .well_known(WellKnown::ToStringTag);
        let key = ctx.heap().symbols.get(symbol).unwrap().key();
        let custom = super::property::get_property(ctx, this, key)?;
        if matches!(custom.decode(), ValueKind::String(_)) {
            ctx.root(custom)?
        } else {
            ctx.new_string(default)?
        }
    } else {
        ctx.new_string(default)?
    };
    let prefix = ctx.new_string("[object ")?;
    let tail = ctx.new_string("]")?;
    let head = ctx.concat_strings(prefix, tag)?;
    let text = ctx.concat_strings(head, tail)?;
    Ok(ctx.get(text))
}

fn wrapper_value_of<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    let value = ctx.get(this);
    if let Some(id) = value.as_object() {
        match &ctx.heap().object(id).data {
            ObjectData::StringObject(wrapper) => Ok(wrapper.value),
            ObjectData::NumberObject(wrapper) => Ok(wrapper.value),
            ObjectData::BooleanObject(wrapper) => Ok(wrapper.value),
            ObjectData::SymbolObject(wrapper) => Ok(wrapper.value),
            ObjectData::BigIntObject(wrapper) => Ok(wrapper.value),
            _ => Err(ctx.throw_type_error("expected a boxed primitive")),
        }
    } else {
        Ok(value)
    }
}

fn wrapper_to_string<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let raw = wrapper_value_of(ctx, this, args)?;
    let value = ctx.root(raw)?;
    if let ValueKind::Symbol(id) = raw.decode() {
        let desc = ctx.heap().symbols.get(id).unwrap().description;
        let desc = match desc {
            SymbolDescription::None => ctx.new_string("")?,
            SymbolDescription::String(id) => ctx.root(Value::string(id))?,
            SymbolDescription::WellKnown(s) => ctx.new_string(s.description())?,
        };
        let prefix = ctx.new_string("Symbol(")?;
        let tail = ctx.new_string(")")?;
        let head = ctx.concat_strings(prefix, desc)?;
        let text = ctx.concat_strings(head, tail)?;
        return Ok(ctx.get(text));
    }
    let text = to_string(ctx, value)?;
    Ok(ctx.get(text))
}

struct TextBuffer {
    data: [u8; 128],
    len: usize,
}
impl TextBuffer {
    fn new() -> Self {
        Self {
            data: [0; 128],
            len: 0,
        }
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.data[..self.len]).unwrap()
    }
}
impl Write for TextBuffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        self.data
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}
