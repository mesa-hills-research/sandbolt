use crate::ledger::{AllocResult, HeapLedger, TrackedBox, TrackedVec};
use crate::object::*;
use crate::realm::Realm;
use crate::root::{ExecCtx, Handle};
use crate::value::*;

pub fn new_weak_map<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
    weak_map_with_target(ctx, None)
}

pub(crate) fn weak_map_with_target<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    new_target: Option<ObjId>,
) -> JsResult<Handle<'s>> {
    new_weak_object(
        ctx,
        new_target,
        |realm| realm.weakmap_proto,
        |ledger| {
            Ok(ObjectData::WeakMap(TrackedBox::new(
                ledger,
                WeakMapData::default(),
            )?))
        },
    )
}

pub fn weak_map_set<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    map: Handle<'s>,
    key: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<()> {
    let map = ctx.object_id(map)?;
    fast_weak_map_set(ctx, map, ctx.get(key), ctx.get(value))
}

// The caller keeps the receiver, key and value rooted across a growing insert.
pub(crate) fn fast_weak_map_set(
    ctx: &mut ExecCtx<'_, '_>,
    map: ObjId,
    key: Value,
    value: Value,
) -> JsResult<()> {
    if !matches!(ctx.heap().object(map).data, ObjectData::WeakMap(_)) {
        return Err(ctx.throw_type_error("expected a WeakMap"));
    }
    let key = weak_key(ctx, key)?;
    let ObjectData::WeakMap(data) = &mut ctx.runtime.heap.objects.get_mut(map).unwrap().data else {
        unreachable!()
    };
    if data.entries.try_insert_reserved((key, value)).is_ok() {
        return Ok(());
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::WeakMap(map) = &mut heap.objects.get_mut(map).unwrap().data else {
            unreachable!()
        };
        map.entries.insert(&heap.ledger, (key, value))
    })
}

pub fn weak_map_get<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    map: Handle<'s>,
    key: Handle<'s>,
) -> JsResult<Value> {
    let map = ctx.object_id(map)?;
    fast_weak_map_get(ctx, map, ctx.get(key))
}

pub(crate) fn fast_weak_map_get(
    ctx: &mut ExecCtx<'_, '_>,
    map: ObjId,
    key: Value,
) -> JsResult<Value> {
    weak_map_lookup_raw(ctx, map, key).map(|value| value.unwrap_or(Value::UNDEFINED))
}

// Keep this available for dispatch that already has a receiver and argument.
#[allow(dead_code)]
pub(crate) fn fast_weak_map_has(
    ctx: &mut ExecCtx<'_, '_>,
    map: ObjId,
    key: Value,
) -> JsResult<bool> {
    weak_map_lookup_raw(ctx, map, key).map(|value| value.is_some())
}

pub(crate) fn weak_map_lookup<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    map: Handle<'s>,
    key: Handle<'s>,
) -> JsResult<Option<Value>> {
    let id = ctx.object_id(map)?;
    weak_map_lookup_raw(ctx, id, ctx.get(key))
}

fn weak_map_lookup_raw(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    key: Value,
) -> JsResult<Option<Value>> {
    let ObjectData::WeakMap(map) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a WeakMap"));
    };
    Ok(ctx
        .heap()
        .weak_key(key)
        .and_then(|key| map.entries.get(key))
        .map(|entry| entry.1))
}

pub fn new_weak_set<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
    weak_set_with_target(ctx, None)
}

pub(crate) fn weak_set_with_target<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    new_target: Option<ObjId>,
) -> JsResult<Handle<'s>> {
    new_weak_object(
        ctx,
        new_target,
        |realm| realm.weakset_proto,
        |ledger| {
            Ok(ObjectData::WeakSet(TrackedBox::new(
                ledger,
                WeakSetData::default(),
            )?))
        },
    )
}

pub fn weak_set_add<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    set: Handle<'s>,
    key: Handle<'s>,
) -> JsResult<()> {
    let id = ctx.object_id(set)?;
    if !matches!(ctx.heap().object(id).data, ObjectData::WeakSet(_)) {
        return Err(ctx.throw_type_error("expected a WeakSet"));
    }
    let key = require_weak_key(ctx, key)?;
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::WeakSet(set) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        set.entries.insert(&heap.ledger, key)
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
        .is_some_and(|key| set.entries.get(key).is_some()))
}

pub(crate) fn weak_delete<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    collection: Handle<'s>,
    key: Handle<'s>,
    is_set: bool,
) -> JsResult<bool> {
    let id = ctx.object_id(collection)?;
    if !is_set {
        return fast_weak_map_delete(ctx, id, ctx.get(key));
    }
    let key = ctx.heap().weak_key(ctx.get(key));
    let ObjectData::WeakSet(set) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        return Err(ctx.throw_type_error("expected a WeakSet"));
    };
    Ok(key.is_some_and(|key| set.entries.remove(key)))
}

pub(crate) fn fast_weak_map_delete(
    ctx: &mut ExecCtx<'_, '_>,
    map: ObjId,
    key: Value,
) -> JsResult<bool> {
    let key = ctx.heap().weak_key(key);
    let ObjectData::WeakMap(map) = &mut ctx.runtime.heap.objects.get_mut(map).unwrap().data else {
        return Err(ctx.throw_type_error("expected a WeakMap"));
    };
    Ok(key.is_some_and(|key| map.entries.remove(key)))
}

pub fn new_weak_ref<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>) -> JsResult<Handle<'s>> {
    weak_ref_with_target(ctx, None, target)
}

pub(crate) fn weak_ref_with_target<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    new_target: Option<ObjId>,
    target: Handle<'s>,
) -> JsResult<Handle<'s>> {
    let key = require_weak_key(ctx, target)?;
    let reference = new_weak_object(
        ctx,
        new_target,
        |realm| realm.weakref_proto,
        |ledger| {
            Ok(ObjectData::WeakRef(TrackedBox::new(
                ledger,
                WeakRefData { target: Some(key) },
            )?))
        },
    )?;
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

pub(crate) fn require_weak_key<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    key: Handle<'s>,
) -> JsResult<WeakKey> {
    weak_key(ctx, ctx.get(key))
}

fn weak_key(ctx: &mut ExecCtx<'_, '_>, key: Value) -> JsResult<WeakKey> {
    ctx.heap()
        .weak_key(key)
        .ok_or_else(|| ctx.throw_type_error("weak key must be an object or an unregistered symbol"))
}

pub fn new_finalization_registry<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    callback: Handle<'s>,
) -> JsResult<Handle<'s>> {
    finalization_registry_with_target(ctx, None, callback)
}

pub(crate) fn finalization_registry_with_target<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    new_target: Option<ObjId>,
    callback: Handle<'s>,
) -> JsResult<Handle<'s>> {
    let cleanup = ctx.object_id(callback)?;
    if !super::call::is_callable(ctx.heap(), cleanup) {
        return Err(ctx.throw_type_error("cleanup callback is not callable"));
    }
    let realm = ctx.realm_id();
    new_weak_object(
        ctx,
        new_target,
        |realm| realm.finalization_registry_proto,
        |ledger| {
            Ok(ObjectData::FinalizationRegistry(TrackedBox::new(
                ledger,
                FinalizationRegistryData {
                    cleanup,
                    realm,
                    entries: TrackedVec::new(),
                },
            )?))
        },
    )
}

fn new_weak_object<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    new_target: Option<ObjId>,
    proto: fn(&Realm) -> ObjId,
    payload: impl Fn(&HeapLedger) -> AllocResult<ObjectData>,
) -> JsResult<Handle<'s>> {
    match new_target {
        Some(new_target) => ctx.ordinary_create_from_constructor(
            new_target,
            proto,
            ObjectFlags::EXTENSIBLE,
            |heap| payload(&heap.ledger),
        ),
        None => ctx.make_object(Some(proto(ctx.realm())), ObjectFlags::EXTENSIBLE, payload),
    }
}

pub fn finalization_register<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    registry: Handle<'s>,
    target: Handle<'s>,
    held: Handle<'s>,
    token: Option<Handle<'s>>,
) -> JsResult<()> {
    let id = require_registry(ctx, registry)?;
    let target = require_weak_key(ctx, target)?;
    let held = ctx.get(held);
    if target.value() == held {
        return Err(ctx.throw_type_error("target and holdings must differ"));
    }
    let token = token
        .filter(|h| ctx.get(*h) != Value::UNDEFINED)
        .map(|h| require_weak_key(ctx, h))
        .transpose()?;
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
    let id = require_registry(ctx, registry)?;
    let token = require_weak_key(ctx, token)?;
    let object = ctx.runtime.heap.objects.get_mut(id).unwrap();
    let ObjectData::FinalizationRegistry(registry) = &mut object.data else {
        unreachable!()
    };
    let before = registry.entries.len();
    registry.entries.retain(|entry| entry.token != Some(token));
    Ok(before != registry.entries.len())
}

fn require_registry<'s>(ctx: &mut ExecCtx<'_, 's>, registry: Handle<'s>) -> JsResult<ObjId> {
    let id = ctx.object_id(registry)?;
    if matches!(
        ctx.heap().object(id).data,
        ObjectData::FinalizationRegistry(_)
    ) {
        Ok(id)
    } else {
        Err(ctx.throw_type_error("expected a FinalizationRegistry"))
    }
}
