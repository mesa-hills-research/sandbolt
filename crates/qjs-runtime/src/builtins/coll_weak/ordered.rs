use super::iterable::{IteratorRecord, array_push, iter_result, nullish, populate};
use super::*;
use crate::atom::Atom;
use crate::builtins::install_accessor;
use crate::ledger::{AllocResult, HeapLedger, TrackedBox};
use crate::object::{
    CollectionCursor, CollectionIteration, CollectionIteratorData, MapData, ObjectData,
    ObjectFlags, OrderedCollection, SetData, canonical_key,
};

pub(super) fn install_map(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let (ctor, proto) = class(
        ctx,
        ctx.realm().map_ctor,
        ctx.realm().map_proto,
        MAP_CTOR,
        "Map",
        0,
    )?;
    install_species(ctx, ctor)?;
    install_methods(ctx, ctor, &[("groupBy", MAP_GROUP_BY, 2)])?;
    install_methods(
        ctx,
        proto,
        &[
            ("set", MAP_SET, 2),
            ("get", MAP_GET, 1),
            ("has", MAP_HAS, 1),
            ("delete", MAP_DELETE, 1),
            ("clear", MAP_CLEAR, 0),
            ("forEach", MAP_FOR_EACH, 1),
            ("keys", MAP_KEYS, 0),
            ("values", MAP_VALUES, 0),
            ("entries", MAP_ENTRIES, 0),
            ("getOrInsert", MAP_UPSERT, 2),
            ("getOrInsertComputed", MAP_UPSERT_COMPUTED, 2),
        ],
    )?;
    install_accessor(
        ctx,
        proto,
        Atom::get("size"),
        Some((MAP_SIZE, "get size")),
        None,
        true,
    )?;
    alias(ctx, proto, wk(ctx, WellKnown::Iterator), "entries")?;
    install_iterator(ctx, false)
}

pub(super) fn install_set(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let (ctor, proto) = class(
        ctx,
        ctx.realm().set_ctor,
        ctx.realm().set_proto,
        SET_CTOR,
        "Set",
        0,
    )?;
    install_species(ctx, ctor)?;
    install_methods(
        ctx,
        proto,
        &[
            ("add", SET_ADD, 1),
            ("has", SET_HAS, 1),
            ("delete", SET_DELETE, 1),
            ("clear", SET_CLEAR, 0),
            ("forEach", SET_FOR_EACH, 1),
            ("values", SET_VALUES, 0),
            ("entries", SET_ENTRIES, 0),
            ("union", UNION, 1),
            ("intersection", INTERSECTION, 1),
            ("difference", DIFFERENCE, 1),
            ("symmetricDifference", SYMMETRIC_DIFFERENCE, 1),
            ("isSubsetOf", IS_SUBSET, 1),
            ("isSupersetOf", IS_SUPERSET, 1),
            ("isDisjointFrom", IS_DISJOINT, 1),
        ],
    )?;
    install_accessor(
        ctx,
        proto,
        Atom::get("size"),
        Some((SET_SIZE, "get size")),
        None,
        true,
    )?;
    alias(ctx, proto, Atom::get("keys"), "values")?;
    alias(ctx, proto, wk(ctx, WellKnown::Iterator), "values")?;
    install_iterator(ctx, true)
}

fn install_species<'s>(ctx: &mut ExecCtx<'_, 's>, ctor: Handle<'s>) -> JsResult<()> {
    install_accessor(
        ctx,
        ctor,
        wk(ctx, WellKnown::Species),
        Some((SPECIES, "get [Symbol.species]")),
        None,
        true,
    )
}

fn install_iterator(ctx: &mut ExecCtx<'_, '_>, is_set: bool) -> JsResult<()> {
    let (proto, id, name) = if is_set {
        (ctx.realm().set_iterator_proto, SET_NEXT, "Set Iterator")
    } else {
        (ctx.realm().map_iterator_proto, MAP_NEXT, "Map Iterator")
    };
    let proto = ctx.root(Value::object(proto))?;
    install_method(ctx, proto, Atom::get("next"), id, "next", 0)?;
    tag(ctx, proto, name)
}

pub(super) fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
    is_set: bool,
) -> JsResult<Value> {
    let new_target = ctx
        .new_target()
        .ok_or_else(|| ctx.throw_type_error("constructor requires new"))?;
    let object = ctx.ordinary_create_from_constructor(
        new_target,
        |realm| {
            if is_set {
                realm.set_proto
            } else {
                realm.map_proto
            }
        },
        ObjectFlags::EXTENSIBLE,
        |heap| payload(&heap.ledger, is_set),
    )?;
    let items = arg(ctx, args, 0)?;
    populate(ctx, object, items, is_set)?;
    Ok(ctx.get(object))
}

pub(super) fn new_collection<'s>(ctx: &mut ExecCtx<'_, 's>, is_set: bool) -> JsResult<Handle<'s>> {
    let proto = if is_set {
        ctx.realm().set_proto
    } else {
        ctx.realm().map_proto
    };
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        payload(ledger, is_set)
    })
}

fn payload(ledger: &HeapLedger, is_set: bool) -> AllocResult<ObjectData> {
    if is_set {
        Ok(ObjectData::Set(TrackedBox::new(
            ledger,
            SetData::default(),
        )?))
    } else {
        Ok(ObjectData::Map(TrackedBox::new(
            ledger,
            MapData::default(),
        )?))
    }
}

pub(super) fn require_collection<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    is_set: bool,
) -> JsResult<ObjId> {
    let id = ctx.object_id(this)?;
    let data = &ctx.heap().object(id).data;
    if (is_set && matches!(data, ObjectData::Set(_)))
        || (!is_set && matches!(data, ObjectData::Map(_)))
    {
        Ok(id)
    } else {
        Err(ctx.throw_type_error(if is_set {
            "expected a Set"
        } else {
            "expected a Map"
        }))
    }
}

pub(super) fn entries<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> &'a OrderedCollection {
    match &ctx.heap().object(id).data {
        ObjectData::Map(data) => &data.entries,
        ObjectData::Set(data) => &data.entries,
        _ => unreachable!(),
    }
}

pub(super) fn lookup(ctx: &ExecCtx<'_, '_>, id: ObjId, key: Value) -> Option<Value> {
    let data = entries(ctx, id);
    data.locate(ctx.heap(), key, data.hash(ctx.heap(), key))
        .map(|row| data.value(row))
}

pub(super) fn insert<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: ObjId,
    key: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<()> {
    let key = ctx.get(key);
    let value = ctx.get(value);
    let data = entries(ctx, id);
    let hash = data.hash(ctx.heap(), key);
    if let Some(row) = data.locate(ctx.heap(), key, hash) {
        collection_mut(&mut ctx.runtime.heap.objects.get_mut(id).unwrap().data).update(row, value);
        return Ok(());
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let data = &mut heap.objects.get_mut(id).unwrap().data;
        collection_mut(data).insert(&heap.ledger, hash, key, value)
    })
}

pub(super) fn delete(ctx: &mut ExecCtx<'_, '_>, id: ObjId, key: Value) -> bool {
    let data = entries(ctx, id);
    let hash = data.hash(ctx.heap(), key);
    let Some(row) = data.locate(ctx.heap(), key, hash) else {
        return false;
    };
    collection_mut(&mut ctx.runtime.heap.objects.get_mut(id).unwrap().data).remove(hash, row);
    true
}

fn collection_mut(data: &mut ObjectData) -> &mut OrderedCollection {
    match data {
        ObjectData::Map(data) => &mut data.entries,
        ObjectData::Set(data) => &mut data.entries,
        _ => unreachable!(),
    }
}

pub(super) fn method<'s>(
    op: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    is_set: bool,
) -> JsResult<Value> {
    let id = require_collection(ctx, this, is_set)?;
    match op {
        MAP_SIZE | SET_SIZE => Ok(Value::number(entries(ctx, id).len() as f64)),
        MAP_CLEAR | SET_CLEAR => {
            collection_mut(&mut ctx.runtime.heap.objects.get_mut(id).unwrap().data).clear();
            Ok(Value::UNDEFINED)
        }
        MAP_KEYS | MAP_VALUES | MAP_ENTRIES | SET_VALUES | SET_ENTRIES => {
            let kind = match op {
                MAP_KEYS | SET_VALUES => CollectionIteration::Keys,
                MAP_VALUES => CollectionIteration::Values,
                _ => CollectionIteration::Entries,
            };
            iterator(ctx, id, is_set, kind)
        }
        _ => {
            let key = arg(ctx, args, 0)?;
            let raw = ctx.get(key);
            match op {
                MAP_GET => Ok(lookup(ctx, id, raw).unwrap_or(Value::UNDEFINED)),
                MAP_HAS | SET_HAS => Ok(Value::bool(lookup(ctx, id, raw).is_some())),
                MAP_DELETE | SET_DELETE => Ok(Value::bool(delete(ctx, id, raw))),
                MAP_SET | SET_ADD => {
                    let value = if is_set {
                        ctx.root(Value::UNDEFINED)?
                    } else {
                        arg(ctx, args, 1)?
                    };
                    insert(ctx, id, key, value)?;
                    Ok(ctx.get(this))
                }
                MAP_UPSERT | MAP_UPSERT_COMPUTED => {
                    let value = arg(ctx, args, 1)?;
                    let computed = op == MAP_UPSERT_COMPUTED;
                    if computed {
                        callable(ctx, value)?;
                    }
                    if let Some(value) = lookup(ctx, id, raw) {
                        return Ok(value);
                    }
                    let value = if computed {
                        let input = ctx.root(canonical_key(raw))?;
                        let this = ctx.root(Value::UNDEFINED)?;
                        let args = ctx.args(&[input])?;
                        let value = call::call(ctx, value, this, args)?;
                        ctx.root(value)?
                    } else {
                        value
                    };
                    insert(ctx, id, key, value)?;
                    Ok(ctx.get(value))
                }
                _ => unreachable!(),
            }
        }
    }
}

fn iterator(
    ctx: &mut ExecCtx<'_, '_>,
    target: ObjId,
    is_set: bool,
    kind: CollectionIteration,
) -> JsResult<Value> {
    let proto = if is_set {
        ctx.realm().set_iterator_proto
    } else {
        ctx.realm().map_iterator_proto
    };
    let object = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::CollectionIterator(TrackedBox::new(
            ledger,
            CollectionIteratorData {
                target: Some(target),
                is_set,
                kind,
                cursor: CollectionCursor::default(),
            },
        )?))
    })?;
    Ok(ctx.get(object))
}

pub(super) fn next<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    is_set: bool,
) -> JsResult<Value> {
    let id = ctx.object_id(this)?;
    let ObjectData::CollectionIterator(iter) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a collection iterator"));
    };
    if iter.is_set != is_set {
        return Err(ctx.throw_type_error("expected a matching collection iterator"));
    }
    let mut cursor = iter.cursor;
    let kind = iter.kind;
    let pair = iter
        .target
        .and_then(|target| entries(ctx, target).next(&mut cursor));
    let ObjectData::CollectionIterator(iter) =
        &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
    else {
        unreachable!()
    };
    iter.cursor = cursor;
    let Some((key, value)) = pair else {
        iter.target = None;
        return iter_result(ctx, Value::UNDEFINED, true);
    };
    let value = if is_set { key } else { value };
    let result = match kind {
        CollectionIteration::Keys => key,
        CollectionIteration::Values => value,
        CollectionIteration::Entries => {
            let key = ctx.root(key)?;
            let value = ctx.root(value)?;
            let pair = ctx.new_array()?;
            array_push(ctx, pair, 0, key)?;
            array_push(ctx, pair, 1, value)?;
            ctx.get(pair)
        }
    };
    iter_result(ctx, result, false)
}

pub(super) fn for_each<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    is_set: bool,
) -> JsResult<Value> {
    let id = require_collection(ctx, this, is_set)?;
    let callback = arg(ctx, args, 0)?;
    callable(ctx, callback)?;
    let this_arg = arg(ctx, args, 1)?;
    let (callback, this_arg) = (ctx.get(callback), ctx.get(this_arg));
    let mut cursor = CollectionCursor::default();
    while let Some((key, value)) = entries(ctx, id).next(&mut cursor) {
        ctx.with_scope(|ctx| {
            let collection = ctx.root(Value::object(id))?;
            let key = ctx.root(key)?;
            let value = if is_set { key } else { ctx.root(value)? };
            let callback = ctx.root(callback)?;
            let this_arg = ctx.root(this_arg)?;
            let args = ctx.args(&[value, key, collection])?;
            call::call(ctx, callback, this_arg, args).map(|_| ())
        })?;
    }
    Ok(Value::UNDEFINED)
}

pub(super) fn group_by<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let items = arg(ctx, args, 0)?;
    if nullish(ctx.get(items)) {
        return Err(ctx.throw_type_error("expected iterable items"));
    }
    let callback = arg(ctx, args, 1)?;
    callable(ctx, callback)?;
    let iter = IteratorRecord::from_iterable(ctx, items)?;
    let callback = ctx.get(callback);
    let groups = new_collection(ctx, false)?;
    let id = ctx.object_id(groups)?;
    let mut index = 0u64;
    while iter.with_scope(ctx, |ctx, iter| {
        if index == (1u64 << 53) - 1 {
            let error = ctx.throw_type_error("too many items to group");
            return iter.close_on_throw(ctx, Err(error));
        }
        let Some(value) = iter.step(ctx)? else {
            return Ok(false);
        };
        let key = (|| {
            let callback = ctx.root(callback)?;
            let this = ctx.root(Value::UNDEFINED)?;
            let index = ctx.root(Value::number(index as f64))?;
            let args = ctx.args(&[value, index])?;
            call::call(ctx, callback, this, args)
        })();
        let key = iter.close_on_throw(ctx, key)?;
        let key = ctx.root(key)?;
        let group = if let Some(group) = lookup(ctx, id, ctx.get(key)) {
            ctx.root(group)?
        } else {
            let group = ctx.new_array()?;
            insert(ctx, id, key, group)?;
            group
        };
        let group_id = ctx.object_id(group)?;
        let ObjectData::Array(array) = &ctx.heap().object(group_id).data else {
            unreachable!()
        };
        let length = array.length;
        if length == u32::MAX {
            return Err(ctx.throw_range_error("group exceeds the array length limit"));
        }
        array_push(ctx, group, length, value)?;
        index += 1;
        Ok(true)
    })? {}
    Ok(ctx.get(groups))
}
