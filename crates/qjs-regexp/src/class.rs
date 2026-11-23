use crate::alloc::Budget;
use crate::compiler::{Compiler, Result, syntax};
use crate::escape::{parse_escape, wtf8};
use crate::{RegExpError, canonicalize};
use qjs_unicode::{CharRange, CharRangeOp, UnicodeError, cr_op};
use std::mem::size_of;

pub(crate) enum Atom {
    Char(u32),
    Set(Set, Option<u8>),
}

#[derive(Default)]
pub(crate) struct Set {
    pub(crate) chars: CharRange,
    pub(crate) strings: Strings,
}

impl Set {
    fn range(&mut self, budget: &mut Budget<'_>, f: impl FnOnce(&mut CharRange)) -> Result<()> {
        let old = self.chars.points.capacity();
        f(&mut self.chars);
        range_charge(budget, old, self.chars.points.capacity())
    }

    fn invert(&mut self, budget: &mut Budget<'_>) -> Result<()> {
        budget.reserve(&mut self.chars.points, 2)?;
        self.chars.invert();
        Ok(())
    }

    fn interval(&mut self, budget: &mut Budget<'_>, low: u32, high: u32) -> Result<()> {
        let mut out = CharRange::new();
        union_points(budget, &mut out, &self.chars.points, &[low, high + 1])?;
        budget.free(std::mem::replace(&mut self.chars.points, out.points));
        Ok(())
    }

    fn add(&mut self, budget: &mut Budget<'_>, seq: &[u32]) -> Result<()> {
        match seq {
            &[ch] => self.interval(budget, ch, ch),
            _ => self.strings.add(budget, seq),
        }
    }

    fn canonicalize(&mut self, budget: &mut Budget<'_>, unicode: bool) -> Result<()> {
        self.range(budget, |chars| chars.regexp_canonicalize(unicode))?;
        let mut old = std::mem::take(&mut self.strings);
        for &head in &old.heads {
            let mut i = head;
            while let Some(index) = i {
                let entry = &mut old.entries[index];
                i = entry.next;
                for c in &mut entry.points {
                    *c = canonicalize(*c, unicode);
                }
                self.strings.add(budget, &entry.points)?;
                budget.free(std::mem::take(&mut entry.points));
            }
        }
        budget.free(old.heads);
        budget.free(old.entries);
        Ok(())
    }

    fn op(&mut self, budget: &mut Budget<'_>, other: Self, op: CharRangeOp) -> Result<()> {
        let mut out = CharRange::new();
        range_op(
            budget,
            &mut out,
            &self.chars.points,
            &other.chars.points,
            op,
        )?;
        budget.free(std::mem::replace(&mut self.chars.points, out.points));
        match op {
            CharRangeOp::Union => {
                for seq in other.strings.iter() {
                    self.strings.add(budget, seq)?;
                }
            }
            CharRangeOp::Inter | CharRangeOp::Sub => {
                self.strings.retain(budget, |seq| {
                    other.strings.contains(seq) == matches!(op, CharRangeOp::Inter)
                });
            }
            CharRangeOp::Xor => unreachable!(),
        }
        other.free(budget);
        Ok(())
    }

    pub(crate) fn free(self, budget: &mut Budget<'_>) {
        budget.free(self.chars.points);
        self.strings.free(budget);
    }
}

fn range_charge(budget: &mut Budget<'_>, old: usize, new: usize) -> Result<()> {
    if let Some(growth) = new.checked_sub(old) {
        budget.charge(growth * size_of::<u32>())?;
    } else {
        budget.release((old - new) * size_of::<u32>());
    }
    Ok(())
}

fn union_points(budget: &mut Budget<'_>, out: &mut CharRange, a: &[u32], b: &[u32]) -> Result<()> {
    range_op(budget, out, a, b, CharRangeOp::Union)
}

fn range_op(
    budget: &mut Budget<'_>,
    out: &mut CharRange,
    a: &[u32],
    b: &[u32],
    op: CharRangeOp,
) -> Result<()> {
    let endpoints = a
        .len()
        .checked_add(b.len())
        .ok_or(RegExpError::OutOfMemory)?;
    budget.reserve(&mut out.points, endpoints)?;
    cr_op(out, a, b, op);
    Ok(())
}

struct Sequence {
    points: Vec<u32>,
    hash: u32,
    next: Option<usize>,
}

#[derive(Default)]
pub(crate) struct Strings {
    heads: Vec<Option<usize>>,
    entries: Vec<Sequence>,
    count: usize,
}

impl Strings {
    fn hash(points: &[u32]) -> u32 {
        points
            .iter()
            .fold(1u32, |h, &c| h.wrapping_mul(263).wrapping_add(c))
            .wrapping_mul(0x61c88647)
    }

    fn bucket(&self, hash: u32) -> usize {
        (hash >> (32 - self.heads.len().ilog2())) as usize
    }

    fn find(&self, points: &[u32], hash: u32) -> bool {
        if self.count == 0 {
            return false;
        }
        let mut i = self.heads[self.bucket(hash)];
        while let Some(index) = i {
            let entry = &self.entries[index];
            if hash == entry.hash && points == entry.points {
                return true;
            }
            i = entry.next;
        }
        false
    }

    fn contains(&self, points: &[u32]) -> bool {
        self.find(points, Self::hash(points))
    }

    fn add(&mut self, budget: &mut Budget<'_>, points: &[u32]) -> Result<()> {
        let hash = Self::hash(points);
        if self.find(points, hash) {
            return Ok(());
        }
        if self.count == self.heads.len() {
            let mut heads = Vec::new();
            let count = (self.heads.len() * 2).max(16);
            budget.reserve(&mut heads, count)?;
            heads.resize(count, None);
            let old = std::mem::replace(&mut self.heads, heads);
            for &head in &old {
                let mut i = head;
                while let Some(index) = i {
                    let bucket = self.bucket(self.entries[index].hash);
                    i = self.entries[index].next;
                    self.entries[index].next = self.heads[bucket].replace(index);
                }
            }
            budget.free(old);
        }
        let bucket = self.bucket(hash);
        let index = self.entries.len();
        let mut stored = Vec::new();
        budget.extend(&mut stored, points)?;
        let next = self.heads[bucket].replace(index);
        budget.push(
            &mut self.entries,
            Sequence {
                points: stored,
                hash,
                next,
            },
        )?;
        self.count += 1;
        Ok(())
    }

    fn retain(&mut self, budget: &mut Budget<'_>, f: impl Fn(&[u32]) -> bool) {
        for head in &mut self.heads {
            let mut previous: Option<usize> = None;
            let mut i = *head;
            while let Some(index) = i {
                i = self.entries[index].next;
                if f(&self.entries[index].points) {
                    previous = Some(index);
                } else {
                    if let Some(previous) = previous {
                        self.entries[previous].next = i;
                    } else {
                        *head = i;
                    }
                    budget.free(std::mem::take(&mut self.entries[index].points));
                    self.count -= 1;
                }
            }
        }
    }

    fn free(self, budget: &mut Budget<'_>) {
        budget.free(self.heads);
        let mut entries = self.entries;
        for entry in &mut entries {
            budget.free(std::mem::take(&mut entry.points));
        }
        budget.free(entries);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &[u32]> {
        self.heads.iter().flat_map(|&head| {
            std::iter::successors(head, |&i| self.entries[i].next)
                .map(|i| self.entries[i].points.as_slice())
        })
    }
}

const DIGITS: &[u32] = &[0x30, 0x3a];
const SPACE: &[u32] = &[
    9, 14, 0x20, 0x21, 0xa0, 0xa1, 0x1680, 0x1681, 0x2000, 0x200b, 0x2028, 0x202a, 0x202f, 0x2030,
    0x205f, 0x2060, 0x3000, 0x3001, 0xfeff, 0xff00,
];
const WORD: &[u32] = &[0x30, 0x3a, 0x41, 0x5b, 0x5f, 0x60, 0x61, 0x7b];

#[derive(Clone, Copy)]
enum Mode {
    First,
    Union,
    Inter,
    Sub,
}

struct Class {
    set: Set,
    invert: bool,
    mode: Mode,
}

impl<'p> Compiler<'p, '_> {
    pub(crate) fn class(&mut self) -> Result<Set> {
        let mut stack = Vec::new();
        self.open_class(&mut stack)?;
        loop {
            let frame = stack.last_mut().expect("class frame");
            if self.at(0) == b']' {
                self.pos += 1;
                let mut frame = stack.pop().expect("class frame");
                if frame.invert {
                    if !frame.set.strings.is_empty() {
                        return Err(syntax(
                            "negated character class with strings in regular expression debugger eval code",
                        ));
                    }
                    frame.set.invert(&mut self.budget)?;
                }
                if let Some(parent) = stack.last_mut() {
                    self.apply_operand(parent, frame.set)?;
                } else {
                    self.budget.free(stack);
                    return Ok(frame.set);
                }
                continue;
            }
            let operator = match frame.mode {
                Mode::Inter => Some(self.intersection()),
                Mode::Sub => Some(self.at(0) == b'-' && self.at(1) == b'-'),
                _ => None,
            };
            if let Some(found) = operator {
                if !found {
                    return Err(syntax("invalid operation in regular expression"));
                }
                self.pos += 2;
            }
            if self.sets() && self.at(0) == b'[' {
                self.open_class(&mut stack)?;
                continue;
            }
            let atom = self.atom(true, true)?;
            if operator.is_some() {
                let atom = match atom {
                    Atom::Char(c) => {
                        let mut set = Set::default();
                        let c = self.fold(c);
                        set.interval(&mut self.budget, c, c)?;
                        set
                    }
                    Atom::Set(set, _) => set,
                };
                self.apply_operand(frame, atom)?;
                continue;
            }
            if self.at(0) == b'-'
                && self.at(1) != b']'
                && !(self.sets() && self.at(1) == b'-' && matches!(frame.mode, Mode::First))
            {
                if let Atom::Char(low) = atom {
                    let dash = self.pos;
                    self.pos += 1;
                    match self.atom(true, true)? {
                        Atom::Char(high) => {
                            if low > high {
                                return Err(syntax("invalid class range"));
                            }
                            if self.ignore_case() {
                                let unicode = self.unicode();
                                let mut range = Set::default();
                                self.budget
                                    .extend(&mut range.chars.points, &[low, high + 1])?;
                                range
                                    .range(&mut self.budget, |r| r.regexp_canonicalize(unicode))?;
                                frame.set.op(&mut self.budget, range, CharRangeOp::Union)?;
                            } else {
                                frame.set.interval(&mut self.budget, low, high)?;
                            }
                            frame.mode = Mode::Union;
                            continue;
                        }
                        Atom::Set(set, _) => {
                            set.free(&mut self.budget);
                            if self.unicode() {
                                return Err(syntax("invalid class range"));
                            }
                            self.pos = dash;
                        }
                    }
                } else if self.unicode() {
                    return Err(syntax("invalid class range"));
                }
            }
            match atom {
                Atom::Char(c) => {
                    let c = self.fold(c);
                    frame.set.interval(&mut self.budget, c, c)?;
                    self.after_operand(frame);
                }
                Atom::Set(set, _) => self.apply_operand(frame, set)?,
            }
        }
    }

    fn open_class(&mut self, stack: &mut Vec<Class>) -> Result<()> {
        self.check_depth((stack.len() + 1).saturating_mul(size_of::<Class>()))?;
        self.pos += 1;
        let invert = self.eat(b'^');
        self.budget.push(
            stack,
            Class {
                set: Set::default(),
                invert,
                mode: Mode::First,
            },
        )
    }

    fn intersection(&self) -> bool {
        self.at(0) == b'&' && self.at(1) == b'&' && self.at(2) != b'&'
    }

    fn after_operand(&self, frame: &mut Class) {
        if matches!(frame.mode, Mode::First) {
            frame.mode = if self.sets() && self.intersection() {
                Mode::Inter
            } else if self.sets() && self.at(0) == b'-' && self.at(1) == b'-' {
                Mode::Sub
            } else {
                Mode::Union
            };
        }
    }

    fn apply_operand(&mut self, frame: &mut Class, set: Set) -> Result<()> {
        let op = match frame.mode {
            Mode::First | Mode::Union => CharRangeOp::Union,
            Mode::Inter => CharRangeOp::Inter,
            Mode::Sub => CharRangeOp::Sub,
        };
        frame.set.op(&mut self.budget, set, op)?;
        self.after_operand(frame);
        Ok(())
    }

    pub(crate) fn atom(&mut self, in_class: bool, allow_set: bool) -> Result<Atom> {
        let c = self.at(0);
        if self.pos == self.pattern.len() {
            return Err(syntax("unexpected end"));
        }
        match c {
            b'\\' => {
                self.pos += 1;
                if self.pos == self.pattern.len() {
                    return Err(syntax("unexpected end"));
                }
                let escaped = self.at(0);
                let range = match escaped | 32 {
                    b'd' => DIGITS,
                    b's' => SPACE,
                    b'w' => WORD,
                    _ => &[],
                };
                if allow_set && !range.is_empty() {
                    self.pos += 1;
                    let mut set = Set::default();
                    if !in_class && escaped.eq_ignore_ascii_case(&b's') {
                        return Ok(Atom::Set(set, Some(escaped)));
                    }
                    self.budget.extend(&mut set.chars.points, range)?;
                    if escaped.is_ascii_uppercase() {
                        set.invert(&mut self.budget)?;
                    }
                    return Ok(Atom::Set(set, None));
                }
                match escaped {
                    b'c' => {
                        let next = self.at(1);
                        if next.is_ascii_alphabetic()
                            || (in_class
                                && !self.unicode()
                                && (next.is_ascii_digit() || next == b'_'))
                        {
                            self.pos += 2;
                            return Ok(Atom::Char(u32::from(next & 31)));
                        }
                        if !self.unicode() {
                            return Ok(Atom::Char(u32::from(b'\\')));
                        }
                    }
                    b'-' if in_class || !self.unicode() => {
                        self.pos += 1;
                        return Ok(Atom::Char(u32::from(escaped)));
                    }
                    b'&' | b'!' | b'#' | b'%' | b',' | b':' | b';' | b'<' | b'=' | b'>' | b'@'
                    | b'`' | b'~'
                        if !self.unicode() || (in_class && self.sets()) =>
                    {
                        self.pos += 1;
                        return Ok(Atom::Char(u32::from(escaped)));
                    }
                    b'^' | b'$' | b'\\' | b'.' | b'*' | b'+' | b'?' | b'(' | b')' | b'[' | b']'
                    | b'{' | b'}' | b'|' | b'/' => {
                        self.pos += 1;
                        return Ok(Atom::Char(u32::from(escaped)));
                    }
                    b'p' | b'P' if self.unicode() && allow_set => {
                        self.pos += 1;
                        return Ok(Atom::Set(self.property(escaped == b'P')?, None));
                    }
                    b'q' if self.sets() && allow_set && in_class => {
                        self.pos += 1;
                        return Ok(Atom::Set(self.string_disjunction()?, None));
                    }
                    _ => {}
                }
                if let Ok((point, used)) =
                    parse_escape(&self.pattern[self.pos..], u8::from(self.unicode()) * 2)
                {
                    self.pos += used;
                    return Ok(Atom::Char(point));
                }
                if self.unicode() {
                    return Err(syntax("invalid escape sequence in regular expression"));
                }
            }
            b'&' | b'!' | b'#' | b'$' | b'%' | b'*' | b'+' | b',' | b'.' | b':' | b';' | b'<'
            | b'=' | b'>' | b'?' | b'@' | b'^' | b'`' | b'~'
                if self.sets() && self.at(1) == c =>
            {
                return Err(syntax("invalid class set operation in regular expression"));
            }
            b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'/' | b'-' | b'|' if self.sets() => {
                return Err(syntax("invalid character in class in regular expression"));
            }
            _ => {}
        }
        let (point, used) = wtf8(&self.pattern[self.pos..]);
        self.pos += used;
        if !self.unicode() && point > 0xffff {
            return Err(syntax("malformed unicode char"));
        }
        Ok(Atom::Char(point))
    }

    fn property(&mut self, invert: bool) -> Result<Set> {
        if !self.eat(b'{') {
            return Err(syntax("expecting '{' after \\p"));
        }
        let name = self.property_word("unknown unicode property name")?;
        let value = if self.eat(b'=') {
            self.property_word("unknown unicode property value")?
        } else {
            ""
        };
        self.expect(b'}')?;
        let mut set = Set::default();
        let mut error = "unknown unicode property name";
        let result = match name {
            "Script" | "sc" | "Script_Extensions" | "scx" => {
                error = "unknown unicode script";
                qjs_unicode::unicode_script(
                    &mut set.chars,
                    value,
                    matches!(name, "Script_Extensions" | "scx"),
                )
            }
            "General_Category" | "gc" => {
                error = "unknown unicode general category";
                qjs_unicode::unicode_general_category(&mut set.chars, value)
            }
            _ if value.is_empty() => {
                let mut result = qjs_unicode::unicode_general_category(&mut set.chars, name);
                if result == Err(UnicodeError::InvalidProperty) {
                    result = qjs_unicode::unicode_prop(&mut set.chars, name);
                }
                if result == Err(UnicodeError::InvalidProperty) && !invert && self.sets() {
                    self.budget
                        .charge(set.chars.points.capacity() * size_of::<u32>())?;
                    let mut scratch = CharRange::new();
                    let mut failure = None;
                    let result = qjs_unicode::unicode_sequence_prop(
                        name,
                        &mut |seq| {
                            if failure.is_none() {
                                failure = set.add(&mut self.budget, seq).err();
                            }
                        },
                        &mut scratch,
                    );
                    let scratch_bytes = scratch.points.capacity() * size_of::<u32>();
                    self.budget.charge(scratch_bytes)?;
                    drop(scratch);
                    self.budget.release(scratch_bytes);
                    if let Some(err) = failure {
                        return Err(err);
                    }
                    return self.finish_property(set, result, error, invert);
                }
                result
            }
            _ => Err(UnicodeError::InvalidProperty),
        };
        self.budget
            .charge(set.chars.points.capacity() * size_of::<u32>())?;
        self.finish_property(set, result, error, invert)
    }

    fn finish_property(
        &mut self,
        mut set: Set,
        result: std::result::Result<(), UnicodeError>,
        error: &str,
        invert: bool,
    ) -> Result<Set> {
        result.map_err(|e| match e {
            UnicodeError::InvalidProperty => syntax(error),
            UnicodeError::OutOfMemory => RegExpError::OutOfMemory,
        })?;
        if self.ignore_case() && self.sets() {
            set.canonicalize(&mut self.budget, true)?;
        }
        if invert {
            set.invert(&mut self.budget)?;
        }
        if self.ignore_case() && !self.sets() {
            let unicode = self.unicode();
            set.canonicalize(&mut self.budget, unicode)?;
        }
        Ok(set)
    }

    fn property_word(&mut self, error: &str) -> Result<&'p str> {
        let start = self.pos;
        while self.at(0).is_ascii_alphanumeric() || self.at(0) == b'_' {
            if self.pos - start == 63 {
                return Err(syntax(error));
            }
            self.pos += 1;
        }
        Ok(str::from_utf8(&self.pattern[start..self.pos]).expect("ASCII property"))
    }

    fn string_disjunction(&mut self) -> Result<Set> {
        if !self.eat(b'{') {
            return Err(syntax("expecting '{' after \\q"));
        }
        let mut set = Set::default();
        let mut seq = Vec::new();
        loop {
            seq.clear();
            while !matches!(self.at(0), b'}' | b'|') {
                let Atom::Char(c) = self.atom(true, false)? else {
                    unreachable!()
                };
                self.budget.push(&mut seq, c)?;
            }
            set.add(&mut self.budget, &seq)?;
            if self.eat(b'}') {
                break;
            }
            self.pos += 1;
        }
        self.budget.free(seq);
        if self.ignore_case() {
            set.canonicalize(&mut self.budget, true)?;
        }
        Ok(set)
    }
}
