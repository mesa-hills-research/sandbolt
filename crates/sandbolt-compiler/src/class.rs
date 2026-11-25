use crate::{
    emit::{Emitter, Result},
    expr::method_flags,
};
use sandbolt_bytecode::*;
use sandbolt_syntax::*;

#[derive(Clone, Copy)]
pub(crate) enum PrivateMember {
    Field,
    Method,
    Accessor { setter: UnitAtom },
}

#[derive(Clone, Copy)]
enum FieldKey {
    Atom(UnitAtom),
    Computed(UnitAtom),
    Private(UnitAtom),
}

enum Init<'a> {
    Field(FieldKey, Option<ExprId>),
    PrivateMethod(UnitAtom, UnitAtom),
    StaticBlock(&'a [StmtId]),
}

impl Emitter<'_> {
    pub(crate) fn class(
        &mut self,
        class: &Class,
        span: Span,
        inferred: Option<UnitAtom>,
    ) -> Result {
        self.descent(|e| e.class_inner(class, span, inferred, false))
    }

    pub(crate) fn computed_class(&mut self, class: &Class, span: Span) -> Result {
        self.descent(|e| e.class_inner(class, span, None, true))
    }

    fn class_inner(
        &mut self,
        class: &Class,
        span: Span,
        inferred: Option<UnitAtom>,
        computed: bool,
    ) -> Result {
        use Opcode::*;
        let identity = class
            .name
            .as_ref()
            .map(|i| self.atom(&i.name))
            .transpose()?;
        let name = identity
            .or(inferred)
            .unwrap_or(BuiltinAtom::EmptyString.id());
        let outer = self.start_scope(Some(self.f().scope))?;
        if let Some(identity) = identity {
            self.bind(identity, VarKind::Const, outer)?;
        }
        if computed {
            self.plain(Dup);
        }
        if let Some(heritage) = class.extends {
            self.expression(heritage)?;
        } else {
            self.plain(Undefined);
        }
        let scope = self.start_scope(Some(outer))?;
        for e in &class.elements {
            let (id, method_kind) = match e {
                ClassElement::Field {
                    key: Name::Private(id),
                    ..
                } => (id, None),
                ClassElement::Method {
                    key: Name::Private(id),
                    kind,
                    ..
                } => (id, Some(*kind)),
                _ => continue,
            };
            let key = self.private_atom(id)?;
            if self.f().private_members.contains_key(&(scope, key)) {
                continue;
            }
            let member = match method_kind {
                None => PrivateMember::Field,
                Some(MethodKind::Getter | MethodKind::Setter) => {
                    let setter = self.atom(&format!("<private-setter:{}>", id.name))?;
                    self.private_symbol(setter, scope, VarKind::Const)?;
                    PrivateMember::Accessor { setter }
                }
                _ => PrivateMember::Method,
            };
            let kind = match member {
                PrivateMember::Field => VarKind::PrivateField,
                PrivateMember::Method => VarKind::PrivateMethod,
                PrivateMember::Accessor { .. } => VarKind::PrivateGetterSetter,
            };
            self.private_symbol(key, scope, kind)?;
            self.fm().private_members.insert((scope, key), member);
        }
        let instance = BuiltinAtom::ClassFieldsInit.id();
        self.bind(instance, VarKind::Const, scope)?;
        let derived = class.extends.is_some();
        if let Some(function) = class.elements.iter().find_map(|e| match e {
            ClassElement::Method {
                function,
                kind: MethodKind::Constructor,
                ..
            } => Some(function),
            _ => None,
        }) {
            let function = Function {
                span,
                ..function.clone()
            };
            self.function(&function, name, false, true, Some(derived))?;
        } else {
            self.default_constructor(span, name, derived)?;
        }
        self.source_loc(span);
        self.flag(
            if computed {
                DefineClassComputed
            } else {
                DefineClass
            },
            name,
            u8::from(derived),
        );
        let prototype = self.stash()?;
        let constructor = self.stash()?;
        if computed {
            self.plain(Drop);
        }
        let mut static_init = Vec::new();
        let mut instance_init = Vec::new();
        let mut static_private = Vec::new();
        let mut instance_private = Vec::new();
        for (index, e) in class.elements.iter().enumerate() {
            match e {
                ClassElement::Method {
                    kind: MethodKind::Constructor,
                    ..
                } => (),
                ClassElement::Method {
                    key: Name::Private(id),
                    kind,
                    function,
                    is_static,
                } => {
                    let key = self.private_atom(id)?;
                    let symbol = match self.private_member(key) {
                        PrivateMember::Accessor { setter } if *kind == MethodKind::Setter => setter,
                        _ => key,
                    };
                    let display = if matches!(kind, MethodKind::Getter | MethodKind::Setter) {
                        let prefix = if *kind == MethodKind::Getter {
                            "get"
                        } else {
                            "set"
                        };
                        self.atom(&format!("{prefix} #{}", id.name.trim_start_matches('#')))?
                    } else {
                        key
                    };
                    let local = self.atom(&format!("<private-method:{}:{index}>", span.start))?;
                    self.bind(local, VarKind::Const, scope)?;
                    self.get(if *is_static { constructor } else { prototype });
                    self.function(function, display, false, true, None)?;
                    self.plain(SetHomeObject);
                    self.ir(IrOp::ScopePutVarInit(self.scoped(local)));
                    self.plain(Drop);
                    let plan = if *is_static {
                        &mut static_private
                    } else {
                        &mut instance_private
                    };
                    plan.push(Init::PrivateMethod(symbol, local));
                }
                ClassElement::Method {
                    key,
                    kind,
                    function,
                    is_static,
                } => {
                    self.get(if *is_static { constructor } else { prototype });
                    let key = self.property_key(key)?;
                    self.function(function, BuiltinAtom::EmptyString.id(), false, true, None)?;
                    if let Some(key) = key {
                        self.flag(DefineMethod, key, method_flags(*kind));
                    } else {
                        self.unsigned(DefineMethodComputed, u32::from(method_flags(*kind)));
                    }
                    self.plain(Drop);
                }
                ClassElement::Field {
                    key,
                    initializer,
                    is_static,
                } => {
                    let key = if let Name::Private(i) = key {
                        FieldKey::Private(self.private_atom(i)?)
                    } else if let Some(key) = self.property_key(key)? {
                        FieldKey::Atom(key)
                    } else {
                        let atom =
                            self.atom(&format!("<computed-field:{}:{index}>", span.start))?;
                        self.bind(atom, VarKind::Const, scope)?;
                        self.ir(IrOp::ScopePutVarInit(self.scoped(atom)));
                        FieldKey::Computed(atom)
                    };
                    let plan = if *is_static {
                        &mut static_init
                    } else {
                        &mut instance_init
                    };
                    plan.push(Init::Field(key, *initializer));
                }
                ClassElement::StaticBlock(body) => static_init.push(Init::StaticBlock(body)),
            }
        }
        let mut accessors: Vec<_> = self
            .f()
            .private_members
            .iter()
            .filter_map(|(&(owner, key), member)| {
                if owner == scope
                    && let PrivateMember::Accessor { setter } = member
                {
                    Some((key, *setter))
                } else {
                    None
                }
            })
            .collect();
        accessors.sort_unstable_by_key(|&(name, _)| name.raw());
        for (key, setter) in accessors {
            for plan in [&mut instance_private, &mut static_private] {
                let has_getter = plan
                    .iter()
                    .any(|op| matches!(op, Init::PrivateMethod(symbol, _) if *symbol == key));
                let has_setter = plan
                    .iter()
                    .any(|op| matches!(op, Init::PrivateMethod(symbol, _) if *symbol == setter));
                if has_getter && !has_setter {
                    plan.push(Init::Field(FieldKey::Private(setter), None));
                }
                if has_setter && !has_getter {
                    plan.push(Init::Field(FieldKey::Private(key), None));
                }
            }
        }
        instance_private.append(&mut instance_init);
        static_private.append(&mut static_init);
        if let Some(identity) = identity {
            self.get(constructor);
            self.ir(IrOp::ScopePutVarInit(self.scoped(identity)));
        }
        if instance_private.is_empty() {
            self.plain(Undefined);
        } else {
            self.get(prototype);
            self.initializer(span, &instance_private)?;
            self.plain(SetHomeObject);
            self.plain(Nip);
        }
        self.ir(IrOp::ScopePutVarInit(self.scoped(instance)));
        if !static_private.is_empty() {
            self.get(constructor);
            self.initializer(span, &static_private)?;
            self.plain(SetHomeObject);
            self.op(CallMethod, Operands::Call(0));
            self.plain(Drop);
        }
        self.get(constructor);
        self.end_scope(scope);
        self.end_scope(outer);
        Ok(())
    }

    fn private_symbol(&mut self, name: UnitAtom, scope: u16, kind: VarKind) -> Result {
        self.bind(name, kind, scope)?;
        self.named(Opcode::PrivateSymbol, name);
        self.ir(IrOp::ScopePutVarInit(self.scoped(name)));
        Ok(())
    }

    pub(crate) fn inherit_private_names(&mut self) -> Result {
        for (name, kind) in self.options.enclosing_private_names.iter().rev() {
            let key = self.atom(&format!("#{name}"))?;
            let member = match kind {
                PrivateNameKind::Field => PrivateMember::Field,
                PrivateNameKind::Method => PrivateMember::Method,
                PrivateNameKind::Accessor => PrivateMember::Accessor {
                    setter: self.atom(&format!("<private-setter:{name}>"))?,
                },
            };
            self.charge(std::mem::size_of::<(UnitAtom, PrivateMember)>())?;
            self.fm().private_members.insert((0, key), member);
        }
        Ok(())
    }

    pub(crate) fn private_member(&self, name: UnitAtom) -> PrivateMember {
        let mut id = self.current;
        let mut scope = self.f().scope;
        loop {
            let f = &self.functions[id];
            if let Some(&member) = f.private_members.get(&(scope, name)) {
                return member;
            }
            if let Some(parent) = f.scopes[usize::from(scope)].info.parent {
                scope = parent;
            } else if let Some((parent, outer)) = f.parent {
                id = parent;
                scope = outer;
            } else {
                return PrivateMember::Field;
            }
        }
    }

    fn closure_constant(&mut self, child: usize, constructor: bool) -> Result {
        let entry = self.constant(ConstEntry::Function(child as u32))?;
        self.op(
            if constructor {
                Opcode::PushConst
            } else {
                Opcode::Fclosure
            },
            Operands::Constant(entry),
        );
        Ok(())
    }

    fn default_constructor(&mut self, span: Span, name: UnitAtom, derived: bool) -> Result {
        use Opcode::*;
        let parent = self.current;
        let child = self.draft(span, name, true, Some((parent, self.f().scope)))?;
        self.start_scope(None)?;
        self.fm().bytecode.flags = FunctionFlags {
            is_derived_class_constructor: derived,
            new_target_allowed: true,
            need_home_object: true,
            super_allowed: true,
            super_call_allowed: derived,
            has_simple_parameter_list: true,
            ..FunctionFlags::default()
        };
        self.function_environment(true, derived, None)?;
        if derived {
            self.plain(InitCtor);
            let this = self.atom("<this>")?;
            self.ir(IrOp::ScopePutVarInit(self.scoped(this)));
        } else {
            self.plain(CheckCtor);
        }
        self.initialize_instance_fields()?;
        self.plain(Undefined);
        self.return_value()?;
        self.end_scope(0);
        self.current = parent;
        self.closure_constant(child, true)
    }

    pub(crate) fn initialize_instance_fields(&mut self) -> Result {
        use Opcode::*;
        self.read(BuiltinAtom::ClassFieldsInit.id());
        let done = self.label();
        self.plain(Dup);
        self.if_false(done);
        self.read_internal("<this>")?;
        self.plain(Swap);
        self.op(CallMethod, Operands::Call(0));
        self.mark(done);
        self.plain(Drop);
        Ok(())
    }

    fn initializer(&mut self, span: Span, plan: &[Init<'_>]) -> Result {
        use Opcode::*;
        let parent = self.current;
        let child = self.draft(
            span,
            BuiltinAtom::ClassFieldsInit.id(),
            true,
            Some((parent, self.f().scope)),
        )?;
        self.start_scope(None)?;
        self.fm().bytecode.flags.need_home_object = true;
        self.fm().bytecode.flags.super_allowed = true;
        self.function_environment(true, false, None)?;
        for item in plan {
            self.read_internal("<this>")?;
            match item {
                Init::Field(key, value) => match key {
                    FieldKey::Atom(name) => {
                        if let Some(value) = value {
                            self.expression_named(*value, Some(*name))?;
                        } else {
                            self.plain(Undefined);
                        }
                        self.named(DefineField, *name);
                    }
                    FieldKey::Computed(key) => {
                        self.read(*key);
                        if let Some(value) = value {
                            self.computed_value(*value)?;
                        } else {
                            self.plain(Undefined);
                        }
                        self.plain(DefineArrayEl);
                        self.plain(Drop);
                    }
                    FieldKey::Private(name) => {
                        self.read(*name);
                        if let Some(value) = value {
                            self.expression_named(*value, Some(*name))?;
                        } else {
                            self.plain(Undefined);
                        }
                        self.plain(DefinePrivateField);
                    }
                },
                Init::PrivateMethod(symbol, method) => {
                    self.read(*symbol);
                    self.read(*method);
                    self.plain(DefinePrivateField);
                }
                Init::StaticBlock(body) => {
                    let function = Function {
                        span,
                        name: None,
                        params: Vec::new(),
                        body: FunctionBody::Block(body.to_vec()),
                        is_async: false,
                        is_generator: false,
                        is_arrow: false,
                        strict: true,
                        use_strict: false,
                    };
                    self.function(
                        &function,
                        BuiltinAtom::ClassFieldsInit.id(),
                        false,
                        true,
                        None,
                    )?;
                    self.plain(SetHomeObject);
                    self.op(CallMethod, Operands::Call(0));
                }
            }
            self.plain(Drop);
        }
        self.plain(ReturnUndef);
        self.end_scope(0);
        self.current = parent;
        self.closure_constant(child, false)
    }
}
