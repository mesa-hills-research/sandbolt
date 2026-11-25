use std::cell::Cell;
use std::hash::{BuildHasher, RandomState};

use crate::ops::call::call;
use crate::ops::convert::{to_object, to_uint32};
use crate::ops::property::{get_property, get_property_with_receiver};
use crate::value::Abrupt;

use super::*;

pub(super) const START: u16 = 0x30;
pub(super) const END: u16 = START + METHODS.len() as u16 - 1;

enum Operation {
    Unary(fn(f64) -> f64),
    Binary(fn(f64, f64) -> f64),
    Round,
    Clz32,
    Imul,
    Hypot,
    Min,
    Max,
    Random,
    SumPrecise,
}

use Operation::*;

const METHODS: &[(&str, u32, Operation)] = &[
    ("abs", 1, Unary(f64::abs)),
    ("acos", 1, Unary(f64::acos)),
    ("acosh", 1, Unary(f64::acosh)),
    ("asin", 1, Unary(f64::asin)),
    ("asinh", 1, Unary(f64::asinh)),
    ("atan", 1, Unary(f64::atan)),
    ("atanh", 1, Unary(f64::atanh)),
    ("atan2", 2, Binary(f64::atan2)),
    ("cbrt", 1, Unary(f64::cbrt)),
    ("ceil", 1, Unary(f64::ceil)),
    ("clz32", 1, Clz32),
    ("cos", 1, Unary(f64::cos)),
    ("cosh", 1, Unary(f64::cosh)),
    ("exp", 1, Unary(f64::exp)),
    ("expm1", 1, Unary(f64::exp_m1)),
    ("f16round", 1, Unary(f16round)),
    ("floor", 1, Unary(f64::floor)),
    ("fround", 1, Unary(|x| (x as f32).into())),
    ("hypot", 2, Hypot),
    ("imul", 2, Imul),
    ("log", 1, Unary(f64::ln)),
    ("log1p", 1, Unary(f64::ln_1p)),
    ("log10", 1, Unary(f64::log10)),
    ("log2", 1, Unary(f64::log2)),
    ("max", 2, Max),
    ("min", 2, Min),
    ("pow", 2, Binary(pow)),
    ("random", 0, Random),
    ("round", 1, Round),
    ("sign", 1, Unary(sign)),
    ("sin", 1, Unary(f64::sin)),
    ("sinh", 1, Unary(f64::sinh)),
    ("sqrt", 1, Unary(f64::sqrt)),
    ("sumPrecise", 1, SumPrecise),
    ("tan", 1, Unary(f64::tan)),
    ("tanh", 1, Unary(f64::tanh)),
    ("trunc", 1, Unary(f64::trunc)),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    use std::f64::consts::*;
    let math = ctx.new_object()?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    define(ctx, global, "Math", math, mutable())?;
    set_tag(ctx, math, "Math")?;
    constants(
        ctx,
        math,
        &[
            ("E", E),
            ("LN10", LN_10),
            ("LN2", LN_2),
            ("LOG10E", LOG10_E),
            ("LOG2E", LOG2_E),
            ("PI", PI),
            ("SQRT1_2", FRAC_1_SQRT_2),
            ("SQRT2", SQRT_2),
        ],
    )?;
    for (i, &(name, length, _)) in METHODS.iter().enumerate() {
        let key = ctx.intern_str(name)?;
        install_method(
            ctx,
            math,
            key,
            NativeFnId(NUM_DATE_BASE + START + i as u16),
            name,
            length,
        )?;
    }
    Ok(())
}

pub(super) fn dispatch<'s>(
    offset: u16,
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
) -> JsResult<Value> {
    let n = match METHODS[(offset - START) as usize].2 {
        Unary(f) => {
            let x = argument(ctx, args, 0)?;
            f(to_number(ctx, x)?)
        }
        Binary(f) => {
            let x = argument(ctx, args, 0)?;
            let y = argument(ctx, args, 1)?;
            f(to_number(ctx, x)?, to_number(ctx, y)?)
        }
        Round => {
            let x = argument(ctx, args, 0)?;
            round(to_number(ctx, x)?)
        }
        Clz32 => {
            let x = argument(ctx, args, 0)?;
            f64::from(to_uint32(ctx, x)?.leading_zeros())
        }
        Imul => {
            let x = argument(ctx, args, 0)?;
            let y = argument(ctx, args, 1)?;
            f64::from(to_uint32(ctx, x)?.wrapping_mul(to_uint32(ctx, y)?) as i32)
        }
        Hypot => {
            let mut norm = Norm::default();
            for i in 0..args.len() {
                norm.add(to_number(ctx, args.get(i).unwrap())?);
            }
            norm.finish()
        }
        Max | Min => {
            let max = matches!(METHODS[(offset - START) as usize].2, Max);
            let mut n = if max {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
            for i in 0..args.len() {
                let x = to_number(ctx, args.get(i).unwrap())?;
                if x.is_nan()
                    || (if max {
                        x.total_cmp(&n).is_gt()
                    } else {
                        x.total_cmp(&n).is_lt()
                    }) && !n.is_nan()
                {
                    n = x;
                }
            }
            n
        }
        Random => random(),
        SumPrecise => {
            let items = argument(ctx, args, 0)?;
            sum_precise(ctx, items, 0)?
        }
    };
    Ok(Value::number(n))
}

#[derive(Default)]
struct Norm {
    exponent: i32,
    sum: f64,
    error: f64,
    infinite: bool,
    nan: bool,
}

impl Norm {
    fn add(&mut self, value: f64) {
        let x = value.abs();
        if !x.is_finite() || x == 0.0 {
            self.infinite |= x.is_infinite();
            self.nan |= x.is_nan();
            return;
        }
        let exponent = ((x.to_bits() >> 52) as i32 - 1023).max(-1022);
        if self.sum == 0.0 {
            self.exponent = exponent;
        } else if exponent > self.exponent {
            // Rescale both parts without squaring the factor, which can underflow first.
            let factor = power_of_two(self.exponent - exponent);
            self.sum = (self.sum * factor) * factor;
            self.error = (self.error * factor) * factor;
            self.exponent = exponent;
        }
        let scaled = x * power_of_two(-self.exponent);
        let (square, square_error) = square_with_error(scaled);
        let total = self.sum + square;
        let roundoff = if self.sum >= square {
            (self.sum - total) + square
        } else {
            (square - total) + self.sum
        };
        self.error += roundoff + square_error;
        self.sum = total;
    }

    fn finish(self) -> f64 {
        if self.infinite {
            f64::INFINITY
        } else if self.nan {
            f64::NAN
        } else if self.sum == 0.0 {
            0.0
        } else {
            let root = self.sum.sqrt();
            let (square, square_error) = square_with_error(root);
            let residual = ((self.sum - square) - square_error) + self.error;
            (root + residual / (2.0 * root)) * power_of_two(self.exponent)
        }
    }
}

fn power_of_two(exponent: i32) -> f64 {
    f64::from_bits(match exponent {
        -1022..=1023 => ((exponent + 1023) as u64) << 52,
        -1074..=-1023 => 1 << (exponent + 1074),
        _ => 0,
    })
}

// Dekker splitting recovers the rounding error without a platform FMA.
fn square_with_error(x: f64) -> (f64, f64) {
    let split = 134_217_729.0 * x;
    let high = split - (split - x);
    let low = x - high;
    let square = x * x;
    let cross = high * low;
    let error = (((high * high - square) + cross) + cross) + low * low;
    (square, error)
}

fn round(x: f64) -> f64 {
    if !x.is_finite() || x.fract() == 0.0 {
        return x;
    }
    let i = x.trunc();
    let f = x - i;
    if f >= 0.5 {
        i + 1.0
    } else if f < -0.5 {
        i - 1.0
    } else {
        i
    }
}

fn sign(x: f64) -> f64 {
    if x == 0.0 || x.is_nan() {
        x
    } else {
        1.0_f64.copysign(x)
    }
}

fn pow(x: f64, y: f64) -> f64 {
    if y.is_nan() || (y.is_infinite() && x.abs() == 1.0) {
        f64::NAN
    } else {
        x.powf(y)
    }
}

fn f16round(x: f64) -> f64 {
    let abs = x.abs();
    if abs >= 65_520.0 {
        return f64::INFINITY.copysign(x);
    }
    if x.is_nan() {
        return x;
    }
    // Round directly from binary64. A binary32 intermediate can round ties twice.
    let exponent = ((abs.to_bits() >> 52) as i32 - 1023 - 10).max(-24);
    let step = f64::from_bits(((exponent + 1023) as u64) << 52);
    (x / step).round_ties_even() * step
}

thread_local! {
    static RANDOM_STATE: Cell<[u64; 4]> = Cell::new({
        let mut seed = RandomState::new().hash_one(0_u64);
        std::array::from_fn(|_| {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let z = (seed ^ (seed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        })
    });
}

fn random() -> f64 {
    RANDOM_STATE.with(|cell| {
        let [mut a, mut b, mut c, mut d] = cell.get();
        let result = b.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = b << 17;
        c ^= a;
        d ^= b;
        b ^= c;
        a ^= d;
        c ^= t;
        cell.set([a, b, c, d.rotate_left(45)]);
        (result >> 11) as f64 * (1.0 / (1_u64 << 53) as f64)
    })
}

pub(super) fn sum_precise<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    items: Handle<'s>,
    mut count: u64,
) -> JsResult<f64> {
    let iterator_key = symbol_key(ctx, WellKnown::Iterator);
    let base = to_object(ctx, items)?;
    let method = get_property_with_receiver(ctx, base, iterator_key, items)?;
    let method = ctx.root(method)?;
    let iterator = call(ctx, method, items, Args::empty())?;
    let iterator = ctx.root(iterator)?;
    if ctx.get(iterator).as_object().is_none() {
        return Err(ctx.throw_type_error("iterator must be an object"));
    }
    let next_key = ctx.intern_str("next")?;
    let next_value = get_property(ctx, iterator, next_key)?;
    let next = ctx.root(next_value)?;
    let done_key = ctx.intern_str("done")?;
    let value_key = ctx.intern_str("value")?;
    let mut sum = Sum::new();
    loop {
        let iterator_value = ctx.get(iterator);
        let next_value = ctx.get(next);
        let item = ctx.with_scope(|ctx| {
            let iterator = ctx.root(iterator_value)?;
            let next = ctx.root(next_value)?;
            let result = call(ctx, next, iterator, Args::empty())?;
            let result = ctx.root(result)?;
            if ctx.get(result).as_object().is_none() {
                return Err(ctx.throw_type_error("iterator result must be an object"));
            }
            let done = get_property(ctx, result, done_key)?;
            let done = ctx.root(done)?;
            if to_boolean(ctx, done) {
                return Ok(None);
            }
            let value = get_property(ctx, result, value_key)?;
            Ok(Some(value.as_number()))
        })?;
        match item {
            None => return Ok(sum.result()),
            Some(_) if count >= (1 << 53) - 1 => {
                let error = ctx.throw_range_error("Math.sumPrecise input is too long");
                return Err(iterator_close_on_throw(ctx, iterator, error));
            }
            Some(Some(n)) => {
                sum.add(n);
                count += 1;
            }
            Some(None) => {
                let error = ctx.throw_type_error("Math.sumPrecise requires numbers");
                return Err(iterator_close_on_throw(ctx, iterator, error));
            }
        }
    }
}

fn iterator_close_on_throw<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
    error: Abrupt,
) -> Abrupt {
    let value = ctx.pending_exception().unwrap();
    let Ok(root) = ctx.root(value) else {
        return error;
    };
    let (_, oom) = ctx.take_exception().unwrap();
    let close = (|| {
        let key = ctx.intern_str("return")?;
        let method = get_property(ctx, iterator, key)?;
        if method != Value::UNDEFINED && method != Value::NULL {
            let method = ctx.root(method)?;
            call(ctx, method, iterator, Args::empty())?;
        }
        Ok::<(), Abrupt>(())
    })();
    if close == Err(Abrupt::Interrupted) {
        return Abrupt::Interrupted;
    }
    ctx.throw_value(ctx.get(root), oom)
}

// Every finite binary64 value is an integer multiple of 2^-1074. The
// accumulator can hold the exact sum of up to 2^53 finite inputs.
struct Sum {
    words: [u64; 34],
    state: SumState,
}

#[derive(Clone, Copy, PartialEq)]
enum SumState {
    MinusZero,
    Finite,
    PosInf,
    NegInf,
    NaN,
}

impl Sum {
    fn new() -> Self {
        Self {
            words: [0; 34],
            state: SumState::MinusZero,
        }
    }

    fn add(&mut self, x: f64) {
        use SumState::*;
        if x.is_nan() {
            self.state = NaN;
            return;
        }
        if x.is_infinite() {
            let state = if x.is_sign_negative() { NegInf } else { PosInf };
            self.state = match self.state {
                NaN => NaN,
                PosInf | NegInf if self.state != state => NaN,
                _ => state,
            };
            return;
        }
        if !matches!(self.state, MinusZero | Finite) || x.to_bits() == (-0.0_f64).to_bits() {
            return;
        }
        self.state = Finite;
        if x == 0.0 {
            return;
        }
        let bits = x.to_bits();
        let exp = ((bits >> 52) & 0x7ff) as usize;
        let mantissa = (bits & ((1_u64 << 52) - 1)) | (u64::from(exp != 0) << 52);
        let bit = exp.saturating_sub(1);
        let index = bit / 64;
        let shift = bit % 64;
        let lo = mantissa << shift;
        let hi = if shift == 0 {
            0
        } else {
            mantissa >> (64 - shift)
        };
        let negative = x.is_sign_negative();
        let sign = if negative { u64::MAX } else { 0 };
        let mut carry = negative;
        for (i, word) in self.words[index..].iter_mut().enumerate() {
            let digit = match i {
                0 => lo,
                1 => hi,
                _ => 0,
            };
            let (sum, out1) = word.overflowing_add(digit ^ sign);
            let (sum, out2) = sum.overflowing_add(u64::from(carry));
            *word = sum;
            carry = out1 || out2;
            if i >= 1 && carry == negative {
                break;
            }
        }
    }

    fn result(mut self) -> f64 {
        use SumState::*;
        match self.state {
            MinusZero => return -0.0,
            PosInf => return f64::INFINITY,
            NegInf => return f64::NEG_INFINITY,
            NaN => return f64::NAN,
            Finite => {}
        }
        let sign = self.words.last().unwrap() & (1 << 63);
        if sign != 0 {
            let mut carry = true;
            for word in &mut self.words {
                (*word, carry) = (!*word).overflowing_add(u64::from(carry));
            }
        }
        let Some(top) = self.words.iter().rposition(|&x| x != 0) else {
            return 0.0;
        };
        let msb = top * 64 + self.words[top].ilog2() as usize;
        if msb < 52 {
            return f64::from_bits(sign | self.words[0]);
        }
        let low = msb - 52;
        let index = low / 64;
        let shift = low % 64;
        let mut mant = self.words[index] >> shift;
        if shift != 0 && index < top {
            mant |= self.words[index + 1] << (64 - shift);
        }
        if low != 0 {
            let midpoint = low - 1;
            let word = midpoint / 64;
            let mask = 1_u64 << (midpoint % 64);
            let sticky =
                (self.words[word] & (mask - 1)) != 0 || self.words[..word].iter().any(|&x| x != 0);
            if self.words[word] & mask != 0 && (sticky || mant & 1 != 0) {
                mant += 1;
            }
        }
        let exponent = low + 1;
        if exponent >= 2047 {
            return f64::from_bits(sign | f64::INFINITY.to_bits());
        }
        // Adding the implicit leading bit also handles carry into the exponent.
        f64::from_bits(sign | ((((exponent - 1) as u64) << 52) + mant))
    }
}
