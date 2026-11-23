use qjs_bytecode::{ConstEntry, ConstString, FunctionKind, ProgramImage, TemplateObject};
use qjs_syntax::{SourceText, Span};

use crate::atom::{Atom, PropKey};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{BytecodeFunctionData, ErrorKind, ObjectData, ObjectFlags};
use crate::ops::{convert, property};
use crate::root::{ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::value::*;
use crate::vm::{BytecodeFunc, CompiledFunc};

pub(super) fn load<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    image: &ProgramImage,
    source: Option<&SourceText>,
) -> JsResult<Handle<'s>> {
    qjs_bytecode::verify_image(image)
        .map_err(|error| ctx.throw_error(ErrorKind::Syntax, &error.to_string()))?;
    let entry = ctx.with_scope(|ctx| {
        let mut atoms = TrackedVec::new();
        ctx.allocate(|rt| atoms.try_reserve_exact(&rt.heap.ledger, image.atoms.len()))?;
        for text in &image.atoms {
            let string = constant_string(ctx, text)?;
            atoms.push_reserved(ctx.intern(string)?);
        }
        let mut functions = TrackedVec::new();
        ctx.allocate(|rt| functions.try_reserve_exact(&rt.heap.ledger, image.functions.len()))?;
        for function in &image.functions {
            let atom = function.name;
            let name = if atom.is_none() {
                None
            } else if let Some(index) = atom.dynamic_index() {
                Some(atoms[index as usize])
            } else {
                Some(PropKey::atom(Atom(atom.raw())))
            };
            let debug = &function.debug;
            let source_id = match source {
                Some(source)
                    if debug.source_start <= debug.source_end
                        && debug.source_end as usize <= source.as_bytes().len() =>
                {
                    let text = ctx.new_utf16(
                        &source.slice_utf16(Span::new(debug.source_start, debug.source_end)),
                    )?;
                    let ValueKind::String(id) = ctx.get(text).decode() else {
                        unreachable!()
                    };
                    Some(id)
                }
                _ => None,
            };
            let filename = if debug.filename.is_empty() {
                source.map_or("<bytecode>", |source| source.filename.as_str())
            } else {
                &debug.filename
            };
            let id = ctx.allocate(|rt| {
                let heap = &mut rt.heap;
                let ledger = &heap.ledger;
                let mut constants = TrackedVec::new();
                constants.try_reserve_exact(ledger, function.constants.len())?;
                let mut regex_flags = TrackedVec::new();
                regex_flags.try_reserve_exact(
                    ledger,
                    function
                        .constants
                        .iter()
                        .filter(|c| matches!(c, ConstEntry::RegExp { .. }))
                        .count(),
                )?;
                for (index, constant) in function.constants.iter().enumerate() {
                    if let ConstEntry::RegExp { flags, .. } = constant {
                        regex_flags.push_reserved((index as u32, *flags));
                    }
                }
                heap.bytecode.insert(
                    ledger,
                    BytecodeFunc {
                        code: TrackedVec::new(),
                        constants,
                        compiled: Some(CompiledFunc {
                            code: TrackedVec::from_slice(ledger, &function.code)?,
                            atoms: TrackedVec::from_slice(ledger, &atoms)?,
                            name,
                            flags: function.flags,
                            kind: function.kind.unwrap_or_default(),
                            strict: function.strict,
                            arg_count: function.arg_count,
                            var_count: function.var_count,
                            defined_arg_count: function.defined_arg_count,
                            stack_size: function.stack_size,
                            vars: clone_items(ledger, &function.vars)?,
                            closures: clone_items(ledger, &function.closure_vars)?,
                            handlers: TrackedVec::from_slice(ledger, &function.exception_handlers)?,
                            filename: TrackedVec::from_slice(ledger, filename.as_bytes())?,
                            line_starts: TrackedVec::from_slice(
                                ledger,
                                source.map_or(&[0][..], |source| &source.line_starts),
                            )?,
                            pc_map: TrackedVec::from_slice(ledger, &debug.pc_map)?,
                            regex_flags,
                            source: source_id,
                            source_start: debug.source_start,
                        }),
                    },
                )
            })?;
            ctx.root(Value::bytecode(id))?;
            functions.push_reserved(id);
        }
        for (index, function) in image.functions.iter().enumerate() {
            let id = functions[index];
            for constant in &function.constants {
                let value = ctx.with_scope(|ctx| materialize(ctx, constant, &functions))?;
                ctx.runtime
                    .heap
                    .bytecode
                    .get_mut(id)
                    .unwrap()
                    .constants
                    .push_reserved(value);
            }
        }
        let id = functions[image.entry_function as usize];
        make_function(ctx, id, &[]).map(|handle| ctx.get(handle))
    })?;
    ctx.root(entry)
}

fn clone_items<T: Clone>(
    ledger: &crate::ledger::HeapLedger,
    items: &[T],
) -> crate::ledger::AllocResult<TrackedVec<T>> {
    let mut output = TrackedVec::new();
    output.try_reserve_exact(ledger, items.len())?;
    for item in items {
        output.push_reserved(item.clone());
    }
    Ok(output)
}

fn constant_string<'s>(ctx: &mut ExecCtx<'_, 's>, text: &ConstString) -> JsResult<Handle<'s>> {
    match text {
        ConstString::Latin1(bytes) => {
            let id = ctx.allocate(|rt| {
                let heap = &mut rt.heap;
                let string = crate::string::JsString::from_latin1(&heap.ledger, bytes)?;
                heap.strings.insert(&heap.ledger, string)
            })?;
            ctx.root(Value::string(id))
        }
        ConstString::Utf16(units) => ctx.new_utf16(units),
    }
}

fn materialize(
    ctx: &mut ExecCtx<'_, '_>,
    constant: &ConstEntry,
    functions: &[BcId],
) -> JsResult<Value> {
    Ok(match constant {
        ConstEntry::Int(int) => Value::int32(*int),
        ConstEntry::Float(number) => Value::number(*number),
        ConstEntry::String(text) | ConstEntry::RegExp { pattern: text, .. } => {
            let text = constant_string(ctx, text)?;
            ctx.get(text)
        }
        ConstEntry::Function(index) => Value::bytecode(functions[*index as usize]),
        ConstEntry::BigInt(text) => super::numeric::parse_bigint(ctx, text)?,
        ConstEntry::TemplateObject(template) => template_object(ctx, template)?,
    })
}

fn template_object(ctx: &mut ExecCtx<'_, '_>, template: &TemplateObject) -> JsResult<Value> {
    let cooked = ctx.new_array()?;
    let raw = ctx.new_array()?;
    for (index, (text, source)) in template.cooked.iter().zip(&template.raw).enumerate() {
        let a = ctx.get(cooked);
        let b = ctx.get(raw);
        ctx.with_scope(|ctx| {
            let cooked = ctx.root(a)?;
            let raw = ctx.root(b)?;
            let key = index_key(ctx, index as u64)?;
            let text = match text {
                Some(text) => constant_string(ctx, text)?,
                None => ctx.root(Value::UNDEFINED)?,
            };
            crate::builtins::define_data(ctx, cooked, key, text, PropFlags::ENUMERABLE)?;
            let source = constant_string(ctx, source)?;
            crate::builtins::define_data(ctx, raw, key, source, PropFlags::ENUMERABLE)
        })?;
    }
    for array in [raw, cooked] {
        property::define_own_property(
            ctx,
            array,
            Atom::get("length"),
            property::PropertyDescriptor {
                writable: Some(false),
                ..Default::default()
            },
        )?;
    }
    crate::builtins::define_data(ctx, cooked, Atom::get("raw"), raw, PropFlags::default())?;
    property::prevent_extensions(ctx, raw)?;
    property::prevent_extensions(ctx, cooked)?;
    Ok(ctx.get(cooked))
}

pub(super) fn index_key(ctx: &mut ExecCtx<'_, '_>, index: u64) -> JsResult<PropKey> {
    if let Some(key) = u32::try_from(index).ok().and_then(PropKey::index) {
        Ok(key)
    } else {
        ctx.intern_str(&index.to_string())
    }
}

pub(super) fn make_function<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    code: BcId,
    captures: &[Value],
) -> JsResult<Handle<'s>> {
    let realm = ctx.realm_id();
    let compiled = ctx
        .heap()
        .bytecode
        .get(code)
        .unwrap()
        .compiled
        .as_ref()
        .unwrap();
    let (kind, name, length, prototype) = (
        compiled.kind,
        compiled.name,
        compiled.defined_arg_count,
        compiled.flags.has_prototype,
    );
    let instance_proto = match kind {
        FunctionKind::Normal => None,
        FunctionKind::Generator => Some(ctx.realm().generator_proto),
        FunctionKind::AsyncGenerator => Some(ctx.realm().async_generator_proto),
        FunctionKind::Async => None,
    };
    let mut flags = ObjectFlags::EXTENSIBLE;
    flags.set(
        ObjectFlags::IS_CONSTRUCTOR,
        prototype && kind == FunctionKind::Normal,
    );
    let function = ctx.make_object(Some(ctx.realm().function_proto), flags, |ledger| {
        Ok(ObjectData::BytecodeFunction(TrackedBox::new(
            ledger,
            BytecodeFunctionData {
                code,
                realm,
                closure: TrackedVec::from_slice(ledger, captures)?,
                home_object: None,
                is_class_constructor: false,
                is_arrow: !prototype && kind == FunctionKind::Normal,
                is_derived_constructor: false,
            },
        )?))
    })?;
    let length = ctx.root(Value::int32(i32::from(length)))?;
    let name = if let Some(key) = name {
        convert::key_value(ctx, key)?
    } else {
        ctx.new_string("")?
    };
    crate::builtins::define_data(
        ctx,
        function,
        Atom::get("length"),
        length,
        PropFlags::CONFIGURABLE,
    )?;
    crate::builtins::define_data(
        ctx,
        function,
        Atom::get("name"),
        name,
        PropFlags::CONFIGURABLE,
    )?;
    if prototype || instance_proto.is_some() {
        let base = instance_proto
            .map(|id| ctx.root(Value::object(id)))
            .transpose()?;
        let object = if base.is_some() {
            ctx.new_object_with_proto(base)?
        } else {
            ctx.new_object()?
        };
        if kind == FunctionKind::Normal {
            crate::builtins::define_data(
                ctx,
                object,
                Atom::get("constructor"),
                function,
                PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
            )?;
        }
        crate::builtins::define_data(
            ctx,
            function,
            Atom::get("prototype"),
            object,
            PropFlags::WRITABLE,
        )?;
    }
    Ok(function)
}
