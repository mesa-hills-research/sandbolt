use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use crate::arena::Arena;
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::object::HashIndex;
use crate::string::{JsString, StringUnits};
use crate::value::StrId;

pub const INLINE_INDEX_LIMIT: u32 = 1 << 30;
const ATOM_TAG: u32 = INLINE_INDEX_LIMIT;
const SYMBOL_TAG: u32 = 2 * INLINE_INDEX_LIMIT;
const KEY_MASK: u32 = INLINE_INDEX_LIMIT - 1;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Atom(pub u32);

impl Atom {
    pub const LENGTH: Self = Self(51);
    pub const MESSAGE: Self = Self(52);
    pub const NAME: Self = Self(58);
    pub const TO_STRING: Self = Self(59);
    pub const VALUE_OF: Self = Self(61);
    pub const PROTOTYPE: Self = Self(63);
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct PropKey(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyKind {
    Index(u32),
    String(Atom),
    Symbol(u32),
}

impl PropKey {
    pub const fn index(index: u32) -> Option<Self> {
        if index < INLINE_INDEX_LIMIT {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn atom(atom: Atom) -> Self {
        assert!(atom.0 != 0 && atom.0 < INLINE_INDEX_LIMIT);
        Self(ATOM_TAG | atom.0)
    }

    pub(crate) const fn symbol(token: u32) -> Self {
        assert!(token < INLINE_INDEX_LIMIT);
        Self(SYMBOL_TAG | token)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn decode(self) -> KeyKind {
        match self.0 >> 30 {
            0 => KeyKind::Index(self.0),
            1 => KeyKind::String(Atom(self.0 & KEY_MASK)),
            2 => KeyKind::Symbol(self.0 & KEY_MASK),
            _ => unreachable!(),
        }
    }
}

impl Trace for PropKey {
    fn trace(&self, tracer: &Tracer<'_>) {
        tracer.key(*self);
    }
}

pub const FIRST_DYNAMIC_ATOM: u32 = BUILTIN_ATOMS.len() as u32 + 1;
const FIRST_SYMBOL_ATOM: u32 = 226;
const BUILTIN_LOOKUP_SIZE: usize = 512;
const BUILTIN_LOOKUP: [u16; BUILTIN_LOOKUP_SIZE] = builtin_lookup();
const FNV_OFFSET: u32 = 2166136261;

const fn hash_unit(hash: u32, unit: u16) -> u32 {
    (hash ^ unit as u32).wrapping_mul(16777619)
}

const fn builtin_slot(hash: u32) -> usize {
    hash as usize & (BUILTIN_LOOKUP_SIZE - 1)
}

const fn builtin_lookup() -> [u16; BUILTIN_LOOKUP_SIZE] {
    let mut lookup = [0; BUILTIN_LOOKUP_SIZE];
    let mut i = 0;
    while i < FIRST_SYMBOL_ATOM as usize - 1 {
        let bytes = BUILTIN_ATOMS[i].as_bytes();
        let mut hash = FNV_OFFSET;
        let mut j = 0;
        while j < bytes.len() {
            assert!(bytes[j].is_ascii());
            hash = hash_unit(hash, bytes[j] as u16);
            j += 1;
        }
        let mut slot = builtin_slot(hash);
        while lookup[slot] != 0 {
            slot = builtin_slot(slot as u32 + 1);
        }
        lookup[slot] = i as u16 + 1;
        i += 1;
    }
    lookup
}

fn lookup_builtin(hash: u32, mut matches: impl FnMut(&str) -> bool) -> Option<PropKey> {
    let mut slot = builtin_slot(hash);
    loop {
        let atom = Atom(u32::from(BUILTIN_LOOKUP[slot]));
        if matches(AtomTable::builtin(atom)?) {
            return Some(PropKey::atom(atom));
        }
        slot = builtin_slot(slot as u32 + 1);
    }
}

#[derive(Clone, Copy)]
struct DynamicAtom {
    atom: Atom,
    id: StrId,
    hash: u64,
}

#[derive(Default)]
pub struct AtomTable {
    dynamic: TrackedVec<DynamicAtom>,
    by_content: HashIndex,
    seed: RandomState,
    issued: u32,
}

impl AtomTable {
    pub fn builtin(atom: Atom) -> Option<&'static str> {
        BUILTIN_ATOMS.get(atom.0.checked_sub(1)? as usize).copied()
    }

    pub fn builtin_key(text: &str) -> Option<PropKey> {
        if !text.is_ascii() {
            return None;
        }
        let hash = text.bytes().map(u16::from).fold(FNV_OFFSET, hash_unit);
        lookup_builtin(hash, |name| name == text)
    }

    pub fn dynamic_string(&self, atom: Atom) -> Option<StrId> {
        let index = self
            .dynamic
            .binary_search_by_key(&atom.0, |entry| entry.atom.0)
            .ok()?;
        Some(self.dynamic[index].id)
    }

    fn content_hash(&self, units: impl Iterator<Item = u16>) -> u64 {
        let mut hasher = self.seed.build_hasher();
        for unit in units {
            hasher.write_u16(unit);
        }
        hasher.finish()
    }

    pub(crate) fn find(&self, strings: &Arena<JsString>, id: StrId) -> Option<PropKey> {
        let units = || StringUnits::new(strings, id);
        if let Some(index) = parse_index(units()).and_then(PropKey::index) {
            return Some(index);
        }
        let len = strings.get(id).unwrap().len();
        let hash = units().fold(FNV_OFFSET, hash_unit);
        if let Some(key) = lookup_builtin(hash, |name| {
            name.len() == len as usize && units().eq(name.encode_utf16())
        }) {
            return Some(key);
        }
        let hash = self.content_hash(units());
        let row = self.by_content.find(hash, |row| {
            let other = self.dynamic[row].id;
            other == id
                || (strings.get(other).unwrap().len() == len
                    && units().eq(StringUnits::new(strings, other)))
        })?;
        Some(PropKey::atom(self.dynamic[row].atom))
    }

    pub(crate) fn insert(
        &mut self,
        ledger: &HeapLedger,
        strings: &Arena<JsString>,
        id: StrId,
    ) -> AllocResult<PropKey> {
        let atom = FIRST_DYNAMIC_ATOM + self.issued;
        if atom >= INLINE_INDEX_LIMIT {
            return Err(AllocError::Limit);
        }
        let atom = Atom(atom);
        let hash = self.content_hash(StringUnits::new(strings, id));
        self.dynamic.try_reserve(ledger, 1)?;
        self.by_content.reserve(ledger)?;
        self.by_content.insert(hash, self.dynamic.len());
        self.dynamic.push_reserved(DynamicAtom { atom, id, hash });
        self.issued += 1;
        Ok(PropKey::atom(atom))
    }

    pub(crate) fn prune(&mut self, strings: &Arena<JsString>) {
        let mut to = 0;
        for from in 0..self.dynamic.len() {
            let entry = self.dynamic[from];
            if strings.is_marked(entry.id) {
                if from != to {
                    self.by_content.relocate(entry.hash, from, to);
                    self.dynamic[to] = entry;
                }
                to += 1;
            } else {
                self.by_content.remove(entry.hash, |row| row == from);
            }
        }
        self.dynamic.truncate(to);
    }

    pub fn array_index(&self, strings: &Arena<JsString>, key: PropKey) -> Option<u32> {
        match key.decode() {
            KeyKind::Index(index) => Some(index),
            KeyKind::String(atom) => {
                let id = self.dynamic_string(atom)?;
                parse_index(StringUnits::new(strings, id))
            }
            KeyKind::Symbol(_) => None,
        }
    }
}

pub fn parse_index(mut units: impl Iterator<Item = u16>) -> Option<u32> {
    let first = units.next()?.checked_sub(48)?;
    if first == 0 {
        return units.next().is_none().then_some(0);
    }
    if first > 9 {
        return None;
    }
    let mut value = u32::from(first);
    for unit in units {
        let digit = unit.checked_sub(48)?;
        if digit > 9 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    (value != u32::MAX).then_some(value)
}

// Ordered as in QuickJS-ng's quickjs-atom.h.
pub const BUILTIN_ATOMS: [&str; 241] = [
    "null",
    "false",
    "true",
    "if",
    "else",
    "return",
    "var",
    "this",
    "delete",
    "void",
    "typeof",
    "new",
    "in",
    "instanceof",
    "do",
    "while",
    "for",
    "break",
    "continue",
    "switch",
    "case",
    "default",
    "throw",
    "try",
    "catch",
    "finally",
    "function",
    "debugger",
    "with",
    "class",
    "const",
    "enum",
    "export",
    "extends",
    "import",
    "super",
    "using",
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "yield",
    "await",
    "",
    "keys",
    "size",
    "length",
    "message",
    "cause",
    "errors",
    "error",
    "suppressed",
    "stack",
    "name",
    "toString",
    "toLocaleString",
    "valueOf",
    "eval",
    "prototype",
    "constructor",
    "configurable",
    "writable",
    "enumerable",
    "value",
    "get",
    "set",
    "of",
    "__proto__",
    "undefined",
    "number",
    "boolean",
    "string",
    "object",
    "symbol",
    "integer",
    "unknown",
    "arguments",
    "callee",
    "caller",
    "<eval>",
    "<ret>",
    "<var>",
    "<arg_var>",
    "<with>",
    "<using_dispose>",
    "use",
    "dispose",
    "disposeAsync",
    "lastIndex",
    "target",
    "index",
    "input",
    "defineProperties",
    "apply",
    "join",
    "concat",
    "split",
    "construct",
    "getPrototypeOf",
    "setPrototypeOf",
    "isExtensible",
    "preventExtensions",
    "has",
    "deleteProperty",
    "defineProperty",
    "getOwnPropertyDescriptor",
    "ownKeys",
    "add",
    "done",
    "next",
    "values",
    "source",
    "flags",
    "global",
    "unicode",
    "raw",
    "rawJSON",
    "new.target",
    "this.active_func",
    "<home_object>",
    "<computed_field>",
    "<static_computed_field>",
    "<class_fields_init>",
    "<brand>",
    "#constructor",
    "as",
    "from",
    "fromAsync",
    "meta",
    "*default*",
    "*",
    "Module",
    "then",
    "resolve",
    "reject",
    "promise",
    "proxy",
    "revoke",
    "async",
    "exec",
    "groups",
    "indices",
    "status",
    "reason",
    "globalThis",
    "bigint",
    "not-equal",
    "timed-out",
    "ok",
    "toJSON",
    "maxByteLength",
    "zip",
    "zipKeyed",
    "Object",
    "Array",
    "Error",
    "Number",
    "String",
    "Boolean",
    "Symbol",
    "Arguments",
    "Math",
    "JSON",
    "Date",
    "Function",
    "GeneratorFunction",
    "ForInIterator",
    "RegExp",
    "ArrayBuffer",
    "SharedArrayBuffer",
    "Uint8ClampedArray",
    "Int8Array",
    "Uint8Array",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "BigInt64Array",
    "BigUint64Array",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "DataView",
    "BigInt",
    "WeakRef",
    "FinalizationRegistry",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "Iterator",
    "Iterator Concat",
    "Iterator Helper",
    "Iterator Wrap",
    "Map Iterator",
    "Set Iterator",
    "Array Iterator",
    "String Iterator",
    "RegExp String Iterator",
    "Generator",
    "Proxy",
    "Promise",
    "PromiseResolveFunction",
    "PromiseRejectFunction",
    "AsyncFunction",
    "AsyncFunctionResolve",
    "AsyncFunctionReject",
    "AsyncGeneratorFunction",
    "AsyncGenerator",
    "EvalError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "TypeError",
    "URIError",
    "InternalError",
    "DOMException",
    "CallSite",
    "DisposableStack",
    "AsyncDisposableStack",
    "SuppressedError",
    "<brand>",
    "Symbol.toPrimitive",
    "Symbol.iterator",
    "Symbol.match",
    "Symbol.matchAll",
    "Symbol.replace",
    "Symbol.search",
    "Symbol.split",
    "Symbol.toStringTag",
    "Symbol.isConcatSpreadable",
    "Symbol.hasInstance",
    "Symbol.species",
    "Symbol.unscopables",
    "Symbol.asyncIterator",
    "Symbol.dispose",
    "Symbol.asyncDispose",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_ids_and_indices() {
        for (id, name) in [
            (Atom::LENGTH, "length"),
            (Atom::MESSAGE, "message"),
            (Atom::NAME, "name"),
            (Atom::TO_STRING, "toString"),
            (Atom::VALUE_OF, "valueOf"),
            (Atom::PROTOTYPE, "prototype"),
        ] {
            assert_eq!(AtomTable::builtin(id), Some(name));
        }
        assert_eq!(AtomTable::builtin(Atom(FIRST_SYMBOL_ATOM)), Some("<brand>"));
        assert_eq!(FIRST_DYNAMIC_ATOM, 242);
        assert_eq!(parse_index("4294967294".encode_utf16()), Some(u32::MAX - 1));
        assert_eq!(parse_index("4294967295".encode_utf16()), None);
        assert_eq!(parse_index("00".encode_utf16()), None);
        assert_eq!(size_of::<PropKey>(), 4);
    }

    #[test]
    fn finds_builtin_atoms_by_text_and_code_units() {
        let ledger = HeapLedger::new(64 * 1024, 0).unwrap();
        let atoms = AtomTable::default();
        let mut strings = Arena::new();
        for (i, name) in BUILTIN_ATOMS[..FIRST_SYMBOL_ATOM as usize - 1]
            .iter()
            .enumerate()
        {
            let key = PropKey::atom(Atom(i as u32 + 1));
            let id = strings
                .insert(&ledger, JsString::from_str(&ledger, name).unwrap())
                .unwrap();
            assert_eq!(AtomTable::builtin_key(name), Some(key));
            assert_eq!(atoms.find(&strings, id), Some(key));
        }
        assert_eq!(AtomTable::builtin_key("Symbol.iterator"), None);
    }

    #[test]
    fn reuses_dynamic_atoms_by_content_and_prunes_dead_rows() {
        let ledger = HeapLedger::new(8192, 0).unwrap();
        let mut atoms = AtomTable::default();
        let mut strings = Arena::new();
        let mut flat = |text| {
            strings
                .insert(&ledger, JsString::from_str(&ledger, text).unwrap())
                .unwrap()
        };
        let dead = flat("discard");
        let left = flat("α");
        let right = flat("😀");
        let live = flat("α😀");
        let old = atoms.insert(&ledger, &strings, dead).unwrap();
        let kept = atoms.insert(&ledger, &strings, live).unwrap();
        let rope = strings
            .insert(
                &ledger,
                JsString::Rope {
                    left,
                    right,
                    len: 3,
                    depth: 1,
                    is_wide: true,
                },
            )
            .unwrap();
        assert_eq!(atoms.find(&strings, rope), Some(kept));
        strings.mark(live);
        atoms.prune(&strings);
        assert_eq!(atoms.find(&strings, rope), Some(kept));
        assert_eq!(atoms.find(&strings, dead), None);
        let KeyKind::String(old_atom) = old.decode() else {
            unreachable!()
        };
        let KeyKind::String(kept_atom) = kept.decode() else {
            unreachable!()
        };
        assert_eq!(atoms.dynamic_string(old_atom), None);
        assert_eq!(atoms.dynamic_string(kept_atom), Some(live));
        assert_ne!(atoms.insert(&ledger, &strings, dead).unwrap(), old);
    }
}
