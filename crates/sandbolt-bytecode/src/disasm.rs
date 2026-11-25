//! Text listings for bytecode, constants and module bindings.

use crate::{
    AtomName, ConstEntry, ConstString, ExportEntry, FunctionBytecode, Instruction, Operands,
    ProgramImage, UnitAtom,
};
use std::fmt::{self, Write as _};

/// Lists one function. Dynamic atom IDs are shown by number unless an image
/// supplies their names. A malformed byte stream ends with a decode error.
pub fn disassemble_function(func: &FunctionBytecode) -> String {
    let mut out = String::new();
    write_function(&mut out, None, func, None).expect("writing to String");
    out
}

/// Lists functions and module bindings in table order.
pub fn disassemble_image(image: &ProgramImage) -> String {
    let mut out = String::new();
    write_image(&mut out, image).expect("writing to String");
    out
}

fn write_image(out: &mut String, image: &ProgramImage) -> fmt::Result {
    writeln!(out, "entry fn[{}]", image.entry_function)?;
    for (i, func) in image.functions.iter().enumerate() {
        out.push('\n');
        write_function(out, Some(i), func, Some(image))?;
    }
    for (i, module) in image.modules.iter().enumerate() {
        write!(out, "\nmodule[{i}] ")?;
        write_atom(out, module.name, Some(image))?;
        writeln!(
            out,
            " fn[{}]{}",
            module.function,
            if module.has_top_level_await {
                " (top-level await)"
            } else {
                ""
            }
        )?;
        for (i, request) in module.requested_modules.iter().enumerate() {
            write!(out, "  request[{i}] ")?;
            write_atom(out, request.name, Some(image))?;
            for attr in &request.attributes {
                out.push(' ');
                write_atom(out, attr.key, Some(image))?;
                out.push('=');
                write_atom(out, attr.value, Some(image))?;
            }
            out.push('\n');
        }
        for import in &module.imports {
            write!(
                out,
                "  import closure[{}] request[{}] ",
                import.closure_var, import.requested_module
            )?;
            write_import_name(out, import.import_name, image)?;
            out.push('\n');
        }
        for export in &module.exports {
            out.push_str("  export ");
            match *export {
                ExportEntry::Local {
                    export_name,
                    closure_var,
                } => {
                    write_atom(out, export_name, Some(image))?;
                    writeln!(out, " closure[{closure_var}]")?;
                }
                ExportEntry::Indirect {
                    export_name,
                    requested_module,
                    import_name,
                } => {
                    write_atom(out, export_name, Some(image))?;
                    write!(out, " request[{requested_module}] ")?;
                    write_import_name(out, import_name, image)?;
                    out.push('\n');
                }
                ExportEntry::Star { requested_module } => {
                    writeln!(out, "* request[{requested_module}]")?;
                }
            }
        }
    }
    Ok(())
}

fn write_import_name(
    out: &mut String,
    name: Option<UnitAtom>,
    image: &ProgramImage,
) -> fmt::Result {
    match name {
        Some(name) => write_atom(out, name, Some(image)),
        None => out.write_str("(namespace)"),
    }
}

fn write_function(
    out: &mut String,
    index: Option<usize>,
    func: &FunctionBytecode,
    image: Option<&ProgramImage>,
) -> fmt::Result {
    out.write_str("fn")?;
    if let Some(index) = index {
        write!(out, "[{index}]")?;
    }
    out.write_char(' ')?;
    if func.name.is_none() {
        out.write_str("<anonymous>")?;
    } else {
        write_atom(out, func.name, image)?;
    }
    writeln!(
        out,
        " (args {}, locals {}, closure {}, stack {})",
        func.arg_count,
        func.var_count,
        func.closure_vars.len(),
        func.stack_size
    )?;
    if !func.debug.filename.is_empty() {
        writeln!(
            out,
            "  source {:?} {}..{}",
            func.debug.filename, func.debug.source_start, func.debug.source_end
        )?;
    }
    for handler in &func.exception_handlers {
        writeln!(
            out,
            "  handler {:04x}..{:04x} -> {:04x} (depth {})",
            handler.start_pc, handler.end_pc, handler.handler_pc, handler.stack_depth
        )?;
    }
    let mut pc = 0;
    let mut last_offset = None;
    while pc < func.code.len() {
        write!(out, "  {pc:04x}  ")?;
        match Instruction::decode(&func.code, pc) {
            Ok(insn) => {
                out.write_str(insn.opcode.info().name)?;
                write_operands(out, pc, insn, func, image)?;
                let offset = u32::try_from(pc)
                    .ok()
                    .and_then(|pc| func.debug.pc_to_byte_offset(pc));
                if let Some(offset) = offset.filter(|_| offset != last_offset) {
                    write!(out, "  ; source {offset}")?;
                    last_offset = Some(offset);
                }
                out.write_char('\n')?;
                pc += usize::from(insn.opcode.info().size);
            }
            Err(error) => {
                writeln!(out, "<{error}>")?;
                break;
            }
        }
    }
    Ok(())
}

fn write_operands(
    out: &mut String,
    pc: usize,
    insn: Instruction,
    func: &FunctionBytecode,
    image: Option<&ProgramImage>,
) -> fmt::Result {
    let target = |out: &mut String| {
        if let Some(target) = u32::try_from(pc).ok().and_then(|pc| insn.branch_target(pc)) {
            if target >= 0 {
                write!(out, "@{target:04x}")
            } else {
                write!(out, "@-{:04x}", -target)
            }
        } else {
            out.write_str("@<out of range>")
        }
    };
    use Operands::*;
    match insn.operands {
        None => Ok(()),
        Signed(n) => write!(out, " {n}"),
        Unsigned(n) => write!(out, " {n}"),
        Pair(a, b) => write!(out, " {a}, {b}"),
        Call(args) => write!(out, " argc={args}"),
        Eval { arguments, scope } => write!(out, " argc={arguments}, scope={scope}"),
        Constant(index) => {
            write!(out, " const[{index}]")?;
            if let Some(value) = func.constants.get(index as usize) {
                out.write_str(" = ")?;
                write_constant(out, value)?;
            }
            Ok(())
        }
        Local(index) => write!(out, " local[{index}]"),
        Locals(a, b) => write!(out, " local[{a}], local[{b}]"),
        Argument(index) => write!(out, " arg[{index}]"),
        VarRef(index) => write!(out, " closure[{index}]"),
        Atom(atom) => {
            out.write_char(' ')?;
            write_atom(out, atom, image)
        }
        AtomU8(atom, operand) => {
            out.write_char(' ')?;
            write_atom(out, atom, image)?;
            write!(out, ", {operand}")
        }
        AtomU16(atom, operand) => {
            out.write_char(' ')?;
            write_atom(out, atom, image)?;
            write!(out, ", {operand}")
        }
        AtomJumpU8(atom, _, operand) => {
            out.write_char(' ')?;
            write_atom(out, atom, image)?;
            out.write_str(", ")?;
            target(out)?;
            write!(out, ", {operand}")
        }
        AtomJumpU16(atom, _, operand) => {
            out.write_char(' ')?;
            write_atom(out, atom, image)?;
            out.write_str(", ")?;
            target(out)?;
            write!(out, ", {operand}")
        }
        Jump(_) => {
            out.write_char(' ')?;
            target(out)
        }
        JumpU16(_, operand) => {
            out.write_char(' ')?;
            target(out)?;
            write!(out, ", {operand}")
        }
    }
}

fn write_atom(out: &mut String, atom: UnitAtom, image: Option<&ProgramImage>) -> fmt::Result {
    let name = atom
        .builtin()
        .map(AtomName::Builtin)
        .or_else(|| image.and_then(|image| image.atom(atom)));
    match name {
        Some(AtomName::Builtin(atom)) => write!(out, "{:?}", atom.as_str()),
        Some(AtomName::Dynamic(name)) => write_string(out, name),
        None if atom.is_none() => out.write_str("<no atom>"),
        None => write!(out, "atom[{}]", atom.raw()),
    }
}

fn write_constant(out: &mut String, value: &ConstEntry) -> fmt::Result {
    match value {
        ConstEntry::Int(value) => write!(out, "{value}"),
        ConstEntry::Float(value) => write!(out, "{value:?}"),
        ConstEntry::String(value) => write_string(out, value),
        ConstEntry::BigInt(value) => write!(out, "{value}n"),
        ConstEntry::RegExp { pattern, flags } => {
            out.write_str("regexp(")?;
            write_string(out, pattern)?;
            write!(out, ", flags={flags:#x})")
        }
        ConstEntry::Function(index) => write!(out, "fn[{index}]"),
        ConstEntry::TemplateObject(value) => write!(out, "template({} parts)", value.raw.len()),
    }
}

fn write_string(out: &mut String, string: &ConstString) -> fmt::Result {
    fn escape(out: &mut String, ch: char) -> fmt::Result {
        for escaped in ch.escape_debug() {
            out.write_char(escaped)?;
        }
        Ok(())
    }
    out.write_char('"')?;
    match string {
        ConstString::Latin1(bytes) => {
            for &byte in bytes {
                escape(out, char::from(byte))?;
            }
        }
        ConstString::Utf16(units) => {
            for ch in char::decode_utf16(units.iter().copied()) {
                match ch {
                    Ok(ch) => escape(out, ch)?,
                    Err(error) => write!(out, "\\u{:04x}", error.unpaired_surrogate())?,
                }
            }
        }
    }
    out.write_char('"')
}
