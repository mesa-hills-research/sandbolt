use crate::case::{CASE_F, CASE_U, case_ranges};
use crate::{CharRangeOp, UnicodeError, canonicalize};

/// Sorted half-open code-point intervals `[start, end)` stored flat as pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CharRange {
    pub points: Vec<u32>,
}

impl CharRange {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }

    pub fn add_interval(&mut self, start: u32, end: u32) {
        self.points.extend([start, end]);
    }

    /// Add an inclusive interval to the set.
    pub fn union_interval(&mut self, c1: u32, c2: u32) {
        self.op_in_place(&[c1, c2.saturating_add(1)], CharRangeOp::Union);
    }

    pub fn op_in_place(&mut self, other: &[u32], op: CharRangeOp) {
        self.try_op_in_place(other, op)
            .expect("character range allocation failed");
    }

    /// Complement the set in `[0, u32::MAX)`.
    pub fn invert(&mut self) {
        self.try_invert()
            .expect("character range allocation failed");
    }

    /// Replace the set with its image under RegExp canonicalization.
    pub fn regexp_canonicalize(&mut self, is_unicode: bool) {
        self.try_regexp_canonicalize(is_unicode)
            .expect("character range allocation failed");
    }

    pub(crate) fn try_add_interval(&mut self, start: u32, end: u32) -> Result<(), UnicodeError> {
        self.points.try_reserve(2)?;
        self.add_interval(start, end);
        Ok(())
    }

    pub(crate) fn try_op_in_place(
        &mut self,
        other: &[u32],
        op: CharRangeOp,
    ) -> Result<(), UnicodeError> {
        let mut out = Self::new();
        try_cr_op(&mut out, &self.points, other, op)?;
        *self = out;
        Ok(())
    }

    pub(crate) fn try_invert(&mut self) -> Result<(), UnicodeError> {
        self.points.try_reserve(2)?;
        self.points.insert(0, 0);
        self.points.push(u32::MAX);
        self.compress();
        Ok(())
    }

    fn try_regexp_canonicalize(&mut self, is_unicode: bool) -> Result<(), UnicodeError> {
        let mask = case_ranges(if is_unicode { CASE_F } else { CASE_U })?;
        let mut changed = Self::new();
        let mut unchanged = Self::new();
        try_cr_op(&mut changed, &self.points, &mask.points, CharRangeOp::Inter)?;
        try_cr_op(&mut unchanged, &self.points, &mask.points, CharRangeOp::Sub)?;

        let mut mapped = Self::new();
        let mut run: Option<(u32, u32)> = None;
        for &[start, end] in changed.points.as_chunks::<2>().0 {
            for c in start..end {
                let d = canonicalize(c, is_unicode) as u32;
                match &mut run {
                    Some((_, last)) if *last == d => *last += 1,
                    _ => {
                        if let Some((first, last)) = run {
                            mapped.try_add_interval(first, last)?;
                        }
                        run = Some((d, d + 1));
                    }
                }
            }
        }
        if let Some((start, end)) = run {
            mapped.try_add_interval(start, end)?;
        }
        mapped.sort_and_merge();
        let mut result = Self::new();
        try_cr_op(
            &mut result,
            &mapped.points,
            &unchanged.points,
            CharRangeOp::Union,
        )?;
        *self = result;
        Ok(())
    }

    fn compress(&mut self) {
        let len = self.points.len();
        let mut read = 0;
        let mut write = 0;
        while read + 1 < len {
            let start = self.points[read];
            if start == self.points[read + 1] {
                read += 2;
                continue;
            }
            while read + 3 < len && self.points[read + 1] == self.points[read + 2] {
                read += 2;
            }
            self.points[write] = start;
            self.points[write + 1] = self.points[read + 1];
            write += 2;
            read += 2;
        }
        self.points.truncate(write);
    }

    fn sort_and_merge(&mut self) {
        let (pairs, _) = self.points.as_chunks_mut::<2>();
        pairs.sort_unstable_by_key(|pair| pair[0]);
        let mut written = 0;
        for i in 0..pairs.len() {
            let [start, end] = pairs[i];
            if written > 0 && start <= pairs[written - 1][1] {
                pairs[written - 1][1] = end.max(pairs[written - 1][1]);
            } else {
                pairs[written] = [start, end];
                written += 1;
            }
        }
        self.points.truncate(written * 2);
    }
}

/// Append the result of a set operation to `out` and merge adjacent intervals.
pub fn cr_op(out: &mut CharRange, a: &[u32], b: &[u32], op: CharRangeOp) {
    try_cr_op(out, a, b, op).expect("character range allocation failed");
}

pub(crate) fn try_cr_op(
    out: &mut CharRange,
    a: &[u32],
    b: &[u32],
    op: CharRangeOp,
) -> Result<(), UnicodeError> {
    let mut i = 0;
    let mut j = 0;
    loop {
        let point = match (a.get(i), b.get(j)) {
            (Some(&x), Some(&y)) => {
                i += usize::from(x <= y);
                j += usize::from(y <= x);
                x.min(y)
            }
            (Some(&x), None) => {
                i += 1;
                x
            }
            (None, Some(&y)) => {
                j += 1;
                y
            }
            (None, None) => break,
        };
        let in_a = i & 1 != 0;
        let in_b = j & 1 != 0;
        let included = match op {
            CharRangeOp::Union => in_a || in_b,
            CharRangeOp::Inter => in_a && in_b,
            CharRangeOp::Xor => in_a ^ in_b,
            CharRangeOp::Sub => in_a && !in_b,
        };
        if included != (out.len() & 1 != 0) {
            out.points.try_reserve(1)?;
            out.points.push(point);
        }
    }
    out.compress();
    Ok(())
}
