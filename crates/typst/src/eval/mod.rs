//! Evaluation of markup into modules.

#[macro_use]
mod library;
#[macro_use]
mod cast;
#[macro_use]
mod array;
#[macro_use]
mod dict;
#[macro_use]
mod str;
#[macro_use]
mod value;
mod args;
mod auto;
mod datetime;
mod fields;
mod func;
mod int;
mod maybe_mut;
mod methods;
mod module;
mod none;
pub mod ops;
mod scope;
mod symbol;

#[doc(hidden)]
pub use {
    self::library::LANG_ITEMS,
    ecow::{eco_format, eco_vec},
    indexmap::IndexMap,
    once_cell::sync::Lazy,
};

#[doc(inline)]
pub use typst_macros::{func, symbols};

pub use self::args::{Arg, Args};
pub use self::array::{array, Array};
pub use self::auto::AutoValue;
pub use self::cast::{
    cast, Cast, CastInfo, FromValue, IntoResult, IntoValue, Never, Reflect, Variadics,
};
pub use self::datetime::Datetime;
pub use self::dict::{dict, Dict};
pub use self::fields::fields_on;
pub use self::func::{Func, FuncInfo, NativeFunc, Param, ParamInfo};
pub use self::library::{set_lang_items, LangItems, Library};
pub use self::maybe_mut::{Immutability, MaybeMut};
pub use self::methods::methods_on;
pub use self::module::Module;
pub use self::none::NoneValue;
pub use self::scope::{Scope, Scopes};
pub use self::str::{format_str, Regex, Str};
pub use self::symbol::Symbol;
pub use self::value::{Dynamic, Type, Value};

use std::collections::HashSet;
use std::mem;
use std::path::Path;

use comemo::{Track, Tracked, TrackedMut, Validate};
use ecow::{EcoString, EcoVec};
use unicode_segmentation::UnicodeSegmentation;

use self::func::{CapturesVisitor, Closure};
use crate::diag::{
    bail, error, At, SourceError, SourceResult, StrResult, Trace, Tracepoint,
};
use crate::file::{FileId, PackageManifest, PackageSpec};
use crate::model::{
    Content, DelayedErrors, Introspector, Label, Locator, Recipe, ShowableSelector,
    Styles, Transform, Unlabellable, Vt,
};
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{parse_code, Source, Span, Spanned, SyntaxKind, SyntaxNode};
use crate::World;

const MAX_ITERATIONS: usize = 10_000;
const MAX_CALL_DEPTH: usize = 64;

/// Evaluate a source file and return the resulting module.
#[comemo::memoize]
#[tracing::instrument(skip(world, route, tracer, source))]
pub fn eval(
    world: Tracked<dyn World + '_>,
    route: Tracked<Route>,
    tracer: TrackedMut<Tracer>,
    source: &Source,
) -> SourceResult<Module> {
    // Prevent cyclic evaluation.
    let id = source.id();
    if route.contains(id) {
        panic!("Tried to cyclicly evaluate {}", id.path().display());
    }

    // Hook up the lang items.
    let library = world.library();
    set_lang_items(library.items.clone());

    // Prepare VT.
    let mut locator = Locator::default();
    let introspector = Introspector::default();
    let mut delayed = DelayedErrors::default();
    let vt = Vt {
        world,
        introspector: introspector.track(),
        locator: &mut locator,
        delayed: delayed.track_mut(),
        tracer,
    };

    // Prepare VM.
    let route = Route::insert(route, id);
    let mut scopes = Scopes::new(Some(library));
    let mut vm = Vm::new(vt, route.track(), id);
    let root = match source.root().cast::<ast::Markup>() {
        Some(markup) if vm.traced.is_some() => markup,
        _ => source.ast()?,
    };

    // Evaluate the module.
    let result = root.eval(&mut vm, &mut scopes);

    // Handle control flow.
    if let Some(flow) = vm.flow {
        bail!(flow.forbidden());
    }

    // Assemble the module.
    let name = id.path().file_stem().unwrap_or_default().to_string_lossy();
    Ok(Module::new(name).with_scope(scopes.top).with_content(result?))
}

/// Evaluate a string as code and return the resulting value.
///
/// Everything in the output is associated with the given `span`.
#[comemo::memoize]
pub fn eval_string(
    world: Tracked<dyn World + '_>,
    code: &str,
    span: Span,
) -> SourceResult<Value> {
    let mut root = parse_code(code);
    root.synthesize(span);

    let errors = root.errors();
    if !errors.is_empty() {
        return Err(Box::new(errors));
    }

    // Prepare VT.
    let mut tracer = Tracer::default();
    let mut locator = Locator::default();
    let mut delayed = DelayedErrors::default();
    let introspector = Introspector::default();
    let vt = Vt {
        world,
        introspector: introspector.track(),
        locator: &mut locator,
        delayed: delayed.track_mut(),
        tracer: tracer.track_mut(),
    };

    // Prepare VM.
    let route = Route::default();
    let id = FileId::detached();
    let mut scopes = Scopes::new(Some(world.library()));
    let mut vm = Vm::new(vt, route.track(), id);

    // Evaluate the code.
    let code = root.cast::<ast::Code>().unwrap();
    let result = code.eval(&mut vm, &mut scopes);

    // Handle control flow.
    if let Some(flow) = vm.flow {
        bail!(flow.forbidden());
    }

    result
}

/// A virtual machine.
///
/// Holds the state needed to [evaluate](eval) Typst sources. A new
/// virtual machine is created for each module evaluation and function call.
pub struct Vm<'a> {
    /// The underlying virtual typesetter.
    pub vt: Vt<'a>,
    /// The language items.
    items: LangItems,
    /// The route of source ids the VM took to reach its current location.
    route: Tracked<'a, Route<'a>>,
    /// The current location.
    location: FileId,
    /// A control flow event that is currently happening.
    flow: Option<FlowEvent>,
    /// The current call depth.
    depth: usize,
    /// A span that is currently traced.
    traced: Option<Span>,
}

impl<'a> Vm<'a> {
    /// Create a new virtual machine.
    fn new(vt: Vt<'a>, route: Tracked<'a, Route>, location: FileId) -> Self {
        let traced = vt.tracer.span(location);
        let items = vt.world.library().items.clone();
        Self {
            vt,
            items,
            route,
            location,
            flow: None,
            depth: 0,
            traced,
        }
    }

    /// Access the underlying world.
    pub fn world(&self) -> Tracked<'a, dyn World + 'a> {
        self.vt.world
    }

    /// The location to which paths are relative currently.
    pub fn location(&self) -> FileId {
        self.location
    }

    /// Define a variable in the current scope.
    #[tracing::instrument(skip_all)]
    pub fn define(
        &mut self,
        scopes: &mut Scopes,
        var: ast::Ident,
        value: impl IntoValue,
    ) {
        let value = value.into_value();
        if self.traced == Some(var.span()) {
            self.vt.tracer.trace(value.clone());
        }
        scopes.top.define(var.take(), value);
    }
}

/// A control flow event that occurred during evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowEvent {
    /// Stop iteration in a loop.
    Break(Span),
    /// Skip the remainder of the current iteration in a loop.
    Continue(Span),
    /// Stop execution of a function early, optionally returning an explicit
    /// value.
    Return(Span, Option<Value>),
}

impl FlowEvent {
    /// Return an error stating that this control flow is forbidden.
    pub fn forbidden(&self) -> SourceError {
        match *self {
            Self::Break(span) => {
                error!(span, "cannot break outside of loop")
            }
            Self::Continue(span) => {
                error!(span, "cannot continue outside of loop")
            }
            Self::Return(span, _) => {
                error!(span, "cannot return outside of function")
            }
        }
    }
}

/// A route of source ids.
#[derive(Default)]
pub struct Route<'a> {
    // We need to override the constraint's lifetime here so that `Tracked` is
    // covariant over the constraint. If it becomes invariant, we're in for a
    // world of lifetime pain.
    outer: Option<Tracked<'a, Self, <Route<'static> as Validate>::Constraint>>,
    id: Option<FileId>,
}

impl<'a> Route<'a> {
    /// Create a new route with just one entry.
    pub fn new(id: FileId) -> Self {
        Self { id: Some(id), outer: None }
    }

    /// Insert a new id into the route.
    ///
    /// You must guarantee that `outer` lives longer than the resulting
    /// route is ever used.
    pub fn insert(outer: Tracked<'a, Self>, id: FileId) -> Self {
        Route { outer: Some(outer), id: Some(id) }
    }

    /// Start tracking this locator.
    ///
    /// In comparison to [`Track::track`], this method skips this chain link
    /// if it does not contribute anything.
    pub fn track(&self) -> Tracked<'_, Self> {
        match self.outer {
            Some(outer) if self.id.is_none() => outer,
            _ => Track::track(self),
        }
    }
}

#[comemo::track]
impl<'a> Route<'a> {
    /// Whether the given id is part of the route.
    fn contains(&self, id: FileId) -> bool {
        self.id == Some(id) || self.outer.map_or(false, |outer| outer.contains(id))
    }
}

/// Traces which values existed for an expression at a span.
#[derive(Default, Clone)]
pub struct Tracer {
    span: Option<Span>,
    values: Vec<Value>,
}

impl Tracer {
    /// The maximum number of traced items.
    pub const MAX: usize = 10;

    /// Create a new tracer, possibly with a span under inspection.
    pub fn new(span: Option<Span>) -> Self {
        Self { span, values: vec![] }
    }

    /// Get the traced values.
    pub fn finish(self) -> Vec<Value> {
        self.values
    }
}

#[comemo::track]
impl Tracer {
    /// The traced span if it is part of the given source file.
    fn span(&self, id: FileId) -> Option<Span> {
        if self.span.map(Span::id) == Some(id) {
            self.span
        } else {
            None
        }
    }

    /// Trace a value for the span.
    fn trace(&mut self, v: Value) {
        if self.values.len() < Self::MAX {
            self.values.push(v);
        }
    }
}

/// Evaluate an expression.
pub(super) trait Eval {
    /// The output of evaluating the expression.
    type Output;

    /// Evaluate the expression to the output value.
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output>;
}

/// Evaluate an expression, to a mutable location if possible.
pub(super) trait EvalMaybeMut {
    /// Evaluate the expression to the mutable location or output value.
    fn eval_maybe_mut<'a>(
        &self,
        vm: &mut Vm,
        scopes: &'a mut Scopes,
    ) -> SourceResult<MaybeMut<'a>>;
}

impl Eval for ast::Markup {
    type Output = Content;

    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        eval_markup(vm, scopes, &mut self.exprs())
    }
}

/// Evaluate a stream of markup.
fn eval_markup(
    vm: &mut Vm,
    scopes: &mut Scopes,
    exprs: &mut impl Iterator<Item = ast::Expr>,
) -> SourceResult<Content> {
    let flow = vm.flow.take();
    let mut seq = Vec::with_capacity(exprs.size_hint().1.unwrap_or_default());

    while let Some(expr) = exprs.next() {
        match expr {
            ast::Expr::Set(set) => {
                let styles = set.eval(vm, scopes)?;
                if vm.flow.is_some() {
                    break;
                }

                seq.push(eval_markup(vm, scopes, exprs)?.styled_with_map(styles))
            }
            ast::Expr::Show(show) => {
                let recipe = show.eval(vm, scopes)?;
                if vm.flow.is_some() {
                    break;
                }

                let tail = eval_markup(vm, scopes, exprs)?;
                seq.push(tail.styled_with_recipe(vm, recipe)?)
            }
            expr => match expr.eval(vm, scopes)? {
                Value::Label(label) => {
                    if let Some(elem) =
                        seq.iter_mut().rev().find(|node| !node.can::<dyn Unlabellable>())
                    {
                        *elem = mem::take(elem).labelled(label);
                    }
                }
                value => seq.push(value.display().spanned(expr.span())),
            },
        }

        if vm.flow.is_some() {
            break;
        }
    }

    if flow.is_some() {
        vm.flow = flow;
    }

    Ok(Content::sequence(seq))
}

impl Eval for ast::Expr {
    type Output = Value;

    #[tracing::instrument(name = "Expr::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let span = self.span();
        let forbidden = |name| {
            error!(span, "{} is only allowed directly in code and content blocks", name)
        };

        let v = match self {
            Self::Text(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Space(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Linebreak(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Parbreak(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Escape(v) => v.eval(vm, scopes),
            Self::Shorthand(v) => v.eval(vm, scopes),
            Self::SmartQuote(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Strong(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Emph(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Raw(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Link(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Label(v) => v.eval(vm, scopes),
            Self::Ref(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Heading(v) => v.eval(vm, scopes).map(Value::Content),
            Self::List(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Enum(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Term(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Equation(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Math(v) => v.eval(vm, scopes).map(Value::Content),
            Self::MathIdent(v) => v.eval(vm, scopes),
            Self::MathAlignPoint(v) => v.eval(vm, scopes).map(Value::Content),
            Self::MathDelimited(v) => v.eval(vm, scopes).map(Value::Content),
            Self::MathAttach(v) => v.eval(vm, scopes).map(Value::Content),
            Self::MathPrimes(v) => v.eval(vm, scopes).map(Value::Content),
            Self::MathFrac(v) => v.eval(vm, scopes).map(Value::Content),
            Self::MathRoot(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Ident(v) => v.eval(vm, scopes),
            Self::None(v) => v.eval(vm, scopes),
            Self::Auto(v) => v.eval(vm, scopes),
            Self::Bool(v) => v.eval(vm, scopes),
            Self::Int(v) => v.eval(vm, scopes),
            Self::Float(v) => v.eval(vm, scopes),
            Self::Numeric(v) => v.eval(vm, scopes),
            Self::Str(v) => v.eval(vm, scopes),
            Self::Code(v) => v.eval(vm, scopes),
            Self::Content(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Array(v) => v.eval(vm, scopes).map(Value::Array),
            Self::Dict(v) => v.eval(vm, scopes).map(Value::Dict),
            Self::Parenthesized(v) => v.eval(vm, scopes),
            Self::FieldAccess(v) => v.eval(vm, scopes),
            Self::FuncCall(v) => v.eval(vm, scopes),
            Self::Closure(v) => v.eval(vm, scopes),
            Self::Unary(v) => v.eval(vm, scopes),
            Self::Binary(v) => v.eval(vm, scopes),
            Self::Let(v) => v.eval(vm, scopes),
            Self::DestructAssign(v) => v.eval(vm, scopes),
            Self::Set(_) => bail!(forbidden("set")),
            Self::Show(_) => bail!(forbidden("show")),
            Self::Conditional(v) => v.eval(vm, scopes),
            Self::While(v) => v.eval(vm, scopes),
            Self::For(v) => v.eval(vm, scopes),
            Self::Import(v) => v.eval(vm, scopes),
            Self::Include(v) => v.eval(vm, scopes).map(Value::Content),
            Self::Break(v) => v.eval(vm, scopes),
            Self::Continue(v) => v.eval(vm, scopes),
            Self::Return(v) => v.eval(vm, scopes),
        }?
        .spanned(span);

        if vm.traced == Some(span) {
            vm.vt.tracer.trace(v.clone());
        }

        Ok(v)
    }
}

impl EvalMaybeMut for ast::Expr {
    fn eval_maybe_mut<'a>(
        &self,
        vm: &mut Vm,
        scopes: &'a mut Scopes,
    ) -> SourceResult<MaybeMut<'a>> {
        let span = self.span();
        let v = match self {
            // These four expressions can possibly be mutated.
            Self::Ident(v) => v.eval_maybe_mut(vm, scopes),
            Self::Parenthesized(v) => v.eval_maybe_mut(vm, scopes),
            Self::FieldAccess(v) => v.eval_maybe_mut(vm, scopes),
            Self::FuncCall(v) => v.eval_maybe_mut(vm, scopes),

            // All other expressions cannot be mutated.
            expr => expr.eval(vm, scopes).map(|v| MaybeMut::temp(v, span)),
        }?
        .spanned(span);

        if vm.traced == Some(span) {
            vm.vt.tracer.trace(v.clone());
        }

        Ok(v)
    }
}

impl ast::Expr {
    fn eval_display(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Content> {
        Ok(self.eval(vm, scopes)?.display().spanned(self.span()))
    }
}

impl Eval for ast::Text {
    type Output = Content;

    #[tracing::instrument(name = "Text::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.text)(self.get().clone()))
    }
}

impl Eval for ast::Space {
    type Output = Content;

    #[tracing::instrument(name = "Space::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.space)())
    }
}

impl Eval for ast::Linebreak {
    type Output = Content;

    #[tracing::instrument(name = "Linebreak::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.linebreak)())
    }
}

impl Eval for ast::Parbreak {
    type Output = Content;

    #[tracing::instrument(name = "Parbreak::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.parbreak)())
    }
}

impl Eval for ast::Escape {
    type Output = Value;

    #[tracing::instrument(name = "Escape::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Symbol(Symbol::new(self.get())))
    }
}

impl Eval for ast::Shorthand {
    type Output = Value;

    #[tracing::instrument(name = "Shorthand::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Symbol(Symbol::new(self.get())))
    }
}

impl Eval for ast::SmartQuote {
    type Output = Content;

    #[tracing::instrument(name = "SmartQuote::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.smart_quote)(self.double()))
    }
}

impl Eval for ast::Strong {
    type Output = Content;

    #[tracing::instrument(name = "Strong::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.strong)(self.body().eval(vm, scopes)?))
    }
}

impl Eval for ast::Emph {
    type Output = Content;

    #[tracing::instrument(name = "Emph::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.emph)(self.body().eval(vm, scopes)?))
    }
}

impl Eval for ast::Raw {
    type Output = Content;

    #[tracing::instrument(name = "Raw::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        let text = self.text();
        let lang = self.lang().map(Into::into);
        let block = self.block();
        Ok((vm.items.raw)(text, lang, block))
    }
}

impl Eval for ast::Link {
    type Output = Content;

    #[tracing::instrument(name = "Link::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.link)(self.get().clone()))
    }
}

impl Eval for ast::Label {
    type Output = Value;

    #[tracing::instrument(name = "Label::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Label(Label(self.get().into())))
    }
}

impl Eval for ast::Ref {
    type Output = Content;

    #[tracing::instrument(name = "Ref::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let label = Label(self.target().into());
        let supplement =
            self.supplement().map(|block| block.eval(vm, scopes)).transpose()?;
        Ok((vm.items.reference)(label, supplement))
    }
}

impl Eval for ast::Heading {
    type Output = Content;

    #[tracing::instrument(name = "Heading::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let level = self.level();
        let body = self.body().eval(vm, scopes)?;
        Ok((vm.items.heading)(level, body))
    }
}

impl Eval for ast::ListItem {
    type Output = Content;

    #[tracing::instrument(name = "ListItem::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.list_item)(self.body().eval(vm, scopes)?))
    }
}

impl Eval for ast::EnumItem {
    type Output = Content;

    #[tracing::instrument(name = "EnumItem::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let number = self.number();
        let body = self.body().eval(vm, scopes)?;
        Ok((vm.items.enum_item)(number, body))
    }
}

impl Eval for ast::TermItem {
    type Output = Content;

    #[tracing::instrument(name = "TermItem::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let term = self.term().eval(vm, scopes)?;
        let description = self.description().eval(vm, scopes)?;
        Ok((vm.items.term_item)(term, description))
    }
}

impl Eval for ast::Equation {
    type Output = Content;

    #[tracing::instrument(name = "Equation::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let body = self.body().eval(vm, scopes)?;
        let block = self.block();
        Ok((vm.items.equation)(body, block))
    }
}

impl Eval for ast::Math {
    type Output = Content;

    #[tracing::instrument(name = "Math::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Content::sequence(
            self.exprs()
                .map(|expr| expr.eval_display(vm, scopes))
                .collect::<SourceResult<Vec<_>>>()?,
        ))
    }
}

impl Eval for ast::MathIdent {
    type Output = Value;

    #[tracing::instrument(name = "MathIdent::eval", skip_all)]
    fn eval(&self, _: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        scopes.get_in_math(self).cloned().at(self.span())
    }
}

impl Eval for ast::MathAlignPoint {
    type Output = Content;

    #[tracing::instrument(name = "MathAlignPoint::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.math_align_point)())
    }
}

impl Eval for ast::MathDelimited {
    type Output = Content;

    #[tracing::instrument(name = "MathDelimited::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let open = self.open().eval_display(vm, scopes)?;
        let body = self.body().eval(vm, scopes)?;
        let close = self.close().eval_display(vm, scopes)?;
        Ok((vm.items.math_delimited)(open, body, close))
    }
}

impl Eval for ast::MathAttach {
    type Output = Content;

    #[tracing::instrument(name = "MathAttach::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let base = self.base().eval_display(vm, scopes)?;

        let mut top = self.top().map(|expr| expr.eval_display(vm, scopes)).transpose()?;
        if top.is_none() {
            if let Some(primes) = self.primes() {
                top = Some(primes.eval(vm, scopes)?);
            }
        }

        let bottom =
            self.bottom().map(|expr| expr.eval_display(vm, scopes)).transpose()?;
        Ok((vm.items.math_attach)(base, top, bottom, None, None, None, None))
    }
}

impl Eval for ast::MathPrimes {
    type Output = Content;

    #[tracing::instrument(name = "MathPrimes::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok((vm.items.math_primes)(self.count()))
    }
}

impl Eval for ast::MathFrac {
    type Output = Content;

    #[tracing::instrument(name = "MathFrac::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let num = self.num().eval_display(vm, scopes)?;
        let denom = self.denom().eval_display(vm, scopes)?;
        Ok((vm.items.math_frac)(num, denom))
    }
}

impl Eval for ast::MathRoot {
    type Output = Content;

    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let index = self.index().map(|i| (vm.items.text)(eco_format!("{i}")));
        let radicand = self.radicand().eval_display(vm, scopes)?;
        Ok((vm.items.math_root)(index, radicand))
    }
}

impl Eval for ast::Ident {
    type Output = Value;

    #[tracing::instrument(name = "Ident::eval", skip_all)]
    fn eval(&self, _: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        scopes.get(self).cloned().at(self.span())
    }
}

impl EvalMaybeMut for ast::Ident {
    #[tracing::instrument(name = "Ident::eval_maybe_mut", skip_all)]
    fn eval_maybe_mut<'a>(
        &self,
        _: &mut Vm,
        scopes: &'a mut Scopes,
    ) -> SourceResult<MaybeMut<'a>> {
        scopes.get_maybe_mut(self, self.span())
    }
}

impl Eval for ast::None {
    type Output = Value;

    #[tracing::instrument(name = "None::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::None)
    }
}

impl Eval for ast::Auto {
    type Output = Value;

    #[tracing::instrument(name = "Auto::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Auto)
    }
}

impl Eval for ast::Bool {
    type Output = Value;

    #[tracing::instrument(name = "Bool::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Bool(self.get()))
    }
}

impl Eval for ast::Int {
    type Output = Value;

    #[tracing::instrument(name = "Int::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Int(self.get()))
    }
}

impl Eval for ast::Float {
    type Output = Value;

    #[tracing::instrument(name = "Float::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Float(self.get()))
    }
}

impl Eval for ast::Numeric {
    type Output = Value;

    #[tracing::instrument(name = "Numeric::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::numeric(self.get()))
    }
}

impl Eval for ast::Str {
    type Output = Value;

    #[tracing::instrument(name = "Str::eval", skip_all)]
    fn eval(&self, _: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        Ok(Value::Str(self.get().into()))
    }
}

impl Eval for ast::CodeBlock {
    type Output = Value;

    #[tracing::instrument(name = "CodeBlock::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        scopes.enter();
        let output = self.body().eval(vm, scopes)?;
        scopes.exit();
        Ok(output)
    }
}

impl Eval for ast::Code {
    type Output = Value;

    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        eval_code(vm, scopes, &mut self.exprs())
    }
}

/// Evaluate a stream of expressions.
fn eval_code(
    vm: &mut Vm,
    scopes: &mut Scopes,
    exprs: &mut impl Iterator<Item = ast::Expr>,
) -> SourceResult<Value> {
    let flow = vm.flow.take();
    let mut output = Value::None;

    while let Some(expr) = exprs.next() {
        let span = expr.span();
        let value = match expr {
            ast::Expr::Set(set) => {
                let styles = set.eval(vm, scopes)?;
                if vm.flow.is_some() {
                    break;
                }

                let tail = eval_code(vm, scopes, exprs)?.display();
                Value::Content(tail.styled_with_map(styles))
            }
            ast::Expr::Show(show) => {
                let recipe = show.eval(vm, scopes)?;
                if vm.flow.is_some() {
                    break;
                }

                let tail = eval_code(vm, scopes, exprs)?.display();
                Value::Content(tail.styled_with_recipe(vm, recipe)?)
            }
            _ => expr.eval(vm, scopes)?,
        };

        output = ops::join(output, value).at(span)?;

        if vm.flow.is_some() {
            break;
        }
    }

    if flow.is_some() {
        vm.flow = flow;
    }

    Ok(output)
}

impl Eval for ast::ContentBlock {
    type Output = Content;

    #[tracing::instrument(name = "ContentBlock::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        scopes.enter();
        let content = self.body().eval(vm, scopes)?;
        scopes.exit();
        Ok(content)
    }
}

impl Eval for ast::Parenthesized {
    type Output = Value;

    #[tracing::instrument(name = "Parenthesized::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        self.expr().eval(vm, scopes)
    }
}

impl EvalMaybeMut for ast::Parenthesized {
    #[tracing::instrument(name = "Parenthesized::eval_maybe_mut", skip_all)]
    fn eval_maybe_mut<'a>(
        &self,
        vm: &mut Vm,
        scopes: &'a mut Scopes,
    ) -> SourceResult<MaybeMut<'a>> {
        self.expr().eval_maybe_mut(vm, scopes)
    }
}

impl Eval for ast::Array {
    type Output = Array;

    #[tracing::instrument(name = "Array::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let items = self.items();

        let mut vec = EcoVec::with_capacity(items.size_hint().0);
        for item in items {
            match item {
                ast::ArrayItem::Pos(expr) => vec.push(expr.eval(vm, scopes)?),
                ast::ArrayItem::Spread(expr) => match expr.eval(vm, scopes)? {
                    Value::None => {}
                    Value::Array(array) => vec.extend(array.into_iter()),
                    v => bail!(expr.span(), "cannot spread {} into array", v.type_name()),
                },
            }
        }

        Ok(vec.into())
    }
}

impl Eval for ast::Dict {
    type Output = Dict;

    #[tracing::instrument(name = "Dict::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let mut map = indexmap::IndexMap::new();

        for item in self.items() {
            match item {
                ast::DictItem::Named(named) => {
                    map.insert(
                        named.name().take().into(),
                        named.expr().eval(vm, scopes)?,
                    );
                }
                ast::DictItem::Keyed(keyed) => {
                    map.insert(keyed.key().get().into(), keyed.expr().eval(vm, scopes)?);
                }
                ast::DictItem::Spread(expr) => match expr.eval(vm, scopes)? {
                    Value::None => {}
                    Value::Dict(dict) => map.extend(dict.into_iter()),
                    v => bail!(
                        expr.span(),
                        "cannot spread {} into dictionary",
                        v.type_name()
                    ),
                },
            }
        }

        Ok(map.into())
    }
}

impl Eval for ast::Unary {
    type Output = Value;

    #[tracing::instrument(name = "Unary::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let value = self.expr().eval(vm, scopes)?;
        let result = match self.op() {
            ast::UnOp::Pos => ops::pos(value),
            ast::UnOp::Neg => ops::neg(value),
            ast::UnOp::Not => ops::not(value),
        };
        result.at(self.span())
    }
}

impl Eval for ast::Binary {
    type Output = Value;

    #[tracing::instrument(name = "Binary::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        match self.op() {
            ast::BinOp::Add => self.apply(vm, scopes, ops::add),
            ast::BinOp::Sub => self.apply(vm, scopes, ops::sub),
            ast::BinOp::Mul => self.apply(vm, scopes, ops::mul),
            ast::BinOp::Div => self.apply(vm, scopes, ops::div),
            ast::BinOp::And => self.apply(vm, scopes, ops::and),
            ast::BinOp::Or => self.apply(vm, scopes, ops::or),
            ast::BinOp::Eq => self.apply(vm, scopes, ops::eq),
            ast::BinOp::Neq => self.apply(vm, scopes, ops::neq),
            ast::BinOp::Lt => self.apply(vm, scopes, ops::lt),
            ast::BinOp::Leq => self.apply(vm, scopes, ops::leq),
            ast::BinOp::Gt => self.apply(vm, scopes, ops::gt),
            ast::BinOp::Geq => self.apply(vm, scopes, ops::geq),
            ast::BinOp::In => self.apply(vm, scopes, ops::in_),
            ast::BinOp::NotIn => self.apply(vm, scopes, ops::not_in),
            ast::BinOp::Assign => self.assign(vm, scopes, |_, b| Ok(b)),
            ast::BinOp::AddAssign => self.assign(vm, scopes, ops::add),
            ast::BinOp::SubAssign => self.assign(vm, scopes, ops::sub),
            ast::BinOp::MulAssign => self.assign(vm, scopes, ops::mul),
            ast::BinOp::DivAssign => self.assign(vm, scopes, ops::div),
        }
    }
}

impl ast::Binary {
    /// Apply a basic binary operation.
    fn apply(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        op: fn(Value, Value) -> StrResult<Value>,
    ) -> SourceResult<Value> {
        let lhs = self.lhs().eval(vm, scopes)?;

        // Short-circuit boolean operations.
        if (self.op() == ast::BinOp::And && lhs == Value::Bool(false))
            || (self.op() == ast::BinOp::Or && lhs == Value::Bool(true))
        {
            return Ok(lhs);
        }

        let rhs = self.rhs().eval(vm, scopes)?;
        op(lhs, rhs).at(self.span())
    }

    /// Apply an assignment operation.
    fn assign(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        op: fn(Value, Value) -> StrResult<Value>,
    ) -> SourceResult<Value> {
        let rhs = self.rhs().eval(vm, scopes)?;
        let lhs = self.lhs();

        // An assignment to a dictionary field is different from a normal access
        // since it can create the field instead of just modifying it.
        if self.op() == ast::BinOp::Assign {
            if let ast::Expr::FieldAccess(access) = &lhs {
                let target = access.target();
                let dict = match target.eval_maybe_mut(vm, scopes)?.mutate()? {
                    Value::Dict(dict) => dict,
                    value => bail!(
                        target.span(),
                        "expected dictionary, found {}",
                        value.type_name(),
                    ),
                };
                dict.insert(access.field().take().into(), rhs);
                return Ok(Value::None);
            }
        }

        let location = self.lhs().eval_maybe_mut(vm, scopes)?.mutate()?;
        let lhs = std::mem::take(&mut *location);
        *location = op(lhs, rhs).at(self.span())?;
        Ok(Value::None)
    }
}

impl Eval for ast::FieldAccess {
    type Output = Value;

    #[tracing::instrument(name = "FieldAccess::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let value = self.target().eval(vm, scopes)?;
        let field = self.field();
        value.field(&field).at(field.span())
    }
}

impl EvalMaybeMut for ast::FieldAccess {
    fn eval_maybe_mut<'a>(
        &self,
        vm: &mut Vm,
        scopes: &'a mut Scopes,
    ) -> SourceResult<MaybeMut<'a>> {
        let span = self.span();
        let value = self.target().eval_maybe_mut(vm, scopes)?;
        let field = self.field();

        // Dictionaries are the only values with mutable fields.
        if let MaybeMut::Mut(Value::Dict(dict)) = value {
            return dict.at_mut(&field, None).at(span);
        }

        value.field(&field).at(span).map(|v| MaybeMut::temp(v, span))
    }
}

impl Eval for ast::FuncCall {
    type Output = Value;

    #[tracing::instrument(name = "FuncCall::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        self.eval_maybe_mut(vm, scopes).map(MaybeMut::take)
    }
}

impl EvalMaybeMut for ast::FuncCall {
    fn eval_maybe_mut<'a>(
        &self,
        vm: &mut Vm,
        scopes: &'a mut Scopes,
    ) -> SourceResult<MaybeMut<'a>> {
        let span = self.span();
        if vm.depth >= MAX_CALL_DEPTH {
            bail!(span, "maximum function call depth exceeded");
        }

        let callee = self.callee();
        let callee_span = callee.span();
        let in_math = in_math(&callee);
        let args = self.args();

        // Try to evaluate as a method call. This is possible if the callee is a
        // field access and does not evaluate to a module.
        let (callee, args) = if let ast::Expr::FieldAccess(access) = callee {
            let target = access.target();
            let field = access.field();
            let field_span = field.span();
            let field = field.take();

            let mut args = args.eval(vm, scopes)?;
            let target = target.eval_maybe_mut(vm, scopes)?;

            // Prioritize a function's own methods (with, where) over its
            // fields. This is fine as we define each field of a function,
            // if it has any.
            // ('methods_on' will be empty for Symbol and Module - their
            // method calls always refer to their fields.)
            if !matches!(*target, Value::Symbol(_) | Value::Module(_) | Value::Func(_))
                || methods_on(target.type_name()).iter().any(|(m, _)| m == &field)
            {
                let point = || Tracepoint::Call(Some(field.clone()));
                let output = methods::call(vm, target, &field, &mut args, span).trace(
                    vm.world(),
                    point,
                    span,
                )?;
                args.finish()?;
                return Ok(output);
            }

            (target.field(&field).at(field_span)?, args)
        } else {
            (callee.eval(vm, scopes)?, args.eval(vm, scopes)?)
        };

        // Handle math special cases for non-functions.
        if in_math && !matches!(callee, Value::Func(_)) {
            return Self::eval_non_func_in_math(vm, callee, callee_span, args)
                .map(|v| MaybeMut::temp(v, span));
        }

        let callee = callee.cast::<Func>().at(callee_span)?;
        let point = || Tracepoint::Call(callee.name().map(Into::into));
        let f = || {
            callee
                .call_vm(vm, args)
                .trace(vm.world(), point, span)
                .map(|v| MaybeMut::temp(v, span))
        };

        // Stacker is broken on WASM.
        #[cfg(target_arch = "wasm32")]
        return f();

        #[cfg(not(target_arch = "wasm32"))]
        stacker::maybe_grow(32 * 1024, 2 * 1024 * 1024, f)
    }
}

impl ast::FuncCall {
    /// Handle a function call of a non-function in math.
    /// - For combining accent symbols, apply them.
    /// - For everything else render it as if it was not parsed as a function
    ///   call.
    fn eval_non_func_in_math(
        vm: &mut Vm,
        callee: Value,
        callee_span: Span,
        mut args: Args,
    ) -> SourceResult<Value> {
        if let Value::Symbol(sym) = &callee {
            let c = sym.get();
            if let Some(accent) = Symbol::combining_accent(c) {
                let base = args.expect("base")?;
                args.finish()?;
                return Ok(Value::Content((vm.items.math_accent)(base, accent)));
            }
        }

        let mut body = Content::empty();
        for (i, arg) in args.all::<Content>()?.into_iter().enumerate() {
            if i > 0 {
                body += (vm.items.text)(','.into());
            }
            body += arg;
        }

        let delimited = (vm.items.math_delimited)(
            (vm.items.text)('('.into()),
            body,
            (vm.items.text)(')'.into()),
        );

        Ok(Value::Content(callee.display().spanned(callee_span) + delimited))
    }
}

fn in_math(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::MathIdent(_) => true,
        ast::Expr::FieldAccess(access) => in_math(&access.target()),
        _ => false,
    }
}

impl Eval for ast::Args {
    type Output = Args;

    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let mut items = EcoVec::new();

        for arg in self.items() {
            let span = arg.span();
            match arg {
                ast::Arg::Pos(expr) => {
                    items.push(Arg {
                        span,
                        name: None,
                        value: Spanned::new(expr.eval(vm, scopes)?, expr.span()),
                    });
                }
                ast::Arg::Named(named) => {
                    items.push(Arg {
                        span,
                        name: Some(named.name().take().into()),
                        value: Spanned::new(
                            named.expr().eval(vm, scopes)?,
                            named.expr().span(),
                        ),
                    });
                }
                ast::Arg::Spread(expr) => match expr.eval(vm, scopes)? {
                    Value::None => {}
                    Value::Array(array) => {
                        items.extend(array.into_iter().map(|value| Arg {
                            span,
                            name: None,
                            value: Spanned::new(value, span),
                        }));
                    }
                    Value::Dict(dict) => {
                        items.extend(dict.into_iter().map(|(key, value)| Arg {
                            span,
                            name: Some(key),
                            value: Spanned::new(value, span),
                        }));
                    }
                    Value::Args(args) => items.extend(args.items),
                    v => bail!(expr.span(), "cannot spread {}", v.type_name()),
                },
            }
        }

        Ok(Args { span: self.span(), items })
    }
}

impl Eval for ast::Closure {
    type Output = Value;

    #[tracing::instrument(name = "Closure::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        // The closure's name is defined by its let binding if there's one.
        let name = self.name();

        // Collect captured variables.
        let captured = {
            let mut visitor = CapturesVisitor::new(scopes);
            visitor.visit(self.as_untyped());
            visitor.finish()
        };

        // Collect parameters and an optional sink parameter.
        let mut params = Vec::new();
        for param in self.params().children() {
            match param {
                ast::Param::Pos(pattern) => params.push(Param::Pos(pattern)),
                ast::Param::Named(named) => {
                    params
                        .push(Param::Named(named.name(), named.expr().eval(vm, scopes)?));
                }
                ast::Param::Sink(spread) => params.push(Param::Sink(spread.name())),
            }
        }

        // Define the closure.
        let closure = Closure {
            location: vm.location,
            name,
            captured,
            params,
            body: self.body(),
        };

        Ok(Value::Func(Func::from(closure).spanned(self.params().span())))
    }
}

impl ast::Pattern {
    fn destruct_array<F>(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        value: Array,
        f: F,
        destruct: &ast::Destructuring,
    ) -> SourceResult<Value>
    where
        F: Fn(&mut Vm, &mut Scopes, ast::Expr, Value) -> SourceResult<Value>,
    {
        let mut i = 0;
        let len = value.as_slice().len();
        for p in destruct.bindings() {
            match p {
                ast::DestructuringKind::Normal(expr) => {
                    let Ok(v) = value.at(i as i64, None) else {
                        bail!(expr.span(), "not enough elements to destructure");
                    };
                    f(vm, scopes, expr, v.clone())?;
                    i += 1;
                }
                ast::DestructuringKind::Sink(spread) => {
                    let sink_size = (1 + len).checked_sub(destruct.bindings().count());
                    let sink = sink_size.and_then(|s| value.as_slice().get(i..i + s));
                    if let (Some(sink_size), Some(sink)) = (sink_size, sink) {
                        if let Some(expr) = spread.expr() {
                            f(vm, scopes, expr, Value::Array(sink.into()))?;
                        }
                        i += sink_size;
                    } else {
                        bail!(self.span(), "not enough elements to destructure")
                    }
                }
                ast::DestructuringKind::Named(named) => {
                    bail!(named.span(), "cannot destructure named elements from an array")
                }
                ast::DestructuringKind::Placeholder(underscore) => {
                    if i < len {
                        i += 1
                    } else {
                        bail!(underscore.span(), "not enough elements to destructure")
                    }
                }
            }
        }
        if i < len {
            bail!(self.span(), "too many elements to destructure");
        }

        Ok(Value::None)
    }

    fn destruct_dict<F>(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        dict: Dict,
        f: F,
        destruct: &ast::Destructuring,
    ) -> SourceResult<Value>
    where
        F: Fn(&mut Vm, &mut Scopes, ast::Expr, Value) -> SourceResult<Value>,
    {
        let mut sink = None;
        let mut used = HashSet::new();
        for p in destruct.bindings() {
            match p {
                ast::DestructuringKind::Normal(ast::Expr::Ident(ident)) => {
                    let v = dict
                        .at(&ident, None)
                        .map_err(|_| "destructuring key not found in dictionary")
                        .at(ident.span())?;
                    f(vm, scopes, ast::Expr::Ident(ident.clone()), v.clone())?;
                    used.insert(ident.take());
                }
                ast::DestructuringKind::Sink(spread) => sink = spread.expr(),
                ast::DestructuringKind::Named(named) => {
                    let name = named.name();
                    let v = dict
                        .at(&name, None)
                        .map_err(|_| "destructuring key not found in dictionary")
                        .at(name.span())?;
                    f(vm, scopes, named.expr(), v.clone())?;
                    used.insert(name.take());
                }
                ast::DestructuringKind::Placeholder(_) => {}
                ast::DestructuringKind::Normal(expr) => {
                    bail!(expr.span(), "expected key, found expression");
                }
            }
        }

        if let Some(expr) = sink {
            let mut sink = Dict::new();
            for (key, value) in dict {
                if !used.contains(key.as_str()) {
                    sink.insert(key, value);
                }
            }
            f(vm, scopes, expr, Value::Dict(sink))?;
        }

        Ok(Value::None)
    }

    /// Destruct the given value into the pattern and apply the function to each binding.
    #[tracing::instrument(skip_all)]
    fn apply<T>(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        value: Value,
        f: T,
    ) -> SourceResult<Value>
    where
        T: Fn(&mut Vm, &mut Scopes, ast::Expr, Value) -> SourceResult<Value>,
    {
        match self {
            ast::Pattern::Normal(expr) => {
                f(vm, scopes, expr.clone(), value)?;
                Ok(Value::None)
            }
            ast::Pattern::Placeholder(_) => Ok(Value::None),
            ast::Pattern::Destructuring(destruct) => match value {
                Value::Array(value) => {
                    self.destruct_array(vm, scopes, value, f, destruct)
                }
                Value::Dict(value) => self.destruct_dict(vm, scopes, value, f, destruct),
                _ => bail!(self.span(), "cannot destructure {}", value.type_name()),
            },
        }
    }

    /// Destruct the value into the pattern by binding.
    pub fn define(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        value: Value,
    ) -> SourceResult<Value> {
        self.apply(vm, scopes, value, |vm, scopes, expr, value| match expr {
            ast::Expr::Ident(ident) => {
                vm.define(scopes, ident, value);
                Ok(Value::None)
            }
            _ => bail!(expr.span(), "nested patterns are currently not supported"),
        })
    }

    /// Destruct the value into the pattern by assignment.
    pub fn assign(
        &self,
        vm: &mut Vm,
        scopes: &mut Scopes,
        value: Value,
    ) -> SourceResult<Value> {
        self.apply(vm, scopes, value, |vm, scopes, expr, value| {
            let location = expr.eval_maybe_mut(vm, scopes)?.mutate()?;
            *location = value;
            Ok(Value::None)
        })
    }
}

impl Eval for ast::LetBinding {
    type Output = Value;

    #[tracing::instrument(name = "LetBinding::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let value = match self.init() {
            Some(expr) => expr.eval(vm, scopes)?,
            None => Value::None,
        };

        match self.kind() {
            ast::LetBindingKind::Normal(pattern) => pattern.define(vm, scopes, value),
            ast::LetBindingKind::Closure(ident) => {
                vm.define(scopes, ident, value);
                Ok(Value::None)
            }
        }
    }
}

impl Eval for ast::DestructAssignment {
    type Output = Value;

    #[tracing::instrument(name = "DestructAssignment::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let value = self.value().eval(vm, scopes)?;
        self.pattern().assign(vm, scopes, value)?;
        Ok(Value::None)
    }
}

impl Eval for ast::SetRule {
    type Output = Styles;

    #[tracing::instrument(name = "SetRule::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        if let Some(condition) = self.condition() {
            if !condition.eval(vm, scopes)?.cast::<bool>().at(condition.span())? {
                return Ok(Styles::new());
            }
        }

        let target = self.target();
        let target = target
            .eval(vm, scopes)?
            .cast::<Func>()
            .and_then(|func| {
                func.element().ok_or_else(|| {
                    "only element functions can be used in set rules".into()
                })
            })
            .at(target.span())?;
        let args = self.args().eval(vm, scopes)?;
        Ok(target.set(vm, args)?.spanned(self.span()))
    }
}

impl Eval for ast::ShowRule {
    type Output = Recipe;

    #[tracing::instrument(name = "ShowRule::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let selector = self
            .selector()
            .map(|sel| sel.eval(vm, scopes)?.cast::<ShowableSelector>().at(sel.span()))
            .transpose()?
            .map(|selector| selector.0);

        let transform = self.transform();
        let span = transform.span();

        let transform = match transform {
            ast::Expr::Set(set) => Transform::Style(set.eval(vm, scopes)?),
            expr => expr.eval(vm, scopes)?.cast::<Transform>().at(span)?,
        };

        Ok(Recipe { span, selector, transform })
    }
}

impl Eval for ast::Conditional {
    type Output = Value;

    #[tracing::instrument(name = "Conditional::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let condition = self.condition();
        if condition.eval(vm, scopes)?.cast::<bool>().at(condition.span())? {
            self.if_body().eval(vm, scopes)
        } else if let Some(else_body) = self.else_body() {
            else_body.eval(vm, scopes)
        } else {
            Ok(Value::None)
        }
    }
}

impl Eval for ast::WhileLoop {
    type Output = Value;

    #[tracing::instrument(name = "WhileLoop::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let flow = vm.flow.take();
        let mut output = Value::None;
        let mut i = 0;

        let condition = self.condition();
        let body = self.body();

        while condition.eval(vm, scopes)?.cast::<bool>().at(condition.span())? {
            if i == 0
                && is_invariant(condition.as_untyped())
                && !can_diverge(body.as_untyped())
            {
                bail!(condition.span(), "condition is always true");
            } else if i >= MAX_ITERATIONS {
                bail!(self.span(), "loop seems to be infinite");
            }

            let value = body.eval(vm, scopes)?;
            output = ops::join(output, value).at(body.span())?;

            match vm.flow {
                Some(FlowEvent::Break(_)) => {
                    vm.flow = None;
                    break;
                }
                Some(FlowEvent::Continue(_)) => vm.flow = None,
                Some(FlowEvent::Return(..)) => break,
                None => {}
            }

            i += 1;
        }

        if flow.is_some() {
            vm.flow = flow;
        }

        Ok(output)
    }
}

/// Whether the expression always evaluates to the same value.
fn is_invariant(expr: &SyntaxNode) -> bool {
    match expr.cast() {
        Some(ast::Expr::Ident(_)) => false,
        Some(ast::Expr::MathIdent(_)) => false,
        Some(ast::Expr::FieldAccess(access)) => {
            is_invariant(access.target().as_untyped())
        }
        Some(ast::Expr::FuncCall(call)) => {
            is_invariant(call.callee().as_untyped())
                && is_invariant(call.args().as_untyped())
        }
        _ => expr.children().all(is_invariant),
    }
}

/// Whether the expression contains a break or return.
fn can_diverge(expr: &SyntaxNode) -> bool {
    matches!(expr.kind(), SyntaxKind::Break | SyntaxKind::Return)
        || expr.children().any(can_diverge)
}

impl Eval for ast::ForLoop {
    type Output = Value;

    #[tracing::instrument(name = "ForLoop::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let flow = vm.flow.take();
        let mut output = Value::None;

        macro_rules! iter {
            (for $pat:ident in $iter:expr) => {{
                scopes.enter();

                #[allow(unused_parens)]
                for value in $iter {
                    $pat.define(vm, scopes, value.into_value())?;

                    let body = self.body();
                    let value = body.eval(vm, scopes)?;
                    output = ops::join(output, value).at(body.span())?;

                    match vm.flow {
                        Some(FlowEvent::Break(_)) => {
                            vm.flow = None;
                            break;
                        }
                        Some(FlowEvent::Continue(_)) => vm.flow = None,
                        Some(FlowEvent::Return(..)) => break,
                        None => {}
                    }
                }

                scopes.exit();
            }};
        }

        let iter = self.iter().eval(vm, scopes)?;
        let pattern = self.pattern();

        match (&pattern, iter.clone()) {
            (ast::Pattern::Normal(_), Value::Str(string)) => {
                // Iterate over graphemes of string.
                iter!(for pattern in string.as_str().graphemes(true));
            }
            (_, Value::Dict(dict)) => {
                // Iterate over pairs of dict.
                iter!(for pattern in dict.pairs());
            }
            (_, Value::Array(array)) => {
                // Iterate over values of array.
                iter!(for pattern in array);
            }
            (ast::Pattern::Normal(_), _) => {
                bail!(self.iter().span(), "cannot loop over {}", iter.type_name());
            }
            (_, _) => {
                bail!(pattern.span(), "cannot destructure values of {}", iter.type_name())
            }
        }

        if flow.is_some() {
            vm.flow = flow;
        }

        Ok(output)
    }
}

/// Applies imports from `import` to the current scope.
fn apply_imports<V: IntoValue>(
    imports: Option<ast::Imports>,
    vm: &mut Vm,
    scopes: &mut Scopes,
    source_value: V,
    name: impl Fn(&V) -> EcoString,
    scope: impl Fn(&V) -> &Scope,
) -> SourceResult<()> {
    match imports {
        None => {
            scopes.top.define(name(&source_value), source_value);
        }
        Some(ast::Imports::Wildcard) => {
            for (var, value) in scope(&source_value).iter() {
                scopes.top.define(var.clone(), value.clone());
            }
        }
        Some(ast::Imports::Items(idents)) => {
            let mut errors = vec![];
            let scope = scope(&source_value);
            for ident in idents {
                if let Some(value) = scope.get(&ident) {
                    vm.define(scopes, ident, value.clone());
                } else {
                    errors.push(error!(ident.span(), "unresolved import"));
                }
            }
            if !errors.is_empty() {
                return Err(Box::new(errors));
            }
        }
    }

    Ok(())
}

impl Eval for ast::ModuleImport {
    type Output = Value;

    #[tracing::instrument(name = "ModuleImport::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let span = self.source().span();
        let source = self.source().eval(vm, scopes)?;
        if let Value::Func(func) = source {
            if func.info().is_none() {
                bail!(span, "cannot import from user-defined functions");
            }
            apply_imports(
                self.imports(),
                vm,
                scopes,
                func,
                |func| func.info().unwrap().name.into(),
                |func| &func.info().unwrap().scope,
            )?;
        } else {
            let module = import(vm, source, span, true)?;
            apply_imports(
                self.imports(),
                vm,
                scopes,
                module,
                |module| module.name().clone(),
                |module| module.scope(),
            )?;
        }

        Ok(Value::None)
    }
}

impl Eval for ast::ModuleInclude {
    type Output = Content;

    #[tracing::instrument(name = "ModuleInclude::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let span = self.source().span();
        let source = self.source().eval(vm, scopes)?;
        let module = import(vm, source, span, false)?;
        Ok(module.content())
    }
}

/// Process an import of a module relative to the current location.
fn import(
    vm: &mut Vm,
    source: Value,
    span: Span,
    accept_functions: bool,
) -> SourceResult<Module> {
    let path = match source {
        Value::Str(path) => path,
        Value::Module(module) => return Ok(module),
        v => {
            if accept_functions {
                bail!(span, "expected path, module or function, found {}", v.type_name())
            } else {
                bail!(span, "expected path or module, found {}", v.type_name())
            }
        }
    };

    // Handle package and file imports.
    let path = path.as_str();
    if path.starts_with('@') {
        let spec = path.parse::<PackageSpec>().at(span)?;
        import_package(vm, spec, span)
    } else {
        import_file(vm, path, span)
    }
}

/// Import an external package.
fn import_package(vm: &mut Vm, spec: PackageSpec, span: Span) -> SourceResult<Module> {
    // Evaluate the manifest.
    let manifest_id = FileId::new(Some(spec.clone()), Path::new("/typst.toml"));
    let bytes = vm.world().file(manifest_id).at(span)?;
    let manifest = PackageManifest::parse(&bytes).at(span)?;
    manifest.validate(&spec).at(span)?;

    // Evaluate the entry point.
    let entrypoint_id = manifest_id.join(&manifest.package.entrypoint).at(span)?;
    let source = vm.world().source(entrypoint_id).at(span)?;
    let point = || Tracepoint::Import;
    Ok(eval(vm.world(), vm.route, TrackedMut::reborrow_mut(&mut vm.vt.tracer), &source)
        .trace(vm.world(), point, span)?
        .with_name(manifest.package.name))
}

/// Import a file from a path.
fn import_file(vm: &mut Vm, path: &str, span: Span) -> SourceResult<Module> {
    // Load the source file.
    let world = vm.world();
    let id = vm.location().join(path).at(span)?;
    let source = world.source(id).at(span)?;

    // Prevent cyclic importing.
    if vm.route.contains(source.id()) {
        bail!(span, "cyclic import");
    }

    // Evaluate the file.
    let point = || Tracepoint::Import;
    eval(world, vm.route, TrackedMut::reborrow_mut(&mut vm.vt.tracer), &source)
        .trace(world, point, span)
}

impl Eval for ast::LoopBreak {
    type Output = Value;

    #[tracing::instrument(name = "LoopBreak::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        if vm.flow.is_none() {
            vm.flow = Some(FlowEvent::Break(self.span()));
        }
        Ok(Value::None)
    }
}

impl Eval for ast::LoopContinue {
    type Output = Value;

    #[tracing::instrument(name = "LoopContinue::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, _: &mut Scopes) -> SourceResult<Self::Output> {
        if vm.flow.is_none() {
            vm.flow = Some(FlowEvent::Continue(self.span()));
        }
        Ok(Value::None)
    }
}

impl Eval for ast::FuncReturn {
    type Output = Value;

    #[tracing::instrument(name = "FuncReturn::eval", skip_all)]
    fn eval(&self, vm: &mut Vm, scopes: &mut Scopes) -> SourceResult<Self::Output> {
        let value = self.body().map(|body| body.eval(vm, scopes)).transpose()?;
        if vm.flow.is_none() {
            vm.flow = Some(FlowEvent::Return(self.span(), value));
        }
        Ok(Value::None)
    }
}
