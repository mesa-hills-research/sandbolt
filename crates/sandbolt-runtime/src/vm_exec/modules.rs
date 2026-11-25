use sandbolt_bytecode::{
    ClosureKind, ExportEntry, FunctionKind, Instruction, Opcode, Operands, ProgramImage, VarKind,
};
use sandbolt_syntax::SourceText;

use crate::api::{EvalOptions, ResolvedModule, parse_options};
use crate::atom::{Atom, KeyKind, PropKey};
use crate::builtins::promise_json_extras::{
    fulfill_promise, new_promise_capability, perform_promise_then, reject_promise,
};
use crate::builtins::vm_internal::{
    DYNAMIC_IMPORT_JOB, MODULE_DONE, MODULE_FAILED, MODULE_IMPORT_DONE, MODULE_IMPORT_FAILED,
    MODULE_READY,
};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{
    ArrayBufferData, ErrorKind, ModuleNamespaceData, ObjectData, ObjectFlags, TypedArrayData,
    TypedArrayKind,
};
use crate::ops::{call, convert, property};
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::*;
use crate::vm::{
    ModuleExport, ModuleImport, ModuleRecord, ModuleRequest, ModuleStatus, VarRef, VarRefState,
};

use super::{compiled, define_flags, load, utf16_lossy, well_known};

// The only supported attribute is `type`. Omitted and empty sets are equal.
const NO_ATTRIBUTES: u8 = 0;
const JSON: u8 = 1;
const TEXT: u8 = 2;
const BYTES: u8 = 3;

pub(super) fn evaluate_image(
    ctx: &mut ExecCtx<'_, '_>,
    image: &ProgramImage,
    source: Option<&SourceText>,
    origin: load::Origin,
) -> JsResult<Value> {
    let linked = ctx.with_scope(|ctx| {
        let id = register_image(ctx, image, source, NO_ATTRIBUTES, origin)?;
        link(ctx, id)?;
        Ok(id)
    });
    let id = match linked {
        Ok(id) => id,
        Err(error) => {
            ctx.runtime.vm.resolution_failed = true;
            return Err(error);
        }
    };
    let promise = evaluate(ctx, id)?;
    if promise != Value::UNDEFINED {
        record_mut(ctx, id).waiting_host = true;
    }
    Ok(promise)
}

fn record<'a>(ctx: &'a ExecCtx<'_, '_>, id: ModId) -> &'a ModuleRecord {
    ctx.heap().modules.get(id).unwrap()
}
fn record_mut<'a>(ctx: &'a mut ExecCtx<'_, '_>, id: ModId) -> &'a mut ModuleRecord {
    ctx.runtime.heap.modules.get_mut(id).unwrap()
}

fn cached(ctx: &ExecCtx<'_, '_>, name: PropKey, attributes: u8) -> Option<ModId> {
    ctx.runtime.modules.iter().copied().find(|&id| {
        let m = record(ctx, id);
        m.name == Some(name) && m.attributes == attributes && m.realm == Some(ctx.realm_id())
    })
}

fn register_image(
    ctx: &mut ExecCtx<'_, '_>,
    image: &ProgramImage,
    source: Option<&SourceText>,
    attributes: u8,
    origin: load::Origin,
) -> JsResult<ModId> {
    let functions = load::load_functions(ctx, image, source, origin)?;
    let saved_modules = ctx.runtime.modules.len();
    let result = (|| {
        let mut primary = None;
        for module in &image.modules {
            let code = functions[module.function as usize];
            let name = compiled(ctx, code).key(module.name);
            let id = if let Some(id) = cached(ctx, name, attributes) {
                id
            } else {
                let id = register(
                    ctx,
                    name,
                    attributes,
                    Some(code),
                    module.has_top_level_await,
                )?;
                for request in &module.requested_modules {
                    let name = compiled(ctx, code).key(request.name);
                    let mut attributes = NO_ATTRIBUTES;
                    let type_key = ctx.intern_str("type")?;
                    for attr in &request.attributes {
                        let key = compiled(ctx, code).key(attr.key);
                        if key != type_key {
                            return Err(
                                ctx.throw_error(ErrorKind::Syntax, "unsupported import attribute")
                            );
                        }
                        let v = compiled(ctx, code).key(attr.value);
                        let v = atom_text(ctx, v)?;
                        attributes = parse_type(ctx, &v)?;
                    }
                    ctx.allocate(|rt| {
                        rt.heap
                            .modules
                            .get_mut(id)
                            .unwrap()
                            .requests
                            .push(&rt.heap.ledger, ModuleRequest { name, attributes })
                    })?;
                }
                for &import in &module.imports {
                    let name = import.import_name.map(|name| compiled(ctx, code).key(name));
                    let import = ModuleImport {
                        name,
                        dependency: import.requested_module as usize,
                        slot: import.closure_var as usize,
                    };
                    ctx.allocate(|rt| {
                        rt.heap
                            .modules
                            .get_mut(id)
                            .unwrap()
                            .imports
                            .push(&rt.heap.ledger, import)
                    })?;
                }
                for &entry in &module.exports {
                    let c = compiled(ctx, code);
                    let entry = match entry {
                        ExportEntry::Local {
                            export_name,
                            closure_var,
                        } => ModuleExport::Local {
                            name: c.key(export_name),
                            slot: closure_var as usize,
                        },
                        ExportEntry::Indirect {
                            export_name,
                            requested_module,
                            import_name,
                        } => ModuleExport::Indirect {
                            name: c.key(export_name),
                            dependency: requested_module as usize,
                            import_name: import_name.map(|name| c.key(name)),
                        },
                        ExportEntry::Star { requested_module } => {
                            ModuleExport::Star(requested_module as usize)
                        }
                    };
                    ctx.allocate(|rt| {
                        rt.heap
                            .modules
                            .get_mut(id)
                            .unwrap()
                            .export_bindings
                            .push(&rt.heap.ledger, entry)
                    })?;
                }
                let count = compiled(ctx, code).closures.len();
                ctx.allocate(|rt| {
                    rt.heap
                        .modules
                        .get_mut(id)
                        .unwrap()
                        .exports
                        .try_reserve_exact(&rt.heap.ledger, count)
                })?;
                for slot in 0..count {
                    let binding = &compiled(ctx, code).closures[slot];
                    let value = if binding.kind == ClosureKind::ModuleDeclaration {
                        let initial = if binding.lexical_tdz {
                            Value::UNINITIALIZED
                        } else {
                            Value::UNDEFINED
                        };
                        Value::var_ref(new_cell(ctx, initial)?)
                    } else {
                        Value::UNDEFINED
                    };
                    record_mut(ctx, id).exports.push_reserved(value);
                }
                mark_unit(ctx, code, id)?;
                id
            };
            if module.function == image.entry_function {
                primary = Some(id);
            }
        }
        primary.ok_or_else(|| ctx.throw_error(ErrorKind::Syntax, "module entry record is missing"))
    })();
    if result.is_err() {
        ctx.runtime.modules.truncate(saved_modules);
    }
    result
}

fn mark_unit(ctx: &mut ExecCtx<'_, '_>, root: BcId, id: ModId) -> JsResult<()> {
    let mut queue = TrackedVec::new();
    ctx.allocate(|rt| queue.push(&rt.heap.ledger, root))?;
    while let Some(code) = queue.pop() {
        let code_data = ctx.runtime.heap.bytecode.get_mut(code).unwrap();
        let function = code_data.compiled.as_mut().unwrap();
        if function.module.is_some() {
            continue;
        }
        function.module = Some(id);
        for index in 0..code_data.constants.len() {
            if let ValueKind::Bytecode(child) =
                ctx.heap().bytecode.get(code).unwrap().constants[index].decode()
            {
                ctx.allocate(|rt| queue.push(&rt.heap.ledger, child))?;
            }
        }
    }
    Ok(())
}

fn register(
    ctx: &mut ExecCtx<'_, '_>,
    name: PropKey,
    attributes: u8,
    code: Option<BcId>,
    has_await: bool,
) -> JsResult<ModId> {
    let realm = ctx.realm_id();
    ctx.allocate(|rt| {
        rt.modules.try_reserve(&rt.heap.ledger, 1)?;
        let id = rt.heap.modules.insert(
            &rt.heap.ledger,
            ModuleRecord {
                realm: Some(realm),
                name: Some(name),
                attributes,
                code,
                has_await,
                ..Default::default()
            },
        )?;
        rt.modules.push_reserved(id);
        Ok(id)
    })
}

fn new_cell(ctx: &mut ExecCtx<'_, '_>, initial: Value) -> JsResult<VarRefId> {
    ctx.allocate(|rt| {
        rt.heap.var_refs.insert(
            &rt.heap.ledger,
            VarRef {
                state: VarRefState::Closed(initial),
            },
        )
    })
}

fn namespace(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<ObjId> {
    if let Some(object) = record(ctx, id).namespace {
        return Ok(object);
    }
    let tag = well_known(ctx, WellKnown::ToStringTag);
    let object = ctx.make_object(
        None,
        ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC,
        |ledger| {
            Ok(ObjectData::ModuleNamespace(TrackedBox::new(
                ledger,
                ModuleNamespaceData { module: None },
            )?))
        },
    )?;
    let name = ctx.new_string("Module")?;
    define_flags(ctx, object, tag, name, PropFlags::default())?;
    property::prevent_extensions(ctx, object)?;
    let object = ctx.object_id(object)?;
    let cell = new_cell(ctx, Value::object(object))?;
    let record = record_mut(ctx, id);
    record.namespace = Some(object);
    record.namespace_ref = Some(cell);
    let ObjectData::ModuleNamespace(data) =
        &mut ctx.runtime.heap.objects.get_mut(object).unwrap().data
    else {
        unreachable!()
    };
    data.module = Some(id);
    Ok(object)
}

fn atom_text(ctx: &mut ExecCtx<'_, '_>, key: PropKey) -> JsResult<String> {
    let value = convert::key_value(ctx, key)?;
    let ValueKind::String(string) = ctx.get(value).decode() else {
        return Err(ctx.throw_type_error("expected a module string"));
    };
    Ok(utf16_lossy(ctx.heap(), string))
}

fn parse_type(ctx: &mut ExecCtx<'_, '_>, kind: &str) -> JsResult<u8> {
    match kind {
        "json" => Ok(JSON),
        "text" => Ok(TEXT),
        "bytes" => Ok(BYTES),
        _ => Err(ctx.throw_error(ErrorKind::Syntax, "unsupported module type")),
    }
}

fn resolve_request(
    ctx: &mut ExecCtx<'_, '_>,
    referrer: &str,
    request: ModuleRequest,
) -> JsResult<ModId> {
    ctx.check_interrupt()?;
    let specifier = atom_text(ctx, request.name)?;
    let kind = match request.attributes {
        JSON => Some("json"),
        TEXT => Some("text"),
        BYTES => Some("bytes"),
        _ => None,
    };
    if ctx.runtime.vm.module_loader.is_none() {
        if let Some(id) = cached(ctx, request.name, request.attributes) {
            return Ok(id);
        }
        return Err(ctx.throw_error(ErrorKind::Reference, "module loader is not installed"));
    }
    let loaded = ctx
        .runtime
        .vm
        .module_loader
        .as_mut()
        .unwrap()
        .resolve(referrer, &specifier, kind);
    let loaded = loaded.map_err(|error| ctx.throw_error(ErrorKind::Reference, &error))?;
    let name = match &loaded {
        ResolvedModule::Source { name, .. }
        | ResolvedModule::Json { name, .. }
        | ResolvedModule::Text { name, .. }
        | ResolvedModule::Bytes { name, .. } => name,
    };
    let key = ctx.intern_str(name)?;
    if let Some(id) = cached(ctx, key, request.attributes) {
        return Ok(id);
    }
    if let ResolvedModule::Source { name, source } = loaded {
        let opts = EvalOptions {
            filename: name.clone(),
            module: true,
            ..Default::default()
        };
        let parse = parse_options(ctx.runtime, &opts);
        if source.len() > parse.max_bytes {
            return Err(ctx.throw_oom());
        }
        let source = SourceText::from_utf8(name, &source);
        let image = crate::api::compile_text(ctx, &source, parse)?;
        return register_image(
            ctx,
            &image,
            Some(&source),
            request.attributes,
            load::Origin::Compiler,
        );
    }
    let value = match loaded {
        ResolvedModule::Json { source, .. } => {
            let source = ctx.new_string(&source)?;
            let args = ctx.args(&[source])?;
            let this = ctx.root(Value::UNDEFINED)?;
            crate::builtins::promise_json_extras::dispatch(
                NativeFnId(crate::builtins::PROMISE_JSON_EXTRAS_BASE + 0x80),
                ctx,
                this,
                args,
            )?
        }
        ResolvedModule::Text { text, .. } => {
            let text = ctx.new_string(&text)?;
            ctx.get(text)
        }
        ResolvedModule::Bytes { bytes, .. } => bytes_module(ctx, &bytes)?,
        ResolvedModule::Source { .. } => unreachable!(),
    };
    ctx.root(value)?;
    let saved_modules = ctx.runtime.modules.len();
    let result = (|| {
        let id = register(ctx, key, request.attributes, None, false)?;
        let binding = new_cell(ctx, value)?;
        ctx.root(Value::var_ref(binding))?;
        ctx.allocate(|rt| {
            let record = rt.heap.modules.get_mut(id).unwrap();
            record.exports.try_reserve(&rt.heap.ledger, 1)?;
            record.export_bindings.try_reserve(&rt.heap.ledger, 1)?;
            record.exports.push_reserved(Value::var_ref(binding));
            record.export_bindings.push_reserved(ModuleExport::Local {
                name: Atom::get("default"),
                slot: 0,
            });
            Ok(())
        })?;
        Ok(id)
    })();
    if result.is_err() {
        ctx.runtime.modules.truncate(saved_modules);
    }
    result
}

fn bytes_module(ctx: &mut ExecCtx<'_, '_>, bytes: &[u8]) -> JsResult<Value> {
    let prototype = ctx.realm().array_buffer_proto;
    let buffer = ctx.make_object(Some(prototype), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::ArrayBuffer(TrackedBox::new(
            ledger,
            ArrayBufferData {
                bytes: TrackedVec::from_slice(ledger, bytes)?,
                immutable: true,
                ..Default::default()
            },
        )?))
    })?;
    let buffer = ctx.object_id(buffer)?;
    let kind = TypedArrayKind::Uint8;
    let proto = ctx.realm().typed_array_concrete[kind as usize]
        .map_or(ctx.realm().typed_array_proto, |(_, proto)| proto);
    let array = ctx.make_object(
        Some(proto),
        ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC,
        |ledger| {
            Ok(ObjectData::TypedArray(TrackedBox::new(
                ledger,
                TypedArrayData {
                    buffer,
                    byte_offset: 0,
                    length: Some(bytes.len()),
                    kind,
                },
            )?))
        },
    )?;
    Ok(ctx.get(array))
}

fn link(ctx: &mut ExecCtx<'_, '_>, root: ModId) -> JsResult<()> {
    if record(ctx, root).status != ModuleStatus::Unlinked {
        return Ok(());
    }
    let mut graph = TrackedVec::new();
    let result = (|| {
        discover(ctx, root, &mut graph)?;
        for &id in graph.iter() {
            link_imports(ctx, id)?;
        }
        for &id in graph.iter() {
            link_namespace(ctx, id)?;
            ctx.with_scope(|ctx| instantiate_functions(ctx, id))?;
        }
        Ok(())
    })();
    for &id in graph.iter() {
        let module = record_mut(ctx, id);
        module.status = if result.is_ok() {
            ModuleStatus::Linked
        } else {
            ModuleStatus::Unlinked
        };
        if result.is_err() {
            module.dependencies.clear();
            module.namespace_bindings.clear();
            let code = module.code;
            for index in 0..module.imports.len() {
                let slot = module.imports[index].slot;
                if let Some(entry) = module.exports.get_mut(slot) {
                    *entry = Value::UNDEFINED;
                }
            }
            if let Some(code) = code
                && let Some(compiled) = ctx
                    .runtime
                    .heap
                    .bytecode
                    .get_mut(code)
                    .and_then(|entry| entry.compiled.as_mut())
            {
                compiled.hoisted.clear();
            }
        }
    }
    result
}

fn instantiate_functions(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<()> {
    let Some(code) = record(ctx, id).code else {
        return Ok(());
    };
    let mut pc = 0;
    let mut skips = TrackedVec::new();
    while let Some((instruction, next)) = instruction_at(ctx, code, pc) {
        if matches!(instruction.opcode, Opcode::Fclosure | Opcode::Fclosure8)
            && let Some((store, end)) = instruction_at(ctx, code, next)
            && matches!(
                store.opcode,
                Opcode::PutVarRefCheckInit
                    | Opcode::PutVarRef
                    | Opcode::PutVarRef0
                    | Opcode::PutVarRef1
                    | Opcode::PutVarRef2
                    | Opcode::PutVarRef3
            )
            && let Operands::VarRef(slot) = store.operands
            && compiled(ctx, code)
                .closures
                .get(slot as usize)
                .is_some_and(|def| {
                    def.kind == ClosureKind::ModuleDeclaration
                        && matches!(
                            def.var_kind,
                            VarKind::FunctionDeclaration | VarKind::AsyncOrGeneratorDeclaration
                        )
                })
        {
            let Operands::Constant(constant) = instruction.operands else {
                unreachable!()
            };
            let ValueKind::Bytecode(child) =
                ctx.heap().bytecode.get(code).unwrap().constants[constant as usize].decode()
            else {
                unreachable!()
            };
            let ValueKind::VarRef(cell) = record(ctx, id).exports[slot as usize].decode() else {
                unreachable!()
            };
            if !skips.iter().any(|&(_, _, other)| other == cell) {
                ctx.with_scope(|ctx| {
                    let mut captures = TrackedVec::new();
                    for index in 0..compiled(ctx, child).closures.len() {
                        let def = &compiled(ctx, child).closures[index];
                        let capture = match def.kind {
                            ClosureKind::Ref | ClosureKind::GlobalRef => {
                                record(ctx, id).exports[def.index as usize]
                            }
                            _ => Value::UNDEFINED,
                        };
                        ctx.allocate(|rt| captures.push(&rt.heap.ledger, capture))?;
                    }
                    let function = load::make_function(ctx, child, &captures)?;
                    super::bindings::write_ref(ctx, cell, ctx.get(function));
                    Ok(())
                })?;
                ctx.allocate(|rt| skips.push(&rt.heap.ledger, (pc, end, cell)))?;
            }
        }
        pc = next;
    }
    ctx.allocate(|rt| {
        let code = rt
            .heap
            .bytecode
            .get_mut(code)
            .unwrap()
            .compiled
            .as_mut()
            .unwrap();
        code.hoisted.clear();
        code.hoisted
            .try_reserve_exact(&rt.heap.ledger, skips.len())?;
        for &(start, end, _) in skips.iter() {
            code.hoisted.push_reserved((start, end));
        }
        Ok(())
    })
}

fn instruction_at(ctx: &ExecCtx<'_, '_>, code: BcId, pc: u32) -> Option<(Instruction, u32)> {
    let inst = Instruction::decode(&compiled(ctx, code).code, pc as usize).ok()?;
    Some((inst, pc + u32::from(inst.opcode.info().size)))
}

fn discover(ctx: &mut ExecCtx<'_, '_>, id: ModId, graph: &mut TrackedVec<ModId>) -> JsResult<()> {
    if record(ctx, id).status != ModuleStatus::Unlinked {
        return Ok(());
    }
    ctx.allocate(|rt| graph.push(&rt.heap.ledger, id))?;
    record_mut(ctx, id).status = ModuleStatus::Linking;
    namespace(ctx, id)?;
    let referrer = atom_text(ctx, record(ctx, id).name.unwrap())?;
    let count = record(ctx, id).requests.len();
    for index in 0..count {
        let request = record(ctx, id).requests[index];
        let dependency = with_graph_depth(ctx, |ctx| {
            let dependency = resolve_request(ctx, &referrer, request)?;
            discover(ctx, dependency, graph)?;
            Ok(dependency)
        })?;
        ctx.allocate(|rt| {
            rt.heap
                .modules
                .get_mut(id)
                .unwrap()
                .dependencies
                .push(&rt.heap.ledger, dependency)
        })?;
    }
    Ok(())
}

fn with_graph_depth<T>(
    ctx: &mut ExecCtx<'_, '_>,
    run: impl for<'s> FnOnce(&mut ExecCtx<'_, 's>) -> JsResult<T>,
) -> JsResult<T> {
    ctx.with_scope(|ctx| {
        ctx.check_interrupt()?;
        if !ctx.runtime.vm.check_budget(0, 0, 1) {
            return Err(ctx.stack_overflow());
        }
        ctx.runtime.vm.reentry_depth += 1;
        run(ctx)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Missing,
    Ambiguous,
    Binding(VarRefId),
}

fn resolve_export(
    ctx: &mut ExecCtx<'_, '_>,
    id: ModId,
    name: PropKey,
    visited: &mut TrackedVec<(ModId, PropKey)>,
) -> JsResult<Resolution> {
    if visited.contains(&(id, name)) {
        return Ok(Resolution::Missing);
    }
    ctx.allocate(|rt| visited.push(&rt.heap.ledger, (id, name)))?;
    let len = record(ctx, id).export_bindings.len();
    for index in 0..len {
        let module = record(ctx, id);
        match module.export_bindings[index] {
            ModuleExport::Local { name: key, slot } if key == name => {
                if let ValueKind::VarRef(cell) = module.exports[slot].decode() {
                    return Ok(Resolution::Binding(cell));
                }
                if let Some(import) = module.imports.iter().find(|i| i.slot == slot) {
                    let target = module.dependencies[import.dependency];
                    return resolve_from(ctx, target, import.name, visited);
                }
                return Ok(Resolution::Missing);
            }
            ModuleExport::Indirect {
                name: key,
                dependency,
                import_name,
            } if key == name => {
                let target = module.dependencies[dependency];
                return resolve_from(ctx, target, import_name, visited);
            }
            _ => {}
        }
    }
    if name == Atom::get("default") {
        return Ok(Resolution::Missing);
    }
    let mut found = Resolution::Missing;
    for index in 0..len {
        let module = record(ctx, id);
        let ModuleExport::Star(dependency) = module.export_bindings[index] else {
            continue;
        };
        let target = module.dependencies[dependency];
        let other = resolve_from(ctx, target, Some(name), visited)?;
        if other == Resolution::Ambiguous
            || (found != Resolution::Missing && other != Resolution::Missing && found != other)
        {
            return Ok(Resolution::Ambiguous);
        }
        if other != Resolution::Missing {
            found = other;
        }
    }
    Ok(found)
}

fn resolve_from(
    ctx: &mut ExecCtx<'_, '_>,
    id: ModId,
    name: Option<PropKey>,
    visited: &mut TrackedVec<(ModId, PropKey)>,
) -> JsResult<Resolution> {
    if let Some(name) = name {
        with_graph_depth(ctx, |ctx| resolve_export(ctx, id, name, visited))
    } else {
        Ok(Resolution::Binding(record(ctx, id).namespace_ref.unwrap()))
    }
}

fn require_export(
    ctx: &mut ExecCtx<'_, '_>,
    id: ModId,
    name: Option<PropKey>,
) -> JsResult<VarRefId> {
    match resolve_from(ctx, id, name, &mut TrackedVec::new())? {
        Resolution::Binding(id) => Ok(id),
        error => {
            let name = atom_text(ctx, name.unwrap())?;
            let message = if error == Resolution::Ambiguous {
                format!("export '{name}' is ambiguous")
            } else {
                format!("module has no export named '{name}'")
            };
            Err(ctx.throw_error(ErrorKind::Syntax, &message))
        }
    }
}

fn link_imports(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<()> {
    for index in 0..record(ctx, id).imports.len() {
        let module = record(ctx, id);
        let import = module.imports[index];
        let target = module.dependencies[import.dependency];
        let binding = require_export(ctx, target, import.name)?;
        record_mut(ctx, id).exports[import.slot] = Value::var_ref(binding);
    }
    for index in 0..record(ctx, id).export_bindings.len() {
        let module = record(ctx, id);
        if let ModuleExport::Indirect {
            dependency,
            import_name,
            ..
        } = module.export_bindings[index]
        {
            require_export(ctx, module.dependencies[dependency], import_name)?;
        }
    }
    Ok(())
}

fn export_names(
    ctx: &mut ExecCtx<'_, '_>,
    id: ModId,
    names: &mut TrackedVec<PropKey>,
    seen: &mut TrackedVec<ModId>,
    star: bool,
) -> JsResult<()> {
    if seen.contains(&id) {
        return Ok(());
    }
    ctx.allocate(|rt| seen.push(&rt.heap.ledger, id))?;
    for index in 0..record(ctx, id).export_bindings.len() {
        let module = record(ctx, id);
        match module.export_bindings[index] {
            ModuleExport::Local { name, .. } | ModuleExport::Indirect { name, .. } => {
                if (!star || name != Atom::get("default")) && !names.contains(&name) {
                    ctx.allocate(|rt| names.push(&rt.heap.ledger, name))?;
                }
            }
            ModuleExport::Star(dependency) => {
                let dep = module.dependencies[dependency];
                with_graph_depth(ctx, |ctx| export_names(ctx, dep, names, seen, true))?;
            }
        }
    }
    Ok(())
}

fn link_namespace(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<()> {
    let mut names = TrackedVec::new();
    export_names(ctx, id, &mut names, &mut TrackedVec::new(), false)?;
    let mut sorted = TrackedVec::new();
    ctx.allocate(|rt| sorted.try_reserve_exact(&rt.heap.ledger, names.len()))?;
    for &name in names.iter() {
        if let Resolution::Binding(binding) = resolve_export(ctx, id, name, &mut TrackedVec::new())?
        {
            let key = convert::key_value(ctx, name)?;
            let ValueKind::String(key) = ctx.get(key).decode() else {
                unreachable!()
            };
            sorted.push_reserved((key, name, binding));
        }
    }
    sorted.sort_unstable_by(|&(a, _, _), &(b, _, _)| {
        ctx.heap().string_units(a).cmp(ctx.heap().string_units(b))
    });
    ctx.allocate(|rt| {
        let names = &mut rt.heap.modules.get_mut(id).unwrap().namespace_bindings;
        names.try_reserve_exact(&rt.heap.ledger, sorted.len())?;
        for &(_, key, binding) in sorted.iter() {
            names.push_reserved((key, binding));
        }
        Ok(())
    })
}

pub(super) fn import_meta(ctx: &mut ExecCtx<'_, '_>, code: BcId) -> JsResult<Value> {
    let Some(id) = compiled(ctx, code).module else {
        return Err(ctx.throw_error(ErrorKind::Syntax, "import.meta outside a module"));
    };
    if let Some(meta) = record(ctx, id).meta {
        return Ok(Value::object(meta));
    }
    let meta = ctx.new_object_with_proto(None)?;
    let url = convert::key_value(ctx, record(ctx, id).name.unwrap())?;
    let key = ctx.intern_str("url")?;
    super::define_default(ctx, meta, key, url)?;
    let object = ctx.object_id(meta)?;
    record_mut(ctx, id).meta = Some(object);
    Ok(Value::object(object))
}

fn evaluate(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<Value> {
    let mut stack = TrackedVec::new();
    let result = evaluate_inner(ctx, id, &mut stack);
    if result == Err(Abrupt::Thrown) {
        while let Some(module) = stack.pop() {
            let _ = fail_pending(ctx, module);
        }
    }
    result
}

fn evaluate_inner(
    ctx: &mut ExecCtx<'_, '_>,
    id: ModId,
    stack: &mut TrackedVec<ModId>,
) -> JsResult<Value> {
    let id = record(ctx, id).cycle_root.unwrap_or(id);
    match record(ctx, id).status {
        ModuleStatus::Evaluated => return Ok(Value::UNDEFINED),
        ModuleStatus::Error => {
            record_mut(ctx, id).waiting_host = false;
            return Err(ctx.throw_value(record(ctx, id).evaluation_error.unwrap(), false));
        }
        ModuleStatus::Async | ModuleStatus::Waiting => {
            return Ok(record(ctx, id).capability.unwrap().0);
        }
        ModuleStatus::Visiting => return Ok(Value::UNDEFINED),
        ModuleStatus::Linked => {}
        _ => return Err(ctx.throw_error(ErrorKind::Internal, "module is not linked")),
    }
    let entered = stack.len();
    ctx.allocate(|rt| stack.push(&rt.heap.ledger, id))?;
    let module = record_mut(ctx, id);
    module.status = ModuleStatus::Visiting;
    module.eval_index = Some(entered);
    module.eval_ancestor = entered;
    for index in 0..record(ctx, id).dependencies.len() {
        let dependency = record(ctx, id).dependencies[index];
        let result = with_graph_depth(ctx, |ctx| evaluate_inner(ctx, dependency, stack));
        let result = match result {
            Ok(value) => value,
            Err(Abrupt::Thrown) => {
                fail_pending(ctx, id)?;
                return Err(Abrupt::Thrown);
            }
            Err(error) => return Err(error),
        };
        let child = record(ctx, dependency);
        if child.cycle_root.is_none() && child.eval_index.is_some() {
            let ancestor = child.eval_ancestor;
            let module = record_mut(ctx, id);
            module.eval_ancestor = module.eval_ancestor.min(ancestor);
        }
        if result != Value::UNDEFINED {
            ensure_capability(ctx, id)?;
            record_mut(ctx, id).pending_dependencies += 1;
            then_module(ctx, result, id, MODULE_READY, MODULE_FAILED)?;
        }
    }
    let result = if record(ctx, id).pending_dependencies != 0 {
        record_mut(ctx, id).status = ModuleStatus::Waiting;
        record(ctx, id).capability.unwrap().0
    } else {
        execute_body(ctx, id)?
    };
    if record(ctx, id).eval_ancestor == entered {
        while let Some(module) = stack.pop() {
            record_mut(ctx, module).cycle_root = Some(id);
            if module == id {
                break;
            }
        }
    }
    Ok(result)
}

fn ensure_capability(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<(Value, (Value, Value))> {
    if let Some(cap) = record(ctx, id).capability {
        return Ok(cap);
    }
    let cap = ctx.with_scope(|ctx| {
        let promise = ctx.root(Value::object(ctx.realm().promise_ctor))?;
        let cap = new_promise_capability(ctx, promise)?;
        Ok((
            ctx.get(cap.promise),
            (ctx.get(cap.resolve), ctx.get(cap.reject)),
        ))
    })?;
    record_mut(ctx, id).capability = Some(cap);
    Ok(cap)
}

fn execute_body(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<Value> {
    let code = record(ctx, id).code;
    let result = if let Some(code) = code {
        ctx.with_scope(|ctx| {
            let exports = ctx.allocate(|rt| {
                TrackedVec::from_slice(&rt.heap.ledger, &rt.heap.modules.get(id).unwrap().exports)
            })?;
            let entry = load::make_function(ctx, code, &exports)?;
            let undef = ctx.root(Value::UNDEFINED)?;
            call::call(ctx, entry, undef, Args::empty())
        })
    } else {
        Ok(Value::UNDEFINED)
    };
    match result {
        Err(Abrupt::Thrown) => {
            fail_pending(ctx, id)?;
            Err(Abrupt::Thrown)
        }
        Err(error) => Err(error),
        Ok(result)
            if code.is_some_and(|code| compiled(ctx, code).kind == FunctionKind::Async)
                || record(ctx, id).has_await =>
        {
            let attach = (|| {
                let result = ctx.root(result)?;
                let cap = ensure_capability(ctx, id)?;
                record_mut(ctx, id).status = ModuleStatus::Async;
                then_module(ctx, ctx.get(result), id, MODULE_DONE, MODULE_FAILED)?;
                Ok(cap.0)
            })();
            match attach {
                Ok(promise) => Ok(promise),
                Err(Abrupt::Thrown) => {
                    let _ = fail_pending(ctx, id);
                    Err(Abrupt::Thrown)
                }
                Err(error) => Err(error),
            }
        }
        Ok(_) => {
            succeed(ctx, id)?;
            Ok(Value::UNDEFINED)
        }
    }
}

fn fail_pending(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<()> {
    let reason = ctx.pending_exception().unwrap_or(Value::UNDEFINED);
    let oom = ctx.is_oom();
    let outcome = ctx.with_scope(|ctx| {
        let root = ctx.root(reason)?;
        fail(ctx, id, reason)?;
        ctx.throw_value(ctx.get(root), oom);
        Ok(())
    });
    if outcome.is_err() {
        if record(ctx, id).status != ModuleStatus::Error {
            let module = record_mut(ctx, id);
            module.status = ModuleStatus::Error;
            module.evaluation_error = Some(reason);
        }
        if !ctx.is_oom() {
            ctx.throw_value(reason, oom);
        }
    }
    outcome
}

fn fail(ctx: &mut ExecCtx<'_, '_>, id: ModId, reason: Value) -> JsResult<()> {
    let module = record_mut(ctx, id);
    if module.status == ModuleStatus::Error {
        return Ok(());
    }
    module.status = ModuleStatus::Error;
    module.evaluation_error = Some(reason);
    if let Some((promise, _)) = module.capability {
        ctx.with_scope(|ctx| {
            let promise = ctx.root(promise)?;
            let value = ctx.root(reason)?;
            reject_promise(ctx, promise, value)
        })?;
    }
    Ok(())
}

fn succeed(ctx: &mut ExecCtx<'_, '_>, id: ModId) -> JsResult<()> {
    let module = record_mut(ctx, id);
    module.status = ModuleStatus::Evaluated;
    if let Some((promise, _)) = module.capability {
        ctx.with_scope(|ctx| {
            let promise = ctx.root(promise)?;
            let value = ctx.root(Value::UNDEFINED)?;
            fulfill_promise(ctx, promise, value)
        })?;
    }
    Ok(())
}

fn then_module(
    ctx: &mut ExecCtx<'_, '_>,
    promise: Value,
    id: ModId,
    success: NativeFnId,
    failure: NativeFnId,
) -> JsResult<()> {
    ctx.with_scope(|ctx| {
        let promise = ctx.root(promise)?;
        let module = ctx.root(Value::module(id))?;
        let yes = ctx.new_native(success, false)?;
        let yes = ctx.bind_function(yes, module, Args::empty())?;
        let no = ctx.new_native(failure, false)?;
        let no = ctx.bind_function(no, module, Args::empty())?;
        perform_promise_then(ctx, promise, yes, no, None)
    })
}

pub(crate) fn module_job<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let ValueKind::Module(module) = ctx.get(this).decode() else {
        unreachable!()
    };
    if id == MODULE_FAILED {
        let reason = args.get(0).map_or(Value::UNDEFINED, |h| ctx.get(h));
        fail(ctx, module, reason)?;
    } else if record(ctx, module).status != ModuleStatus::Error {
        if id == MODULE_DONE {
            succeed(ctx, module)?;
        } else {
            let pending = &mut record_mut(ctx, module).pending_dependencies;
            *pending -= 1;
            if *pending == 0
                && let Err(error) = execute_body(ctx, module)
            {
                if error == Abrupt::Interrupted || ctx.is_oom() {
                    return Err(error);
                }
                ctx.take_exception();
                ctx.runtime.vm.thrown_frames.clear();
            }
        }
    }
    Ok(Value::UNDEFINED)
}

pub(crate) fn check_host_modules(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    for i in 0..ctx.runtime.modules.len() {
        let id = ctx.runtime.modules[i];
        let m = record(ctx, id);
        if !m.waiting_host {
            continue;
        }
        let error = m.evaluation_error;
        if matches!(m.status, ModuleStatus::Evaluated | ModuleStatus::Error) {
            record_mut(ctx, id).waiting_host = false;
        }
        if let Some(error) = error {
            return Err(ctx.throw_value(error, false));
        }
    }
    Ok(())
}

pub(super) fn dynamic_import<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    code: BcId,
    specifier: Handle<'s>,
    options: Handle<'s>,
) -> JsResult<Value> {
    let constructor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
    let capability = new_promise_capability(ctx, constructor)?;
    let result = (|| {
        let specifier = convert::to_string(ctx, specifier)?;
        let attributes = if ctx.get(options) == Value::UNDEFINED {
            NO_ATTRIBUTES
        } else {
            ctx.object_id(options)?;
            let with = ctx.intern_str("with")?;
            let attributes = property::get_property(ctx, options, with)?;
            let attrs = ctx.root(attributes)?;
            if attributes == Value::UNDEFINED {
                NO_ATTRIBUTES
            } else {
                ctx.object_id(attrs)?;
                let mut attributes = NO_ATTRIBUTES;
                let type_key = ctx.intern_str("type")?;
                for &key in property::own_property_keys(ctx, attrs)?.iter() {
                    if matches!(key.decode(), KeyKind::Symbol(_))
                        || !property::get_own_property_descriptor(ctx, attrs, key)?
                            .is_some_and(|d| d.enumerable == Some(true))
                    {
                        continue;
                    }
                    let value = property::get_property(ctx, attrs, key)?;
                    let ValueKind::String(id) = value.decode() else {
                        return Err(ctx.throw_type_error("import attribute must be a string"));
                    };
                    if key != type_key {
                        return Err(ctx.throw_type_error("unsupported import attribute"));
                    }
                    let text = utf16_lossy(ctx.heap(), id);
                    attributes = parse_type(ctx, &text)?;
                }
                attributes
            }
        };
        let source = String::from_utf8_lossy(&compiled(ctx, code).filename).into_owned();
        let name = ctx.new_string(&source)?;
        let kind = ctx.root(Value::int32(i32::from(attributes)))?;
        let args = ctx.args(&[name, specifier, kind, capability.resolve, capability.reject])?;
        let callback = ctx.new_native(DYNAMIC_IMPORT_JOB, false)?;
        let this = ctx.root(Value::UNDEFINED)?;
        let callback = ctx.bind_function(callback, this, args)?;
        ctx.enqueue_job(callback, this)
    })();
    if result == Err(Abrupt::Thrown) {
        let reason = ctx.take_exception().unwrap().0;
        let reason = ctx.root(reason)?;
        reject_promise(ctx, capability.promise, reason)?;
    } else {
        result?;
    }
    Ok(ctx.get(capability.promise))
}

pub(crate) fn dynamic_job<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let resolve = args.get(3).unwrap();
    let reject = args.get(4).unwrap();
    let result = (|| {
        let name = args.get(0).unwrap();
        let ValueKind::String(name) = ctx.get(name).decode() else {
            unreachable!()
        };
        let name = utf16_lossy(ctx.heap(), name);
        let specifier = ctx.intern(args.get(1).unwrap())?;
        let attributes = ctx.get(args.get(2).unwrap()).as_number().unwrap() as u8;
        let id = resolve_request(
            ctx,
            &name,
            ModuleRequest {
                name: specifier,
                attributes,
            },
        )?;
        link(ctx, id)?;
        let result = evaluate(ctx, id)?;
        let object = Value::object(record(ctx, id).namespace.unwrap());
        let namespace = ctx.root(object)?;
        if result == Value::UNDEFINED {
            let undefined = ctx.root(Value::UNDEFINED)?;
            let args = ctx.args(&[namespace])?;
            call::call(ctx, resolve, undefined, args)?;
        } else {
            let promise = ctx.root(result)?;
            let yes = ctx.new_native(MODULE_IMPORT_DONE, false)?;
            let yes_args = ctx.args(&[namespace])?;
            let yes = ctx.bind_function(yes, resolve, yes_args)?;
            let no = ctx.new_native(MODULE_IMPORT_FAILED, false)?;
            let no = ctx.bind_function(no, reject, Args::empty())?;
            perform_promise_then(ctx, promise, yes, no, None)?;
        }
        Ok(())
    })();
    if result == Err(Abrupt::Thrown) {
        let reason = ctx.take_exception().unwrap().0;
        let reason = ctx.root(reason)?;
        let undefined = ctx.root(Value::UNDEFINED)?;
        let args = ctx.args(&[reason])?;
        call::call(ctx, reject, undefined, args)?;
    } else {
        result?;
    }
    Ok(Value::UNDEFINED)
}

pub(crate) fn finish_import<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let args = ctx.args(&[args.get(0).unwrap()])?;
    let undefined = ctx.root(Value::UNDEFINED)?;
    call::call(ctx, this, undefined, args)
}
