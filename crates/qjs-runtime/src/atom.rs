use crate::arena::Arena;
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
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

#[derive(Default)]
pub struct AtomTable {
    dynamic: TrackedVec<(u32, StrId)>,
    issued: u32,
}

impl AtomTable {
    pub fn builtin(atom: Atom) -> Option<&'static str> {
        BUILTIN_ATOMS.get(atom.0.checked_sub(1)? as usize).copied()
    }

    pub fn builtin_key(text: &str) -> Option<PropKey> {
        let i = BUILTIN_ATOMS[..FIRST_SYMBOL_ATOM as usize - 1]
            .iter()
            .position(|&name| name == text)?;
        Some(PropKey::atom(Atom(i as u32 + 1)))
    }

    pub fn dynamic_string(&self, atom: Atom) -> Option<StrId> {
        let index = self
            .dynamic
            .binary_search_by_key(&atom.0, |entry| entry.0)
            .ok()?;
        Some(self.dynamic[index].1)
    }

    pub(crate) fn find(&self, strings: &Arena<JsString>, id: StrId) -> Option<PropKey> {
        let units = || StringUnits::new(strings, id);
        if let Some(index) = parse_index(units()).and_then(PropKey::index) {
            return Some(index);
        }
        for (i, builtin) in BUILTIN_ATOMS[..FIRST_SYMBOL_ATOM as usize - 1]
            .iter()
            .enumerate()
        {
            if units().eq(builtin.encode_utf16()) {
                return Some(PropKey::atom(Atom(i as u32 + 1)));
            }
        }
        for &(atom, other) in self.dynamic.iter() {
            if other == id || units().eq(StringUnits::new(strings, other)) {
                return Some(PropKey::atom(Atom(atom)));
            }
        }
        None
    }

    pub(crate) fn insert(&mut self, ledger: &HeapLedger, id: StrId) -> AllocResult<PropKey> {
        let atom = FIRST_DYNAMIC_ATOM + self.issued;
        if atom >= INLINE_INDEX_LIMIT {
            return Err(AllocError::Limit);
        }
        self.dynamic.push(ledger, (atom, id))?;
        self.issued += 1;
        Ok(PropKey::atom(Atom(atom)))
    }

    pub(crate) fn prune(&mut self, strings: &Arena<JsString>) {
        self.dynamic.retain(|&(_, id)| strings.is_marked(id));
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
}
