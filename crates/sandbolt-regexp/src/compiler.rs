use crate::alloc::Budget;
use crate::bytecode::{CompiledPlan, HEADER, Header, Op, atom_effects, read32, register_count};
use crate::class::{Atom, Set};
use crate::escape::{is_high, is_low, join_surrogates, parse_escape, wtf8};
use crate::{CompiledRegExp, RegExpError, RegExpFlags, RegExpHost, canonicalize};
use std::mem::size_of;

pub(crate) type Result<T> = std::result::Result<T, RegExpError>;
const MAX_REPEAT: u32 = i32::MAX as u32;

pub(crate) fn syntax(message: &str) -> RegExpError {
    RegExpError::Syntax(message.into())
}

pub(crate) struct Compiler<'p, 'h> {
    pub(crate) pattern: &'p [u8],
    pub(crate) pos: usize,
    pub(crate) budget: Budget<'h>,
    code: Vec<u8>,
    names: Vec<u8>,
    captures: u8,
    scope: u8,
    total: Option<(u8, bool)>,
    named: bool,
    flags: RegExpFlags,
    mods: u16,
    group_depth: usize,
}

#[derive(Clone, Copy)]
struct Term {
    start: usize,
    capture: u8,
}

#[derive(Clone, Copy)]
enum Group {
    Root,
    Plain {
        term: Term,
        mods: u16,
    },
    Capture {
        term: Term,
        backward: bool,
    },
    Look {
        jump: usize,
        neg: bool,
        repeat: Option<Term>,
    },
}

struct Frame {
    start: usize,
    alt: usize,
    backward: bool,
    group: Group,
}

pub(crate) fn compile(
    pattern: &[u8],
    flags: RegExpFlags,
    host: &mut dyn RegExpHost,
) -> Result<CompiledRegExp> {
    let flags = RegExpFlags(flags.0 & !RegExpFlags::NAMED_GROUPS);
    let mut c = Compiler {
        pattern,
        pos: 0,
        budget: Budget::new(host),
        code: Vec::new(),
        names: Vec::new(),
        captures: 1,
        scope: 0,
        total: None,
        named: false,
        flags,
        mods: flags.0,
        group_depth: 0,
    };
    c.emit(&[0; HEADER])?;
    if !flags.contains(RegExpFlags::STICKY) {
        let loop_at = c.code.len();
        let split = c.arg32(Op::SplitGoto, 0)?;
        c.op(Op::Any)?;
        c.branch(Op::Goto, loop_at)?;
        c.patch(split, c.code.len());
    }
    c.arg8(Op::SaveStart, 0)?;
    c.parse()?;
    c.arg8(Op::SaveEnd, 0)?;
    c.op(Op::Match)?;
    if c.at(0) != 0 {
        return Err(syntax("extraneous characters at the end"));
    }
    let registers = register_count(&mut c.code[HEADER..])
        .ok_or_else(|| syntax("too many imbricated quantifiers"))?;
    c.code[2] = c.captures;
    c.code[3] = registers;
    let body_len = (c.code.len() - HEADER) as u32;
    c.code[4..8].copy_from_slice(&body_len.to_ne_bytes());
    if c.named {
        c.check_size(c.names.len())?;
        c.budget.extend(&mut c.code, &c.names)?;
        c.flags.0 |= RegExpFlags::NAMED_GROUPS;
    }
    c.code[..2].copy_from_slice(&c.flags.0.to_ne_bytes());
    let h = Header::parse(&c.code).expect("emitted header");
    let group_names = collect_names(&h, &mut c.budget)?;
    let plan = CompiledPlan::from_validated(&c.code, &h);
    Ok(CompiledRegExp {
        bytecode: c.code,
        flags: c.flags,
        capture_count: u16::from(c.captures),
        group_names,
        plan,
    })
}

pub(crate) fn collect_names(
    h: &Header<'_>,
    budget: &mut Budget<'_>,
) -> Result<Vec<Option<String>>> {
    let mut names = Vec::new();
    budget.reserve(&mut names, h.captures)?;
    for name in h.names() {
        let name = if let Some(name) = name? {
            let mut bytes = Vec::new();
            budget.extend(&mut bytes, name.as_bytes())?;
            Some(String::from_utf8(bytes).expect("validated group name"))
        } else {
            None
        };
        names.push(name);
    }
    Ok(names)
}

impl Compiler<'_, '_> {
    pub(crate) fn at(&self, offset: usize) -> u8 {
        self.pattern.get(self.pos + offset).copied().unwrap_or(0)
    }

    pub(crate) fn eat(&mut self, byte: u8) -> bool {
        if self.at(0) != byte {
            return false;
        }
        self.pos += 1;
        true
    }

    pub(crate) fn expect(&mut self, byte: u8) -> Result<()> {
        self.eat(byte)
            .then_some(())
            .ok_or_else(|| syntax(&format!("expecting '{}'", byte as char)))
    }

    pub(crate) fn unicode(&self) -> bool {
        self.flags
            .contains(RegExpFlags::UNICODE | RegExpFlags::UNICODE_SETS)
    }

    pub(crate) fn sets(&self) -> bool {
        self.flags.contains(RegExpFlags::UNICODE_SETS)
    }

    pub(crate) fn ignore_case(&self) -> bool {
        self.mods & RegExpFlags::IGNORECASE != 0
    }

    pub(crate) fn fold(&self, c: u32) -> u32 {
        if self.ignore_case() {
            canonicalize(c, self.unicode())
        } else {
            c
        }
    }

    pub(crate) fn check_depth(&mut self, extra_bytes: usize) -> Result<()> {
        let depth = self
            .group_depth
            .saturating_mul(size_of::<Frame>())
            .saturating_add(extra_bytes);
        if self.budget.host.check_stack(depth) {
            Ok(())
        } else {
            Err(RegExpError::StackOverflow)
        }
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<()> {
        self.check_size(bytes.len())?;
        self.budget.extend(&mut self.code, bytes)
    }

    fn check_size(&self, extra: usize) -> Result<()> {
        if self.code.len().saturating_add(extra) > MAX_REPEAT as usize / 2 {
            Err(RegExpError::OutOfMemory)
        } else {
            Ok(())
        }
    }

    fn insert(&mut self, pos: usize, bytes: &[u8]) -> Result<()> {
        self.check_size(bytes.len())?;
        let old = self.code.len();
        self.budget.reserve(&mut self.code, bytes.len())?;
        self.code.resize(old + bytes.len(), 0);
        self.code.copy_within(pos..old, pos + bytes.len());
        self.code[pos..pos + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn op(&mut self, op: Op) -> Result<()> {
        self.emit(&[op as u8])
    }

    fn arg8(&mut self, op: Op, arg: u8) -> Result<()> {
        self.emit(&[op as u8, arg])
    }

    fn arg32(&mut self, op: Op, arg: u32) -> Result<usize> {
        let pos = self.code.len() + 1;
        self.op(op)?;
        self.emit(&arg.to_ne_bytes())?;
        Ok(pos)
    }

    fn patch(&mut self, operand: usize, target: usize) {
        let rel = (target as u32).wrapping_sub(operand as u32 + 4);
        self.code[operand..operand + 4].copy_from_slice(&rel.to_ne_bytes());
    }

    fn branch(&mut self, op: Op, target: usize) -> Result<()> {
        let patch = self.arg32(op, 0)?;
        self.patch(patch, target);
        Ok(())
    }

    fn char(&mut self, c: u32) -> Result<()> {
        if let Ok(c) = u16::try_from(c) {
            self.op(if self.ignore_case() {
                Op::CharI
            } else {
                Op::Char
            })?;
            self.emit(&c.to_ne_bytes())
        } else {
            self.arg32(
                if self.ignore_case() {
                    Op::Char32I
                } else {
                    Op::Char32
                },
                c,
            )?;
            Ok(())
        }
    }

    fn range(&mut self, points: &[u32]) -> Result<()> {
        let count = points.len() / 2;
        if count >= 65535 {
            return Err(syntax("too many ranges"));
        }
        if count == 0 {
            self.arg32(Op::Char32, u32::MAX)?;
            return Ok(());
        }
        let mut high = points[points.len() - 1];
        if high == u32::MAX {
            high = points[points.len() - 2];
        }
        let wide = high > 0xffff;
        self.op(match (wide, self.ignore_case()) {
            (false, false) => Op::Range,
            (false, true) => Op::RangeI,
            (true, false) => Op::Range32,
            (true, true) => Op::Range32I,
        })?;
        self.emit(&(count as u16).to_ne_bytes())?;
        for interval in points.chunks_exact(2) {
            let [low, high] = [interval[0], interval[1] - 1];
            if wide {
                self.emit(&low.to_ne_bytes())?;
                self.emit(&high.to_ne_bytes())?;
            } else {
                let high = if high == u32::MAX - 1 {
                    0xffff
                } else {
                    high as u16
                };
                self.emit(&(low as u16).to_ne_bytes())?;
                self.emit(&high.to_ne_bytes())?;
            }
        }
        Ok(())
    }

    fn emit_set(&mut self, set: Set) -> Result<()> {
        if set.strings.is_empty() {
            self.range(&set.chars.points)?;
        } else {
            let mut tab = Vec::new();
            let mut empty = false;
            for seq in set.strings.iter() {
                if seq.is_empty() {
                    empty = true;
                } else {
                    self.budget.push(&mut tab, seq)?;
                }
            }
            sort_by_length(&mut tab);
            let mut last = u32::MAX;
            for (i, seq) in tab.iter().enumerate() {
                let split = if !empty && set.chars.is_empty() && i + 1 == tab.len() {
                    None
                } else {
                    Some(self.arg32(Op::SplitNext, 0)?)
                };
                for &ch in *seq {
                    self.char(ch)?;
                }
                if let Some(split) = split {
                    last = self.arg32(Op::Goto, last)? as u32;
                    self.patch(split, self.code.len());
                }
            }
            if !set.chars.is_empty() {
                let split = empty.then(|| self.arg32(Op::SplitNext, 0)).transpose()?;
                self.range(&set.chars.points)?;
                if let Some(split) = split {
                    self.patch(split, self.code.len());
                }
            }
            while last != u32::MAX {
                let pos = last as usize;
                last = read32(&self.code, pos);
                self.patch(pos, self.code.len());
            }
            self.budget.free(tab);
        }
        set.free(&mut self.budget);
        Ok(())
    }

    fn open(&mut self, frames: &mut Vec<Frame>, backward: bool, group: Group) -> Result<()> {
        self.group_depth = frames.len() + 1;
        self.check_depth(0)?;
        let start = self.code.len();
        self.budget.push(
            frames,
            Frame {
                start,
                alt: start,
                backward,
                group,
            },
        )
    }

    fn parse(&mut self) -> Result<()> {
        let mut frames = Vec::new();
        self.open(&mut frames, false, Group::Root)?;
        loop {
            let frame = frames.last_mut().expect("group frame");
            if self.eat(b'|') {
                self.patch_alternative(frame);
                let len = self.code.len() - frame.start;
                let mut split = [0; 5];
                split[0] = Op::SplitNext as u8;
                split[1..].copy_from_slice(&((len + 5) as u32).to_ne_bytes());
                self.insert(frame.start, &split)?;
                let end_jump = self.arg32(Op::Goto, u32::MAX)?;
                // The next alternative owns the pending jump immediately before it.
                if self.has_named() && self.scope == u8::MAX {
                    return Err(syntax("too many named groups"));
                }
                self.scope = self.scope.wrapping_add(1);
                frame.alt = end_jump + 4;
                continue;
            }
            if self.pos == self.pattern.len() || self.at(0) == b')' {
                self.patch_alternative(frame);
                if matches!(frame.group, Group::Root) {
                    self.budget.free(frames);
                    return Ok(());
                }
                let group = frame.group;
                frames.pop();
                self.group_depth = frames.len();
                let parent = frames.last().expect("parent frame");
                let term = match group {
                    Group::Plain { term, mods } => {
                        self.expect(b')')?;
                        self.mods = mods;
                        Some(term)
                    }
                    Group::Capture { term, backward } => {
                        self.arg8(
                            if backward { Op::SaveStart } else { Op::SaveEnd },
                            term.capture,
                        )?;
                        self.expect(b')')?;
                        Some(term)
                    }
                    Group::Look { jump, neg, repeat } => {
                        self.expect(b')')?;
                        self.op(if neg { Op::NegLookMatch } else { Op::LookMatch })?;
                        self.patch(jump, self.code.len());
                        repeat
                    }
                    Group::Root => unreachable!(),
                };
                let start = match group {
                    Group::Plain { term, .. } | Group::Capture { term, .. } => term.start,
                    Group::Look { jump, .. } => jump - 1,
                    Group::Root => unreachable!(),
                };
                self.finish_term(parent, start, term)?;
                continue;
            }
            let start = self.code.len();
            let term = Term {
                start,
                capture: self.captures,
            };
            if self.eat(b'(') {
                let backward = frame.backward;
                let (group, direction) = self.group(term, backward)?;
                self.open(&mut frames, direction, group)?;
            } else {
                let repeatable = self.term(frame.backward)?;
                self.finish_term(frame, start, repeatable.then_some(term))?;
            }
        }
    }

    fn patch_alternative(&mut self, frame: &Frame) {
        if frame.alt != frame.start {
            self.patch(frame.alt - 4, self.code.len());
        }
    }

    fn finish_term(&mut self, frame: &Frame, start: usize, term: Option<Term>) -> Result<()> {
        if let Some(term) = term {
            self.quantifier(term)?;
        }
        if frame.backward {
            let len = self.code.len() - start;
            self.code[frame.alt..].rotate_right(len);
        }
        Ok(())
    }

    fn group(&mut self, term: Term, backward: bool) -> Result<(Group, bool)> {
        let mut name = [0; 128];
        let len = if !self.eat(b'?') {
            0
        } else {
            match self.at(0) {
                b':' => {
                    self.pos += 1;
                    return Ok((
                        Group::Plain {
                            term,
                            mods: self.mods,
                        },
                        backward,
                    ));
                }
                b'i' | b'm' | b's' | b'-' => {
                    let add = self.modifiers()?;
                    let remove = if self.eat(b'-') { self.modifiers()? } else { 0 };
                    if add | remove == 0 || add & remove != 0 {
                        return Err(syntax("invalid modifiers"));
                    }
                    self.expect(b':')?;
                    let mods = self.mods;
                    self.mods = (mods | add) & !remove;
                    return Ok((Group::Plain { term, mods }, backward));
                }
                b'=' | b'!' => return self.look(term, false),
                b'<' if matches!(self.at(1), b'=' | b'!') => {
                    self.pos += 1;
                    return self.look(term, true);
                }
                b'<' => {
                    self.pos += 1;
                    let (end, len) = group_name(self.pattern, self.pos, &mut name)
                        .ok_or_else(|| syntax("invalid group name"))?;
                    self.pos = end;
                    if self
                        .known_names()
                        .any(|(n, scope)| n == &name[..len] && scope == self.scope)
                    {
                        return Err(syntax("duplicate group name"));
                    }
                    self.named = true;
                    len
                }
                _ => return Err(syntax("invalid group")),
            }
        };
        if self.captures == u8::MAX {
            return Err(syntax("too many captures"));
        }
        self.budget.extend(&mut self.names, &name[..len])?;
        self.budget
            .extend(&mut self.names, &[0, if len != 0 { self.scope } else { 0 }])?;
        self.arg8(
            if backward { Op::SaveEnd } else { Op::SaveStart },
            self.captures,
        )?;
        self.captures += 1;
        Ok((Group::Capture { term, backward }, backward))
    }

    fn look(&mut self, term: Term, backward: bool) -> Result<(Group, bool)> {
        let neg = self.at(0) == b'!';
        self.pos += 1;
        let jump = self.arg32(if neg { Op::NegLook } else { Op::Look }, 0)?;
        let repeat = (!backward && !self.unicode()).then_some(term);
        Ok((Group::Look { jump, neg, repeat }, backward))
    }

    fn modifiers(&mut self) -> Result<u16> {
        let mut mask = 0;
        loop {
            let value = match self.at(0) {
                b'i' => RegExpFlags::IGNORECASE,
                b'm' => RegExpFlags::MULTILINE,
                b's' => RegExpFlags::DOTALL,
                _ => return Ok(mask),
            };
            if mask & value != 0 {
                return Err(syntax(&format!(
                    "duplicate modifier: '{}'",
                    self.at(0) as char
                )));
            }
            self.pos += 1;
            mask |= value;
        }
    }

    fn term(&mut self, backward: bool) -> Result<bool> {
        let atom = match self.at(0) {
            b'^' | b'$' => {
                let multi = self.mods & RegExpFlags::MULTILINE != 0;
                let start = self.at(0) == b'^';
                self.pos += 1;
                self.op(match (start, multi) {
                    (true, false) => Op::LineStart,
                    (true, true) => Op::LineStartM,
                    (false, false) => Op::LineEnd,
                    (false, true) => Op::LineEndM,
                })?;
                return Ok(false);
            }
            b'.' => {
                self.pos += 1;
                self.prev(backward)?;
                self.op(if self.mods & RegExpFlags::DOTALL != 0 {
                    Op::Any
                } else {
                    Op::Dot
                })?;
                self.prev(backward)?;
                return Ok(true);
            }
            b'{' if !self.unicode() && !self.looks_like_repeat() => self.atom(false, true)?,
            b'{' | b'}' | b']' if self.unicode() => return Err(syntax("syntax error")),
            b'{' | b'*' | b'+' | b'?' => return Err(syntax("nothing to repeat")),
            b'[' => {
                self.prev(backward)?;
                let set = self.class()?;
                self.emit_set(set)?;
                self.prev(backward)?;
                return Ok(true);
            }
            b'\\' => match self.at(1) {
                b'b' | b'B' => {
                    self.op(
                        match (self.at(1) == b'b', self.ignore_case() && self.unicode()) {
                            (true, true) => Op::WordBoundaryI,
                            (true, false) => Op::WordBoundary,
                            (false, true) => Op::NotWordBoundaryI,
                            (false, false) => Op::NotWordBoundary,
                        },
                    )?;
                    self.pos += 2;
                    return Ok(false);
                }
                b'k' if self.named_ref(backward)? => return Ok(true),
                b'0' => {
                    self.pos += 2;
                    if self.unicode() && self.at(0).is_ascii_digit() {
                        return Err(syntax("invalid decimal escape in regular expression"));
                    }
                    let mut c = 0;
                    if !self.unicode() {
                        for _ in 0..2 {
                            if !matches!(self.at(0), b'0'..=b'7') {
                                break;
                            }
                            c = (c << 3) | u32::from(self.at(0) - b'0');
                            self.pos += 1;
                        }
                    }
                    Atom::Char(c)
                }
                b'1'..=b'9' => {
                    self.pos += 1;
                    if let Some((c, next)) = digits(self.pattern, self.pos, false)
                        && (c < u32::from(self.captures) || c < u32::from(self.total().0))
                    {
                        self.pos = next;
                        self.ref_op(backward, &[c as u8])?;
                        return Ok(true);
                    }
                    if self.unicode() {
                        return Err(syntax("back reference out of range in regular expression"));
                    }
                    if let Ok((c, len)) = parse_escape(&self.pattern[self.pos..], 0) {
                        self.pos += len;
                        Atom::Char(c)
                    } else {
                        let c = self.at(0);
                        self.pos += 1;
                        Atom::Char(u32::from(c))
                    }
                }
                _ => self.atom(false, true)?,
            },
            _ => self.atom(false, true)?,
        };
        self.prev(backward)?;
        match atom {
            Atom::Char(c) => self.char(self.fold(c))?,
            Atom::Set(set, Some(space)) => {
                self.op(if space == b's' {
                    Op::Space
                } else {
                    Op::NotSpace
                })?;
                set.free(&mut self.budget);
            }
            Atom::Set(set, None) => self.emit_set(set)?,
        }
        self.prev(backward)?;
        Ok(true)
    }

    fn prev(&mut self, backward: bool) -> Result<()> {
        if backward {
            self.op(Op::Prev)?;
        }
        Ok(())
    }

    fn known_names(&self) -> impl Iterator<Item = (&[u8], u8)> {
        let mut names = self.names.as_slice();
        std::iter::from_fn(move || {
            let len = names.iter().position(|&b| b == 0)?;
            let pair = (&names[..len], names[len + 1]);
            names = &names[len + 2..];
            Some(pair)
        })
    }

    fn named_ref(&mut self, backward: bool) -> Result<bool> {
        let mut name = [0; 128];
        let parsed = if self.at(2) == b'<' {
            group_name(self.pattern, self.pos + 3, &mut name).ok_or("invalid group name")
        } else {
            Err("expecting group name")
        };
        let mut refs = [0; 254];
        let mut count = 0;
        let error = match parsed {
            Ok((pos, len)) => {
                let name = &name[..len];
                for (i, (known, _)) in self.known_names().enumerate() {
                    if known == name {
                        refs[count] = (i + 1) as u8;
                        count += 1;
                    }
                }
                if count == 0 {
                    count = scan_captures(self.pattern, Some((name, &mut refs))).2;
                }
                if count != 0 {
                    self.pos = pos;
                    self.ref_op(backward, &refs[..count])?;
                    return Ok(true);
                }
                "group name not defined"
            }
            Err(error) => error,
        };
        if self.unicode() || self.has_named() {
            Err(syntax(error))
        } else {
            Ok(false)
        }
    }

    fn ref_op(&mut self, backward: bool, refs: &[u8]) -> Result<()> {
        self.arg8(
            match (backward, self.ignore_case()) {
                (false, false) => Op::BackRef,
                (false, true) => Op::BackRefI,
                (true, false) => Op::BackwardRef,
                (true, true) => Op::BackwardRefI,
            },
            refs.len() as u8,
        )?;
        self.emit(refs)
    }

    fn total(&mut self) -> (u8, bool) {
        *self.total.get_or_insert_with(|| {
            let (count, named, _) = scan_captures(self.pattern, None);
            (count, named)
        })
    }

    fn has_named(&mut self) -> bool {
        self.named || self.total().1
    }

    fn looks_like_repeat(&self) -> bool {
        if !self.at(1).is_ascii_digit() {
            return false;
        }
        let (_, mut next) = digits(self.pattern, self.pos + 1, true).expect("saturating");
        if self.pattern.get(next) == Some(&b',') {
            (_, next) = digits(self.pattern, next + 1, true).expect("saturating");
        }
        self.pattern.get(next) == Some(&b'}')
    }

    fn repeat(&mut self) -> Result<Option<(u32, u32)>> {
        let pair = match self.at(0) {
            b'*' => (0, MAX_REPEAT),
            b'+' => (1, MAX_REPEAT),
            b'?' => (0, 1),
            b'{' => {
                let start = self.pos;
                if !self.at(1).is_ascii_digit() {
                    return if self.unicode() {
                        Err(syntax("invalid repetition count"))
                    } else {
                        Ok(None)
                    };
                }
                let (min, next) = digits(self.pattern, start + 1, true).expect("saturating");
                self.pos = next;
                let mut max = min;
                if self.eat(b',') {
                    if self.at(0).is_ascii_digit() {
                        (max, self.pos) = digits(self.pattern, self.pos, true).expect("saturating");
                        if max < min {
                            return Err(syntax("invalid repetition count"));
                        }
                    } else {
                        max = MAX_REPEAT;
                    }
                }
                if self.at(0) != b'}' && !self.unicode() {
                    self.pos = start;
                    return Ok(None);
                }
                self.expect(b'}')?;
                return Ok(Some((min, max)));
            }
            _ => return Ok(None),
        };
        self.pos += 1;
        Ok(Some(pair))
    }

    fn quantifier(&mut self, term: Term) -> Result<()> {
        let Some((min, max)) = self.repeat()? else {
            return Ok(());
        };
        let greedy = !self.eat(b'?');
        let mut start = term.start;
        let (mut check, reset_each) = atom_effects(&self.code[start..]);
        let has_captures = term.capture != self.captures;
        let reset = [Op::SaveReset as u8, term.capture, self.captures - 1];
        if reset_each && has_captures {
            self.insert(start, &reset)?;
        }
        if min == 0 {
            if !reset_each && has_captures {
                self.insert(start, &reset)?;
                start += reset.len();
            }
            if max == 0 {
                self.code.truncate(start);
                return Ok(());
            }
            let split = if greedy { Op::SplitNext } else { Op::SplitGoto };
            self.insert(start, &[split as u8, 0, 0, 0, 0])?;
            let loop_entry = start;
            start += 5;
            if max == 1 || max == MAX_REPEAT {
                if check {
                    self.insert(start, &[Op::SetCharPos as u8, 0])?;
                    self.arg8(Op::CheckAdvance, 0)?;
                }
                if max == MAX_REPEAT {
                    self.branch(Op::Goto, loop_entry)?;
                }
            } else {
                self.counted(start, max, max, greedy, check, false)?;
            }
            self.patch(loop_entry + 1, self.code.len());
        } else if min == 1 && max == MAX_REPEAT && !check {
            self.branch(if greedy { Op::SplitGoto } else { Op::SplitNext }, start)?;
        } else {
            check &= min != max;
            self.counted(start, max, max - min, greedy, check, min == max)?;
        }
        Ok(())
    }

    fn counted(
        &mut self,
        start: usize,
        count: u32,
        limit: u32,
        greedy: bool,
        check: bool,
        fixed: bool,
    ) -> Result<()> {
        let mut init = [0; 8];
        init[0] = Op::SetI32 as u8;
        init[2..6].copy_from_slice(&count.to_ne_bytes());
        init[6] = Op::SetCharPos as u8;
        self.insert(start, &init[..if check { 8 } else { 6 }])?;
        let op = match (fixed, check, greedy) {
            (true, _, _) => Op::Loop,
            (false, true, true) => Op::LoopAdvGoto,
            (false, true, false) => Op::LoopAdvNext,
            (false, false, true) => Op::LoopSplitGoto,
            (false, false, false) => Op::LoopSplitNext,
        };
        self.arg8(op, 0)?;
        if !fixed {
            self.emit(&limit.to_ne_bytes())?;
        }
        let operand = self.code.len();
        self.emit(&[0; 4])?;
        self.patch(operand, start + 6);
        Ok(())
    }
}

fn digits(pattern: &[u8], mut pos: usize, saturate: bool) -> Option<(u32, usize)> {
    let mut value = 0u64;
    while let Some(&d @ b'0'..=b'9') = pattern.get(pos) {
        value = value * 10 + u64::from(d - b'0');
        if value >= u64::from(MAX_REPEAT) {
            if !saturate {
                return None;
            }
            value = u64::from(MAX_REPEAT);
        }
        pos += 1;
    }
    Some((value as u32, pos))
}

fn group_name(bytes: &[u8], mut pos: usize, name: &mut [u8; 128]) -> Option<(usize, usize)> {
    let mut len = 0;
    loop {
        let c = match *bytes.get(pos)? {
            b'>' => return (len != 0).then_some((pos + 1, len)),
            b'\\' => {
                pos += 1;
                if bytes.get(pos) != Some(&b'u') {
                    return None;
                }
                let (c, used) = parse_escape(&bytes[pos..], 2).ok()?;
                pos += used;
                c
            }
            _ => {
                let (mut c, used) = wtf8(&bytes[pos..]);
                pos += used;
                if is_high(c) {
                    let (low, used) = wtf8(&bytes[pos..]);
                    if is_low(low) {
                        c = join_surrogates(c, low);
                        pos += used;
                    }
                }
                c
            }
        };
        let valid = if c < 128 {
            let b = c as u8;
            b.is_ascii_alphabetic() || matches!(b, b'$' | b'_') || (len != 0 && b.is_ascii_digit())
        } else if len == 0 {
            sandbolt_unicode::is_js_ident_start(c)
        } else {
            sandbolt_unicode::is_js_ident_continue(c)
        };
        if !valid || len + 5 > name.len() {
            return None;
        }
        len += char::from_u32(c)?.encode_utf8(&mut name[len..]).len();
    }
}

fn scan_captures(pattern: &[u8], mut lookup: Option<(&[u8], &mut [u8; 254])>) -> (u8, bool, usize) {
    let at = |i| pattern.get(i).copied().unwrap_or(0);
    let mut pos = 0;
    let mut capture = 1;
    let mut named = false;
    let mut found = 0;
    let mut name = [0; 128];
    while pos < pattern.len() {
        match at(pos) {
            b'(' if at(pos + 1) != b'?' => capture += 1,
            b'(' if at(pos + 2) == b'<' && !matches!(at(pos + 3), b'=' | b'!') => {
                named = true;
                if let Some((target, refs)) = &mut lookup {
                    pos += 3;
                    if let Some((end, len)) = group_name(pattern, pos, &mut name) {
                        pos = end;
                        if *target == &name[..len] {
                            refs[found] = capture;
                            found += 1;
                        }
                    }
                }
                capture += 1;
            }
            b'\\' => pos += 1,
            b'[' => {
                pos += 1;
                while pos < pattern.len() && at(pos) != b']' {
                    if at(pos) == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
            }
            _ => {}
        }
        if capture == u8::MAX {
            break;
        }
        pos += 1;
    }
    (capture, named, found)
}

fn sort_by_length(tab: &mut [&[u32]]) {
    // Preserve libregexp's order for equal-length alternatives in the bytecode.
    fn sift(tab: &mut [&[u32]], mut root: usize) {
        loop {
            let mut child = root * 2 + 1;
            if child >= tab.len() {
                break;
            }
            if child + 1 < tab.len() && tab[child].len() >= tab[child + 1].len() {
                child += 1;
            }
            if tab[root].len() < tab[child].len() {
                break;
            }
            tab.swap(root, child);
            root = child;
        }
    }

    fn heap_sort(tab: &mut [&[u32]]) {
        let n = tab.len();
        for root in (0..n / 2).rev() {
            sift(tab, root);
        }
        for i in (1..n).rev() {
            tab.swap(0, i);
            sift(&mut tab[..i], 0);
        }
    }

    fn sort(tab: &mut [&[u32]], depth: u8) {
        let n = tab.len();
        if n <= 6 {
            for i in 1..n {
                let mut j = i;
                while j != 0 && tab[j - 1].len() < tab[j].len() {
                    tab.swap(j - 1, j);
                    j -= 1;
                }
            }
            return;
        }
        if depth >= 50 {
            heap_sort(tab);
            return;
        }
        let q = n / 4;
        let (a, b, c) = (tab[q].len(), tab[2 * q].len(), tab[3 * q].len());
        let median = if a > b {
            if b > c {
                2
            } else if a > c {
                3
            } else {
                1
            }
        } else if b < c {
            2
        } else if a > c {
            1
        } else {
            3
        };
        tab.swap(0, median * q);
        let pivot = tab[0].len();
        let (mut left, mut i, mut j, mut right) = (1, 1, n, n);
        loop {
            while i < j && pivot <= tab[i].len() {
                if pivot == tab[i].len() {
                    tab.swap(left, i);
                    left += 1;
                }
                i += 1;
            }
            j -= 1;
            while i < j && pivot >= tab[j].len() {
                if pivot == tab[j].len() {
                    right -= 1;
                    tab.swap(right, j);
                }
                j -= 1;
            }
            if i >= j {
                break;
            }
            tab.swap(i, j);
            i += 1;
        }
        let span = left.min(i - left);
        for k in 0..span {
            tab.swap(k, i - span + k);
        }
        let span = (n - right).min(right - i);
        for k in 0..span {
            tab.swap(i + k, n - span + k);
        }
        let (before, after) = (i - left, n - (right - i));
        sort(&mut tab[..before], depth + 1);
        sort(&mut tab[after..], depth + 1);
    }
    sort(tab, 0);
}
