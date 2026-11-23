use crate::alloc::Budget;
use crate::{RegExpError, RegExpFlags};

pub(crate) const HEADER: usize = 8;

macro_rules! opcodes {
    ($($op:ident = $size:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u8)]
        pub(crate) enum Op { $($op),+ }
        impl Op {
            pub(crate) fn decode(byte: u8) -> Option<Self> {
                const OPS: &[Op] = &[$(Op::$op),+];
                OPS.get(usize::from(byte)).copied().filter(|&op| op != Self::Invalid)
            }
            fn fixed_size(self) -> usize {
                match self { $(Self::$op => $size),+ }
            }
        }
    };
}

opcodes! {
    Invalid = 1, Char = 3, CharI = 3, Char32 = 5, Char32I = 5,
    Dot = 1, Any = 1, Space = 1, NotSpace = 1,
    LineStart = 1, LineStartM = 1, LineEnd = 1, LineEndM = 1,
    Goto = 5, SplitGoto = 5, SplitNext = 5, Match = 1,
    LookMatch = 1, NegLookMatch = 1, SaveStart = 2, SaveEnd = 2, SaveReset = 3,
    Loop = 6, LoopSplitGoto = 10, LoopSplitNext = 10,
    LoopAdvGoto = 10, LoopAdvNext = 10, SetI32 = 6,
    WordBoundary = 1, WordBoundaryI = 1, NotWordBoundary = 1, NotWordBoundaryI = 1,
    BackRef = 2, BackRefI = 2, BackwardRef = 2, BackwardRefI = 2,
    Range = 3, RangeI = 3, Range32 = 3, Range32I = 3,
    Look = 5, NegLook = 5, SetCharPos = 2, CheckAdvance = 2, Prev = 1,
}

impl Op {
    pub(crate) fn end(self, bytes: &[u8], pc: usize) -> Option<usize> {
        let fixed_end = pc.checked_add(self.fixed_size())?;
        if fixed_end > bytes.len() {
            return None;
        }
        let extra = match self {
            Self::Range | Self::RangeI => usize::from(read16(bytes, pc + 1)) * 4,
            Self::Range32 | Self::Range32I => usize::from(read16(bytes, pc + 1)) * 8,
            Self::BackRef | Self::BackRefI | Self::BackwardRef | Self::BackwardRefI => {
                usize::from(bytes[pc + 1])
            }
            _ => 0,
        };
        let end = fixed_end.checked_add(extra)?;
        (end <= bytes.len()).then_some(end)
    }

    fn jump_operand(self, pc: usize) -> Option<usize> {
        match self {
            Self::Goto | Self::SplitGoto | Self::SplitNext | Self::Look | Self::NegLook => {
                Some(pc + 1)
            }
            Self::Loop => Some(pc + 2),
            Self::LoopSplitGoto | Self::LoopSplitNext | Self::LoopAdvGoto | Self::LoopAdvNext => {
                Some(pc + 6)
            }
            _ => None,
        }
    }
}

pub(crate) fn read16(bytes: &[u8], pc: usize) -> u16 {
    u16::from_ne_bytes(bytes[pc..pc + 2].try_into().expect("u16 operand"))
}

pub(crate) fn read32(bytes: &[u8], pc: usize) -> u32 {
    u32::from_ne_bytes(bytes[pc..pc + 4].try_into().expect("u32 operand"))
}

pub(crate) fn jump(bytes: &[u8], operand: usize) -> Option<usize> {
    (operand + 4).checked_add_signed(read32(bytes, operand) as i32 as isize)
}

pub(crate) struct Header<'a> {
    pub(crate) flags: RegExpFlags,
    pub(crate) captures: usize,
    pub(crate) registers: usize,
    pub(crate) body: &'a [u8],
    pub(crate) names: &'a [u8],
}

impl<'a> Header<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER {
            return None;
        }
        let end = HEADER.checked_add(read32(bytes, 4) as usize)?;
        let body = bytes.get(HEADER..end)?;
        Some(Self {
            flags: RegExpFlags(read16(bytes, 0)),
            captures: usize::from(bytes[2]),
            registers: usize::from(bytes[3]),
            body,
            names: &bytes[end..],
        })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = Result<Option<&'a str>, RegExpError>> {
        let has_names = self.flags.contains(RegExpFlags::NAMED_GROUPS);
        let mut tail = self.names;
        (0..self.captures).map(move |i| {
            if i == 0 || !has_names {
                return Ok(None);
            }
            let len = tail
                .iter()
                .position(|&b| b == 0)
                .ok_or(RegExpError::InvalidBytecode)?;
            let name = str::from_utf8(&tail[..len]).map_err(|_| RegExpError::InvalidBytecode)?;
            tail = tail.get(len + 2..).ok_or(RegExpError::InvalidBytecode)?;
            if i + 1 == self.captures && !tail.is_empty() {
                return Err(RegExpError::InvalidBytecode);
            }
            Ok((!name.is_empty()).then_some(name))
        })
    }
}

pub(crate) fn validate<'a>(
    bytes: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<Header<'a>, RegExpError> {
    use Op::*;
    let invalid = RegExpError::InvalidBytecode;
    let h = Header::parse(bytes).ok_or(invalid.clone())?;
    if h.captures == 0 || h.body.last() != Some(&(Match as u8)) {
        return Err(invalid);
    }
    if h.flags.contains(RegExpFlags::NAMED_GROUPS) {
        let mut named = false;
        for name in h.names() {
            named |= name?.is_some();
        }
        if !named {
            return Err(invalid);
        }
    } else if !h.names.is_empty() {
        return Err(invalid);
    }

    let mut boundaries = Vec::new();
    let size = h.body.len().div_ceil(8);
    budget.reserve(&mut boundaries, size)?;
    boundaries.resize(size, 0u8);
    let mut pc = 0;
    let mut last = Invalid;
    while pc < h.body.len() {
        let op = Op::decode(h.body[pc]).ok_or(invalid.clone())?;
        let end = op.end(h.body, pc).ok_or(invalid.clone())?;
        boundaries[pc / 8] |= 1 << (pc % 8);
        let argument = || usize::from(h.body[pc + 1]);
        let valid = match op {
            SaveStart | SaveEnd => argument() < h.captures,
            SaveReset => {
                argument() <= usize::from(h.body[pc + 2])
                    && usize::from(h.body[pc + 2]) < h.captures
            }
            SetI32 | SetCharPos | CheckAdvance | Loop | LoopSplitGoto | LoopSplitNext => {
                argument() < h.registers
            }
            LoopAdvGoto | LoopAdvNext => argument() + 1 < h.registers,
            BackRef | BackRefI | BackwardRef | BackwardRefI => {
                argument() > 0
                    && h.body[pc + 2..end]
                        .iter()
                        .all(|&i| usize::from(i) < h.captures)
            }
            Range | RangeI | Range32 | Range32I => {
                check_ranges(h.body, pc + 3, end, matches!(op, Range32 | Range32I))
            }
            _ => true,
        };
        if !valid {
            return Err(invalid);
        }
        last = op;
        pc = end;
    }
    if last != Match {
        return Err(invalid);
    }
    pc = 0;
    while pc < h.body.len() {
        let op = Op::decode(h.body[pc]).expect("decoded opcode");
        if let Some(operand) = op.jump_operand(pc) {
            let target = jump(h.body, operand).ok_or(invalid.clone())?;
            if target >= h.body.len() || boundaries[target / 8] & (1 << (target % 8)) == 0 {
                return Err(invalid);
            }
        }
        pc = op.end(h.body, pc).expect("decoded operands");
    }
    budget.free(boundaries);
    Ok(h)
}

fn check_ranges(bytes: &[u8], mut pc: usize, end: usize, wide: bool) -> bool {
    if pc == end {
        return false;
    }
    let step = if wide { 4 } else { 2 };
    let read = |i| {
        if wide {
            read32(bytes, i)
        } else {
            u32::from(read16(bytes, i))
        }
    };
    let mut previous = None;
    while pc < end {
        let low = read(pc);
        let high = read(pc + step);
        if low > high || previous.is_some_and(|p| p >= low) {
            return false;
        }
        previous = Some(high);
        pc += 2 * step;
    }
    true
}

pub(crate) fn register_count(body: &mut [u8]) -> Option<u8> {
    use Op::*;
    let mut pc = 0;
    let mut depth = 0u8;
    let mut max = 0;
    while pc < body.len() {
        let op = Op::decode(body[pc])?;
        let end = op.end(body, pc)?;
        match op {
            SetI32 | SetCharPos => {
                body[pc + 1] = depth;
                depth = depth.checked_add(1)?;
                max = max.max(depth);
            }
            CheckAdvance | Loop | LoopSplitGoto | LoopSplitNext | LoopAdvGoto | LoopAdvNext => {
                depth = depth.checked_sub(if matches!(op, LoopAdvGoto | LoopAdvNext) {
                    2
                } else {
                    1
                })?;
                body[pc + 1] = depth;
            }
            _ => {}
        }
        pc = end;
    }
    (depth == 0).then_some(max)
}

pub(crate) fn atom_effects(bytes: &[u8]) -> (bool, bool) {
    use Op::*;
    let mut advance_check = true;
    let mut reset_captures = false;
    let mut pc = 0;
    while pc < bytes.len() {
        let op = Op::decode(bytes[pc]).expect("emitted opcode");
        match op {
            Range | RangeI | Range32 | Range32I | Char | CharI | Char32 | Char32I | Dot | Any
            | Space | NotSpace => advance_check = false,
            LineStart | LineStartM | LineEnd | LineEndM | SetI32 | SetCharPos | WordBoundary
            | WordBoundaryI | NotWordBoundary | NotWordBoundaryI | Prev | SaveStart | SaveEnd
            | SaveReset => {}
            BackRef | BackRefI | BackwardRef | BackwardRefI => reset_captures = true,
            _ => return (advance_check, true),
        }
        pc = op.end(bytes, pc).expect("emitted operands");
    }
    (advance_check, reset_captures)
}
