use std::cmp::Ordering;

use crate::builtins::{INSERTION_SORT_LIMIT, stable_insertion_sort};

use super::*;

pub(super) fn sort<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    copy: bool,
) -> JsResult<Value> {
    let compare = arg(ctx, args, 0)?;
    let comparator = ctx.get(compare);
    if comparator != Value::UNDEFINED && !callable(ctx, comparator) {
        return Err(ctx.throw_type_error("Comparator is not callable"));
    }
    let object = to_object(ctx, this)?;
    let len = get_length(ctx, object)?;
    let receiver = Receiver { object, len };
    let output = if copy {
        array_of_length(ctx, len)?
    } else {
        object
    };
    let out = ctx.get(output);
    let roots = ctx.new_array()?;
    let root_array = ctx.get(roots);
    let root_id = ctx.object_id(roots)?;
    let mut order = TrackedVec::new();
    for i in 0..len {
        let found = receiver.at(ctx, |ctx, object| {
            let value = if copy {
                Some(get_index(ctx, object, i)?)
            } else {
                present(ctx, object, i)?
            };
            let Some(value) = value else {
                return Ok(false);
            };
            let value = ctx.root(value)?;
            let roots = ctx.root(root_array)?;
            create_index(ctx, roots, order.len() as u64, ctx.get(value))?;
            Ok(true)
        })?;
        if found {
            let position = order.len();
            if order.len() == order.capacity() {
                ctx.allocate(|runtime| order.try_reserve(&runtime.heap.ledger, 1))?;
            }
            order.push_reserved(position);
        }
    }
    let n = order.len();
    if n <= INSERTION_SORT_LIMIT {
        stable_insertion_sort(&mut order, |left, right| {
            ctx.check_interrupt()?;
            compare_items(
                ctx,
                comparator,
                item(ctx, root_id, left),
                item(ctx, root_id, right),
            )
        })?;
    } else {
        let mut temp = TrackedVec::new();
        ctx.allocate(|runtime| temp.try_reserve_exact(&runtime.heap.ledger, n))?;
        let mut width = 1;
        while width < n {
            temp.clear();
            for start in (0..n).step_by(width * 2) {
                let middle = (start + width).min(n);
                let end = (middle + width).min(n);
                let (mut left, mut right) = (start, middle);
                while left < middle || right < end {
                    ctx.check_interrupt()?;
                    let choose_left = if left == middle {
                        false
                    } else if right == end {
                        true
                    } else {
                        let x = item(ctx, root_id, order[left]);
                        let y = item(ctx, root_id, order[right]);
                        compare_items(ctx, comparator, x, y)? != Ordering::Greater
                    };
                    if choose_left {
                        temp.push_reserved(order[left]);
                        left += 1;
                    } else {
                        temp.push_reserved(order[right]);
                        right += 1;
                    }
                }
            }
            std::mem::swap(&mut order, &mut temp);
            width *= 2;
        }
    }
    for (i, &position) in order.iter().enumerate() {
        let value = item(ctx, root_id, position);
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let output = ctx.root(out)?;
            if copy {
                create_index(ctx, output, i as u64, value)
            } else {
                set_index(ctx, output, i as u64, value)
            }
        })?;
    }
    if !copy {
        for i in n as u64..len {
            receiver.at(ctx, |ctx, object| delete_index(ctx, object, i))?;
        }
    }
    Ok(out)
}

fn item(ctx: &ExecCtx<'_, '_>, roots: ObjId, index: usize) -> Value {
    let ObjectData::Array(data) = &ctx.heap().object(roots).data else {
        unreachable!()
    };
    data.elements[index]
}

fn compare_items(
    ctx: &mut ExecCtx<'_, '_>,
    compare: Value,
    a: Value,
    b: Value,
) -> JsResult<Ordering> {
    match (a, b) {
        (Value::UNDEFINED, Value::UNDEFINED) => return Ok(Ordering::Equal),
        (Value::UNDEFINED, _) => return Ok(Ordering::Greater),
        (_, Value::UNDEFINED) => return Ok(Ordering::Less),
        _ => {}
    }
    ctx.with_scope(|ctx| {
        let a = ctx.root(a)?;
        let b = ctx.root(b)?;
        if compare == Value::UNDEFINED {
            let a = to_string(ctx, a)?;
            let b = to_string(ctx, b)?;
            let (ValueKind::String(a), ValueKind::String(b)) =
                (ctx.get(a).decode(), ctx.get(b).decode())
            else {
                unreachable!()
            };
            Ok(ctx.heap().string_units(a).cmp(ctx.heap().string_units(b)))
        } else {
            let compare = ctx.root(compare)?;
            let this = ctx.root(Value::UNDEFINED)?;
            let args = ctx.args(&[a, b])?;
            let result = call(ctx, compare, this, args)?;
            let result = ctx.root(result)?;
            Ok(to_number(ctx, result)?
                .partial_cmp(&0.0)
                .unwrap_or(Ordering::Equal))
        }
    })
}
