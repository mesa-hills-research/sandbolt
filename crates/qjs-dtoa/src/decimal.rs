use std::cmp::Ordering;

const POW5: [u128; 32] = powers(5);
const POW10: [u128; 32] = powers(10);

const fn powers(base: u128) -> [u128; 32] {
    let mut table = [1; 32];
    let mut i = 1;
    while i < table.len() {
        table[i] = table[i - 1] * base;
        i += 1;
    }
    table
}

// Scale a binary unit by 10^decimal and leave powers of two in the ratio.
fn ratio(binary: i32, decimal: i32) -> Option<(u128, u128)> {
    let power = *POW5.get(decimal.unsigned_abs() as usize)?;
    let (num, den) = if decimal < 0 { (1, power) } else { (power, 1) };
    let binary = binary + decimal;
    if binary >= 0 {
        Some((shift(num, binary as u32)?, den))
    } else {
        Some((num, shift(den, binary.unsigned_abs())?))
    }
}

fn shift(n: u128, bits: u32) -> Option<u128> {
    if bits <= n.leading_zeros() {
        Some(n << bits)
    } else {
        None
    }
}

fn div_rem(n: u128, d: u128) -> (u128, u128) {
    if d.is_power_of_two() {
        (n >> d.trailing_zeros(), n & (d - 1))
    } else if let (Ok(n), Ok(d)) = (u64::try_from(n), u64::try_from(d)) {
        (u128::from(n / d), u128::from(n % d))
    } else {
        (n / d, n % d)
    }
}

pub(super) fn fraction(mantissa: u64, binary: i32, digits: i32) -> Option<u128> {
    let (num, den) = ratio(binary, digits)?;
    let (whole, rem) = div_rem(num.checked_mul(u128::from(mantissa))?, den);
    Some(whole + u128::from(rem >= den - rem))
}

pub(super) fn fixed(
    mantissa: u64,
    binary: i32,
    digits: i32,
    mut point: i32,
) -> Option<(u128, i32)> {
    if digits > 22 {
        return None;
    }
    let limit = POW10[digits as usize];
    loop {
        let rounded = fraction(mantissa, binary, digits - point)?;
        if rounded < limit {
            return Some((rounded, point));
        }
        point += 1;
    }
}

pub(super) fn shortest(mantissa: u64, binary: i32, mut point: i32) -> Option<(u128, i32)> {
    // The smallest normal has equal gaps above and below.
    if binary <= -1074 {
        return None;
    }
    let center = u128::from(mantissa) << 2;
    // Units of a quarter ULP give exact endpoints, including the narrower gap
    // below an exact power of two. Even mantissas own both midpoints.
    let below = center - if mantissa == 1 << 52 { 1 } else { 2 };
    let above = center + 2;
    let odd = mantissa & 1 != 0;
    let round = |whole: u64, cmp_half| {
        whole
            + u64::from(
                cmp_half == Ordering::Greater || cmp_half == Ordering::Equal && whole & 1 != 0,
            )
    };
    loop {
        let (unit, den) = ratio(binary - 2, 17 - point)?;
        let center = unit.checked_mul(center)?;
        let (whole, rem) = div_rem(center, den);
        if whole >= POW10[17] {
            point += 1;
            continue;
        }
        let mut whole = whole as u64;
        let mut sticky = rem != 0;
        let mut best = round(whole, rem.cmp(&(den - rem)));
        if best == POW10[17] as u64 {
            point += 1;
            continue;
        }

        let (lower, rem) = div_rem(unit.checked_mul(below)?, den);
        let (upper, rem_upper) = div_rem(unit.checked_mul(above)?, den);
        let mut lower = lower as u64 + u64::from(rem != 0 || odd);
        let mut upper = upper as u64 - u64::from(rem_upper == 0 && odd);
        // Only the closest decimal at each precision is a candidate. Shrink
        // the admissible interval and round from the original remainder.
        let mut best_point = point;
        for digits in (1..17).rev() {
            let digit = whole % 10;
            whole /= 10;
            let cmp_half = match digit.cmp(&5) {
                Ordering::Equal if sticky => Ordering::Greater,
                cmp => cmp,
            };
            sticky |= digit != 0;
            lower = lower.div_ceil(10);
            upper /= 10;
            let rounded = round(whole, cmp_half);
            if !(lower..=upper).contains(&rounded) {
                break;
            }
            best = rounded;
            best_point = point + i32::from(rounded == POW10[digits] as u64);
        }
        while best % 10 == 0 {
            best /= 10;
        }
        return Some((u128::from(best), best_point));
    }
}
