use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::value::{JsResult, Value};

pub fn install(_ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    Ok(())
}

pub fn dispatch<'s>(
    _id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    _this: Handle<'s>,
    _args: Args<'s>,
) -> JsResult<Value> {
    Err(ctx.throw_type_error("core builtin not yet installed"))
}
