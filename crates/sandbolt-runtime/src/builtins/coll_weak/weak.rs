use super::iterable::populate;
use super::*;
use crate::ops::weak as ops;

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    ctx.with_scope(|ctx| {
        let (_, proto) = class(
            ctx,
            ctx.realm().weakmap_ctor,
            ctx.realm().weakmap_proto,
            WEAK_MAP_CTOR,
            "WeakMap",
            0,
        )?;
        install_methods(
            ctx,
            proto,
            &[
                ("set", WEAK_MAP_SET, 2),
                ("get", WEAK_MAP_GET, 1),
                ("has", WEAK_MAP_HAS, 1),
                ("delete", WEAK_MAP_DELETE, 1),
                ("getOrInsert", WEAK_MAP_UPSERT, 2),
                ("getOrInsertComputed", WEAK_MAP_UPSERT_COMPUTED, 2),
            ],
        )
    })?;
    ctx.with_scope(|ctx| {
        let (_, proto) = class(
            ctx,
            ctx.realm().weakset_ctor,
            ctx.realm().weakset_proto,
            WEAK_SET_CTOR,
            "WeakSet",
            0,
        )?;
        install_methods(
            ctx,
            proto,
            &[
                ("add", WEAK_SET_ADD, 1),
                ("has", WEAK_SET_HAS, 1),
                ("delete", WEAK_SET_DELETE, 1),
            ],
        )
    })?;
    ctx.with_scope(|ctx| {
        let (_, proto) = class(
            ctx,
            ctx.realm().weakref_ctor,
            ctx.realm().weakref_proto,
            WEAK_REF_CTOR,
            "WeakRef",
            1,
        )?;
        install_methods(ctx, proto, &[("deref", WEAK_REF_DEREF, 0)])
    })?;
    ctx.with_scope(|ctx| {
        let (_, proto) = class(
            ctx,
            ctx.realm().finalization_registry_ctor,
            ctx.realm().finalization_registry_proto,
            REGISTRY_CTOR,
            "FinalizationRegistry",
            1,
        )?;
        install_methods(
            ctx,
            proto,
            &[("register", REGISTER, 2), ("unregister", UNREGISTER, 1)],
        )
    })
}

pub(super) fn construct<'s>(
    op: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
) -> JsResult<Value> {
    let new_target = Some(
        ctx.new_target()
            .ok_or_else(|| ctx.throw_type_error("constructor requires new"))?,
    );
    let input = arg(ctx, args, 0)?;
    let object = match op {
        WEAK_MAP_CTOR => {
            let map = ops::weak_map_with_target(ctx, new_target)?;
            populate(ctx, map, input, false)?;
            map
        }
        WEAK_SET_CTOR => {
            let set = ops::weak_set_with_target(ctx, new_target)?;
            populate(ctx, set, input, true)?;
            set
        }
        WEAK_REF_CTOR => ops::weak_ref_with_target(ctx, new_target, input)?,
        REGISTRY_CTOR => ops::finalization_registry_with_target(ctx, new_target, input)?,
        _ => unreachable!(),
    };
    Ok(ctx.get(object))
}

pub(super) fn method<'s>(
    op: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    if op == WEAK_REF_DEREF {
        return ops::weak_ref_deref(ctx, this);
    }
    let key = arg(ctx, args, 0)?;
    match op {
        WEAK_MAP_SET => {
            let value = arg(ctx, args, 1)?;
            ops::weak_map_set(ctx, this, key, value)?;
            Ok(ctx.get(this))
        }
        WEAK_MAP_GET => ops::weak_map_get(ctx, this, key),
        WEAK_MAP_HAS => {
            ops::weak_map_lookup(ctx, this, key).map(|value| Value::bool(value.is_some()))
        }
        WEAK_MAP_DELETE | WEAK_SET_DELETE => {
            ops::weak_delete(ctx, this, key, op == WEAK_SET_DELETE).map(Value::bool)
        }
        WEAK_MAP_UPSERT | WEAK_MAP_UPSERT_COMPUTED => {
            let existing = ops::weak_map_lookup(ctx, this, key)?;
            let input = arg(ctx, args, 1)?;
            let computed = op == WEAK_MAP_UPSERT_COMPUTED;
            if computed {
                callable(ctx, input)?;
            }
            ops::require_weak_key(ctx, key)?;
            if let Some(existing) = existing {
                return Ok(existing);
            }
            let value = if computed {
                let this_arg = ctx.root(Value::UNDEFINED)?;
                let args = ctx.args(&[key])?;
                let value = call::call(ctx, input, this_arg, args)?;
                ctx.root(value)?
            } else {
                input
            };
            ops::weak_map_set(ctx, this, key, value)?;
            Ok(ctx.get(value))
        }
        WEAK_SET_ADD => {
            ops::weak_set_add(ctx, this, key)?;
            Ok(ctx.get(this))
        }
        WEAK_SET_HAS => ops::weak_set_has(ctx, this, key).map(Value::bool),
        REGISTER => {
            let held = arg(ctx, args, 1)?;
            ops::finalization_register(ctx, this, key, held, args.get(2))?;
            Ok(Value::UNDEFINED)
        }
        UNREGISTER => ops::finalization_unregister(ctx, this, key).map(Value::bool),
        _ => unreachable!(),
    }
}
