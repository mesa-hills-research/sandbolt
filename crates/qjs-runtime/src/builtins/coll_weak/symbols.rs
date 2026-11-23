use super::*;
use crate::builtins::{install_accessor, make_native_fn};
use crate::object::ObjectData;
use crate::ops::convert;
use crate::symbol::SymbolDescription;
use crate::value::{SymId, ValueKind};

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor_id = ctx.realm().symbol_ctor;
    let (ctor, proto) = class(ctx, ctor_id, ctx.realm().symbol_proto, SYMBOL, "Symbol", 0)?;
    install_methods(
        ctx,
        ctor,
        &[("for", SYMBOL_FOR, 1), ("keyFor", SYMBOL_KEY_FOR, 1)],
    )?;
    let ctor = ctx.get(ctor);
    for symbol in WellKnown::ALL {
        ctx.with_scope(|ctx| {
            let ctor = ctx.root(ctor)?;
            let value = ctx.root(Value::symbol(ctx.heap().symbol_registry.well_known(symbol)))?;
            let name = symbol.description().strip_prefix("Symbol.").unwrap();
            let key = ctx.intern_str(name)?;
            define_data(ctx, ctor, key, value, PropFlags::default())
        })?;
    }
    install_methods(
        ctx,
        proto,
        &[
            ("toString", SYMBOL_TO_STRING, 0),
            ("valueOf", SYMBOL_VALUE_OF, 0),
        ],
    )?;
    let description = ctx.intern_str("description")?;
    install_accessor(
        ctx,
        proto,
        description,
        Some((SYMBOL_DESCRIPTION, "get description")),
        None,
        true,
    )?;
    let to_primitive = make_native_fn(ctx, SYMBOL_TO_PRIMITIVE, "[Symbol.toPrimitive]", 1, false)?;
    let key = wk(ctx, WellKnown::ToPrimitive);
    define_data(ctx, proto, key, to_primitive, PropFlags::CONFIGURABLE)
}

fn symbol_value<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<SymId> {
    let this = ctx.get(this);
    let value = match this.as_object().map(|id| &ctx.heap().object(id).data) {
        Some(ObjectData::SymbolObject(symbol)) => symbol.value,
        _ => this,
    };
    if let ValueKind::Symbol(id) = value.decode() {
        Ok(id)
    } else {
        Err(ctx.throw_type_error("expected a Symbol"))
    }
}

pub(super) fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        SYMBOL | SYMBOL_FOR => {
            if id == SYMBOL && ctx.new_target().is_some() {
                return Err(ctx.throw_type_error("Symbol is not a constructor"));
            }
            let input = arg(ctx, args, 0)?;
            let symbol = if id == SYMBOL_FOR {
                let key = convert::to_string(ctx, input)?;
                ctx.symbol_for(key)?
            } else {
                let description = if ctx.get(input) == Value::UNDEFINED {
                    None
                } else {
                    Some(convert::to_string(ctx, input)?)
                };
                ctx.new_symbol(description, false)?
            };
            Ok(ctx.get(symbol))
        }
        SYMBOL_KEY_FOR => {
            let input = arg(ctx, args, 0)?;
            let ValueKind::Symbol(id) = ctx.get(input).decode() else {
                return Err(ctx.throw_type_error("expected a Symbol"));
            };
            let symbol = ctx.heap().symbols.get(id).unwrap();
            match symbol.description {
                SymbolDescription::String(key) if symbol.is_registered => Ok(Value::string(key)),
                _ => Ok(Value::UNDEFINED),
            }
        }
        SYMBOL_VALUE_OF | SYMBOL_TO_PRIMITIVE => symbol_value(ctx, this).map(Value::symbol),
        SYMBOL_DESCRIPTION | SYMBOL_TO_STRING => {
            let symbol = symbol_value(ctx, this)?;
            let desc = ctx.heap().symbols.get(symbol).unwrap().description;
            if id == SYMBOL_DESCRIPTION && desc == SymbolDescription::None {
                return Ok(Value::UNDEFINED);
            }
            let desc = match desc {
                SymbolDescription::None => ctx.new_string("")?,
                SymbolDescription::String(s) => ctx.root(Value::string(s))?,
                SymbolDescription::WellKnown(s) => ctx.new_string(s.description())?,
            };
            if id == SYMBOL_DESCRIPTION {
                Ok(ctx.get(desc))
            } else {
                let prefix = ctx.new_string("Symbol(")?;
                let tail = ctx.new_string(")")?;
                let head = ctx.concat_strings(prefix, desc)?;
                let result = ctx.concat_strings(head, tail)?;
                Ok(ctx.get(result))
            }
        }
        _ => unreachable!(),
    }
}
