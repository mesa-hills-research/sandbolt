//! Versioned Rust image format. The payload uses QuickJS-ng v0.17.0 opcode
//! bytes, but the container is separate from `JS_WriteObject`.
//!
//! The magic and version precede the entry function, atom table, functions
//! and modules in that order. Counts and indices use unsigned LEB128. Signed
//! integers and source-offset deltas use zigzag LEB128. Floats and UTF-16
//! code units use little-endian bytes. Tables retain their input order.

use crate::image::*;
use crate::{ImageVerifyError, UnitAtom, verify_image};
use std::fmt;

pub const IMAGE_MAGIC: &[u8; 8] = b"QJSRBC\0\0";
pub const IMAGE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError {
    pub offset: usize,
    pub kind: CodecErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecErrorKind {
    BadMagic,
    UnsupportedVersion(u32),
    Truncated,
    TrailingBytes,
    InvalidLeb128,
    IntegerOverflow,
    InvalidTag(&'static str, u8),
    InvalidFlags(&'static str),
    InvalidUtf8,
    Allocation,
    InvalidImage(ImageVerifyError),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "image byte {}: ", self.offset)?;
        match &self.kind {
            CodecErrorKind::BadMagic => f.write_str("invalid image magic"),
            CodecErrorKind::UnsupportedVersion(version) => {
                write!(f, "unsupported image version {version}")
            }
            CodecErrorKind::Truncated => f.write_str("truncated image"),
            CodecErrorKind::TrailingBytes => f.write_str("trailing bytes after image"),
            CodecErrorKind::InvalidLeb128 => f.write_str("invalid or noncanonical LEB128"),
            CodecErrorKind::IntegerOverflow => f.write_str("integer out of range"),
            CodecErrorKind::InvalidTag(field, tag) => write!(f, "invalid {field} tag {tag}"),
            CodecErrorKind::InvalidFlags(field) => write!(f, "reserved {field} flag is set"),
            CodecErrorKind::InvalidUtf8 => f.write_str("invalid UTF-8 text"),
            CodecErrorKind::Allocation => f.write_str("cannot allocate image data"),
            CodecErrorKind::InvalidImage(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            CodecErrorKind::InvalidImage(error) => Some(error),
            _ => None,
        }
    }
}

type Result<T> = std::result::Result<T, CodecError>;

/// Checks the image and encodes it in the current wire format.
pub fn encode_image(image: &ProgramImage) -> Result<Vec<u8>> {
    verify_image(image).map_err(|error| CodecError {
        offset: 0,
        kind: CodecErrorKind::InvalidImage(error),
    })?;
    let mut w = Writer { bytes: Vec::new() };
    w.bytes(IMAGE_MAGIC)?;
    w.uint(IMAGE_VERSION)?;
    w.uint(image.entry_function)?;
    w.list(&image.atoms, write_const_string)?;
    w.list(&image.functions, write_function)?;
    w.list(&image.modules, write_module)?;
    Ok(w.bytes)
}

/// Decodes a complete image and checks all bytecode and table references.
pub fn decode_image(bytes: &[u8]) -> Result<ProgramImage> {
    let mut r = Reader {
        remaining: bytes,
        offset: 0,
    };
    if r.take(IMAGE_MAGIC.len())? != IMAGE_MAGIC {
        return Err(CodecError {
            offset: 0,
            kind: CodecErrorKind::BadMagic,
        });
    }
    let version = r.uint()?;
    if version != IMAGE_VERSION {
        return Err(r.error(CodecErrorKind::UnsupportedVersion(version)));
    }
    let image = ProgramImage {
        entry_function: r.uint()?,
        atoms: r.list(2, read_const_string)?,
        functions: r.list(1, read_function)?,
        modules: r.list(1, read_module)?,
    };
    if !r.remaining.is_empty() {
        return Err(r.error(CodecErrorKind::TrailingBytes));
    }
    verify_image(&image).map_err(|error| r.error(CodecErrorKind::InvalidImage(error)))?;
    Ok(image)
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn error(&self, kind: CodecErrorKind) -> CodecError {
        CodecError {
            offset: self.bytes.len(),
            kind,
        }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| self.error(CodecErrorKind::Allocation))?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn byte(&mut self, byte: u8) -> Result<()> {
        self.bytes(&[byte])
    }

    fn leb(&mut self, mut value: u64) -> Result<()> {
        let mut output = [0; 10];
        let mut count = 0;
        while value >= 128 {
            output[count] = value as u8 | 0x80;
            value >>= 7;
            count += 1;
        }
        output[count] = value as u8;
        self.bytes(&output[..=count])
    }

    fn uint(&mut self, value: impl Into<u32>) -> Result<()> {
        self.leb(u64::from(value.into()))
    }

    fn signed(&mut self, value: i64) -> Result<()> {
        self.leb(((value << 1) ^ (value >> 63)) as u64)
    }

    fn count(&mut self, len: usize) -> Result<()> {
        self.uint(u32::try_from(len).map_err(|_| self.error(CodecErrorKind::IntegerOverflow))?)
    }

    fn atom(&mut self, atom: UnitAtom) -> Result<()> {
        self.uint(atom.raw())
    }

    fn blob(&mut self, bytes: &[u8]) -> Result<()> {
        self.count(bytes.len())?;
        self.bytes(bytes)
    }

    fn option<T>(
        &mut self,
        value: Option<T>,
        write: impl FnOnce(&mut Self, T) -> Result<()>,
    ) -> Result<()> {
        self.byte(u8::from(value.is_some()))?;
        if let Some(value) = value {
            write(self, value)?;
        }
        Ok(())
    }

    fn list<T>(&mut self, values: &[T], write: impl Fn(&mut Self, &T) -> Result<()>) -> Result<()> {
        self.count(values.len())?;
        for value in values {
            write(self, value)?;
        }
        Ok(())
    }
}

struct Reader<'a> {
    remaining: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn error(&self, kind: CodecErrorKind) -> CodecError {
        CodecError {
            offset: self.offset,
            kind,
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (bytes, rest) = self
            .remaining
            .split_at_checked(len)
            .ok_or_else(|| self.error(CodecErrorKind::Truncated))?;
        self.remaining = rest;
        self.offset += len;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn leb(&mut self) -> Result<u64> {
        let mut value = 0;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            if shift == 63 && byte > 1 || shift > 0 && byte == 0 {
                return Err(self.error(CodecErrorKind::InvalidLeb128));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 128 {
                return Ok(value);
            }
        }
        Err(self.error(CodecErrorKind::InvalidLeb128))
    }

    fn uint(&mut self) -> Result<u32> {
        u32::try_from(self.leb()?).map_err(|_| self.error(CodecErrorKind::IntegerOverflow))
    }

    fn uint16(&mut self) -> Result<u16> {
        u16::try_from(self.leb()?).map_err(|_| self.error(CodecErrorKind::IntegerOverflow))
    }

    fn signed(&mut self) -> Result<i64> {
        let value = self.leb()?;
        Ok((value >> 1) as i64 ^ -((value & 1) as i64))
    }

    fn atom(&mut self) -> Result<UnitAtom> {
        self.uint().map(UnitAtom)
    }

    fn count(&mut self, min_bytes: usize) -> Result<usize> {
        let len = usize::try_from(self.uint()?)
            .map_err(|_| self.error(CodecErrorKind::IntegerOverflow))?;
        if len > self.remaining.len() / min_bytes {
            return Err(self.error(CodecErrorKind::Truncated));
        }
        Ok(len)
    }

    fn bytes(&mut self) -> Result<Vec<u8>> {
        let len = self.count(1)?;
        let input = self.take(len)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(len)
            .map_err(|_| self.error(CodecErrorKind::Allocation))?;
        output.extend_from_slice(input);
        Ok(output)
    }

    fn text(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| self.error(CodecErrorKind::InvalidUtf8))
    }

    fn boolean(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(self.error(CodecErrorKind::InvalidTag("boolean", tag))),
        }
    }

    fn flags(&mut self, mask: u32, field: &'static str) -> Result<u32> {
        let flags = self.uint()?;
        if flags & !mask != 0 {
            return Err(self.error(CodecErrorKind::InvalidFlags(field)));
        }
        Ok(flags)
    }

    fn option<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if self.boolean()? {
            read(self).map(Some)
        } else {
            Ok(None)
        }
    }

    fn list<T>(
        &mut self,
        min_bytes: usize,
        mut read: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let len = self.count(min_bytes)?;
        let mut values = Vec::new();
        for _ in 0..len {
            let value = read(self)?;
            values
                .try_reserve(1)
                .map_err(|_| self.error(CodecErrorKind::Allocation))?;
            values.push(value);
        }
        Ok(values)
    }
}

macro_rules! wire_enum {
    ($writer:ident, $reader:ident, $ty:ty, $name:literal, {$($first:ident $(::$part:ident)* $(($arg:path))? => $tag:literal),+ $(,)?}) => {
        fn $writer(w: &mut Writer, value: $ty) -> Result<()> {
            w.byte(match value { $($first $(::$part)* $(($arg))? => $tag,)+ })
        }

        fn $reader(r: &mut Reader<'_>) -> Result<$ty> {
            match r.byte()? {
                $($tag => Ok($first $(::$part)* $(($arg))?),)+
                tag => Err(r.error(CodecErrorKind::InvalidTag($name, tag))),
            }
        }
    };
}

wire_enum!(write_function_kind, read_function_kind, Option<FunctionKind>, "function kind", {
    None => 0,
    Some(FunctionKind::Normal) => 1,
    Some(FunctionKind::Generator) => 2,
    Some(FunctionKind::Async) => 3,
    Some(FunctionKind::AsyncGenerator) => 4,
});

wire_enum!(write_var_kind, read_var_kind, VarKind, "binding kind", {
    VarKind::Var => 0,
    VarKind::Let => 1,
    VarKind::Const => 2,
    VarKind::Catch => 3,
    VarKind::Using => 4,
    VarKind::AwaitUsing => 5,
    VarKind::FunctionName => 6,
    VarKind::FunctionDeclaration => 7,
    VarKind::AsyncOrGeneratorDeclaration => 8,
    VarKind::PrivateField => 9,
    VarKind::PrivateMethod => 10,
    VarKind::PrivateGetter => 11,
    VarKind::PrivateSetter => 12,
    VarKind::PrivateGetterSetter => 13,
    VarKind::DisposeMethod => 14,
});

wire_enum!(write_closure_kind, read_closure_kind, ClosureKind, "closure kind", {
    ClosureKind::Local => 0,
    ClosureKind::Argument => 1,
    ClosureKind::Ref => 2,
    ClosureKind::GlobalRef => 3,
    ClosureKind::GlobalDeclaration => 4,
    ClosureKind::Global => 5,
    ClosureKind::ModuleDeclaration => 6,
    ClosureKind::ModuleImport => 7,
});

fn write_const_string(w: &mut Writer, value: &ConstString) -> Result<()> {
    match value {
        ConstString::Latin1(bytes) => {
            w.byte(0)?;
            w.blob(bytes)
        }
        ConstString::Utf16(units) => {
            w.byte(1)?;
            w.list(units, |w, unit| w.bytes(&unit.to_le_bytes()))
        }
    }
}

fn read_const_string(r: &mut Reader<'_>) -> Result<ConstString> {
    match r.byte()? {
        0 => r.bytes().map(ConstString::Latin1),
        1 => r
            .list(2, |r| {
                let bytes = r.take(2)?;
                Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
            })
            .map(ConstString::Utf16),
        tag => Err(r.error(CodecErrorKind::InvalidTag("string", tag))),
    }
}

fn write_constant(w: &mut Writer, entry: &ConstEntry) -> Result<()> {
    match entry {
        ConstEntry::Int(value) => {
            w.byte(0)?;
            w.signed(i64::from(*value))
        }
        ConstEntry::Float(value) => {
            w.byte(1)?;
            w.bytes(&value.to_bits().to_le_bytes())
        }
        ConstEntry::String(value) => {
            w.byte(2)?;
            write_const_string(w, value)
        }
        ConstEntry::BigInt(value) => {
            w.byte(3)?;
            w.blob(value.as_bytes())
        }
        ConstEntry::RegExp { pattern, flags } => {
            w.byte(4)?;
            write_const_string(w, pattern)?;
            w.uint(*flags)
        }
        ConstEntry::Function(index) => {
            w.byte(5)?;
            w.uint(*index)
        }
        ConstEntry::TemplateObject(value) => {
            w.byte(6)?;
            w.list(&value.cooked, |w, value| {
                w.option(value.as_ref(), write_const_string)
            })?;
            w.list(&value.raw, write_const_string)
        }
    }
}

fn read_constant(r: &mut Reader<'_>) -> Result<ConstEntry> {
    Ok(match r.byte()? {
        0 => ConstEntry::Int(
            i32::try_from(r.signed()?).map_err(|_| r.error(CodecErrorKind::IntegerOverflow))?,
        ),
        1 => {
            let mut bits = [0; 8];
            bits.copy_from_slice(r.take(8)?);
            ConstEntry::Float(f64::from_bits(u64::from_le_bytes(bits)))
        }
        2 => ConstEntry::String(read_const_string(r)?),
        3 => ConstEntry::BigInt(r.text()?),
        4 => ConstEntry::RegExp {
            pattern: read_const_string(r)?,
            flags: r.uint16()?,
        },
        5 => ConstEntry::Function(r.uint()?),
        6 => ConstEntry::TemplateObject(TemplateObject {
            cooked: r.list(1, |r| r.option(read_const_string))?,
            raw: r.list(2, read_const_string)?,
        }),
        tag => return Err(r.error(CodecErrorKind::InvalidTag("constant", tag))),
    })
}

fn bits<const N: usize>(flags: [bool; N]) -> u32 {
    flags
        .into_iter()
        .enumerate()
        .fold(0, |mask, (i, set)| mask | (u32::from(set) << i))
}

fn write_debug(w: &mut Writer, debug: &DebugInfo) -> Result<()> {
    w.blob(debug.filename.as_bytes())?;
    w.uint(debug.source_start)?;
    w.uint(debug.source_end)?;
    w.count(debug.pc_map.len())?;
    let (mut pc, mut offset) = (0, debug.source_start);
    for point in &debug.pc_map {
        w.uint(point.pc - pc)?;
        w.signed(i64::from(point.byte_offset) - i64::from(offset))?;
        (pc, offset) = (point.pc, point.byte_offset);
    }
    Ok(())
}

fn read_debug(r: &mut Reader<'_>) -> Result<DebugInfo> {
    let filename = r.text()?;
    let source_start = r.uint()?;
    let source_end = r.uint()?;
    let (mut pc, mut offset) = (0_u32, source_start);
    let pc_map = r.list(2, |r| {
        pc = pc
            .checked_add(r.uint()?)
            .ok_or_else(|| r.error(CodecErrorKind::IntegerOverflow))?;
        let delta = r.signed()?;
        offset = i64::from(offset)
            .checked_add(delta)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| r.error(CodecErrorKind::IntegerOverflow))?;
        Ok(PcSourceMap {
            pc,
            byte_offset: offset,
        })
    })?;
    Ok(DebugInfo {
        filename,
        source_start,
        source_end,
        pc_map,
    })
}

fn write_var(w: &mut Writer, var: &VarDef) -> Result<()> {
    w.atom(var.name)?;
    write_var_kind(w, var.kind)?;
    w.uint(var.scope_level)?;
    w.uint(match var.scope_next {
        ScopeLink::End => 0,
        ScopeLink::ParameterEnd => 1,
        ScopeLink::Local(index) => u32::from(index) + 2,
    })?;
    w.uint(bits([var.is_const, var.lexical_tdz, var.captured]))?;
    w.option(var.var_ref_index, Writer::uint)
}

fn read_var(r: &mut Reader<'_>) -> Result<VarDef> {
    let name = r.atom()?;
    let kind = read_var_kind(r)?;
    let scope_level = r.uint16()?;
    let scope_next = match r.uint()? {
        0 => ScopeLink::End,
        1 => ScopeLink::ParameterEnd,
        index => ScopeLink::Local(
            u16::try_from(index - 2).map_err(|_| r.error(CodecErrorKind::IntegerOverflow))?,
        ),
    };
    let flags = r.flags(7, "binding")?;
    Ok(VarDef {
        name,
        kind,
        scope_level,
        scope_next,
        is_const: flags & 1 != 0,
        lexical_tdz: flags & 2 != 0,
        captured: flags & 4 != 0,
        var_ref_index: r.option(Reader::uint16)?,
    })
}

fn write_function(w: &mut Writer, func: &FunctionBytecode) -> Result<()> {
    let f = &func.flags;
    w.atom(func.name)?;
    write_function_kind(w, func.kind)?;
    w.uint(bits([
        func.strict,
        f.has_prototype,
        f.has_simple_parameter_list,
        f.is_derived_class_constructor,
        f.need_home_object,
        f.new_target_allowed,
        f.super_call_allowed,
        f.super_allowed,
        f.arguments_allowed,
        f.backtrace_barrier,
    ]))?;
    w.uint(func.arg_count)?;
    w.uint(func.var_count)?;
    w.uint(func.defined_arg_count)?;
    w.uint(func.stack_size)?;
    w.uint(func.var_ref_count)?;
    w.uint(func.atom_count)?;
    w.list(&func.vars, write_var)?;
    w.list(&func.closure_vars, |w, var| {
        w.atom(var.name)?;
        write_closure_kind(w, var.kind)?;
        w.uint(var.index)?;
        write_var_kind(w, var.var_kind)?;
        w.uint(bits([var.is_const, var.lexical_tdz]))
    })?;
    w.list(&func.scopes, |w, scope| {
        w.option(scope.parent, Writer::uint)?;
        w.uint(scope.start_pc)?;
        w.uint(scope.end_pc)?;
        w.uint(bits([scope.has_using, scope.has_await_using]))
    })?;
    w.list(&func.exception_handlers, |w, handler| {
        w.uint(handler.start_pc)?;
        w.uint(handler.end_pc)?;
        w.uint(handler.handler_pc)?;
        w.uint(handler.stack_depth)
    })?;
    w.list(&func.constants, write_constant)?;
    w.blob(&func.code)?;
    write_debug(w, &func.debug)
}

fn read_function(r: &mut Reader<'_>) -> Result<FunctionBytecode> {
    let name = r.atom()?;
    let kind = read_function_kind(r)?;
    let flags = r.flags(0x3ff, "function")?;
    let set = |index: u32| flags & (1 << index) != 0;
    Ok(FunctionBytecode {
        name,
        kind,
        strict: set(0),
        flags: FunctionFlags {
            has_prototype: set(1),
            has_simple_parameter_list: set(2),
            is_derived_class_constructor: set(3),
            need_home_object: set(4),
            new_target_allowed: set(5),
            super_call_allowed: set(6),
            super_allowed: set(7),
            arguments_allowed: set(8),
            backtrace_barrier: set(9),
        },
        arg_count: r.uint16()?,
        var_count: r.uint16()?,
        defined_arg_count: r.uint16()?,
        stack_size: r.uint16()?,
        var_ref_count: r.uint16()?,
        atom_count: r.uint()?,
        vars: r.list(6, read_var)?,
        closure_vars: r.list(5, |r| {
            let name = r.atom()?;
            let kind = read_closure_kind(r)?;
            let index = r.uint16()?;
            let var_kind = read_var_kind(r)?;
            let flags = r.flags(3, "closure")?;
            Ok(ClosureVarDef {
                name,
                kind,
                index,
                var_kind,
                is_const: flags & 1 != 0,
                lexical_tdz: flags & 2 != 0,
            })
        })?,
        scopes: r.list(4, |r| {
            let parent = r.option(Reader::uint16)?;
            let start_pc = r.uint()?;
            let end_pc = r.uint()?;
            let flags = r.flags(3, "scope")?;
            Ok(ScopeInfo {
                parent,
                start_pc,
                end_pc,
                has_using: flags & 1 != 0,
                has_await_using: flags & 2 != 0,
            })
        })?,
        exception_handlers: r.list(4, |r| {
            Ok(ExceptionHandler {
                start_pc: r.uint()?,
                end_pc: r.uint()?,
                handler_pc: r.uint()?,
                stack_depth: r.uint16()?,
            })
        })?,
        constants: r.list(1, read_constant)?,
        code: r.bytes()?,
        debug: read_debug(r)?,
    })
}

fn write_module(w: &mut Writer, module: &ModuleRecord) -> Result<()> {
    w.atom(module.name)?;
    w.uint(module.function)?;
    w.byte(u8::from(module.has_top_level_await))?;
    w.list(&module.requested_modules, |w, request| {
        w.atom(request.name)?;
        w.list(&request.attributes, |w, attr| {
            w.atom(attr.key)?;
            w.atom(attr.value)
        })
    })?;
    w.list(&module.imports, |w, import| {
        w.uint(import.requested_module)?;
        w.option(import.import_name, Writer::atom)?;
        w.uint(import.closure_var)
    })?;
    w.list(&module.exports, |w, export| match *export {
        ExportEntry::Local {
            export_name,
            closure_var,
        } => {
            w.byte(0)?;
            w.atom(export_name)?;
            w.uint(closure_var)
        }
        ExportEntry::Indirect {
            export_name,
            requested_module,
            import_name,
        } => {
            w.byte(1)?;
            w.atom(export_name)?;
            w.uint(requested_module)?;
            w.option(import_name, Writer::atom)
        }
        ExportEntry::Star { requested_module } => {
            w.byte(2)?;
            w.uint(requested_module)
        }
    })
}

fn read_module(r: &mut Reader<'_>) -> Result<ModuleRecord> {
    Ok(ModuleRecord {
        name: r.atom()?,
        function: r.uint()?,
        has_top_level_await: r.boolean()?,
        requested_modules: r.list(2, |r| {
            Ok(RequestedModule {
                name: r.atom()?,
                attributes: r.list(2, |r| {
                    Ok(ImportAttribute {
                        key: r.atom()?,
                        value: r.atom()?,
                    })
                })?,
            })
        })?,
        imports: r.list(3, |r| {
            Ok(ImportEntry {
                requested_module: r.uint()?,
                import_name: r.option(Reader::atom)?,
                closure_var: r.uint16()?,
            })
        })?,
        exports: r.list(2, |r| {
            Ok(match r.byte()? {
                0 => ExportEntry::Local {
                    export_name: r.atom()?,
                    closure_var: r.uint16()?,
                },
                1 => ExportEntry::Indirect {
                    export_name: r.atom()?,
                    requested_module: r.uint()?,
                    import_name: r.option(Reader::atom)?,
                },
                2 => ExportEntry::Star {
                    requested_module: r.uint()?,
                },
                tag => return Err(r.error(CodecErrorKind::InvalidTag("export", tag))),
            })
        })?,
    })
}
