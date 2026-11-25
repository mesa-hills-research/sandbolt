use std::cmp::Ordering;

const LIMBS: usize = 52;
const LIMB_BITS: i32 = u32::BITS as i32;

#[derive(Clone, Copy)]
pub(crate) enum Round {
    Even,
    Away,
    Truncate,
}

// Little-endian base-2^32 limbs. Zero has one limb, and every other value has
// a nonzero top limb. The largest scaling needs less than 52 limbs.
pub(crate) struct Limbs {
    words: [u32; LIMBS],
    len: usize,
}

impl Limbs {
    pub(crate) fn new(value: u64) -> Self {
        let mut n = Self {
            words: [0; LIMBS],
            len: 1,
        };
        n.set(value);
        n
    }

    pub(crate) fn set(&mut self, value: u64) {
        self.words[0] = value as u32;
        self.words[1] = (value >> LIMB_BITS) as u32;
        self.len = if self.words[1] == 0 { 1 } else { 2 };
    }

    pub(crate) fn low_limb(&self) -> u32 {
        self.words[0]
    }

    pub(crate) fn as_u64(&self) -> u64 {
        debug_assert!(self.len <= 2);
        u64::from(self.words[0])
            | if self.len == 2 {
                u64::from(self.words[1]) << LIMB_BITS
            } else {
                0
            }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.len == 1 && self.words[0] == 0
    }

    pub(crate) fn floor_log2(&self) -> i32 {
        self.len as i32 * LIMB_BITS - 1 - self.words[self.len - 1].leading_zeros() as i32
    }

    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        self.len.cmp(&other.len).then_with(|| {
            self.words[..self.len]
                .iter()
                .rev()
                .cmp(other.words[..other.len].iter().rev())
        })
    }

    fn push(&mut self, word: u32) {
        self.words[self.len] = word;
        self.len += 1;
    }

    fn normalize(&mut self) {
        while self.len > 1 && self.words[self.len - 1] == 0 {
            self.len -= 1;
        }
    }

    fn add_one(&mut self) {
        for word in &mut self.words[..self.len] {
            *word = word.wrapping_add(1);
            if *word != 0 {
                return;
            }
        }
        self.push(1);
    }

    pub(crate) fn mul_add(&mut self, multiplier: u64, addend: u32) {
        let mut carry = u64::from(addend);
        for word in &mut self.words[..self.len] {
            carry += u64::from(*word) * multiplier;
            *word = carry as u32;
            carry >>= LIMB_BITS;
        }
        if carry != 0 {
            self.push(carry as u32);
        }
    }

    pub(crate) fn div_rem(&mut self, divisor: u32) -> u32 {
        let divisor = u64::from(divisor);
        let mut rem = 0;
        for word in self.words[..self.len].iter_mut().rev() {
            let numerator = (rem << LIMB_BITS) | u64::from(*word);
            *word = (numerator / divisor) as u32;
            rem = numerator % divisor;
        }
        self.normalize();
        rem as u32
    }

    pub(crate) fn set_sticky(&mut self, discarded_nonzero: bool) {
        self.words[0] |= u32::from(discarded_nonzero);
    }

    fn bit(&self, index: usize) -> bool {
        let word = index / u32::BITS as usize;
        word < self.len && (self.words[word] >> (index % u32::BITS as usize)) & 1 != 0
    }

    fn bits_below(&self, index: usize) -> bool {
        let limbs = index / u32::BITS as usize;
        let bits = index % u32::BITS as usize;
        self.words[..limbs.min(self.len)].iter().any(|&v| v != 0)
            || (limbs < self.len && self.words[limbs] & ((1 << bits) - 1) != 0)
    }

    fn shift_left(&mut self, shift: usize) {
        if self.is_zero() {
            return;
        }
        let limbs = shift / u32::BITS as usize;
        let bits = shift % u32::BITS as usize;
        if limbs != 0 {
            self.words.copy_within(..self.len, limbs);
            self.words[..limbs].fill(0);
            self.len += limbs;
        }
        if bits != 0 {
            let mut carry = 0;
            for word in &mut self.words[limbs..self.len] {
                let high = *word >> (u32::BITS as usize - bits);
                *word = (*word << bits) | carry;
                carry = high;
            }
            if carry != 0 {
                self.push(carry);
            }
        }
    }

    fn shift_right(&mut self, shift: usize) {
        let limbs = shift / u32::BITS as usize;
        let bits = shift % u32::BITS as usize;
        if limbs >= self.len {
            self.len = 1;
            self.words[0] = 0;
            return;
        }
        if limbs != 0 {
            self.words.copy_within(limbs..self.len, 0);
            self.len -= limbs;
        }
        if bits != 0 {
            let mut carry = 0;
            for word in self.words[..self.len].iter_mut().rev() {
                let low = *word << (u32::BITS as usize - bits);
                *word = (*word >> bits) | carry;
                carry = low;
            }
            self.normalize();
        }
    }

    // Negative shifts multiply by a power of two and need no rounding.
    pub(crate) fn round_shift(&mut self, shift: i32, mode: Round) {
        match shift.cmp(&0) {
            Ordering::Less => self.shift_left(shift.unsigned_abs() as usize),
            Ordering::Equal => {}
            Ordering::Greater => {
                let shift = shift as usize;
                let round_up = match mode {
                    Round::Truncate => false,
                    Round::Away => self.bit(shift - 1),
                    Round::Even => {
                        self.bit(shift - 1) && (self.bit(shift) || self.bits_below(shift - 1))
                    }
                };
                self.shift_right(shift);
                if round_up {
                    self.add_one();
                }
            }
        }
    }
}
