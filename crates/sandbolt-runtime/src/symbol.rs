use crate::arena::{Arena, ArenaItem};
use crate::atom::{INLINE_INDEX_LIMIT, PropKey};
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::string::{JsString, StringUnits};
use crate::value::{StrId, SymId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WellKnown {
    ToPrimitive,
    Iterator,
    Match,
    MatchAll,
    Replace,
    Search,
    Split,
    ToStringTag,
    IsConcatSpreadable,
    HasInstance,
    Species,
    Unscopables,
    AsyncIterator,
    Dispose,
    AsyncDispose,
}

impl WellKnown {
    pub const ALL: [Self; 15] = [
        Self::ToPrimitive,
        Self::Iterator,
        Self::Match,
        Self::MatchAll,
        Self::Replace,
        Self::Search,
        Self::Split,
        Self::ToStringTag,
        Self::IsConcatSpreadable,
        Self::HasInstance,
        Self::Species,
        Self::Unscopables,
        Self::AsyncIterator,
        Self::Dispose,
        Self::AsyncDispose,
    ];

    pub fn description(self) -> &'static str {
        crate::atom::BUILTIN_ATOMS[226 + self as usize]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolDescription {
    None,
    String(StrId),
    WellKnown(WellKnown),
}

#[derive(Debug)]
pub struct SymbolData {
    pub description: SymbolDescription,
    pub is_registered: bool,
    pub is_private: bool,
    token: u32,
}

impl SymbolData {
    pub fn key(&self) -> PropKey {
        PropKey::symbol(self.token)
    }
}

impl ArenaItem for SymbolData {
    type Id = SymId;
}

impl Trace for SymbolData {
    fn trace(&self, tracer: &Tracer<'_>) {
        if let SymbolDescription::String(id) = self.description {
            id.trace(tracer);
        }
    }
}

#[derive(Default)]
pub struct SymbolRegistry {
    keys: TrackedVec<(u32, SymId)>,
    issued: u32,
    registered: TrackedVec<(StrId, SymId)>,
    well_known: [Option<SymId>; 15],
}

impl SymbolRegistry {
    pub fn well_known(&self, symbol: WellKnown) -> SymId {
        self.well_known[symbol as usize].unwrap()
    }

    pub fn from_token(&self, token: u32) -> Option<SymId> {
        let index = self
            .keys
            .binary_search_by_key(&token, |entry| entry.0)
            .ok()?;
        Some(self.keys[index].1)
    }

    pub(crate) fn find_registered(
        &self,
        strings: &Arena<JsString>,
        description: StrId,
    ) -> Option<SymId> {
        self.registered.iter().find_map(|&(key, sym)| {
            (key == description
                || StringUnits::new(strings, key).eq(StringUnits::new(strings, description)))
            .then_some(sym)
        })
    }

    pub(crate) fn insert(
        &mut self,
        arena: &mut Arena<SymbolData>,
        ledger: &HeapLedger,
        description: SymbolDescription,
        is_registered: bool,
        is_private: bool,
    ) -> AllocResult<SymId> {
        let token = self.issued;
        if token >= INLINE_INDEX_LIMIT {
            return Err(AllocError::Limit);
        }
        let name = if is_registered {
            let SymbolDescription::String(name) = description else {
                unreachable!()
            };
            self.registered.try_reserve(ledger, 1)?;
            Some(name)
        } else {
            None
        };
        self.keys.try_reserve(ledger, 1)?;
        let id = arena.insert(
            ledger,
            SymbolData {
                description,
                is_registered,
                is_private,
                token,
            },
        )?;
        self.keys.push_reserved((token, id));
        self.issued += 1;
        if let Some(name) = name {
            self.registered.push_reserved((name, id));
        }
        if let SymbolDescription::WellKnown(which) = description {
            self.well_known[which as usize] = Some(id);
        }
        Ok(id)
    }

    pub(crate) fn prune(&mut self, arena: &Arena<SymbolData>) {
        self.keys.retain(|&(_, id)| arena.is_marked(id));
    }
}

impl Trace for SymbolRegistry {
    fn trace(&self, tracer: &Tracer<'_>) {
        for &(_, sym) in self.registered.iter() {
            sym.trace(tracer);
        }
        for id in self.well_known {
            id.trace(tracer);
        }
    }
}
