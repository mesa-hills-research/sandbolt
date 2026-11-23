use std::cmp::Ordering;

use super::convert::{self, PreferredType};
use crate::heap::{BigIntData, Heap};
use crate::root::{ExecCtx, Handle};
use crate::value::*;
use crate::vm::exec::numeric::compare_big_string;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Equality {
    Same,
    SameZero,
    Strict,
}

pub fn same_value<'s>(ctx: &ExecCtx<'_, 's>, x: Handle<'s>, y: Handle<'s>) -> bool {
    same_value_raw(ctx.heap(), ctx.get(x), ctx.get(y))
}
pub fn same_value_zero<'s>(ctx: &ExecCtx<'_, 's>, x: Handle<'s>, y: Handle<'s>) -> bool {
    equal(ctx.heap(), ctx.get(x), ctx.get(y), Equality::SameZero)
}
pub fn strict_eq<'s>(ctx: &ExecCtx<'_, 's>, x: Handle<'s>, y: Handle<'s>) -> bool {
    equal(ctx.heap(), ctx.get(x), ctx.get(y), Equality::Strict)
}
pub(crate) fn same_value_raw(heap: &Heap, x: Value, y: Value) -> bool {
    equal(heap, x, y, Equality::Same)
}

fn equal(heap: &Heap, x: Value, y: Value, kind: Equality) -> bool {
    if let (Some(x), Some(y)) = (x.as_number(), y.as_number()) {
        return (x == y && (kind != Equality::Same || x.to_bits() == y.to_bits()))
            || (kind != Equality::Strict && x.is_nan() && y.is_nan());
    }
    if x == y {
        return true;
    }
    if let (Some(x), Some(y)) = (BigView::of(heap, x), BigView::of(heap, y)) {
        return compare_big(x, y) == Ordering::Equal;
    }
    match (x.decode(), y.decode()) {
        (ValueKind::String(x), ValueKind::String(y)) => {
            heap.string(x).len() == heap.string(y).len()
                && heap.string_units(x).eq(heap.string_units(y))
        }
        _ => false,
    }
}

pub fn abstract_eq<'s>(ctx: &mut ExecCtx<'_, 's>, x: Handle<'s>, y: Handle<'s>) -> JsResult<bool> {
    let (x, y) = (ctx.get(x), ctx.get(y));
    ctx.with_scope(|ctx| {
        let x = ctx.mut_root(x)?;
        let y = ctx.mut_root(y)?;
        loop {
            let (a, b) = (ctx.get(x.handle()), ctx.get(y.handle()));
            if equal(ctx.heap(), a, b, Equality::Strict) {
                return Ok(true);
            }
            let nullish = |v| v == Value::NULL || v == Value::UNDEFINED;
            let dda = |v: Value| {
                v.as_object().is_some_and(|id| {
                    ctx.heap()
                        .object(id)
                        .flags
                        .contains(crate::object::ObjectFlags::IS_HTML_DDA)
                })
            };
            if nullish(a) {
                return Ok(nullish(b) || dda(b));
            }
            if nullish(b) {
                return Ok(dda(a));
            }
            match (a.decode(), b.decode()) {
                (ValueKind::Bool(v), _) => ctx.set(&x, Value::int32(i32::from(v))),
                (_, ValueKind::Bool(v)) => ctx.set(&y, Value::int32(i32::from(v))),
                (ValueKind::Object(_), ValueKind::Object(_)) => return Ok(false),
                (ValueKind::Object(_), _) => {
                    let v = convert::to_primitive(ctx, x.handle(), PreferredType::Default)?;
                    ctx.set(&x, v);
                }
                (_, ValueKind::Object(_)) => {
                    let v = convert::to_primitive(ctx, y.handle(), PreferredType::Default)?;
                    ctx.set(&y, v);
                }
                (ValueKind::String(id), _) if b.is_number() => {
                    let n = convert::string_to_number(ctx, id)?;
                    ctx.set(&x, Value::number(n));
                }
                (_, ValueKind::String(id)) if a.is_number() => {
                    let n = convert::string_to_number(ctx, id)?;
                    ctx.set(&y, Value::number(n));
                }
                (ValueKind::String(id), ValueKind::BigInt(_) | ValueKind::ShortBigInt(_)) => {
                    return Ok(compare_big_string(ctx, b, id)? == Some(Ordering::Equal));
                }
                (ValueKind::BigInt(_) | ValueKind::ShortBigInt(_), ValueKind::String(id)) => {
                    return Ok(compare_big_string(ctx, a, id)? == Some(Ordering::Equal));
                }
                _ => {
                    let compare = if let (Some(a), Some(b)) =
                        (BigView::of(ctx.heap(), a), b.as_number())
                    {
                        compare_big_number(a, b)
                    } else if let (Some(a), Some(b)) = (a.as_number(), BigView::of(ctx.heap(), b)) {
                        compare_big_number(b, a)
                    } else {
                        None
                    };
                    return Ok(compare == Some(Ordering::Equal));
                }
            }
        }
    })
}

/// If `left_first` is false, convert `right` before `left`.
/// `None` is the ECMAScript undefined result for an unordered comparison.
pub fn abstract_relational<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    left: Handle<'s>,
    right: Handle<'s>,
    left_first: bool,
) -> JsResult<Option<bool>> {
    let (left, right) = (ctx.get(left), ctx.get(right));
    ctx.with_scope(|ctx| {
        let (first, second) = if left_first {
            (left, right)
        } else {
            (right, left)
        };
        let first = ctx.root(first)?;
        let second = ctx.root(second)?;
        let a = convert::to_primitive(ctx, first, PreferredType::Number)?;
        let a = ctx.root(a)?;
        let b = convert::to_primitive(ctx, second, PreferredType::Number)?;
        let b = ctx.root(b)?;
        let (left, right) = if left_first { (a, b) } else { (b, a) };
        let (a, b) = (ctx.get(left), ctx.get(right));
        let result = match (a.decode(), b.decode()) {
            (ValueKind::String(a), ValueKind::String(b)) => {
                Some(ctx.heap().string_units(a).cmp(ctx.heap().string_units(b)))
            }
            (ValueKind::String(id), ValueKind::BigInt(_) | ValueKind::ShortBigInt(_)) => {
                compare_big_string(ctx, b, id)?.map(Ordering::reverse)
            }
            (ValueKind::BigInt(_) | ValueKind::ShortBigInt(_), ValueKind::String(id)) => {
                compare_big_string(ctx, a, id)?
            }
            _ => {
                let big_a = BigView::of(ctx.heap(), a).is_some();
                let big_b = BigView::of(ctx.heap(), b).is_some();
                match (big_a, big_b) {
                    (true, true) => Some(compare_big(
                        BigView::of(ctx.heap(), a).unwrap(),
                        BigView::of(ctx.heap(), b).unwrap(),
                    )),
                    (true, false) => {
                        let n = convert::to_number(ctx, right)?;
                        compare_big_number(BigView::of(ctx.heap(), a).unwrap(), n)
                    }
                    (false, true) => {
                        let n = convert::to_number(ctx, left)?;
                        compare_big_number(BigView::of(ctx.heap(), b).unwrap(), n)
                            .map(Ordering::reverse)
                    }
                    (false, false) => {
                        convert::to_number(ctx, left)?.partial_cmp(&convert::to_number(ctx, right)?)
                    }
                }
            }
        };
        Ok(result.map(|order| order == Ordering::Less))
    })
}

#[derive(Clone, Copy)]
enum Magnitude<'a> {
    Inline([u32; 2]),
    Limbs(&'a [u32]),
}
#[derive(Clone, Copy)]
struct BigView<'a> {
    negative: bool,
    limbs: Magnitude<'a>,
}

impl<'a> BigView<'a> {
    fn of(heap: &'a Heap, value: Value) -> Option<Self> {
        match value.decode() {
            ValueKind::ShortBigInt(n) => {
                let m = n.unsigned_abs();
                Some(Self {
                    negative: n < 0,
                    limbs: Magnitude::Inline([m as u32, (m >> 32) as u32]),
                })
            }
            ValueKind::BigInt(id) => Some(Self::from_data(heap.bigints.get(id).unwrap())),
            _ => None,
        }
    }
    fn from_data(data: &'a BigIntData) -> Self {
        Self {
            negative: data.negative,
            limbs: Magnitude::Limbs(&data.limbs),
        }
    }
    fn magnitude(&self) -> &[u32] {
        let slice: &[u32] = match &self.limbs {
            Magnitude::Inline(data) => data,
            Magnitude::Limbs(data) => data,
        };
        trim(slice)
    }
    fn is_negative(&self) -> bool {
        self.negative && !self.magnitude().is_empty()
    }
}

fn trim(limbs: &[u32]) -> &[u32] {
    let len = limbs.iter().rposition(|&n| n != 0).map_or(0, |i| i + 1);
    &limbs[..len]
}

fn compare_big(x: BigView<'_>, y: BigView<'_>) -> Ordering {
    match (x.is_negative(), y.is_negative()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (negative, _) => {
            let (a, b) = (x.magnitude(), y.magnitude());
            let order = a
                .len()
                .cmp(&b.len())
                .then_with(|| a.iter().rev().cmp(b.iter().rev()));
            if negative { order.reverse() } else { order }
        }
    }
}

fn compare_big_number(big: BigView<'_>, number: f64) -> Option<Ordering> {
    if number.is_nan() {
        return None;
    }
    if number.is_infinite() {
        return Some(if number.is_sign_positive() {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let mut limbs = [0; 32];
    let abs = number.abs().trunc();
    if abs != 0.0 {
        let bits = abs.to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023;
        let mantissa = (bits & ((1 << 52) - 1)) | (1 << 52);
        if exponent <= 52 {
            let n = mantissa >> (52 - exponent);
            limbs[0] = n as u32;
            limbs[1] = (n >> 32) as u32;
        } else {
            let shift = (exponent as usize) - 52;
            let n = u128::from(mantissa) << (shift % 32);
            for (i, limb) in limbs[shift / 32..].iter_mut().take(3).enumerate() {
                *limb = (n >> (i * 32)) as u32;
            }
        }
    }
    let order = compare_big(
        big,
        BigView {
            negative: number < 0.0,
            limbs: Magnitude::Limbs(&limbs),
        },
    );
    Some(if order == Ordering::Equal && number.fract() != 0.0 {
        if number < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Less
        }
    } else {
        order
    })
}
