use sandbolt_bytecode::{BuiltinAtom, Opcode, ScopeLink, UnitAtom, VarDef, VarKind};
use sandbolt_syntax::{ParseGoal, ParseOptions, PrivateNameKind, SourceText};

use crate::api::{EvalOptions, parse_options};
use crate::atom::{AtomTable, KeyKind, PropKey};
use crate::builtins::vm_internal::EVAL;
use crate::ledger::TrackedVec;
use crate::object::{ErrorKind, ObjectData};
use crate::ops::{call, convert, property};
use crate::root::{Args, ExecCtx, Handle};
use crate::symbol::WellKnown;
use crate::value::*;
use crate::vm::{CallFrame, EvalBinding, EvalDeclarations, EvalEnvironment};

use super::{bindings, compiled, key, load, well_known};

pub(super) fn is_intrinsic(ctx: &ExecCtx<'_, '_>, value: Value) -> bool {
    value.as_object().is_some_and(|id| matches!(&ctx.heap().object(id).data, ObjectData::NativeFunction(f) if f.function == EVAL && f.realm == ctx.realm_id()))
}

pub(crate) fn execute_indirect<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    input: Handle<'s>,
) -> JsResult<Value> {
    let value = ctx.get(input);
    let ValueKind::String(string) = value.decode() else {
        return Ok(value);
    };
    let parse = ParseOptions {
        goal: ParseGoal::DirectEval {
            allow_new_target: false,
            allow_super_property: false,
            allow_super_call: false,
            in_class_field_init: false,
        },
        ..parse_options(ctx.runtime, &EvalOptions::default())
    };
    if (ctx.heap().string(string).len() as usize).saturating_mul(3) > parse.max_bytes {
        return Err(ctx.throw_oom());
    }
    let source = SourceText::from_utf16("<input>", ctx.heap().string_units(string).collect());
    let image = crate::api::compile_text(ctx, &source, parse)?;
    let functions = load::load_functions(ctx, &image, Some(&source))?;
    let code = functions[image.entry_function as usize];
    let compiled = ctx
        .runtime
        .heap
        .bytecode
        .get_mut(code)
        .unwrap()
        .compiled
        .as_mut()
        .unwrap();
    compiled.eval = Some(EvalEnvironment {
        bindings: TrackedVec::new(),
        declarations: None,
        special: None,
    });
    let entry = load::make_function(ctx, code, &[])?;
    let this = ctx.root(Value::object(ctx.realm().global_object))?;
    call::call(ctx, entry, this, Args::empty())
}

pub(super) fn execute(
    ctx: &mut ExecCtx<'_, '_>,
    caller: CallFrame,
    scope: u16,
    input: Value,
) -> JsResult<Value> {
    let ValueKind::String(string) = input.decode() else {
        return Ok(input);
    };
    ctx.check_interrupt()?;
    let code = caller.code.unwrap();
    let f = compiled(ctx, code);
    let options = EvalOptions {
        strict: f.strict,
        ..Default::default()
    };
    let field_context = f.class_field_context;
    let goal = ParseGoal::DirectEval {
        allow_new_target: f.flags.new_target_allowed || field_context,
        allow_super_property: f.flags.super_allowed,
        allow_super_call: f.flags.super_call_allowed,
        in_class_field_init: field_context,
    };
    let environment = environment(ctx, caller, scope)?;
    let mut parse = ParseOptions {
        goal,
        ..parse_options(ctx.runtime, &options)
    };
    let private_names = private_names(ctx, &environment, &mut parse.max_bytes)?;
    parse.enclosing_private_names = &private_names;
    if (ctx.heap().string(string).len() as usize).saturating_mul(3) > parse.max_bytes {
        return Err(ctx.throw_oom());
    }
    let units = ctx.heap().string_units(string).collect();
    let source = SourceText::from_utf16("<input>", units);
    let image = crate::api::compile_text(ctx, &source, parse)?;
    let this_key = ctx.intern_str("<this>")?;
    let this = find_cell(ctx, &environment, this_key)
        .unwrap_or(ctx.runtime.vm.stack[caller.base as usize + 1]);
    let specials = if let Some(special) = compiled(ctx, code)
        .eval
        .as_ref()
        .and_then(|env| env.special)
    {
        special
    } else {
        let active = ctx.intern_str("<active_function>")?;
        let target = ctx.intern_str("<new.target>")?;
        let home = ctx.intern_str("<home_object>")?;
        let fallback_home = match &ctx.heap().object(caller.callee).data {
            ObjectData::BytecodeFunction(f) => {
                f.home_object.map_or(Value::UNDEFINED, Value::object)
            }
            _ => Value::UNDEFINED,
        };
        [
            find_cell(ctx, &environment, active).unwrap_or(Value::object(caller.callee)),
            find_cell(ctx, &environment, target)
                .unwrap_or(caller.new_target.map_or(Value::UNDEFINED, Value::object)),
            find_cell(ctx, &environment, home).unwrap_or(fallback_home),
        ]
    };
    let functions = load::load_functions(ctx, &image, Some(&source))?;
    for (index, &function) in functions.iter().enumerate() {
        let entry = index == image.entry_function as usize;
        let eval = ctx.allocate(|rt| {
            Ok(EvalEnvironment {
                bindings: TrackedVec::from_slice(&rt.heap.ledger, &environment.bindings)?,
                declarations: if entry {
                    environment.declarations
                } else {
                    None
                },
                special: entry.then_some(specials),
            })
        })?;
        let function = ctx
            .runtime
            .heap
            .bytecode
            .get_mut(function)
            .unwrap()
            .compiled
            .as_mut()
            .unwrap();
        function.eval = Some(eval);
        if entry {
            function.class_field_context |= field_context;
        }
    }
    let entry = load::make_function(ctx, functions[image.entry_function as usize], &[])?;
    let this = ctx.root(this)?;
    call::call(ctx, entry, this, Args::empty())
}

fn find_cell(ctx: &ExecCtx<'_, '_>, env: &EvalEnvironment, key: PropKey) -> Option<Value> {
    env.bindings.iter().find_map(|binding| match *binding {
        EvalBinding::Cell { name, id, .. } if name == key => Some(bindings::read_ref(ctx, id)),
        _ => None,
    })
}

fn environment(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    scope: u16,
) -> JsResult<EvalEnvironment> {
    let code = frame.code.unwrap();
    let mut env = EvalEnvironment {
        bindings: TrackedVec::new(),
        declarations: None,
        special: None,
    };
    let f = compiled(ctx, code);
    let args = f.arg_count as usize;
    let mut arg_scope = f
        .vars
        .first()
        .filter(|var| args != 0 && !var.name.is_none())
        .map(|var| var.scope_level);
    let arg_base = frame.base as usize + 2;
    let var_base = arg_base + frame.arg_slots as usize;
    let mut next = match scope {
        0 => ScopeLink::End,
        u16::MAX => ScopeLink::ParameterEnd,
        n => ScopeLink::Local(n - 1),
    };
    while let ScopeLink::Local(index) = next {
        let index = index as usize;
        let Some(var) = compiled(ctx, code).vars.get(args + index).cloned() else {
            break;
        };
        next = var.scope_next;
        if arg_scope.is_some_and(|scope| var.scope_level <= scope) {
            add_arguments(ctx, &mut env, frame, code, arg_base, args)?;
            arg_scope = None;
        }
        add_local(ctx, &mut env, frame, code, var_base + index, &var)?;
    }
    if arg_scope.is_some() && next == ScopeLink::End {
        add_arguments(ctx, &mut env, frame, code, arg_base, args)?;
    }
    let local_end = env.bindings.len();
    for index in 0..compiled(ctx, code).closures.len() {
        let def = compiled(ctx, code).closures[index].clone();
        let value = bindings::captured(ctx, frame, index as u16);
        if let ValueKind::VarRef(id) = value.decode() {
            append(
                ctx,
                &mut env,
                code,
                def.name,
                id,
                (def.var_kind, def.lexical_tdz, !def.is_const),
            )?;
        }
    }
    if let Some(inherited) = compiled(ctx, code).eval.as_ref() {
        let len = inherited.bindings.len();
        if env.declarations.is_none() {
            env.declarations = inherited.declarations.map(|target| EvalDeclarations {
                end: env.bindings.len() + target.end,
                object: target.object,
            });
        }
        for index in 0..len {
            let binding = compiled(ctx, code).eval.as_ref().unwrap().bindings[index];
            ctx.allocate(|rt| env.bindings.push(&rt.heap.ledger, binding))?;
        }
    }
    env.declarations.get_or_insert(EvalDeclarations {
        end: local_end,
        object: None,
    });
    Ok(env)
}

fn add_arguments(
    ctx: &mut ExecCtx<'_, '_>,
    env: &mut EvalEnvironment,
    frame: CallFrame,
    code: BcId,
    arg_base: usize,
    args: usize,
) -> JsResult<()> {
    // The last occurrence of a duplicate formal is the visible binding.
    for index in (0..args).rev() {
        let var = compiled(ctx, code).vars[index].clone();
        add_local(ctx, env, frame, code, arg_base + index, &var)?;
    }
    Ok(())
}

fn has_env_object(name: UnitAtom) -> bool {
    matches!(
        name.builtin(),
        Some(BuiltinAtom::InternalWith | BuiltinAtom::InternalVar | BuiltinAtom::InternalArgVar)
    )
}

fn add_local(
    ctx: &mut ExecCtx<'_, '_>,
    env: &mut EvalEnvironment,
    frame: CallFrame,
    code: BcId,
    stack: usize,
    var: &VarDef,
) -> JsResult<()> {
    if var.name.is_none() {
        return Ok(());
    }
    let value = bindings::local_ref(ctx, frame, stack)?;
    let ValueKind::VarRef(id) = value.decode() else {
        unreachable!()
    };
    if env.declarations.is_none()
        && matches!(
            var.name.builtin(),
            Some(BuiltinAtom::InternalVar | BuiltinAtom::InternalArgVar)
        )
        && let Some(object) = bindings::read_ref(ctx, id).as_object()
    {
        env.declarations = Some(EvalDeclarations {
            end: env.bindings.len(),
            object: Some(object),
        });
    }
    append(
        ctx,
        env,
        code,
        var.name,
        id,
        (
            var.kind,
            var.lexical_tdz && var.kind != VarKind::Var,
            !var.is_const,
        ),
    )
}

fn append(
    ctx: &mut ExecCtx<'_, '_>,
    env: &mut EvalEnvironment,
    code: BcId,
    name: UnitAtom,
    id: VarRefId,
    (kind, lexical, writable): (VarKind, bool, bool),
) -> JsResult<()> {
    let binding = if has_env_object(name) {
        let with = name == BuiltinAtom::InternalWith.id();
        let Some(object) = bindings::read_ref(ctx, id).as_object() else {
            return Ok(());
        };
        EvalBinding::Object { id: object, with }
    } else {
        EvalBinding::Cell {
            name: key(ctx, code, name),
            id,
            kind,
            lexical,
            writable,
        }
    };
    ctx.allocate(|rt| env.bindings.push(&rt.heap.ledger, binding))?;
    Ok(())
}

fn private_names(
    ctx: &mut ExecCtx<'_, '_>,
    env: &EvalEnvironment,
    budget: &mut usize,
) -> JsResult<TrackedVec<(String, PrivateNameKind)>> {
    let mut names = TrackedVec::new();
    for binding in env.bindings.iter() {
        let EvalBinding::Cell { name, kind, .. } = *binding else {
            continue;
        };
        let kind = match kind {
            VarKind::PrivateField => PrivateNameKind::Field,
            VarKind::PrivateMethod => PrivateNameKind::Method,
            VarKind::PrivateGetter | VarKind::PrivateSetter | VarKind::PrivateGetterSetter => {
                PrivateNameKind::Accessor
            }
            _ => continue,
        };
        let KeyKind::String(atom) = name.decode() else {
            continue;
        };
        let Some(string) = ctx.heap().atoms.dynamic_string(atom) else {
            continue;
        };
        let bytes = (ctx.heap().string(string).len() as usize).saturating_mul(3);
        *budget = budget
            .checked_sub(bytes + std::mem::size_of::<(String, PrivateNameKind)>())
            .ok_or_else(|| ctx.throw_oom())?;
        let mut name = String::new();
        name.try_reserve_exact(bytes).map_err(|_| ctx.throw_oom())?;
        name.extend(
            char::decode_utf16(ctx.heap().string_units(string).skip(1))
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
        );
        ctx.allocate(|rt| names.try_reserve(&rt.heap.ledger, 1))?;
        names.push_reserved((name, kind));
    }
    Ok(names)
}

pub(super) fn lookup(
    ctx: &mut ExecCtx<'_, '_>,
    code: BcId,
    name: PropKey,
) -> JsResult<Option<Value>> {
    let Some(env) = compiled(ctx, code).eval.as_ref() else {
        return Ok(None);
    };
    let len = env.bindings.len();
    let internal = match name.decode() {
        KeyKind::String(atom) => AtomTable::builtin(atom)
            .map(|s| s.starts_with(['#', '<']))
            .or_else(|| {
                let string = ctx.heap().atoms.dynamic_string(atom)?;
                Some(matches!(
                    ctx.heap().string_units(string).next(),
                    Some(35 | 60)
                ))
            })
            .unwrap_or(false),
        _ => false,
    };
    for index in 0..len {
        let binding = compiled(ctx, code).eval.as_ref().unwrap().bindings[index];
        match binding {
            EvalBinding::Cell { name: key, id, .. } if key == name => {
                return Ok(Some(Value::var_ref(id)));
            }
            EvalBinding::Object { id, with } if !internal => {
                let found = ctx.with_scope(|ctx| {
                    let object = ctx.root(Value::object(id))?;
                    if !property::has_property(ctx, object, name)? {
                        return Ok(false);
                    }
                    if with {
                        let excluded = property::get_property(
                            ctx,
                            object,
                            well_known(ctx, WellKnown::Unscopables),
                        )?;
                        if excluded.as_object().is_some() {
                            let excluded = ctx.root(excluded)?;
                            let skip = property::get_property(ctx, excluded, name)?;
                            if convert::to_boolean_value(ctx.heap(), skip) {
                                return Ok(false);
                            }
                        }
                    }
                    Ok(true)
                })?;
                if found {
                    return Ok(Some(Value::object(id)));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(super) fn immutable_kind(ctx: &ExecCtx<'_, '_>, id: VarRefId) -> Option<VarKind> {
    let code = ctx.runtime.vm.frames.last()?.code?;
    compiled(ctx, code)
        .eval
        .as_ref()?
        .bindings
        .iter()
        .find_map(|binding| match *binding {
            EvalBinding::Cell {
                id: other,
                writable: false,
                kind,
                ..
            } if other == id => Some(kind),
            _ => None,
        })
}

pub(super) fn declaration<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    code: BcId,
    name: PropKey,
    op: Opcode,
    function: Option<Handle<'s>>,
) -> JsResult<bool> {
    let Some(env) = compiled(ctx, code).eval.as_ref() else {
        return Ok(false);
    };
    let Some(target) = env.declarations else {
        return Ok(false);
    };
    for binding in &env.bindings[..target.end] {
        if let EvalBinding::Cell {
            name: key,
            id,
            lexical,
            kind,
            ..
        } = *binding
            && key == name
            && kind != VarKind::Catch
        {
            if lexical {
                return Err(ctx.throw_error(ErrorKind::Syntax, "redeclaration of lexical binding"));
            }
            if let Some(function) = function {
                bindings::write_ref(ctx, id, ctx.get(function));
            }
            return Ok(true);
        }
    }
    let Some(object) = target.object else {
        return Ok(false);
    };
    if op == Opcode::CheckDefineVar {
        return Ok(true);
    }
    let target = ctx.root(Value::object(object))?;
    if let Some(function) = function {
        super::define_default(ctx, target, name, function)?;
    } else if !property::has_property(ctx, target, name)? {
        let undefined = ctx.root(Value::UNDEFINED)?;
        super::define_default(ctx, target, name, undefined)?;
    }
    Ok(true)
}
