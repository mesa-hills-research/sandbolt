use crate::radix_index;

pub(crate) const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const BILLION: u64 = 1_000_000_000;

fn write_unsigned<const SHIFT: u32>(buf: &mut [u8], mut n: u64, radix: u64) -> usize {
    let mut digits = [0; 64];
    let mut start = digits.len();
    loop {
        let digit = if SHIFT == 0 {
            let digit = n % radix;
            n /= radix;
            digit
        } else {
            let digit = n & (radix - 1);
            n >>= radix.trailing_zeros();
            digit
        };
        start -= 1;
        digits[start] = DIGITS[digit as usize];
        if n == 0 {
            break;
        }
    }
    let digits = &digits[start..];
    buf[..digits.len()].copy_from_slice(digits);
    digits.len()
}

pub(crate) fn write_limb(buf: &mut [u8], mut n: u32, radix: u32, bits: i32) {
    if radix == 10 {
        write_decimal_limb(buf, n);
    } else if bits != 0 {
        for byte in buf.iter_mut().rev() {
            *byte = DIGITS[(n & (radix - 1)) as usize];
            n >>= bits;
        }
    } else {
        for byte in buf.iter_mut().rev() {
            *byte = DIGITS[(n % radix) as usize];
            n /= radix;
        }
    }
}

fn write_decimal_limb(buf: &mut [u8], mut n: u32) {
    for byte in buf.iter_mut().rev() {
        *byte = b'0' + (n % 10) as u8;
        n /= 10;
    }
}

/// Writes the decimal digits of `n` and returns their count. No NUL is written.
/// The buffer must have room for the result.
pub fn u32toa(buf: &mut [u8], mut n: u32) -> usize {
    let mut digits = [0; 10];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let digits = &digits[start..];
    buf[..digits.len()].copy_from_slice(digits);
    digits.len()
}

/// Like [`u32toa`], with a leading minus sign for negative numbers.
pub fn i32toa(buf: &mut [u8], n: i32) -> usize {
    let sign = write_sign(buf, n < 0);
    sign + u32toa(&mut buf[sign..], n.unsigned_abs())
}

/// Writes the decimal digits of `n` and returns their count. No NUL is written.
/// The buffer must have room for the result.
pub fn u64toa(buf: &mut [u8], n: u64) -> usize {
    if let Ok(n) = u32::try_from(n) {
        return u32toa(buf, n);
    }
    let hi = n / BILLION;
    let lo = n % BILLION;
    let len = if let Ok(hi) = u32::try_from(hi) {
        u32toa(buf, hi)
    } else {
        let len = u32toa(buf, (hi / BILLION) as u32);
        write_decimal_limb(&mut buf[len..len + 9], (hi % BILLION) as u32);
        len + 9
    };
    write_decimal_limb(&mut buf[len..len + 9], lo as u32);
    len + 9
}

/// Like [`u64toa`], with a leading minus sign for negative numbers.
pub fn i64toa(buf: &mut [u8], n: i64) -> usize {
    let sign = write_sign(buf, n < 0);
    sign + u64toa(&mut buf[sign..], n.unsigned_abs())
}

/// Like [`u64toa`], in radix 2 through 36 using lowercase letters.
pub fn u64toa_radix(buf: &mut [u8], n: u64, radix: u32) -> usize {
    radix_index(radix);
    match radix {
        10 => u64toa(buf, n),
        radix if radix.is_power_of_two() => write_unsigned::<1>(buf, n, u64::from(radix)),
        _ => write_unsigned::<0>(buf, n, u64::from(radix)),
    }
}

/// Like [`u64toa_radix`], with a leading minus sign for negative numbers.
pub fn i64toa_radix(buf: &mut [u8], n: i64, radix: u32) -> usize {
    let sign = write_sign(buf, n < 0);
    sign + u64toa_radix(&mut buf[sign..], n.unsigned_abs(), radix)
}

fn write_sign(buf: &mut [u8], negative: bool) -> usize {
    if negative {
        buf[0] = b'-';
    }
    usize::from(negative)
}
