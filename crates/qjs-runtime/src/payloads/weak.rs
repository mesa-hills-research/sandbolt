use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use super::collections::HashIndex;
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocResult, HeapLedger, TrackedVec};
use crate::value::{ObjId, RealmId, SymId, Value};

payloads!(SymbolObjectData { value: Value });

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WeakKey {
    Object(ObjId),
    Symbol(SymId),
}

impl WeakKey {
    pub const fn value(self) -> Value {
        match self {
            Self::Object(id) => Value::object(id),
            Self::Symbol(id) => Value::symbol(id),
        }
    }
}

pub(crate) trait WeakEntry {
    fn key(&self) -> WeakKey;
}

impl WeakEntry for WeakKey {
    fn key(&self) -> WeakKey {
        *self
    }
}

impl WeakEntry for (WeakKey, Value) {
    fn key(&self) -> WeakKey {
        self.0
    }
}

#[derive(Debug)]
pub(crate) struct WeakEntries<T> {
    rows: TrackedVec<T>,
    index: HashIndex,
    seed: RandomState,
}

impl<T> Default for WeakEntries<T> {
    fn default() -> Self {
        Self {
            rows: TrackedVec::new(),
            index: HashIndex::default(),
            seed: RandomState::new(),
        }
    }
}

impl<T: WeakEntry> WeakEntries<T> {
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, T> {
        self.rows.iter()
    }

    pub(crate) fn get(&self, key: WeakKey) -> Option<&T> {
        let hash = self.seed.hash_one(key);
        let row = self.index.find(hash, |row| self.rows[row].key() == key)?;
        Some(&self.rows[row])
    }

    pub(crate) fn insert(&mut self, ledger: &HeapLedger, entry: T) -> AllocResult<()> {
        let key = entry.key();
        let hash = self.seed.hash_one(key);
        if let Some(row) = self.index.find(hash, |row| self.rows[row].key() == key) {
            self.rows[row] = entry;
        } else {
            self.rows.try_reserve(ledger, 1)?;
            self.index.reserve(ledger)?;
            self.index.insert(hash, self.rows.len());
            self.rows.push_reserved(entry);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, key: WeakKey) -> bool {
        let hash = self.seed.hash_one(key);
        let Some(row) = self.index.remove(hash, |row| self.rows[row].key() == key) else {
            return false;
        };
        self.rows.swap_remove(row);
        if let Some(moved) = self.rows.get(row) {
            self.index
                .relocate(self.seed.hash_one(moved.key()), self.rows.len(), row);
        }
        true
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let mut row = 0;
        while let Some(entry) = self.rows.get(row) {
            if keep(entry) {
                row += 1;
            } else {
                self.remove(entry.key());
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct WeakMapData {
    pub(crate) entries: WeakEntries<(WeakKey, Value)>,
}
impl Trace for WeakMapData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Debug, Default)]
pub struct WeakSetData {
    pub(crate) entries: WeakEntries<WeakKey>,
}
impl Trace for WeakSetData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Debug)]
pub struct WeakRefData {
    pub(crate) target: Option<WeakKey>,
}
impl Trace for WeakRefData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Clone, Copy, Debug)]
pub struct Registration {
    pub target: WeakKey,
    pub held: Value,
    pub token: Option<WeakKey>,
}

#[derive(Debug)]
pub struct FinalizationRegistryData {
    pub(crate) cleanup: ObjId,
    pub(crate) realm: RealmId,
    pub(crate) entries: TrackedVec<Registration>,
}
impl Trace for FinalizationRegistryData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.cleanup.trace(tracer);
        self.realm.trace(tracer);
        for entry in self.entries.iter() {
            entry.held.trace(tracer);
        }
    }
}
