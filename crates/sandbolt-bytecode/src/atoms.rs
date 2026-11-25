//! Atom IDs shared by the lexer, compiler and VM.

use std::fmt;
use std::sync::OnceLock;

macro_rules! builtin_atoms {
    ($first:ident => $first_text:literal, $($atom:ident => $text:literal,)+) => {
        /// Predefined atoms in QuickJS-ng v0.17.0 ID order.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u32)]
        pub enum BuiltinAtom {
            $first = 1,
            $($atom,)+
        }

        impl BuiltinAtom {
            /// All predefined atoms in ID order, excluding the zero sentinel.
            pub const ALL: &'static [Self] = &[Self::$first, $(Self::$atom,)+];

            pub const fn as_str(&self) -> &'static str {
                match self {
                    Self::$first => $first_text,
                    $(Self::$atom => $text,)+
                }
            }
        }
    };
}

// From quickjs-atom.h. The reference and its license are in tests/reference.
builtin_atoms! {
    Null => "null",
    False => "false",
    True => "true",
    If => "if",
    Else => "else",
    Return => "return",
    Var => "var",
    This => "this",
    Delete => "delete",
    Void => "void",
    Typeof => "typeof",
    New => "new",
    In => "in",
    Instanceof => "instanceof",
    Do => "do",
    While => "while",
    For => "for",
    Break => "break",
    Continue => "continue",
    Switch => "switch",
    Case => "case",
    Default => "default",
    Throw => "throw",
    Try => "try",
    Catch => "catch",
    Finally => "finally",
    FunctionKeyword => "function",
    Debugger => "debugger",
    With => "with",
    Class => "class",
    Const => "const",
    Enum => "enum",
    Export => "export",
    Extends => "extends",
    Import => "import",
    Super => "super",
    Using => "using",
    Implements => "implements",
    Interface => "interface",
    Let => "let",
    Package => "package",
    Private => "private",
    Protected => "protected",
    Public => "public",
    Static => "static",
    Yield => "yield",
    Await => "await",
    EmptyString => "",
    Keys => "keys",
    Size => "size",
    Length => "length",
    Message => "message",
    Cause => "cause",
    Errors => "errors",
    ErrorName => "error",
    Suppressed => "suppressed",
    Stack => "stack",
    Name => "name",
    ToString => "toString",
    ToLocaleString => "toLocaleString",
    ValueOf => "valueOf",
    Eval => "eval",
    Prototype => "prototype",
    Constructor => "constructor",
    Configurable => "configurable",
    Writable => "writable",
    Enumerable => "enumerable",
    Value => "value",
    Get => "get",
    SetName => "set",
    Of => "of",
    Proto => "__proto__",
    Undefined => "undefined",
    NumberType => "number",
    BooleanType => "boolean",
    StringType => "string",
    ObjectType => "object",
    SymbolType => "symbol",
    Integer => "integer",
    Unknown => "unknown",
    ArgumentsName => "arguments",
    Callee => "callee",
    Caller => "caller",
    InternalEval => "<eval>",
    InternalRet => "<ret>",
    InternalVar => "<var>",
    InternalArgVar => "<arg_var>",
    InternalWith => "<with>",
    InternalUsingDispose => "<using_dispose>",
    Use => "use",
    Dispose => "dispose",
    DisposeAsync => "disposeAsync",
    LastIndex => "lastIndex",
    Target => "target",
    Index => "index",
    Input => "input",
    DefineProperties => "defineProperties",
    Apply => "apply",
    Join => "join",
    Concat => "concat",
    Split => "split",
    Construct => "construct",
    GetPrototypeOf => "getPrototypeOf",
    SetPrototypeOf => "setPrototypeOf",
    IsExtensible => "isExtensible",
    PreventExtensions => "preventExtensions",
    Has => "has",
    DeleteProperty => "deleteProperty",
    DefineProperty => "defineProperty",
    GetOwnPropertyDescriptor => "getOwnPropertyDescriptor",
    OwnKeys => "ownKeys",
    Add => "add",
    Done => "done",
    Next => "next",
    Values => "values",
    Source => "source",
    Flags => "flags",
    Global => "global",
    Unicode => "unicode",
    Raw => "raw",
    RawJSON => "rawJSON",
    NewTarget => "new.target",
    ThisActiveFunc => "this.active_func",
    HomeObject => "<home_object>",
    ComputedField => "<computed_field>",
    StaticComputedField => "<static_computed_field>",
    ClassFieldsInit => "<class_fields_init>",
    Brand => "<brand>",
    HashConstructor => "#constructor",
    As => "as",
    From => "from",
    FromAsync => "fromAsync",
    Meta => "meta",
    DefaultExport => "*default*",
    Star => "*",
    Module => "Module",
    Then => "then",
    Resolve => "resolve",
    Reject => "reject",
    PromiseName => "promise",
    ProxyName => "proxy",
    Revoke => "revoke",
    Async => "async",
    Exec => "exec",
    Groups => "groups",
    Indices => "indices",
    Status => "status",
    Reason => "reason",
    GlobalThis => "globalThis",
    Bigint => "bigint",
    NotEqual => "not-equal",
    TimedOut => "timed-out",
    Ok => "ok",
    ToJSON => "toJSON",
    MaxByteLength => "maxByteLength",
    Zip => "zip",
    ZipKeyed => "zipKeyed",
    Object => "Object",
    Array => "Array",
    Error => "Error",
    Number => "Number",
    String => "String",
    Boolean => "Boolean",
    Symbol => "Symbol",
    Arguments => "Arguments",
    Math => "Math",
    JSON => "JSON",
    Date => "Date",
    Function => "Function",
    GeneratorFunction => "GeneratorFunction",
    ForInIterator => "ForInIterator",
    RegExp => "RegExp",
    ArrayBuffer => "ArrayBuffer",
    SharedArrayBuffer => "SharedArrayBuffer",
    Uint8ClampedArray => "Uint8ClampedArray",
    Int8Array => "Int8Array",
    Uint8Array => "Uint8Array",
    Int16Array => "Int16Array",
    Uint16Array => "Uint16Array",
    Int32Array => "Int32Array",
    Uint32Array => "Uint32Array",
    BigInt64Array => "BigInt64Array",
    BigUint64Array => "BigUint64Array",
    Float16Array => "Float16Array",
    Float32Array => "Float32Array",
    Float64Array => "Float64Array",
    DataView => "DataView",
    BigInt => "BigInt",
    WeakRef => "WeakRef",
    FinalizationRegistry => "FinalizationRegistry",
    Map => "Map",
    Set => "Set",
    WeakMap => "WeakMap",
    WeakSet => "WeakSet",
    Iterator => "Iterator",
    IteratorConcat => "Iterator Concat",
    IteratorHelper => "Iterator Helper",
    IteratorWrap => "Iterator Wrap",
    MapIterator => "Map Iterator",
    SetIterator => "Set Iterator",
    ArrayIterator => "Array Iterator",
    StringIterator => "String Iterator",
    RegExpStringIterator => "RegExp String Iterator",
    Generator => "Generator",
    Proxy => "Proxy",
    Promise => "Promise",
    PromiseResolveFunction => "PromiseResolveFunction",
    PromiseRejectFunction => "PromiseRejectFunction",
    AsyncFunction => "AsyncFunction",
    AsyncFunctionResolve => "AsyncFunctionResolve",
    AsyncFunctionReject => "AsyncFunctionReject",
    AsyncGeneratorFunction => "AsyncGeneratorFunction",
    AsyncGenerator => "AsyncGenerator",
    EvalError => "EvalError",
    RangeError => "RangeError",
    ReferenceError => "ReferenceError",
    SyntaxError => "SyntaxError",
    TypeError => "TypeError",
    URIError => "URIError",
    InternalError => "InternalError",
    DOMException => "DOMException",
    CallSite => "CallSite",
    DisposableStack => "DisposableStack",
    AsyncDisposableStack => "AsyncDisposableStack",
    SuppressedError => "SuppressedError",
    PrivateBrand => "<brand>",
    SymbolToPrimitive => "Symbol.toPrimitive",
    SymbolIterator => "Symbol.iterator",
    SymbolMatch => "Symbol.match",
    SymbolMatchAll => "Symbol.matchAll",
    SymbolReplace => "Symbol.replace",
    SymbolSearch => "Symbol.search",
    SymbolSplit => "Symbol.split",
    SymbolToStringTag => "Symbol.toStringTag",
    SymbolIsConcatSpreadable => "Symbol.isConcatSpreadable",
    SymbolHasInstance => "Symbol.hasInstance",
    SymbolSpecies => "Symbol.species",
    SymbolUnscopables => "Symbol.unscopables",
    SymbolAsyncIterator => "Symbol.asyncIterator",
    SymbolDispose => "Symbol.dispose",
    SymbolAsyncDispose => "Symbol.asyncDispose",
}

/// Number of predefined atoms, excluding [`UnitAtom::NONE`].
pub const BUILTIN_ATOM_COUNT: u32 = BuiltinAtom::ALL.len() as u32;
/// First ID allocated in the compile unit's atom table.
pub const FIRST_DYNAMIC_ATOM: u32 = BUILTIN_ATOM_COUNT + 1;

impl BuiltinAtom {
    pub const fn from_u32(id: u32) -> Option<Self> {
        if id == 0 || id > BUILTIN_ATOM_COUNT {
            None
        } else {
            Some(Self::ALL[(id - 1) as usize])
        }
    }

    /// The predefined atom spelled `text`, the first in ID order where two
    /// share a spelling.
    pub fn from_text(text: &[u8]) -> Option<Self> {
        static BY_TEXT: OnceLock<Vec<(&'static [u8], BuiltinAtom)>> = OnceLock::new();
        let by_text = BY_TEXT.get_or_init(|| {
            let mut all: Vec<_> = Self::ALL
                .iter()
                .map(|&atom| (atom.as_str().as_bytes(), atom))
                .collect();
            all.sort_by_key(|&(text, atom)| (text, atom as u32));
            all.dedup_by_key(|&mut (text, _)| text);
            all
        });
        by_text
            .binary_search_by_key(&text, |&(spelling, _)| spelling)
            .ok()
            .map(|i| by_text[i].1)
    }

    /// Looks up the keyword and reserved-word prefix, including `using`
    /// and `await`. The parser decides when contextual words are reserved.
    pub fn from_str_keyword(text: &str) -> Option<Self> {
        Self::ALL[..Self::Await as usize]
            .iter()
            .copied()
            .find(|atom| atom.as_str() == text)
    }

    pub const fn id(self) -> UnitAtom {
        UnitAtom(self as u32)
    }
}

impl fmt::Display for BuiltinAtom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Compile-unit-local atom ID. Dynamic IDs index the image's atom table
/// after subtracting [`FIRST_DYNAMIC_ATOM`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct UnitAtom(pub u32);

impl UnitAtom {
    pub const NONE: Self = Self(0);

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    pub const fn builtin(self) -> Option<BuiltinAtom> {
        BuiltinAtom::from_u32(self.0)
    }

    pub const fn from_dynamic_index(index: u32) -> Option<Self> {
        match index.checked_add(FIRST_DYNAMIC_ATOM) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    pub const fn dynamic_index(self) -> Option<u32> {
        self.0.checked_sub(FIRST_DYNAMIC_ATOM)
    }

    pub const fn is_in_bounds(self, dynamic_count: u32) -> bool {
        match self.dynamic_index() {
            Some(index) => index < dynamic_count,
            None => !self.is_none(),
        }
    }
}

impl From<BuiltinAtom> for UnitAtom {
    fn from(atom: BuiltinAtom) -> Self {
        atom.id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_text_finds_the_first_atom_of_each_spelling() {
        for atom in BuiltinAtom::ALL {
            let first = BuiltinAtom::ALL
                .iter()
                .find(|other| other.as_str() == atom.as_str());
            assert_eq!(
                BuiltinAtom::from_text(atom.as_str().as_bytes()).as_ref(),
                first
            );
        }
        for text in ["", "lengthy", "Length", "toStrin", "\u{e9}"] {
            let expected = BuiltinAtom::ALL.iter().find(|atom| atom.as_str() == text);
            assert_eq!(
                BuiltinAtom::from_text(text.as_bytes()).as_ref(),
                expected,
                "{text:?}"
            );
        }
    }
}
