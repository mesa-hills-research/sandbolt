use sandbolt_regexp::{RegExpError, RegExpFlags, RegExpHost};

use super::*;
use crate::source::push_encoded;

struct Budget {
    bytes: usize,
    stack_bytes: usize,
}

impl RegExpHost for Budget {
    fn poll_interrupt(&mut self) -> bool {
        false
    }

    fn charge(&mut self, bytes: usize) -> std::result::Result<(), RegExpError> {
        self.bytes = self
            .bytes
            .checked_sub(bytes)
            .ok_or(RegExpError::OutOfMemory)?;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.bytes += bytes;
    }

    fn check_stack(&mut self, depth_bytes: usize) -> bool {
        depth_bytes <= self.stack_bytes
    }
}

impl Parser<'_> {
    pub(super) fn check_regexp(&self, literal: &RegExpLiteral, span: Span) -> Result<()> {
        let flags = RegExpFlags(
            literal
                .flags
                .bytes()
                .map(|b| match b {
                    b'd' => RegExpFlags::INDICES,
                    b'g' => RegExpFlags::GLOBAL,
                    b'i' => RegExpFlags::IGNORECASE,
                    b'm' => RegExpFlags::MULTILINE,
                    b's' => RegExpFlags::DOTALL,
                    b'u' => RegExpFlags::UNICODE,
                    b'v' => RegExpFlags::UNICODE_SETS,
                    b'y' => RegExpFlags::STICKY,
                    _ => unreachable!(),
                })
                .fold(0, |flags, bit| flags | bit),
        );
        let mut pattern = Vec::new();
        let reserve = literal.pattern.units().count().saturating_mul(3);
        let mut budget = Budget {
            bytes: self.remaining,
            stack_bytes: self
                .opts
                .max_depth
                .saturating_sub(self.depth)
                .saturating_mul(64),
        };
        if budget.charge(reserve).is_err() || pattern.try_reserve_exact(reserve).is_err() {
            return Err(self.oom());
        }
        if literal.flags.contains(['u', 'v']) {
            for c in char::decode_utf16(literal.pattern.units()) {
                push_encoded(
                    &mut pattern,
                    c.map_or_else(|e| u32::from(e.unpaired_surrogate()), u32::from),
                );
            }
        } else {
            for u in literal.pattern.units() {
                push_encoded(&mut pattern, u32::from(u));
            }
        }
        match sandbolt_regexp::compile(&pattern, flags, &mut budget) {
            Ok(_) => Ok(()),
            Err(RegExpError::Syntax(message)) => Err(self.error_at(span, &message)),
            Err(RegExpError::OutOfMemory) => Err(self.oom()),
            Err(RegExpError::StackOverflow) => Err(ParseError::new(
                self.source,
                ParseErrorKind::StackOverflow,
                span,
                "Regular expression exceeds the parse depth limit",
            )),
            Err(RegExpError::Interrupted | RegExpError::InvalidBytecode) => {
                Err(self.error_at(span, "Invalid regular expression"))
            }
        }
    }
}
