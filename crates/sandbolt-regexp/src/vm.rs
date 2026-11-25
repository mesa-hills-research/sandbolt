use crate::alloc::Budget;
use crate::bytecode::{self, FastMatch, Header, Op, jump, read16, read32};
use crate::escape::{is_high, is_low, join_surrogates};
use crate::{
    CompiledRegExp, INLINE_CAPTURES, MatchResult, MatchSpans, RegExpError, RegExpFlags, RegExpHost,
    RegExpInput, canonicalize,
};
use std::mem::size_of;

type Result<T> = std::result::Result<T, RegExpError>;
const POLL_INTERVAL: u16 = 512;
const SCAN_POLL_CHUNK: usize = 4096;
const MISSING: usize = usize::MAX;
const INLINE_SLOTS: usize = 16;
const INLINE_FRAMES: usize = 16;
const INLINE_UNDO: usize = 16;

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

impl Frame {
    const ZERO: Self = Self {
        pc: 0,
        pos: 0,
        undo: 0,
        kind: Choice::Split,
    };
}

#[derive(Clone, Copy)]
struct Undo {
    index: usize,
    old: usize,
}

impl Undo {
    const ZERO: Self = Self { index: 0, old: 0 };
}

enum SmallBuf<T: Copy, const N: usize> {
    Inline { data: [T; N], len: usize },
    Heap(Vec<T>),
}

impl<T: Copy, const N: usize> SmallBuf<T, N> {
    fn new(fill: T) -> Self {
        Self::Inline {
            data: [fill; N],
            len: 0,
        }
    }

    fn filled(count: usize, fill: T, budget: &mut Budget<'_>) -> Result<Self> {
        if count <= N {
            Ok(Self::Inline {
                data: [fill; N],
                len: count,
            })
        } else {
            let mut heap = Vec::new();
            budget.reserve(&mut heap, count)?;
            heap.resize(count, fill);
            Ok(Self::Heap(heap))
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => *len,
            Self::Heap(heap) => heap.len(),
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn as_slice(&self) -> &[T] {
        match self {
            Self::Inline { data, len } => &data[..*len],
            Self::Heap(heap) => heap,
        }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Self::Inline { data, len } => &mut data[..*len],
            Self::Heap(heap) => heap,
        }
    }

    #[inline]
    fn last(&self) -> Option<&T> {
        self.as_slice().last()
    }

    #[inline]
    fn last_mut(&mut self) -> Option<&mut T> {
        self.as_mut_slice().last_mut()
    }

    #[inline]
    fn push(&mut self, value: T, budget: &mut Budget<'_>) -> Result<()> {
        match self {
            Self::Inline { data, len } => {
                if *len < N {
                    data[*len] = value;
                    *len += 1;
                    Ok(())
                } else {
                    let mut heap = Vec::new();
                    budget.reserve(&mut heap, N * 2)?;
                    heap.extend_from_slice(data);
                    heap.push(value);
                    *self = Self::Heap(heap);
                    Ok(())
                }
            }
            Self::Heap(heap) => budget.push(heap, value),
        }
    }

    #[inline]
    fn push_within_capacity(&mut self, value: T) {
        match self {
            Self::Inline { data, len } => {
                data[*len] = value;
                *len += 1;
            }
            Self::Heap(heap) => heap.push(value),
        }
    }

    #[inline]
    fn pop(&mut self) -> Option<T> {
        match self {
            Self::Inline { data, len } => {
                if *len == 0 {
                    None
                } else {
                    *len -= 1;
                    Some(data[*len])
                }
            }
            Self::Heap(heap) => heap.pop(),
        }
    }

    #[inline]
    fn clear(&mut self) {
        match self {
            Self::Inline { len, .. } => *len = 0,
            Self::Heap(heap) => heap.clear(),
        }
    }
}

pub(crate) fn exec(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchResult>> {
    let Some(spans) = exec_spans(re, input, start, host)? else {
        return Ok(None);
    };
    let captures = match spans {
        MatchSpans::Heap(captures) => captures,
        MatchSpans::Inline { len, captures } => {
            let mut budget = Budget::new(host);
            let mut out = Vec::new();
            budget.reserve(&mut out, usize::from(len))?;
            out.extend_from_slice(&captures[..usize::from(len)]);
            out
        }
    };
    Ok(Some(MatchResult { captures }))
}

pub(crate) fn exec_single(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<(usize, usize)>> {
    let verified = re.plan.fingerprint == bytecode::fingerprint(&re.bytecode);
    if verified {
        if start > input.len() {
            return Ok(None);
        }
        let unicode = re
            .plan
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
        match re.plan.fast {
            FastMatch::LiteralLatin1 { bytes, len, sticky } => {
                let depth = re
                    .plan
                    .captures
                    .saturating_mul(2)
                    .saturating_mul(size_of::<usize>());
                if !host.check_stack(depth) {
                    return Err(RegExpError::StackOverflow);
                }
                return exec_literal_latin1_span(
                    &bytes[..usize::from(len)],
                    sticky,
                    input,
                    start,
                    host,
                );
            }
            FastMatch::LiteralUtf16 { units, len, sticky } => {
                let depth = re
                    .plan
                    .captures
                    .saturating_mul(2)
                    .saturating_mul(size_of::<usize>());
                if !host.check_stack(depth) {
                    return Err(RegExpError::StackOverflow);
                }
                return exec_literal_utf16_span(
                    &units[..usize::from(len)],
                    sticky,
                    input,
                    start,
                    host,
                );
            }
            _ => {}
        }
    }
    Ok(exec_spans(re, input, start, host)?.and_then(|spans| spans.as_slice()[0]))
}

pub(crate) fn exec_spans(
    re: &CompiledRegExp,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchSpans>> {
    let verified = re.plan.fingerprint == bytecode::fingerprint(&re.bytecode);
    let mut budget = Budget::new(host);
    let (h, fast) = if verified {
        (re.plan.header(&re.bytecode), re.plan.fast)
    } else {
        (
            bytecode::validate(&re.bytecode, &mut budget)?,
            FastMatch::None,
        )
    };
    if start > input.len() {
        return Ok(None);
    }
    let unicode = h
        .flags
        .contains(RegExpFlags::UNICODE | RegExpFlags::UNICODE_SETS);
    let mut start = if unicode
        && start > 0
        && input.unit(start).is_some_and(is_low)
        && input.unit(start - 1).is_some_and(is_high)
    {
        start - 1
    } else {
        start
    };
    match fast {
        FastMatch::LiteralLatin1 { bytes, len, sticky } => {
            let depth = h
                .captures
                .saturating_mul(2)
                .saturating_mul(size_of::<usize>());
            if !budget.host.check_stack(depth) {
                return Err(RegExpError::StackOverflow);
            }
            return exec_literal_latin1(
                &bytes[..usize::from(len)],
                sticky,
                input,
                start,
                budget.host,
            );
        }
        FastMatch::LiteralUtf16 { units, len, sticky } => {
            let depth = h
                .captures
                .saturating_mul(2)
                .saturating_mul(size_of::<usize>());
            if !budget.host.check_stack(depth) {
                return Err(RegExpError::StackOverflow);
            }
            return exec_literal_utf16(
                &units[..usize::from(len)],
                sticky,
                input,
                start,
                budget.host,
            );
        }
        FastMatch::PrefixChar { unit } => {
            let Some(found) = find_first_unit(input, start, unit, budget.host)? else {
                return Ok(None);
            };
            start = found;
        }
        FastMatch::None => {}
    }
    let slot_count = h.captures * 2 + h.registers;
    let depth = slot_count.saturating_mul(size_of::<usize>());
    if !budget.host.check_stack(depth) {
        return Err(RegExpError::StackOverflow);
    }
    let slots = SmallBuf::filled(slot_count, MISSING, &mut budget)?;
    let mut vm = Vm {
        h,
        input,
        unicode,
        pc: 0,
        pos: start,
        slots,
        frames: SmallBuf::new(Frame::ZERO),
        undo: SmallBuf::new(Undo::ZERO),
        budget,
        remaining: POLL_INTERVAL,
    };
    vm.run()
}

fn find_first_unit(
    input: RegExpInput<'_>,
    mut pos: usize,
    unit: u16,
    host: &mut dyn RegExpHost,
) -> Result<Option<usize>> {
    match input {
        RegExpInput::Latin1(hay) => {
            let Ok(byte) = u8::try_from(unit) else {
                return Ok(None);
            };
            while pos < hay.len() {
                let end = pos.saturating_add(SCAN_POLL_CHUNK).min(hay.len());
                if let Some(rel) = hay[pos..end].iter().position(|&b| b == byte) {
                    return Ok(Some(pos + rel));
                }
                pos = end;
                if pos < hay.len() && host.poll_interrupt() {
                    return Err(RegExpError::Interrupted);
                }
            }
            Ok(None)
        }
        RegExpInput::Utf16(hay) => {
            while pos < hay.len() {
                let end = pos.saturating_add(SCAN_POLL_CHUNK).min(hay.len());
                if let Some(rel) = hay[pos..end].iter().position(|&u| u == unit) {
                    return Ok(Some(pos + rel));
                }
                pos = end;
                if pos < hay.len() && host.poll_interrupt() {
                    return Err(RegExpError::Interrupted);
                }
            }
            Ok(None)
        }
    }
}

fn exec_literal_latin1(
    pat: &[u8],
    sticky: bool,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchSpans>> {
    Ok(exec_literal_latin1_span(pat, sticky, input, start, host)?
        .map(|(s, e)| MatchSpans::single(s, e)))
}

fn exec_literal_latin1_span(
    pat: &[u8],
    sticky: bool,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<(usize, usize)>> {
    let pat_len = pat.len();
    match input {
        RegExpInput::Latin1(hay) => {
            if sticky {
                return Ok(hay
                    .get(start..start.saturating_add(pat_len))
                    .is_some_and(|slice| slice == pat)
                    .then_some((start, start + pat_len)));
            }
            if start.saturating_add(pat_len) > hay.len() {
                return Ok(None);
            }
            let first = pat[0];
            let tail = &pat[1..];
            let limit = hay.len() - pat_len;
            let mut pos = start;
            let mut polled_until = start.saturating_add(SCAN_POLL_CHUNK);
            while pos <= limit {
                if pos >= polled_until {
                    if host.poll_interrupt() {
                        return Err(RegExpError::Interrupted);
                    }
                    polled_until = pos.saturating_add(SCAN_POLL_CHUNK);
                }
                let scan_end = polled_until.min(limit + 1);
                if let Some(rel) = hay[pos..scan_end].iter().position(|&b| b == first) {
                    pos += rel;
                    if hay[pos + 1..pos + pat_len] == *tail {
                        return Ok(Some((pos, pos + pat_len)));
                    }
                    pos += 1;
                } else {
                    pos = scan_end;
                }
            }
            Ok(None)
        }
        RegExpInput::Utf16(hay) => {
            if sticky {
                let matched = hay
                    .get(start..start.saturating_add(pat_len))
                    .is_some_and(|slice| {
                        slice
                            .iter()
                            .zip(pat.iter())
                            .all(|(&u, &b)| u == u16::from(b))
                    });
                return Ok(matched.then_some((start, start + pat_len)));
            }
            if start.saturating_add(pat_len) > hay.len() {
                return Ok(None);
            }
            let first = u16::from(pat[0]);
            let tail = &pat[1..];
            let limit = hay.len() - pat_len;
            let mut pos = start;
            let mut polled_until = start.saturating_add(SCAN_POLL_CHUNK);
            while pos <= limit {
                if pos >= polled_until {
                    if host.poll_interrupt() {
                        return Err(RegExpError::Interrupted);
                    }
                    polled_until = pos.saturating_add(SCAN_POLL_CHUNK);
                }
                let scan_end = polled_until.min(limit + 1);
                if let Some(rel) = hay[pos..scan_end].iter().position(|&u| u == first) {
                    pos += rel;
                    if hay[pos + 1..pos + pat_len]
                        .iter()
                        .zip(tail.iter())
                        .all(|(&u, &b)| u == u16::from(b))
                    {
                        return Ok(Some((pos, pos + pat_len)));
                    }
                    pos += 1;
                } else {
                    pos = scan_end;
                }
            }
            Ok(None)
        }
    }
}

fn exec_literal_utf16(
    pat: &[u16],
    sticky: bool,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<MatchSpans>> {
    Ok(exec_literal_utf16_span(pat, sticky, input, start, host)?
        .map(|(s, e)| MatchSpans::single(s, e)))
}

fn exec_literal_utf16_span(
    pat: &[u16],
    sticky: bool,
    input: RegExpInput<'_>,
    start: usize,
    host: &mut dyn RegExpHost,
) -> Result<Option<(usize, usize)>> {
    let RegExpInput::Utf16(hay) = input else {
        return Ok(None);
    };
    let pat_len = pat.len();
    if sticky {
        return Ok(hay
            .get(start..start.saturating_add(pat_len))
            .is_some_and(|slice| slice == pat)
            .then_some((start, start + pat_len)));
    }
    if start.saturating_add(pat_len) > hay.len() {
        return Ok(None);
    }
    let first = pat[0];
    let tail = &pat[1..];
    let limit = hay.len() - pat_len;
    let mut pos = start;
    let mut polled_until = start.saturating_add(SCAN_POLL_CHUNK);
    while pos <= limit {
        if pos >= polled_until {
            if host.poll_interrupt() {
                return Err(RegExpError::Interrupted);
            }
            polled_until = pos.saturating_add(SCAN_POLL_CHUNK);
        }
        let scan_end = polled_until.min(limit + 1);
        if let Some(rel) = hay[pos..scan_end].iter().position(|&u| u == first) {
            pos += rel;
            if hay[pos + 1..pos + pat_len] == *tail {
                return Ok(Some((pos, pos + pat_len)));
            }
            pos += 1;
        } else {
            pos = scan_end;
        }
    }
    Ok(None)
}

struct Vm<'b, 'i, 'h> {
    h: Header<'b>,
    input: RegExpInput<'i>,
    unicode: bool,
    pc: usize,
    pos: usize,
    slots: SmallBuf<usize, INLINE_SLOTS>,
    frames: SmallBuf<Frame, INLINE_FRAMES>,
    undo: SmallBuf<Undo, INLINE_UNDO>,
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
        self.frames.push(
            Frame {
                pc,
                pos: self.pos,
                undo: self.undo.len(),
                kind,
            },
            &mut self.budget,
        )
    }

    fn write(&mut self, index: usize, value: usize, dedup: bool) -> Result<()> {
        if let Some(frame) = self.frames.last()
            && (!dedup
                || !self.undo.as_slice()[frame.undo..]
                    .iter()
                    .rev()
                    .any(|u| u.index == index))
        {
            self.check_depth(self.slots.len(), self.frames.len(), self.undo.len() + 1)?;
            let old = self.slots.as_slice()[index];
            self.undo.push(Undo { index, old }, &mut self.budget)?;
        }
        self.slots.as_mut_slice()[index] = value;
        Ok(())
    }

    fn restore(&mut self, frame: Frame) -> Result<()> {
        while self.undo.len() > frame.undo {
            let saved = self.undo.pop().expect("pending undo");
            self.slots.as_mut_slice()[saved.index] = saved.old;
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
                self.frames.push_within_capacity(frame);
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
        let value = self.slots.as_slice()[i];
        if value == MISSING {
            Err(RegExpError::InvalidBytecode)
        } else {
            Ok(value)
        }
    }

    fn run(&mut self) -> Result<Option<MatchSpans>> {
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
            let slots = self.slots.as_slice();
            let lo = slots[i];
            let hi = slots[i + 1];
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

    fn result(&mut self) -> Result<MatchSpans> {
        let slots = &self.slots.as_slice()[..self.h.captures * 2];
        if self.h.captures <= INLINE_CAPTURES {
            let mut captures = [None; INLINE_CAPTURES];
            for (dst, pair) in captures.iter_mut().zip(slots.chunks_exact(2)) {
                *dst = (pair[0] != MISSING && pair[1] != MISSING).then_some((pair[0], pair[1]));
            }
            return Ok(MatchSpans::Inline {
                len: self.h.captures as u8,
                captures,
            });
        }
        let mut captures = Vec::new();
        self.budget.reserve(&mut captures, self.h.captures)?;
        for pair in slots.chunks_exact(2) {
            captures.push((pair[0] != MISSING && pair[1] != MISSING).then_some((pair[0], pair[1])));
        }
        Ok(MatchSpans::Heap(captures))
    }
}

fn is_line(c: u32) -> bool {
    matches!(c, 10 | 13 | 0x2028 | 0x2029)
}

fn is_space(c: u32) -> bool {
    if c < 256 {
        matches!(c, 9..=13 | 32 | 0xa0)
    } else {
        sandbolt_unicode::is_space(c)
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
    #[inline]
    fn unit(self, at: usize) -> Option<u32> {
        match self {
            Self::Latin1(bytes) => bytes.get(at).copied().map(u32::from),
            Self::Utf16(units) => units.get(at).copied().map(u32::from),
        }
    }

    #[inline]
    fn next(self, at: usize, end: usize, unicode: bool) -> Option<(u32, usize)> {
        match self {
            Self::Latin1(bytes) => {
                if at >= end {
                    None
                } else {
                    Some((u32::from(bytes[at]), at + 1))
                }
            }
            Self::Utf16(units) => {
                if at >= end {
                    return None;
                }
                let c = u32::from(units[at]);
                if unicode && is_high(c) && at + 1 < end {
                    let low = u32::from(units[at + 1]);
                    if is_low(low) {
                        return Some((join_surrogates(c, low), at + 2));
                    }
                }
                Some((c, at + 1))
            }
        }
    }

    #[inline]
    fn prev(self, at: usize, start: usize, unicode: bool) -> Option<(u32, usize)> {
        match self {
            Self::Latin1(bytes) => {
                if at <= start {
                    None
                } else {
                    let prior = at - 1;
                    Some((u32::from(bytes[prior]), prior))
                }
            }
            Self::Utf16(units) => {
                if at <= start {
                    return None;
                }
                let prior = at - 1;
                let c = u32::from(units[prior]);
                if unicode && is_low(c) && prior > start {
                    let high = u32::from(units[prior - 1]);
                    if is_high(high) {
                        return Some((join_surrogates(high, c), prior - 1));
                    }
                }
                Some((c, prior))
            }
        }
    }
}
