use crate::ledger::TrackedBox;
use crate::object::{ObjectData, ObjectFlags, PromiseData, Reaction};
use crate::root::{ExecCtx, Handle};
use crate::value::JsResult;
use crate::vm::Job;

pub fn new_promise<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().promise_proto;
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::Promise(TrackedBox::new(
            ledger,
            PromiseData::default(),
        )?))
    })
}

/// Schedule `callback` when `promise` is fulfilled. The callback's result is discarded.
pub fn react_to_fulfillment<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    callback: Handle<'s>,
) -> JsResult<()> {
    let id = ctx.object_id(promise)?;
    let callback_id = ctx.object_id(callback)?;
    if !super::call::is_callable(ctx.heap(), callback_id) {
        return Err(ctx.throw_type_error("reaction is not callable"));
    }
    let ObjectData::Promise(data) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("expected a Promise"));
    };
    let realm = ctx.realm_id();
    if let Some(argument) = data.result {
        return ctx.allocate(|runtime| {
            runtime.reserve_jobs(1)?;
            runtime.job_queue.push_reserved(Job {
                callback: callback_id,
                argument,
                realm,
            });
            Ok(())
        });
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::Promise(promise) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        promise.reactions.push(
            &heap.ledger,
            Reaction {
                callback: callback_id,
                realm,
            },
        )
    })
}

/// Fulfill a pending promise without running its reactions on the current stack.
pub fn fulfill<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<()> {
    crate::builtins::promise_json_extras::fulfill_promise(ctx, promise, value)
}
