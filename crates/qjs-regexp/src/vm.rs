use crate::alloc::Budget;
use crate::bytecode::{self, Header, Op, jump, read16, read32};
use crate::escape::{is_high, is_low, join_surrogates};
use crate::{
    CompiledRegExp, MatchResult, RegExpError, RegExpFlags, RegExpHost, RegExpInput, canonicalize,
};
use std::mem::size_of;

type Result<T> = std::result::Result<T, RegExpError>;
const POLL_INTERVAL: u16 = 512;
const MISSING: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Split,
    SplitRun { first: usize },
    Positive,
    Negative,
}

#[derive(Clone, Copy)]
struct Frame {
    pc: usize,
    pos: usize,
    undo: usize,
    kind: Choice,
}

struct Undo {
    index: usize,
    old: usize,
}

pub(crate) fn exec(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchResult>> {
    let mut budget = Budget::new(host);
    let h = bytecode::validate(&re.bytecode, &mut budget)?;
    if start > input.len() {
        return Ok(None);
    }
    let unicode = h
        .flags
        .contains(RegExpFlags::UNICODE | RegExpFlags::UNICODE_SETS);
    let start = if unicode
        && start > 0
        && input.unit(start).is_some_and(is_low)
        && input.unit(start - 1).is_some_and(is_high)
    {
        start - 1
    } else {
        start
    };
    let mut vm = Vm {
        h,
        input,
        unicode,
        pc: 0,
        pos: start,
        slots: Vec::new(),
        frames: Vec::new(),
        undo: Vec::new(),
        budget,
        remaining: POLL_INTERVAL,
    };
    let slots = vm.h.captures * 2 + vm.h.registers;
    vm.check_depth(slots, 0, 0)?;
    vm.budget.reserve(&mut vm.slots, slots)?;
    vm.slots.resize(slots, MISSING);
    vm.run()
}

struct Vm<'b, 'i, 'h> {
    h: Header<'b>,
    input: RegExpInput<'i>,
    unicode: bool,
    pc: usize,
    pos: usize,
    slots: Vec<usize>,
    frames: Vec<Frame>,
    undo: Vec<Undo>,
    budget: Budget<'h>,
    remaining: u16,
}

impl Vm<'_, '_, '_> {
    fn tick(&mut self) -> Result<()> {
        self.remaining -= 1;
        if self.remaining == 0 {
            self.remaining = POLL_INTERVAL;
            if self.budget.host.poll_interrupt() {
                return Err(RegExpError::Interrupted);
            }
        }
        Ok(())
    }

    fn check_depth(&mut self, slots: usize, frames: usize, undo: usize) -> Result<()> {
        let depth = slots
            .saturating_mul(size_of::<usize>())
            .saturating_add(frames.saturating_mul(size_of::<Frame>()))
            .saturating_add(undo.saturating_mul(size_of::<Undo>()));
        if self.budget.host.check_stack(depth) {
            Ok(())
        } else {
            Err(RegExpError::StackOverflow)
        }
    }

    fn push(&mut self, pc: usize, kind: Choice) -> Result<()> {
        // Adjacent splits with no slot changes can share one frame. Greedy
        // character repeats otherwise need a separate frame for every match.
        if kind == Choice::Split
            && let Some(frame) = self.frames.last_mut()
            && frame.pc == pc
            && frame.undo == self.undo.len()
        {
            let first = match frame.kind {
                Choice::Split => Some(frame.pos),
                Choice::SplitRun { first } => Some(first),
                _ => None,
            };
            if let Some(first) = first
                && self
                    .input
                    .prev(self.pos, first, self.unicode)
                    .map(|(_, pos)| pos)
                    == Some(frame.pos)
            {
                frame.pos = self.pos;
                frame.kind = Choice::SplitRun { first };
                return Ok(());
            }
        }
        self.check_depth(self.slots.len(), self.frames.len() + 1, self.undo.len())?;
        self.budget.push(
            &mut self.frames,
            Frame {
                pc,
                pos: self.pos,
                undo: self.undo.len(),
                kind,
            },
        )
    }

    fn write(&mut self, index: usize, value: usize, dedup: bool) -> Result<()> {
        if let Some(frame) = self.frames.last()
            && (!dedup
                || !self.undo[frame.undo..]
                    .iter()
                    .rev()
                    .any(|u| u.index == index))
        {
            self.check_depth(self.slots.len(), self.frames.len(), self.undo.len() + 1)?;
            self.budget.push(
                &mut self.undo,
                Undo {
                    index,
                    old: self.slots[index],
                },
            )?;
        }
        self.slots[index] = value;
        Ok(())
    }

    fn restore(&mut self, frame: Frame) -> Result<()> {
        while self.undo.len() > frame.undo {
            let saved = self.undo.pop().expect("pending undo");
            self.slots[saved.index] = saved.old;
            self.tick()?;
        }
        self.pc = frame.pc;
        self.pos = frame.pos;
        Ok(())
    }

    fn backtrack(&mut self) -> Result<bool> {
        while let Some(mut frame) = self.frames.pop() {
            self.tick()?;
            self.restore(frame)?;
            if let Choice::SplitRun { first } = frame.kind
                && frame.pos > first
            {
                let (_, prev) = self
                    .input
                    .prev(frame.pos, first, self.unicode)
                    .expect("previous split in run");
                frame.pos = prev;
                self.frames.push(frame);
            }
            if frame.kind != Choice::Positive {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn finish_look(&mut self, kind: Choice) -> Result<()> {
        loop {
            self.tick()?;
            let frame = self.frames.pop().ok_or(RegExpError::InvalidBytecode)?;
            if kind == Choice::Negative {
                self.restore(frame)?;
            }
            if frame.kind == kind {
                self.pc = frame.pc;
                self.pos = frame.pos;
                if self.frames.is_empty() {
                    self.undo.clear();
                }
                return Ok(());
            }
            if !matches!(frame.kind, Choice::Split | Choice::SplitRun { .. }) {
                return Err(RegExpError::InvalidBytecode);
            }
        }
    }

    fn split(&mut self, target: usize, goto_first: bool) -> Result<()> {
        let saved = if goto_first {
            std::mem::replace(&mut self.pc, target)
        } else {
            target
        };
        self.push(saved, Choice::Split)
    }

    fn next(&mut self) -> Option<u32> {
        let (c, end) = self.input.next(self.pos, self.input.len(), self.unicode)?;
        self.pos = end;
        Some(c)
    }

    fn reg(&self, i: u8) -> usize {
        self.h.captures * 2 + usize::from(i)
    }

    fn reg_value(&self, i: usize) -> Result<usize> {
        let value = self.slots[i];
        if value == MISSING {
            Err(RegExpError::InvalidBytecode)
        } else {
            Ok(value)
        }
    }

    fn run(&mut self) -> Result<Option<MatchResult>> {
        use Op::*;
        let body = self.h.body;
        loop {
            self.tick()?;
            let pc = self.pc;
            let op = Op::decode(body[pc]).expect("validated opcode");
            self.pc = op.end(body, pc).expect("validated operands");
            let argument = || body[pc + 1];
            let branch = |offset| jump(body, pc + offset).expect("validated jump");
            let ok = match op {
                Match => return self.result().map(Some),
                LookMatch => {
                    self.finish_look(Choice::Positive)?;
                    true
                }
                NegLookMatch => {
                    self.finish_look(Choice::Negative)?;
                    false
                }
                SplitGoto | SplitNext => {
                    self.split(branch(1), op == SplitGoto)?;
                    true
                }
                Look | NegLook => {
                    self.push(
                        branch(1),
                        if op == Look {
                            Choice::Positive
                        } else {
                            Choice::Negative
                        },
                    )?;
                    true
                }
                Goto => {
                    self.pc = branch(1);
                    true
                }
                Char | CharI | Char32 | Char32I => {
                    let value = if matches!(op, Char | CharI) {
                        u32::from(read16(body, pc + 1))
                    } else {
                        read32(body, pc + 1)
                    };
                    self.next().is_some_and(|c| {
                        value
                            == if matches!(op, CharI | Char32I) {
                                canonicalize(c, self.unicode)
                            } else {
                                c
                            }
                    })
                }
                LineStart | LineStartM => {
                    self.pos == 0
                        || (op == LineStartM
                            && self
                                .input
                                .prev(self.pos, 0, self.unicode)
                                .is_some_and(|(c, _)| is_line(c)))
                }
                LineEnd | LineEndM => {
                    self.pos == self.input.len()
                        || (op == LineEndM
                            && self
                                .input
                                .next(self.pos, self.input.len(), self.unicode)
                                .is_some_and(|(c, _)| is_line(c)))
                }
                Dot => self.next().is_some_and(|c| !is_line(c)),
                Any => self.next().is_some(),
                Space | NotSpace => self.next().is_some_and(|c| is_space(c) == (op == Space)),
                SaveStart | SaveEnd => {
                    let index = usize::from(argument()) * 2 + usize::from(op == SaveEnd);
                    self.write(index, self.pos, false)?;
                    true
                }
                SaveReset => {
                    for i in usize::from(argument()) * 2..(usize::from(body[pc + 2]) + 1) * 2 {
                        self.write(i, MISSING, false)?;
                    }
                    true
                }
                SetI32 | SetCharPos => {
                    let i = self.reg(argument());
                    let value = if op == SetI32 {
                        read32(body, pc + 2) as usize
                    } else {
                        self.pos
                    };
                    self.write(i, value, true)?;
                    true
                }
                CheckAdvance => self.reg_value(self.reg(argument()))? != self.pos,
                Loop | LoopSplitGoto | LoopSplitNext | LoopAdvGoto | LoopAdvNext => {
                    let i = self.reg(argument());
                    let counter = (self.reg_value(i)? as u32).wrapping_sub(1);
                    self.write(i, counter as usize, true)?;
                    if op == Loop {
                        if counter != 0 {
                            self.pc = branch(2);
                        }
                        true
                    } else {
                        let limit = read32(body, pc + 2);
                        if counter > limit {
                            self.pc = branch(6);
                            true
                        } else if matches!(op, LoopAdvGoto | LoopAdvNext)
                            && counter != limit
                            && self.reg_value(i + 1)? == self.pos
                        {
                            false
                        } else {
                            if counter != 0 {
                                self.split(branch(6), matches!(op, LoopSplitGoto | LoopAdvGoto))?;
                            }
                            true
                        }
                    }
                }
                WordBoundary | WordBoundaryI | NotWordBoundary | NotWordBoundaryI => {
                    let ignore = matches!(op, WordBoundaryI | NotWordBoundaryI);
                    let previous = self
                        .input
                        .prev(self.pos, 0, self.unicode)
                        .is_some_and(|(c, _)| is_word(c, ignore));
                    let current = self
                        .input
                        .next(self.pos, self.input.len(), self.unicode)
                        .is_some_and(|(c, _)| is_word(c, ignore));
                    (previous != current) == matches!(op, WordBoundary | WordBoundaryI)
                }
                BackRef | BackRefI | BackwardRef | BackwardRefI => self.backrefs(
                    &body[pc + 2..self.pc],
                    matches!(op, BackwardRef | BackwardRefI),
                    matches!(op, BackRefI | BackwardRefI),
                )?,
                Range | RangeI | Range32 | Range32I => self.next().is_some_and(|c| {
                    let c = if matches!(op, RangeI | Range32I) {
                        canonicalize(c, self.unicode)
                    } else {
                        c
                    };
                    in_range(c, &body[pc + 3..self.pc], matches!(op, Range32 | Range32I))
                }),
                Prev => {
                    if let Some((_, prev)) = self.input.prev(self.pos, 0, self.unicode) {
                        self.pos = prev;
                        true
                    } else {
                        false
                    }
                }
                Invalid => return Err(RegExpError::InvalidBytecode),
            };
            if !ok && !self.backtrack()? {
                return Ok(None);
            }
        }
    }

    fn backrefs(&mut self, refs: &[u8], backward: bool, ignore: bool) -> Result<bool> {
        for &capture in refs {
            let i = usize::from(capture) * 2;
            let lo = self.slots[i];
            let hi = self.slots[i + 1];
            if lo == MISSING || hi == MISSING {
                continue;
            }
            let mut at = if backward { hi } else { lo };
            while if backward { at > lo } else { at < hi } {
                self.tick()?;
                let (left, right) = if backward {
                    (
                        self.input.prev(at, lo, self.unicode),
                        self.input.prev(self.pos, 0, self.unicode),
                    )
                } else {
                    (
                        self.input.next(at, hi, self.unicode),
                        self.input.next(self.pos, self.input.len(), self.unicode),
                    )
                };
                let (Some((a, next_a)), Some((b, next_b))) = (left, right) else {
                    return Ok(false);
                };
                if a != b
                    && (!ignore || canonicalize(a, self.unicode) != canonicalize(b, self.unicode))
                {
                    return Ok(false);
                }
                at = next_a;
                self.pos = next_b;
            }
            break;
        }
        Ok(true)
    }

    fn result(&mut self) -> Result<MatchResult> {
        let mut captures = Vec::new();
        self.budget.reserve(&mut captures, self.h.captures)?;
        for pair in self.slots[..self.h.captures * 2].chunks_exact(2) {
            captures.push((pair[0] != MISSING && pair[1] != MISSING).then_some((pair[0], pair[1])));
        }
        Ok(MatchResult { captures })
    }
}

fn is_line(c: u32) -> bool {
    matches!(c, 10 | 13 | 0x2028 | 0x2029)
}

fn is_space(c: u32) -> bool {
    if c < 256 {
        matches!(c, 9..=13 | 32 | 0xa0)
    } else {
        qjs_unicode::is_space(c)
    }
}

fn is_word(c: u32, ignore: bool) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x5a | 0x5f | 0x61..=0x7a)
        || (ignore && matches!(c, 0x017f | 0x212a))
}

fn in_range(c: u32, bytes: &[u8], wide: bool) -> bool {
    let step = if wide { 4 } else { 2 };
    let read = |i| {
        if wide {
            read32(bytes, i)
        } else {
            u32::from(read16(bytes, i))
        }
    };
    let mut lo = 0;
    let mut hi = bytes.len() / (step * 2);
    if !wide && c >= 0xffff && read(bytes.len() - step) == 0xffff {
        return true;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let p = mid * 2 * step;
        if c < read(p) {
            hi = mid;
        } else if c > read(p + step) {
            lo = mid + 1;
        } else {
            return true;
        }
    }
    false
}

impl RegExpInput<'_> {
    fn unit(self, at: usize) -> Option<u32> {
        match self {
            Self::Latin1(bytes) => bytes.get(at).copied().map(u32::from),
            Self::Utf16(units) => units.get(at).copied().map(u32::from),
        }
    }

    fn next(self, at: usize, end: usize, unicode: bool) -> Option<(u32, usize)> {
        if at >= end {
            return None;
        }
        let c = self.unit(at)?;
        if unicode
            && is_high(c)
            && at + 1 < end
            && let Some(low) = self.unit(at + 1).filter(|&c| is_low(c))
        {
            return Some((join_surrogates(c, low), at + 2));
        }
        Some((c, at + 1))
    }

    fn prev(self, at: usize, start: usize, unicode: bool) -> Option<(u32, usize)> {
        if at <= start {
            return None;
        }
        let prior = at - 1;
        let c = self.unit(prior)?;
        if unicode
            && is_low(c)
            && prior > start
            && let Some(high) = self.unit(prior - 1).filter(|&c| is_high(c))
        {
            return Some((join_surrogates(high, c), prior - 1));
        }
        Some((c, prior))
    }
}
