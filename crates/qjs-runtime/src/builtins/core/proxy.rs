use crate::atom::Atom;
use crate::builtins::make_native_fn;

use super::*;

const CTOR: u16 = 0xa0;
const REVOCABLE: u16 = 0xa1;
const REVOKE: u16 = 0xa2;

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = make_native_fn(ctx, id(CTOR), "Proxy", 2, true)?;
    methods(ctx, ctor, &[(REVOCABLE, "revocable", 2)])?;
    define_global(ctx, "Proxy", ctor)
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match op {
        CTOR | REVOCABLE => {
            if op == CTOR && ctx.new_target().is_none() {
                return Err(ctx.throw_type_error("Proxy must be called with new"));
            }
            let target = arg(ctx, args, 0)?;
            let handler = arg(ctx, args, 1)?;
            let proxy = property::new_proxy(ctx, target, handler)?;
            if op == CTOR {
                return Ok(ctx.get(proxy));
            }
            let key = Atom::get("proxy");
            let slot = ctx.new_object_with_proto(None)?;
            put(ctx, slot, key, proxy)?;
            // The bound receiver is private and can release the proxy after revocation.
            let callback = ctx.new_native(id(REVOKE), false)?;
            let revoke = ctx.bind_function(callback, slot, Args::empty())?;
            super::super::set_fn_metadata(ctx, revoke, "", 0)?;
            let result = ctx.new_object()?;
            put(ctx, result, key, proxy)?;
            put(ctx, result, Atom::get("revoke"), revoke)?;
            Ok(ctx.get(result))
        }
        REVOKE => {
            let key = Atom::get("proxy");
            let proxy = get(ctx, this, key)?;
            if ctx.get(proxy) != Value::UNDEFINED {
                let undefined = ctx.root(Value::UNDEFINED)?;
                set_or_throw(ctx, this, key, undefined)?;
                property::revoke_proxy(ctx, proxy)?;
            }
            Ok(Value::UNDEFINED)
        }
        _ => Err(ctx.throw_type_error("unknown Proxy builtin")),
    }
}
