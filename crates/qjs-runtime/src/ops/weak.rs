use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::*;
use crate::root::{ExecCtx, Handle};
use crate::value::*;

pub fn new_weak_map<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().weakmap_proto;
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::WeakMap(TrackedBox::new(
            ledger,
            WeakMapData::default(),
        )?))
    })
}

pub fn weak_map_set<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    map: Handle<'s>,
    key: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<()> {
    let map = ctx.object_id(map)?;
    let key = require_weak_key(ctx, key)?;
    let value = ctx.get(value);
    if !matches!(ctx.heap().object(map).data, ObjectData::WeakMap(_)) {
        return Err(ctx.throw_type_error("expected a WeakMap"));
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::WeakMap(map) = &mut heap.objects.get_mut(map).unwrap().data else {
            unreachable!()
        };
        if let Some(entry) = map.entries.iter_mut().find(|entry| entry.0 == key) {
            entry.1 = value;
        } else {
            map.entries.push(&heap.ledger, (key, value))?;
        }
        Ok(())
    })
}

pub fn weak_map_get<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    map: Handle<'s>,
    key: Handle<'s>,
) -> JsResult<Value> {
    let id = ctx.object_id(map)?;
    let ObjectData::WeakMap(map) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a WeakMap"));
    };
    let Some(key) = ctx.heap().weak_key(ctx.get(key)) else {
        return Ok(Value::UNDEFINED);
    };
    Ok(map
        .entries
        .iter()
        .find_map(|&(stored, value)| (stored == key).then_some(value))
        .unwrap_or(Value::UNDEFINED))
}

pub fn new_weak_set<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().weakset_proto;
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::WeakSet(TrackedBox::new(
            ledger,
            WeakSetData::default(),
        )?))
    })
}

pub fn weak_set_add<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    set: Handle<'s>,
    key: Handle<'s>,
) -> JsResult<()> {
    let id = ctx.object_id(set)?;
    let key = require_weak_key(ctx, key)?;
    if !matches!(ctx.heap().object(id).data, ObjectData::WeakSet(_)) {
        return Err(ctx.throw_type_error("expected a WeakSet"));
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::WeakSet(set) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        if !set.entries.contains(&key) {
            set.entries.push(&heap.ledger, key)?;
        }
        Ok(())
    })
}

pub fn weak_set_has<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    set: Handle<'s>,
    key: Handle<'s>,
) -> JsResult<bool> {
    let id = ctx.object_id(set)?;
    let ObjectData::WeakSet(set) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a WeakSet"));
    };
    Ok(ctx
        .heap()
        .weak_key(ctx.get(key))
        .is_some_and(|key| set.entries.contains(&key)))
}

pub fn new_weak_ref<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>) -> JsResult<Handle<'s>> {
    let key = require_weak_key(ctx, target)?;
    let proto = ctx.realm().weakref_proto;
    let reference = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::WeakRef(TrackedBox::new(
            ledger,
            WeakRefData { target: Some(key) },
        )?))
    })?;
    keep_during_job(ctx, key)?;
    Ok(reference)
}

pub fn weak_ref_deref<'s>(ctx: &mut ExecCtx<'_, 's>, reference: Handle<'s>) -> JsResult<Value> {
    let id = ctx.object_id(reference)?;
    let ObjectData::WeakRef(reference) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a WeakRef"));
    };
    let Some(key) = reference.target else {
        return Ok(Value::UNDEFINED);
    };
    ctx.with_scope(|ctx| {
        ctx.root(key.value())?;
        keep_during_job(ctx, key)?;
        Ok(key.value())
    })
}

fn keep_during_job(ctx: &mut ExecCtx<'_, '_>, key: WeakKey) -> JsResult<()> {
    if ctx.heap().kept_objects.contains(&key) {
        return Ok(());
    }
    ctx.allocate(|runtime| runtime.heap.kept_objects.push(&runtime.heap.ledger, key))
}

fn require_weak_key<'s>(ctx: &mut ExecCtx<'_, 's>, key: Handle<'s>) -> JsResult<WeakKey> {
    ctx.heap()
        .weak_key(ctx.get(key))
        .ok_or_else(|| ctx.throw_type_error("weak key must be an object or an unregistered symbol"))
}

pub fn new_finalization_registry<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callback: Handle<'s>,
) -> JsResult<Handle<'s>> {
    let cleanup = ctx.object_id(callback)?;
    if !super::call::is_callable(ctx.heap(), cleanup) {
        return Err(ctx.throw_type_error("cleanup callback is not callable"));
    }
    let proto = ctx.realm().finalization_registry_proto;
    let realm = ctx.realm_id();
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::FinalizationRegistry(TrackedBox::new(
            ledger,
            FinalizationRegistryData {
                cleanup,
                realm,
                entries: TrackedVec::new(),
            },
        )?))
    })
}

pub fn finalization_register<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    registry: Handle<'s>,
    target: Handle<'s>,
    held: Handle<'s>,
    token: Option<Handle<'s>>,
) -> JsResult<()> {
    let id = ctx.object_id(registry)?;
    let target = require_weak_key(ctx, target)?;
    let held = ctx.get(held);
    if target.value() == held {
        return Err(ctx.throw_type_error("target and holdings must differ"));
    }
    let token = token.map(|h| require_weak_key(ctx, h)).transpose()?;
    if !matches!(
        ctx.heap().object(id).data,
        ObjectData::FinalizationRegistry(_)
    ) {
        return Err(ctx.throw_type_error("expected a FinalizationRegistry"));
    }
    ctx.allocate(|runtime| {
        runtime.reserve_jobs(1)?;
        let heap = &mut runtime.heap;
        let ObjectData::FinalizationRegistry(registry) =
            &mut heap.objects.get_mut(id).unwrap().data
        else {
            unreachable!()
        };
        registry.entries.push(
            &heap.ledger,
            Registration {
                target,
                held,
                token,
            },
        )
    })
}

pub fn finalization_unregister<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    registry: Handle<'s>,
    token: Handle<'s>,
) -> JsResult<bool> {
    let id = ctx.object_id(registry)?;
    let token = require_weak_key(ctx, token)?;
    let Some(object) = ctx.runtime.heap.objects.get_mut(id) else {
        unreachable!()
    };
    let ObjectData::FinalizationRegistry(registry) = &mut object.data else {
        return Err(ctx.throw_type_error("expected a FinalizationRegistry"));
    };
    let before = registry.entries.len();
    registry.entries.retain(|entry| entry.token != Some(token));
    Ok(before != registry.entries.len())
}
