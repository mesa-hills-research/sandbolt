use crate::{
    CompileError,
    class::PrivateMember,
    emit::{Emitter, Result, string},
};
use sandbolt_bytecode::*;
use sandbolt_syntax::*;

#[derive(Clone, Copy)]
pub(crate) enum Reference {
    Binding(ScopedName),
    Dynamic(ScopedName, u16),
    Field(UnitAtom),
    Element,
    Super,
    Private(UnitAtom, PrivateMember),
    Invalid,
}

impl Reference {
    pub(crate) fn width(self) -> u8 {
        match self {
            Self::Binding(_) | Self::Invalid => 0,
            Self::Field(_) => 1,
            Self::Super => 3,
            _ => 2,
        }
    }
}

fn binary(op: BinaryOp) -> Opcode {
    use BinaryOp::*;
    match op {
        Add => Opcode::Add,
        Sub => Opcode::Sub,
        Mul => Opcode::Mul,
        Div => Opcode::Div,
        Rem => Opcode::Mod,
        Pow => Opcode::Pow,
        Shl => Opcode::Shl,
        Shr => Opcode::Sar,
        UShr => Opcode::Shr,
        BitAnd => Opcode::And,
        BitXor => Opcode::Xor,
        BitOr => Opcode::Or,
        Lt => Opcode::Lt,
        Le => Opcode::Lte,
        Gt => Opcode::Gt,
        Ge => Opcode::Gte,
        Eq => Opcode::Eq,
        Ne => Opcode::Neq,
        StrictEq => Opcode::StrictEq,
        StrictNe => Opcode::StrictNeq,
        In => Opcode::In,
        Instanceof => Opcode::Instanceof,
        And | Or | Nullish => unreachable!(),
    }
}

impl Emitter<'_> {
    pub(crate) fn expression(&mut self, id: ExprId) -> Result {
        self.descent(|e| e.expr(id, None))
    }

    pub(crate) fn unparen(&self, mut id: ExprId) -> ExprId {
        while let Expr::Parenthesized(inner) = self.ast().expr(id).kind {
            id = inner;
        }
        id
    }

    pub(crate) fn expression_named(&mut self, id: ExprId, name: Option<UnitAtom>) -> Result {
        let id = self.unparen(id);
        let node = self.ast().expr(id);
        self.source_loc(node.span);
        match &node.kind {
            Expr::Function(function) if function.name.is_none() && name.is_some() => {
                self.function(function, name.unwrap_or_default(), true, false, None)
            }
            Expr::Class(class) if class.name.is_none() && name.is_some() => {
                self.class(class, node.span, name)
            }
            _ => self.expression(id),
        }
    }

    pub(crate) fn anonymous_definition(&self, id: ExprId) -> bool {
        match &self.ast().expr(self.unparen(id)).kind {
            Expr::Function(f) => f.name.is_none(),
            Expr::Class(c) => c.name.is_none(),
            _ => false,
        }
    }

    fn expr(&mut self, id: ExprId, chain: Option<u32>) -> Result {
        use Opcode::*;
        let node = self.ast().expr(id);
        self.source_loc(node.span);
        match &node.kind {
            Expr::Identifier(id) => {
                let atom = self.atom(&id.name)?;
                self.read(atom);
            }
            Expr::PrivateIdentifier(id) => {
                let atom = self.private_atom(id)?;
                self.read(atom);
            }
            Expr::This => self.read_internal("<this>")?,
            Expr::NewTarget => self.read_internal("<new.target>")?,
            Expr::Super => {
                return Err(self.error(
                    node.span,
                    "Expected a reference to a superclass member or constructor",
                ));
            }
            Expr::Null => self.plain(Null),
            Expr::Boolean(value) => self.plain(if *value { PushTrue } else { PushFalse }),
            Expr::Number(literal) => self.number(literal)?,
            Expr::String(literal) => self.push_string(&literal.value)?,
            Expr::RegExp(regex) => {
                let flags = regex.flags.bytes().fold(0, |bits, flag| {
                    bits | match flag {
                        b'g' => 1,
                        b'i' => 2,
                        b'm' => 4,
                        b's' => 8,
                        b'u' => 16,
                        b'y' => 32,
                        b'd' => 64,
                        b'v' => 256,
                        _ => 0,
                    }
                });
                let constant = self.constant(ConstEntry::RegExp {
                    pattern: string(&regex.pattern),
                    flags,
                })?;
                self.op(PushConst, Operands::Constant(constant));
            }
            Expr::Array { items, .. } => self.array(items.iter().copied(), true)?,
            Expr::Object { properties, .. } => self.object(properties)?,
            Expr::Function(f) => {
                let name = f
                    .name
                    .as_ref()
                    .map(|i| self.atom(&i.name))
                    .transpose()?
                    .unwrap_or(BuiltinAtom::EmptyString.id());
                self.function(f, name, true, false, None)?;
            }
            Expr::Class(c) => self.class(c, node.span, None)?,
            Expr::Template(t) => self.template(t)?,
            Expr::TaggedTemplate { tag, template } => self.tagged_template(*tag, template)?,
            Expr::Parenthesized(id) => self.expression(*id)?,
            Expr::Sequence(items) => {
                for (i, expr) in items.iter().enumerate() {
                    self.expression(*expr)?;
                    if i + 1 != items.len() {
                        self.plain(Drop);
                    }
                }
            }
            Expr::Unary { op, argument } => self.unary(*op, *argument)?,
            Expr::Update {
                op,
                argument,
                prefix,
            } => self.update(*op, *argument, *prefix)?,
            Expr::Binary { op, left, right } => self.binary(*op, *left, *right)?,
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                let no = self.label();
                let done = self.label();
                self.expression(*test)?;
                self.if_false(no);
                self.expression(*consequent)?;
                self.jump(done);
                self.mark(no);
                self.expression(*alternate)?;
                self.mark(done);
            }
            Expr::Assignment { op, target, value } => self.assignment(*op, *target, *value)?,
            Expr::Member {
                object,
                key,
                optional,
            } => self.member(*object, key, *optional, chain, false)?,
            Expr::Call {
                callee,
                arguments,
                optional,
            } => self.call(*callee, arguments, *optional, chain, false)?,
            Expr::Chain(inner) => self.chain(*inner, false)?,
            Expr::New { callee, arguments } => self.call(*callee, arguments, false, None, true)?,
            Expr::ImportMeta => self.unsigned(SpecialObject, 6),
            Expr::ImportCall { specifier, options } => {
                self.expression(*specifier)?;
                if let Some(options) = options {
                    self.expression(*options)?;
                } else {
                    self.plain(Undefined);
                }
                self.plain(Import);
            }
            Expr::Yield { argument, delegate } => self.yield_expression(*argument, *delegate)?,
        }
        Ok(())
    }

    pub(crate) fn read_internal(&mut self, key: &str) -> Result {
        let name = self.atom(key)?;
        if self.current == 0
            && !self.f().scope().names.contains_key(&name)
            && !self.local_binding(self.scoped(name))
        {
            match key {
                "<this>" => {
                    if self.f().module.is_some() {
                        self.plain(Opcode::Undefined);
                    } else {
                        self.plain(Opcode::PushThis);
                    }
                }
                "<new.target>" => self.unsigned(Opcode::SpecialObject, 3),
                "<home_object>" => self.unsigned(Opcode::SpecialObject, 4),
                "<active_function>" => self.unsigned(Opcode::SpecialObject, 2),
                _ => unreachable!(),
            }
        } else {
            self.read(name);
        }
        Ok(())
    }

    pub(crate) fn push_string(&mut self, literal: &JsStrLit) -> Result {
        if literal.units().next().is_none() {
            self.plain(Opcode::PushEmptyString);
        } else {
            let index = self.constant(ConstEntry::String(string(literal)))?;
            self.op(Opcode::PushConst, Operands::Constant(index));
        }
        Ok(())
    }

    fn number(&mut self, number: &NumericLiteral) -> Result {
        if let NumericKind::BigInt { radix } = number.kind {
            let raw = number.raw.strip_suffix('n').unwrap_or(&number.raw);
            let digits = if radix == 10 { raw } else { &raw[2..] };
            let value = decimal_bigint(digits, radix);
            if let Ok(small) = value.parse::<i32>() {
                self.op(Opcode::PushBigintI32, Operands::Signed(small));
            } else {
                let c = self.constant(ConstEntry::BigInt(value))?;
                self.op(Opcode::PushConst, Operands::Constant(c));
            }
        } else if let Some(num) = number.parse_numeric_value() {
            if num == f64::from(num as i32) {
                self.int(num as i32);
            } else {
                let c = self.constant(ConstEntry::Float(num))?;
                self.op(Opcode::PushConst, Operands::Constant(c));
            }
        }
        Ok(())
    }

    pub(crate) fn property_key(&mut self, key: &Name) -> Result<Option<UnitAtom>> {
        let static_name = match key {
            Name::Ident(id) => self.atom(&id.name)?,
            Name::String(value) => self.atom_string(string(&value.value))?,
            Name::Private(id) => self.private_atom(id)?,
            Name::Number(literal) => {
                self.number(literal)?;
                self.plain(Opcode::ToPropkey);
                return Ok(None);
            }
            Name::Computed(expression) => {
                self.expression(*expression)?;
                self.plain(Opcode::ToPropkey);
                return Ok(None);
            }
        };
        Ok(Some(static_name))
    }

    pub(crate) fn private_atom(&mut self, ident: &Ident) -> Result<UnitAtom> {
        if ident.name.starts_with('#') {
            self.atom(&ident.name)
        } else {
            self.atom(&format!("#{}", ident.name))
        }
    }

    pub(crate) fn computed_value(&mut self, value: ExprId) -> Result {
        let node = self.ast().expr(self.unparen(value));
        if let Expr::Class(class) = &node.kind
            && class.name.is_none()
        {
            return self.computed_class(class, node.span);
        }
        self.expression(value)?;
        if self.anonymous_definition(value) {
            self.plain(Opcode::SetNameComputed);
        }
        Ok(())
    }

    fn object(&mut self, props: &[Property]) -> Result {
        use Opcode::*;
        self.plain(Object);
        for prop in props {
            match prop {
                Property::Value { key, value } => {
                    let key = self.property_key(key)?;
                    if self.options.annex_b && key == Some(BuiltinAtom::Proto.id()) {
                        self.expression(*value)?;
                        self.plain(SetProto);
                    } else if let Some(key) = key {
                        self.expression_named(*value, Some(key))?;
                        self.named(DefineField, key);
                    } else {
                        self.computed_value(*value)?;
                        self.plain(DefineArrayEl);
                        self.plain(Drop);
                    }
                }
                Property::Shorthand(name) | Property::CoverInitialized { key: name, .. } => {
                    let name = self.atom(&name.name)?;
                    self.read(name);
                    self.named(DefineField, name);
                }
                Property::Method {
                    key,
                    kind,
                    function,
                } => {
                    let key = self.property_key(key)?;
                    self.function(
                        function,
                        key.unwrap_or(BuiltinAtom::EmptyString.id()),
                        false,
                        true,
                        None,
                    )?;
                    let flags = method_flags(*kind) | 4;
                    if let Some(key) = key {
                        self.flag(DefineMethod, key, flags);
                    } else {
                        self.unsigned(DefineMethodComputed, u32::from(flags));
                    }
                }
                Property::Spread(expr) => {
                    self.expression(*expr)?;
                    self.plain(Null);
                    self.unsigned(CopyDataProperties, 6);
                    self.plain(Drop);
                    self.plain(Drop);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn array(
        &mut self,
        items: impl IntoIterator<Item = Option<Item>>,
        holes: bool,
    ) -> Result {
        use Opcode::*;
        self.op(ArrayFrom, Operands::Call(0));
        self.int(0);
        let mut trailing_hole = false;
        for item in items {
            trailing_hole = item.is_none();
            match item {
                Some(Item::Value(value)) => {
                    self.expression(value)?;
                    self.plain(DefineArrayEl);
                    self.plain(Inc);
                }
                Some(Item::Spread(value)) => {
                    self.expression(value)?;
                    self.plain(Append);
                }
                None => self.plain(Inc),
            }
        }
        if holes && trailing_hole {
            self.plain(Dup1);
            self.named(PutField, BuiltinAtom::Length.id());
        } else {
            self.plain(Drop);
        }
        Ok(())
    }

    fn template(&mut self, t: &Template) -> Result {
        if let Some(head) = t.elements.first().and_then(|e| e.cooked.as_ref()) {
            self.push_string(head)?;
        } else {
            self.plain(Opcode::PushEmptyString);
        }
        for (&expr, element) in t.substitutions.iter().zip(&t.elements[1..]) {
            self.expression(expr)?;
            self.plain(Opcode::ToPropkey);
            self.plain(Opcode::Add);
            if let Some(s) = element
                .cooked
                .as_ref()
                .filter(|s| s.units().next().is_some())
            {
                self.push_string(s)?;
                self.plain(Opcode::Add);
            }
        }
        Ok(())
    }

    fn tagged_template(&mut self, tag: ExprId, t: &Template) -> Result {
        let this = self.callee(tag, None)?;
        let c = self.constant(ConstEntry::TemplateObject(TemplateObject {
            cooked: t
                .elements
                .iter()
                .map(|e| e.cooked.as_ref().map(string))
                .collect(),
            raw: t.elements.iter().map(|e| string(&e.raw)).collect(),
        }))?;
        self.op(Opcode::PushConst, Operands::Constant(c));
        for &arg in &t.substitutions {
            self.expression(arg)?;
        }
        let argc =
            u16::try_from(t.substitutions.len() + 1).map_err(|_| CompileError::StackOverflow)?;
        self.op(
            if this {
                Opcode::CallMethod
            } else {
                Opcode::Call
            },
            Operands::Call(argc),
        );
        Ok(())
    }

    fn binary(&mut self, op: BinaryOp, left: ExprId, right: ExprId) -> Result {
        use Opcode::*;
        if op == BinaryOp::In
            && let Expr::PrivateIdentifier(id) = &self.ast().expr(left).kind
        {
            self.expression(right)?;
            let atom = self.private_atom(id)?;
            self.read(atom);
            self.plain(PrivateIn);
            return Ok(());
        }
        self.expression(left)?;
        match op {
            BinaryOp::And | BinaryOp::Or | BinaryOp::Nullish => {
                let done = self.label();
                self.plain(Dup);
                match op {
                    BinaryOp::And => self.if_false(done),
                    BinaryOp::Or => self.if_true(done),
                    _ => {
                        self.plain(IsUndefinedOrNull);
                        self.if_false(done);
                    }
                }
                self.plain(Drop);
                self.expression(right)?;
                self.mark(done);
            }
            _ => {
                self.expression(right)?;
                self.plain(binary(op));
            }
        }
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp, id: ExprId) -> Result {
        use Opcode::*;
        let id = self.unparen(id);
        if op == UnaryOp::Delete {
            return self.delete(id, None);
        }
        if op == UnaryOp::Typeof
            && let Expr::Identifier(i) = &self.ast().expr(id).kind
        {
            let atom = self.atom(&i.name)?;
            self.ir(IrOp::ScopeGetVarUndef(self.scoped(atom)));
        } else {
            self.expression(id)?;
        }
        match op {
            UnaryOp::Void => {
                self.plain(Drop);
                self.plain(Undefined);
            }
            UnaryOp::Plus => self.plain(Plus),
            UnaryOp::Minus => self.plain(Neg),
            UnaryOp::Not => self.plain(Lnot),
            UnaryOp::BitNot => self.plain(Not),
            UnaryOp::Typeof => self.plain(Typeof),
            UnaryOp::Await => self.await_value(),
            UnaryOp::Delete => unreachable!(),
        }
        Ok(())
    }

    pub(crate) fn await_value(&mut self) {
        if self.f().module.is_some() {
            self.fm().module_await = true;
        }
        self.plain(Opcode::Await);
    }

    fn delete(&mut self, id: ExprId, chain: Option<u32>) -> Result {
        use Opcode::*;
        let node = self.ast().expr(id);
        match &node.kind {
            Expr::Chain(inner) => self.chain(*inner, true)?,
            Expr::Identifier(i) => {
                let atom = self.atom(&i.name)?;
                self.ir(IrOp::ScopeDeleteVar(self.scoped(atom)));
            }
            Expr::Member {
                object,
                key,
                optional,
            } => {
                if self.is_super(*object) {
                    self.super_base(key)?;
                    self.flag(ThrowError, BuiltinAtom::Super.id(), 3);
                    return Ok(());
                }
                self.descent(|e| e.expr(*object, chain))?;
                if *optional && let Some(short) = chain {
                    self.null_guard(short, false);
                }
                if let Some(atom) = self.member_key(key)? {
                    self.named(PushAtomValue, atom);
                }
                self.plain(Delete);
            }
            _ => {
                self.descent(|e| e.expr(id, chain))?;
                self.plain(Drop);
                self.plain(PushTrue);
            }
        }
        Ok(())
    }

    fn chain(&mut self, id: ExprId, deleting: bool) -> Result {
        let short = self.label();
        let done = self.label();
        if deleting {
            self.delete(id, Some(short))?;
        } else {
            self.descent(|e| e.expr(id, Some(short)))?;
        }
        self.jump(done);
        self.mark(short);
        self.plain(if deleting {
            Opcode::PushTrue
        } else {
            Opcode::Undefined
        });
        self.mark(done);
        Ok(())
    }

    fn null_guard(&mut self, short: u32, receiver: bool) {
        let pass = self.label();
        self.plain(Opcode::Dup);
        self.plain(Opcode::IsUndefinedOrNull);
        self.if_false(pass);
        self.plain(Opcode::Drop);
        if receiver {
            self.plain(Opcode::Drop);
        }
        self.jump(short);
        self.mark(pass);
    }

    fn is_super(&self, expr: ExprId) -> bool {
        matches!(self.ast().expr(expr).kind, Expr::Super)
    }

    fn member(
        &mut self,
        object: ExprId,
        key: &Name,
        optional: bool,
        chain: Option<u32>,
        receiver: bool,
    ) -> Result {
        use Opcode::*;
        if self.is_super(object) {
            self.super_base(key)?;
            if receiver {
                self.plain(Dup3);
                self.plain(GetSuperValue);
                self.plain(Nip);
                self.plain(Nip);
            } else {
                self.plain(GetSuperValue);
            }
        } else {
            self.descent(|e| e.expr(object, chain))?;
            if optional && let Some(short) = chain {
                self.null_guard(short, false);
            }
            if receiver {
                self.plain(Dup);
            }
            if let Name::Private(id) = key {
                let atom = self.private_atom(id)?;
                self.read(atom);
                self.get_private(self.private_member(atom));
            } else if let Some(key) = self.member_key(key)? {
                self.named(GetField, key);
            } else {
                self.plain(GetArrayEl);
            }
        }
        Ok(())
    }

    fn member_key(&mut self, key: &Name) -> Result<Option<UnitAtom>> {
        if let Name::Computed(expression) = key {
            self.expression(*expression)?;
            Ok(None)
        } else {
            self.property_key(key)
        }
    }

    fn super_base(&mut self, key: &Name) -> Result {
        self.read_internal("<this>")?;
        if let Some(atom) = self.member_key(key)? {
            self.named(Opcode::PushAtomValue, atom);
        }
        self.read_internal("<home_object>")?;
        self.plain(Opcode::GetSuper);
        self.plain(Opcode::Swap);
        Ok(())
    }

    fn callee(&mut self, id: ExprId, chain: Option<u32>) -> Result<bool> {
        let id = self.unparen(id);
        match &self.ast().expr(id).kind {
            Expr::Member {
                object,
                key,
                optional,
            } => {
                self.member(*object, key, *optional, chain, true)?;
                Ok(true)
            }
            Expr::Identifier(i) => {
                self.source_loc(self.ast().expr(id).span);
                let atom = self.atom(&i.name)?;
                self.ir(IrOp::ScopeGetRef(self.scoped(atom)));
                Ok(true)
            }
            Expr::Chain(inner) if matches!(self.ast().expr(*inner).kind, Expr::Member { .. }) => {
                let short = self.label();
                let end = self.label();
                self.descent(|e| e.callee(*inner, Some(short)))?;
                self.jump(end);
                self.mark(short);
                self.plain(Opcode::Undefined);
                self.plain(Opcode::Undefined);
                self.mark(end);
                Ok(true)
            }
            _ => {
                self.descent(|e| e.expr(id, chain))?;
                Ok(false)
            }
        }
    }

    fn call(
        &mut self,
        callee: ExprId,
        args: &[Item],
        optional: bool,
        chain: Option<u32>,
        constructor: bool,
    ) -> Result {
        use Opcode::*;
        let super_call = self.is_super(callee);
        let constructor_site =
            (constructor || super_call).then(|| self.ast().expr(self.unparen(callee)).span);
        let direct = !constructor
            && !optional
            && matches!(&self.ast().expr(self.unparen(callee)).kind, Expr::Identifier(i) if i.name == "eval");
        let receiver = if super_call {
            self.read_internal("<active_function>")?;
            self.plain(GetSuper);
            self.read_internal("<new.target>")?;
            false
        } else if constructor {
            self.expression(callee)?;
            self.plain(Dup);
            false
        } else if direct {
            self.read(BuiltinAtom::Eval.id());
            false
        } else {
            self.callee(callee, chain)?
        };
        if optional && let Some(short) = chain {
            self.null_guard(short, receiver);
        }
        if args.iter().any(|item| matches!(item, Item::Spread(_))) {
            if !direct && !constructor && !super_call {
                if receiver {
                    self.plain(Swap);
                } else {
                    self.plain(Undefined);
                }
            }
            self.array(args.iter().copied().map(Some), false)?;
            if let Some(site) = constructor_site {
                self.source_loc(site);
            }
            if direct {
                self.ir(IrOp::ApplyEval {
                    scope: self.f().scope,
                });
            } else {
                self.unsigned(Apply, u32::from(constructor || super_call));
            }
        } else {
            for &arg in args {
                if let Item::Value(expr) = arg {
                    self.expression(expr)?;
                }
            }
            let argc = u16::try_from(args.len()).map_err(|_| CompileError::StackOverflow)?;
            if let Some(site) = constructor_site {
                self.source_loc(site);
            }
            if direct {
                self.ir(IrOp::DirectEval {
                    arguments: argc,
                    scope: self.f().scope,
                });
            } else {
                self.op(
                    if constructor || super_call {
                        CallConstructor
                    } else if receiver {
                        CallMethod
                    } else {
                        Call
                    },
                    Operands::Call(argc),
                );
            }
        }
        if super_call {
            self.plain(Dup);
            let name = self.atom("<this>")?;
            self.ir(IrOp::ScopePutVarInit(self.scoped(name)));
            self.initialize_instance_fields()?;
        }
        Ok(())
    }

    /// The reference `name` makes. `reads_first` says its value is read
    /// before anything is stored to it.
    pub(crate) fn reference_name(
        &mut self,
        name: &Ident,
        init: bool,
        reads_first: bool,
    ) -> Result<Reference> {
        let atom = self.atom(&name.name)?;
        let binding = self.scoped(atom);
        if init || self.local_binding(binding) {
            return Ok(Reference::Binding(binding));
        }
        // Loading and storing a global by name gives what a reference gives,
        // unless strict code stores to a name it hasn't read and this script
        // doesn't declare, which must still exist after its value is made.
        if let Some(declared) = self.static_global(binding)
            && (declared || reads_first || !self.f().bytecode.strict)
        {
            return Ok(Reference::Binding(binding));
        }
        self.ir(IrOp::ScopeMakeRef {
            binding,
            store_label: 0,
        });
        Ok(Reference::Dynamic(binding, self.stash()?))
    }

    pub(crate) fn reference(&mut self, id: ExprId, reads_first: bool) -> Result<Reference> {
        use Opcode::*;
        let node = self.ast().expr(self.unparen(id));
        Ok(match &node.kind {
            Expr::Identifier(name) => return self.reference_name(name, false, reads_first),
            Expr::Member { object, key, .. } if self.is_super(*object) => {
                self.super_base(key)?;
                Reference::Super
            }
            Expr::Member { object, key, .. } => {
                self.expression(*object)?;
                match key {
                    Name::Private(i) => {
                        let name = self.private_atom(i)?;
                        self.read(name);
                        Reference::Private(name, self.private_member(name))
                    }
                    _ => {
                        if let Some(name) = self.member_key(key)? {
                            Reference::Field(name)
                        } else {
                            Reference::Element
                        }
                    }
                }
            }
            Expr::Call { .. } => {
                self.expression(id)?;
                self.plain(Drop);
                self.flag(ThrowError, BuiltinAtom::EmptyString.id(), 2);
                Reference::Invalid
            }
            _ => return Err(self.error(node.span, "Expected an assignment target")),
        })
    }

    fn load_reference(&mut self, r: Reference) {
        use Opcode::*;
        match r {
            Reference::Binding(name) => self.ir(IrOp::ScopeGetVar(name)),
            Reference::Dynamic(name, with) => {
                let load = self.label();
                let done = self.label();
                self.get(with);
                self.if_false(load);
                self.plain(Dup2);
                self.plain(Swap);
                self.plain(In);
                self.if_true(load);
                if self.f().bytecode.strict {
                    self.flag(ThrowError, name.name, 2);
                } else {
                    self.plain(Undefined);
                    self.jump(done);
                }
                self.mark(load);
                self.plain(GetRefValue);
                self.mark(done);
            }
            Reference::Field(atom) => self.named(GetField2, atom),
            Reference::Element => {
                self.plain(ToPropkey2);
                self.plain(Dup2);
                self.plain(GetArrayEl);
            }
            Reference::Super => {
                self.plain(ToPropkey);
                self.plain(Dup3);
                self.plain(GetSuperValue);
            }
            Reference::Private(_, member) => {
                self.plain(Dup2);
                self.get_private(member);
            }
            Reference::Invalid => (),
        }
    }

    fn get_private(&mut self, member: PrivateMember) {
        use Opcode::*;
        if matches!(member, PrivateMember::Accessor { .. }) {
            self.plain(Dup1);
            self.plain(GetPrivateField);
            self.op(CallMethod, Operands::Call(0));
        } else {
            self.plain(GetPrivateField);
        }
    }

    pub(crate) fn store_reference(&mut self, r: Reference, init: bool, keep: bool) {
        use Opcode::*;
        if keep {
            self.plain(match r.width() {
                0 => Dup,
                1 => Insert2,
                2 => Insert3,
                3 => Insert4,
                _ => unreachable!(),
            });
        }
        match r {
            Reference::Binding(name) => self.ir(if init {
                IrOp::ScopePutVarInit(name)
            } else {
                IrOp::ScopePutVar(name)
            }),
            Reference::Dynamic(_, with) => {
                if !self.f().bytecode.strict {
                    let store = self.label();
                    self.get(with);
                    self.if_false(store);
                    self.plain(Dup3);
                    self.plain(Drop);
                    self.plain(Swap);
                    self.plain(In);
                    self.plain(Drop);
                    self.mark(store);
                }
                self.plain(PutRefValue);
            }
            Reference::Field(name) => self.named(PutField, name),
            Reference::Element => self.plain(PutArrayEl),
            Reference::Super => self.plain(PutSuperValue),
            Reference::Private(_, PrivateMember::Field) => {
                self.plain(Swap);
                self.plain(PutPrivateField);
            }
            Reference::Private(name, PrivateMember::Method) => {
                self.plain(Drop);
                self.plain(GetPrivateField);
                self.plain(Drop);
                self.flag(ThrowError, name, 0);
            }
            Reference::Private(_, PrivateMember::Accessor { setter }) => {
                self.plain(Swap);
                self.plain(Drop);
                self.plain(Swap);
                self.plain(Dup);
                self.read(setter);
                self.plain(GetPrivateField);
                self.plain(Rot3l);
                self.op(CallMethod, Operands::Call(1));
                self.plain(Drop);
            }
            Reference::Invalid => self.plain(Drop),
        }
    }

    fn assignment(&mut self, op: AssignOp, target: PatternId, expr: ExprId) -> Result {
        use AssignOp::*;
        let pattern = &self.ast().pattern(target).kind;
        let (r, name) = match pattern {
            Pattern::Identifier(i) => (
                self.reference_name(i, false, !matches!(op, Assign))?,
                Some(self.atom(&i.name)?),
            ),
            Pattern::Reference(id) => (self.reference(*id, !matches!(op, Assign))?, None),
            _ => {
                self.expression(expr)?;
                self.plain(Opcode::Dup);
                self.bind_value(target, false)?;
                return Ok(());
            }
        };
        match op {
            Assign => {
                self.expression_named(expr, name)?;
                self.store_reference(r, false, true);
            }
            And | Or | Nullish => {
                self.load_reference(r);
                let short = self.label();
                let done = self.label();
                self.plain(Opcode::Dup);
                match op {
                    And => self.if_false(short),
                    Or => self.if_true(short),
                    _ => {
                        self.plain(Opcode::IsUndefinedOrNull);
                        self.if_false(short);
                    }
                }
                self.plain(Opcode::Drop);
                self.expression_named(expr, name)?;
                self.store_reference(r, false, true);
                self.jump(done);
                self.mark(short);
                for _ in 0..r.width() {
                    self.plain(Opcode::Nip);
                }
                self.mark(done);
            }
            _ => {
                self.load_reference(r);
                self.expression(expr)?;
                let op = match op {
                    Add => BinaryOp::Add,
                    Sub => BinaryOp::Sub,
                    Mul => BinaryOp::Mul,
                    Div => BinaryOp::Div,
                    Rem => BinaryOp::Rem,
                    Pow => BinaryOp::Pow,
                    Shl => BinaryOp::Shl,
                    Shr => BinaryOp::Shr,
                    UShr => BinaryOp::UShr,
                    BitAnd => BinaryOp::BitAnd,
                    BitXor => BinaryOp::BitXor,
                    BitOr => BinaryOp::BitOr,
                    _ => unreachable!(),
                };
                self.plain(binary(op));
                self.store_reference(r, false, true);
            }
        }
        Ok(())
    }

    fn update(&mut self, op: UpdateOp, id: ExprId, prefix: bool) -> Result {
        use Opcode::*;
        let reference = self.reference(id, true)?;
        self.load_reference(reference);
        self.plain(match (prefix, op) {
            (true, UpdateOp::Inc) => Inc,
            (true, UpdateOp::Dec) => Dec,
            (false, UpdateOp::Inc) => PostInc,
            (false, UpdateOp::Dec) => PostDec,
        });
        if prefix {
            self.store_reference(reference, false, true);
        } else {
            self.plain(Swap);
            let old = self.stash()?;
            self.store_reference(reference, false, false);
            self.get(old);
        }
        Ok(())
    }

    fn yield_expression(&mut self, argument: Option<ExprId>, delegate: bool) -> Result {
        use Opcode::*;
        if let Some(argument) = argument {
            self.expression(argument)?;
        } else {
            self.plain(Undefined);
        }
        let asynchronous = self.kind() == FunctionKind::AsyncGenerator;
        if !delegate {
            if asynchronous {
                self.await_value();
            }
            self.plain(Yield);
            let done = self.label();
            self.if_false(done);
            if asynchronous {
                self.await_value();
            }
            self.return_value()?;
            self.mark(done);
            return Ok(());
        }
        let advance = self.label();
        let yielded = self.label();
        let done = self.label();
        let throwing = self.label();
        let no_return = self.label();
        let no_throw = self.label();
        let missing = self.label();
        self.plain(if asynchronous {
            ForAwaitOfStart
        } else {
            ForOfStart
        });
        self.plain(Drop);
        self.plain(Undefined);
        self.plain(Undefined);
        self.mark(advance);
        self.plain(IteratorNext);
        if asynchronous {
            self.await_value();
        }
        self.plain(CheckObject);
        self.named(GetField2, BuiltinAtom::Done.id());
        self.if_true(done);
        self.mark(yielded);
        if asynchronous {
            self.named(GetField, BuiltinAtom::Value.id());
            self.plain(AsyncYieldStar);
        } else {
            self.plain(YieldStar);
        }
        self.plain(Dup);
        let abrupt = self.label();
        self.if_true(abrupt);
        self.plain(Drop);
        self.jump(advance);
        self.mark(abrupt);
        self.int(2);
        self.plain(StrictEq);
        self.if_true(throwing);
        if asynchronous {
            self.await_value();
        }
        self.unsigned(IteratorCall, 0);
        self.if_true(no_return);
        if asynchronous {
            self.await_value();
        }
        self.plain(CheckObject);
        self.named(GetField2, BuiltinAtom::Done.id());
        self.if_false(yielded);
        self.named(GetField, BuiltinAtom::Value.id());
        self.mark(no_return);
        for _ in 0..3 {
            self.plain(Nip);
        }
        if asynchronous {
            self.await_value();
        }
        self.return_value()?;
        self.mark(throwing);
        self.unsigned(IteratorCall, 1);
        self.if_true(no_throw);
        if asynchronous {
            self.await_value();
        }
        self.plain(CheckObject);
        self.named(GetField2, BuiltinAtom::Done.id());
        self.if_false(yielded);
        self.jump(done);
        self.mark(no_throw);
        self.unsigned(IteratorCall, 2);
        self.if_true(missing);
        if asynchronous {
            self.await_value();
        }
        self.plain(CheckObject);
        self.mark(missing);
        self.flag(ThrowError, BuiltinAtom::EmptyString.id(), 4);
        self.mark(done);
        self.named(GetField, BuiltinAtom::Value.id());
        for _ in 0..3 {
            self.plain(Nip);
        }
        Ok(())
    }
}

pub(crate) fn method_flags(kind: MethodKind) -> u8 {
    match kind {
        MethodKind::Getter => 1,
        MethodKind::Setter => 2,
        _ => 0,
    }
}

fn decimal_bigint(digits: &str, radix: u8) -> String {
    let bytes = digits.bytes().filter(|&b| b != b'_');
    if radix == 10 {
        let value: String = bytes.skip_while(|&b| b == b'0').map(char::from).collect();
        return if value.is_empty() { "0".into() } else { value };
    }
    let mut limbs = vec![0u64];
    for digit in bytes {
        let mut carry = u64::from((digit as char).to_digit(u32::from(radix)).unwrap_or(0));
        for limb in &mut limbs {
            let n = *limb * u64::from(radix) + carry;
            *limb = n % 1_000_000_000;
            carry = n / 1_000_000_000;
        }
        if carry != 0 {
            limbs.push(carry);
        }
    }
    let mut result = limbs.pop().unwrap_or_default().to_string();
    for limb in limbs.into_iter().rev() {
        use std::fmt::Write;
        let _ = write!(result, "{limb:09}");
    }
    result
}
