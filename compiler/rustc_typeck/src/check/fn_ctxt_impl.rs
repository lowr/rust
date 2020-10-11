use super::callee::{self, DeferredCallResolution};
use super::coercion::CoerceMany;
use super::method::{self, MethodCallee, SelfSource};
use super::Expectation::*;
use super::TupleArgumentsFlag::*;
use super::{
    potentially_plural_count, struct_span_err, BreakableCtxt, Diverges, Expectation, FallbackMode,
    FnCtxt, LocalTy, Needs, TupleArgumentsFlag,
};
use crate::astconv::{
    AstConv, ExplicitLateBound, GenericArgCountMismatch, GenericArgCountResult, PathSeg,
};

use rustc_ast as ast;
use rustc_data_structures::captures::Captures;
use rustc_data_structures::fx::FxHashSet;
use rustc_errors::{Applicability, DiagnosticBuilder, DiagnosticId, ErrorReported};
use rustc_hir as hir;
use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::lang_items::LangItem;
use rustc_hir::{ExprKind, GenericArg, Node, QPath};
use rustc_infer::infer::canonical::{Canonical, OriginalQueryValues, QueryResponse};
use rustc_infer::infer::error_reporting::TypeAnnotationNeeded::E0282;
use rustc_infer::infer::{InferOk, InferResult};
use rustc_middle::ty::adjustment::{
    Adjust, Adjustment, AllowTwoPhase, AutoBorrow, AutoBorrowMutability,
};
use rustc_middle::ty::fold::TypeFoldable;
use rustc_middle::ty::subst::{
    self, GenericArgKind, InternalSubsts, Subst, SubstsRef, UserSelfTy, UserSubsts,
};
use rustc_middle::ty::{
    self, AdtKind, CanonicalUserType, DefIdTree, GenericParamDefKind, ToPolyTraitRef, ToPredicate,
    Ty, UserType,
};
use rustc_session::{lint, Session};
use rustc_span::hygiene::DesugaringKind;
use rustc_span::source_map::{original_sp, DUMMY_SP};
use rustc_span::symbol::{kw, sym, Ident};
use rustc_span::{self, BytePos, MultiSpan, Span};
use rustc_trait_selection::infer::InferCtxtExt as _;
use rustc_trait_selection::opaque_types::InferCtxtExt as _;
use rustc_trait_selection::traits::error_reporting::InferCtxtExt as _;
use rustc_trait_selection::traits::{self, ObligationCauseCode, TraitEngine, TraitEngineExt};

use std::collections::hash_map::Entry;
use std::mem::replace;
use std::slice;

impl<'a, 'tcx> FnCtxt<'a, 'tcx> {
    /// Produces warning on the given node, if the current point in the
    /// function is unreachable, and there hasn't been another warning.
    pub(super) fn warn_if_unreachable(&self, id: hir::HirId, span: Span, kind: &str) {
        // FIXME: Combine these two 'if' expressions into one once
        // let chains are implemented
        if let Diverges::Always { span: orig_span, custom_note } = self.diverges.get() {
            // If span arose from a desugaring of `if` or `while`, then it is the condition itself,
            // which diverges, that we are about to lint on. This gives suboptimal diagnostics.
            // Instead, stop here so that the `if`- or `while`-expression's block is linted instead.
            if !span.is_desugaring(DesugaringKind::CondTemporary)
                && !span.is_desugaring(DesugaringKind::Async)
                && !orig_span.is_desugaring(DesugaringKind::Await)
            {
                self.diverges.set(Diverges::WarnedAlways);

                debug!("warn_if_unreachable: id={:?} span={:?} kind={}", id, span, kind);

                self.tcx().struct_span_lint_hir(lint::builtin::UNREACHABLE_CODE, id, span, |lint| {
                    let msg = format!("unreachable {}", kind);
                    lint.build(&msg)
                        .span_label(span, &msg)
                        .span_label(
                            orig_span,
                            custom_note
                                .unwrap_or("any code following this expression is unreachable"),
                        )
                        .emit();
                })
            }
        }
    }

    /// Resolves type and const variables in `ty` if possible. Unlike the infcx
    /// version (resolve_vars_if_possible), this version will
    /// also select obligations if it seems useful, in an effort
    /// to get more type information.
    pub(super) fn resolve_vars_with_obligations(&self, mut ty: Ty<'tcx>) -> Ty<'tcx> {
        debug!("resolve_vars_with_obligations(ty={:?})", ty);

        // No Infer()? Nothing needs doing.
        if !ty.has_infer_types_or_consts() {
            debug!("resolve_vars_with_obligations: ty={:?}", ty);
            return ty;
        }

        // If `ty` is a type variable, see whether we already know what it is.
        ty = self.resolve_vars_if_possible(&ty);
        if !ty.has_infer_types_or_consts() {
            debug!("resolve_vars_with_obligations: ty={:?}", ty);
            return ty;
        }

        // If not, try resolving pending obligations as much as
        // possible. This can help substantially when there are
        // indirect dependencies that don't seem worth tracking
        // precisely.
        self.select_obligations_where_possible(false, |_| {});
        ty = self.resolve_vars_if_possible(&ty);

        debug!("resolve_vars_with_obligations: ty={:?}", ty);
        ty
    }

    pub(super) fn record_deferred_call_resolution(
        &self,
        closure_def_id: DefId,
        r: DeferredCallResolution<'tcx>,
    ) {
        let mut deferred_call_resolutions = self.deferred_call_resolutions.borrow_mut();
        deferred_call_resolutions.entry(closure_def_id).or_default().push(r);
    }

    pub(super) fn remove_deferred_call_resolutions(
        &self,
        closure_def_id: DefId,
    ) -> Vec<DeferredCallResolution<'tcx>> {
        let mut deferred_call_resolutions = self.deferred_call_resolutions.borrow_mut();
        deferred_call_resolutions.remove(&closure_def_id).unwrap_or(vec![])
    }

    pub fn tag(&self) -> String {
        format!("{:p}", self)
    }

    pub fn local_ty(&self, span: Span, nid: hir::HirId) -> LocalTy<'tcx> {
        self.locals.borrow().get(&nid).cloned().unwrap_or_else(|| {
            span_bug!(span, "no type for local variable {}", self.tcx.hir().node_to_string(nid))
        })
    }

    #[inline]
    pub fn write_ty(&self, id: hir::HirId, ty: Ty<'tcx>) {
        debug!(
            "write_ty({:?}, {:?}) in fcx {}",
            id,
            self.resolve_vars_if_possible(&ty),
            self.tag()
        );
        self.typeck_results.borrow_mut().node_types_mut().insert(id, ty);

        if ty.references_error() {
            self.has_errors.set(true);
            self.set_tainted_by_errors();
        }
    }

    pub fn write_field_index(&self, hir_id: hir::HirId, index: usize) {
        self.typeck_results.borrow_mut().field_indices_mut().insert(hir_id, index);
    }

    fn write_resolution(&self, hir_id: hir::HirId, r: Result<(DefKind, DefId), ErrorReported>) {
        self.typeck_results.borrow_mut().type_dependent_defs_mut().insert(hir_id, r);
    }

    pub fn write_method_call(&self, hir_id: hir::HirId, method: MethodCallee<'tcx>) {
        debug!("write_method_call(hir_id={:?}, method={:?})", hir_id, method);
        self.write_resolution(hir_id, Ok((DefKind::AssocFn, method.def_id)));
        self.write_substs(hir_id, method.substs);

        // When the method is confirmed, the `method.substs` includes
        // parameters from not just the method, but also the impl of
        // the method -- in particular, the `Self` type will be fully
        // resolved. However, those are not something that the "user
        // specified" -- i.e., those types come from the inferred type
        // of the receiver, not something the user wrote. So when we
        // create the user-substs, we want to replace those earlier
        // types with just the types that the user actually wrote --
        // that is, those that appear on the *method itself*.
        //
        // As an example, if the user wrote something like
        // `foo.bar::<u32>(...)` -- the `Self` type here will be the
        // type of `foo` (possibly adjusted), but we don't want to
        // include that. We want just the `[_, u32]` part.
        if !method.substs.is_noop() {
            let method_generics = self.tcx.generics_of(method.def_id);
            if !method_generics.params.is_empty() {
                let user_type_annotation = self.infcx.probe(|_| {
                    let user_substs = UserSubsts {
                        substs: InternalSubsts::for_item(self.tcx, method.def_id, |param, _| {
                            let i = param.index as usize;
                            if i < method_generics.parent_count {
                                self.infcx.var_for_def(DUMMY_SP, param)
                            } else {
                                method.substs[i]
                            }
                        }),
                        user_self_ty: None, // not relevant here
                    };

                    self.infcx.canonicalize_user_type_annotation(&UserType::TypeOf(
                        method.def_id,
                        user_substs,
                    ))
                });

                debug!("write_method_call: user_type_annotation={:?}", user_type_annotation);
                self.write_user_type_annotation(hir_id, user_type_annotation);
            }
        }
    }

    pub fn write_substs(&self, node_id: hir::HirId, substs: SubstsRef<'tcx>) {
        if !substs.is_noop() {
            debug!("write_substs({:?}, {:?}) in fcx {}", node_id, substs, self.tag());

            self.typeck_results.borrow_mut().node_substs_mut().insert(node_id, substs);
        }
    }

    /// Given the substs that we just converted from the HIR, try to
    /// canonicalize them and store them as user-given substitutions
    /// (i.e., substitutions that must be respected by the NLL check).
    ///
    /// This should be invoked **before any unifications have
    /// occurred**, so that annotations like `Vec<_>` are preserved
    /// properly.
    pub fn write_user_type_annotation_from_substs(
        &self,
        hir_id: hir::HirId,
        def_id: DefId,
        substs: SubstsRef<'tcx>,
        user_self_ty: Option<UserSelfTy<'tcx>>,
    ) {
        debug!(
            "write_user_type_annotation_from_substs: hir_id={:?} def_id={:?} substs={:?} \
             user_self_ty={:?} in fcx {}",
            hir_id,
            def_id,
            substs,
            user_self_ty,
            self.tag(),
        );

        if Self::can_contain_user_lifetime_bounds((substs, user_self_ty)) {
            let canonicalized = self.infcx.canonicalize_user_type_annotation(&UserType::TypeOf(
                def_id,
                UserSubsts { substs, user_self_ty },
            ));
            debug!("write_user_type_annotation_from_substs: canonicalized={:?}", canonicalized);
            self.write_user_type_annotation(hir_id, canonicalized);
        }
    }

    pub fn write_user_type_annotation(
        &self,
        hir_id: hir::HirId,
        canonical_user_type_annotation: CanonicalUserType<'tcx>,
    ) {
        debug!(
            "write_user_type_annotation: hir_id={:?} canonical_user_type_annotation={:?} tag={}",
            hir_id,
            canonical_user_type_annotation,
            self.tag(),
        );

        if !canonical_user_type_annotation.is_identity() {
            self.typeck_results
                .borrow_mut()
                .user_provided_types_mut()
                .insert(hir_id, canonical_user_type_annotation);
        } else {
            debug!("write_user_type_annotation: skipping identity substs");
        }
    }

    pub fn apply_adjustments(&self, expr: &hir::Expr<'_>, adj: Vec<Adjustment<'tcx>>) {
        debug!("apply_adjustments(expr={:?}, adj={:?})", expr, adj);

        if adj.is_empty() {
            return;
        }

        let autoborrow_mut = adj.iter().any(|adj| {
            matches!(adj, &Adjustment {
                kind: Adjust::Borrow(AutoBorrow::Ref(_, AutoBorrowMutability::Mut { .. })),
                ..
            })
        });

        match self.typeck_results.borrow_mut().adjustments_mut().entry(expr.hir_id) {
            Entry::Vacant(entry) => {
                entry.insert(adj);
            }
            Entry::Occupied(mut entry) => {
                debug!(" - composing on top of {:?}", entry.get());
                match (&entry.get()[..], &adj[..]) {
                    // Applying any adjustment on top of a NeverToAny
                    // is a valid NeverToAny adjustment, because it can't
                    // be reached.
                    (&[Adjustment { kind: Adjust::NeverToAny, .. }], _) => return,
                    (&[
                        Adjustment { kind: Adjust::Deref(_), .. },
                        Adjustment { kind: Adjust::Borrow(AutoBorrow::Ref(..)), .. },
                    ], &[
                        Adjustment { kind: Adjust::Deref(_), .. },
                        .. // Any following adjustments are allowed.
                    ]) => {
                        // A reborrow has no effect before a dereference.
                    }
                    // FIXME: currently we never try to compose autoderefs
                    // and ReifyFnPointer/UnsafeFnPointer, but we could.
                    _ =>
                        bug!("while adjusting {:?}, can't compose {:?} and {:?}",
                             expr, entry.get(), adj)
                };
                *entry.get_mut() = adj;
            }
        }

        // If there is an mutable auto-borrow, it is equivalent to `&mut <expr>`.
        // In this case implicit use of `Deref` and `Index` within `<expr>` should
        // instead be `DerefMut` and `IndexMut`, so fix those up.
        if autoborrow_mut {
            self.convert_place_derefs_to_mutable(expr);
        }
    }

    /// Basically whenever we are converting from a type scheme into
    /// the fn body space, we always want to normalize associated
    /// types as well. This function combines the two.
    fn instantiate_type_scheme<T>(&self, span: Span, substs: SubstsRef<'tcx>, value: &T) -> T
    where
        T: TypeFoldable<'tcx>,
    {
        let value = value.subst(self.tcx, substs);
        let result = self.normalize_associated_types_in(span, &value);
        debug!("instantiate_type_scheme(value={:?}, substs={:?}) = {:?}", value, substs, result);
        result
    }

    /// As `instantiate_type_scheme`, but for the bounds found in a
    /// generic type scheme.
    fn instantiate_bounds(
        &self,
        span: Span,
        def_id: DefId,
        substs: SubstsRef<'tcx>,
    ) -> (ty::InstantiatedPredicates<'tcx>, Vec<Span>) {
        let bounds = self.tcx.predicates_of(def_id);
        let spans: Vec<Span> = bounds.predicates.iter().map(|(_, span)| *span).collect();
        let result = bounds.instantiate(self.tcx, substs);
        let result = self.normalize_associated_types_in(span, &result);
        debug!(
            "instantiate_bounds(bounds={:?}, substs={:?}) = {:?}, {:?}",
            bounds, substs, result, spans,
        );
        (result, spans)
    }

    /// Replaces the opaque types from the given value with type variables,
    /// and records the `OpaqueTypeMap` for later use during writeback. See
    /// `InferCtxt::instantiate_opaque_types` for more details.
    pub(super) fn instantiate_opaque_types_from_value<T: TypeFoldable<'tcx>>(
        &self,
        parent_id: hir::HirId,
        value: &T,
        value_span: Span,
    ) -> T {
        let parent_def_id = self.tcx.hir().local_def_id(parent_id);
        debug!(
            "instantiate_opaque_types_from_value(parent_def_id={:?}, value={:?})",
            parent_def_id, value
        );

        let (value, opaque_type_map) =
            self.register_infer_ok_obligations(self.instantiate_opaque_types(
                parent_def_id,
                self.body_id,
                self.param_env,
                value,
                value_span,
            ));

        let mut opaque_types = self.opaque_types.borrow_mut();
        let mut opaque_types_vars = self.opaque_types_vars.borrow_mut();
        for (ty, decl) in opaque_type_map {
            let _ = opaque_types.insert(ty, decl);
            let _ = opaque_types_vars.insert(decl.concrete_ty, decl.opaque_type);
        }

        value
    }

    pub(super) fn normalize_associated_types_in<T>(&self, span: Span, value: &T) -> T
    where
        T: TypeFoldable<'tcx>,
    {
        self.inh.normalize_associated_types_in(span, self.body_id, self.param_env, value)
    }

    pub(super) fn normalize_associated_types_in_as_infer_ok<T>(
        &self,
        span: Span,
        value: &T,
    ) -> InferOk<'tcx, T>
    where
        T: TypeFoldable<'tcx>,
    {
        self.inh.partially_normalize_associated_types_in(span, self.body_id, self.param_env, value)
    }

    pub fn require_type_meets(
        &self,
        ty: Ty<'tcx>,
        span: Span,
        code: traits::ObligationCauseCode<'tcx>,
        def_id: DefId,
    ) {
        self.register_bound(ty, def_id, traits::ObligationCause::new(span, self.body_id, code));
    }

    pub fn require_type_is_sized(
        &self,
        ty: Ty<'tcx>,
        span: Span,
        code: traits::ObligationCauseCode<'tcx>,
    ) {
        if !ty.references_error() {
            let lang_item = self.tcx.require_lang_item(LangItem::Sized, None);
            self.require_type_meets(ty, span, code, lang_item);
        }
    }

    pub fn require_type_is_sized_deferred(
        &self,
        ty: Ty<'tcx>,
        span: Span,
        code: traits::ObligationCauseCode<'tcx>,
    ) {
        if !ty.references_error() {
            self.deferred_sized_obligations.borrow_mut().push((ty, span, code));
        }
    }

    pub fn register_bound(
        &self,
        ty: Ty<'tcx>,
        def_id: DefId,
        cause: traits::ObligationCause<'tcx>,
    ) {
        if !ty.references_error() {
            self.fulfillment_cx.borrow_mut().register_bound(
                self,
                self.param_env,
                ty,
                def_id,
                cause,
            );
        }
    }

    pub fn to_ty(&self, ast_t: &hir::Ty<'_>) -> Ty<'tcx> {
        let t = AstConv::ast_ty_to_ty(self, ast_t);
        self.register_wf_obligation(t.into(), ast_t.span, traits::MiscObligation);
        t
    }

    pub fn to_ty_saving_user_provided_ty(&self, ast_ty: &hir::Ty<'_>) -> Ty<'tcx> {
        let ty = self.to_ty(ast_ty);
        debug!("to_ty_saving_user_provided_ty: ty={:?}", ty);

        if Self::can_contain_user_lifetime_bounds(ty) {
            let c_ty = self.infcx.canonicalize_response(&UserType::Ty(ty));
            debug!("to_ty_saving_user_provided_ty: c_ty={:?}", c_ty);
            self.typeck_results.borrow_mut().user_provided_types_mut().insert(ast_ty.hir_id, c_ty);
        }

        ty
    }

    pub fn to_const(&self, ast_c: &hir::AnonConst) -> &'tcx ty::Const<'tcx> {
        let const_def_id = self.tcx.hir().local_def_id(ast_c.hir_id);
        let c = ty::Const::from_anon_const(self.tcx, const_def_id);
        self.register_wf_obligation(
            c.into(),
            self.tcx.hir().span(ast_c.hir_id),
            ObligationCauseCode::MiscObligation,
        );
        c
    }

    pub fn const_arg_to_const(
        &self,
        ast_c: &hir::AnonConst,
        param_def_id: DefId,
    ) -> &'tcx ty::Const<'tcx> {
        let const_def = ty::WithOptConstParam {
            did: self.tcx.hir().local_def_id(ast_c.hir_id),
            const_param_did: Some(param_def_id),
        };
        let c = ty::Const::from_opt_const_arg_anon_const(self.tcx, const_def);
        self.register_wf_obligation(
            c.into(),
            self.tcx.hir().span(ast_c.hir_id),
            ObligationCauseCode::MiscObligation,
        );
        c
    }

    // If the type given by the user has free regions, save it for later, since
    // NLL would like to enforce those. Also pass in types that involve
    // projections, since those can resolve to `'static` bounds (modulo #54940,
    // which hopefully will be fixed by the time you see this comment, dear
    // reader, although I have my doubts). Also pass in types with inference
    // types, because they may be repeated. Other sorts of things are already
    // sufficiently enforced with erased regions. =)
    fn can_contain_user_lifetime_bounds<T>(t: T) -> bool
    where
        T: TypeFoldable<'tcx>,
    {
        t.has_free_regions() || t.has_projections() || t.has_infer_types()
    }

    pub fn node_ty(&self, id: hir::HirId) -> Ty<'tcx> {
        match self.typeck_results.borrow().node_types().get(id) {
            Some(&t) => t,
            None if self.is_tainted_by_errors() => self.tcx.ty_error(),
            None => {
                bug!(
                    "no type for node {}: {} in fcx {}",
                    id,
                    self.tcx.hir().node_to_string(id),
                    self.tag()
                );
            }
        }
    }

    /// Registers an obligation for checking later, during regionck, that `arg` is well-formed.
    pub fn register_wf_obligation(
        &self,
        arg: subst::GenericArg<'tcx>,
        span: Span,
        code: traits::ObligationCauseCode<'tcx>,
    ) {
        // WF obligations never themselves fail, so no real need to give a detailed cause:
        let cause = traits::ObligationCause::new(span, self.body_id, code);
        self.register_predicate(traits::Obligation::new(
            cause,
            self.param_env,
            ty::PredicateAtom::WellFormed(arg).to_predicate(self.tcx),
        ));
    }

    /// Registers obligations that all `substs` are well-formed.
    pub fn add_wf_bounds(&self, substs: SubstsRef<'tcx>, expr: &hir::Expr<'_>) {
        for arg in substs.iter().filter(|arg| {
            matches!(arg.unpack(), GenericArgKind::Type(..) | GenericArgKind::Const(..))
        }) {
            self.register_wf_obligation(arg, expr.span, traits::MiscObligation);
        }
    }

    /// Given a fully substituted set of bounds (`generic_bounds`), and the values with which each
    /// type/region parameter was instantiated (`substs`), creates and registers suitable
    /// trait/region obligations.
    ///
    /// For example, if there is a function:
    ///
    /// ```
    /// fn foo<'a,T:'a>(...)
    /// ```
    ///
    /// and a reference:
    ///
    /// ```
    /// let f = foo;
    /// ```
    ///
    /// Then we will create a fresh region variable `'$0` and a fresh type variable `$1` for `'a`
    /// and `T`. This routine will add a region obligation `$1:'$0` and register it locally.
    pub fn add_obligations_for_parameters(
        &self,
        cause: traits::ObligationCause<'tcx>,
        predicates: ty::InstantiatedPredicates<'tcx>,
    ) {
        assert!(!predicates.has_escaping_bound_vars());

        debug!("add_obligations_for_parameters(predicates={:?})", predicates);

        for obligation in traits::predicates_for_generics(cause, self.param_env, predicates) {
            self.register_predicate(obligation);
        }
    }

    // FIXME(arielb1): use this instead of field.ty everywhere
    // Only for fields! Returns <none> for methods>
    // Indifferent to privacy flags
    pub fn field_ty(
        &self,
        span: Span,
        field: &'tcx ty::FieldDef,
        substs: SubstsRef<'tcx>,
    ) -> Ty<'tcx> {
        self.normalize_associated_types_in(span, &field.ty(self.tcx, substs))
    }

    pub(super) fn check_casts(&self) {
        let mut deferred_cast_checks = self.deferred_cast_checks.borrow_mut();
        for cast in deferred_cast_checks.drain(..) {
            cast.check(self);
        }
    }

    pub(super) fn resolve_generator_interiors(&self, def_id: DefId) {
        let mut generators = self.deferred_generator_interiors.borrow_mut();
        for (body_id, interior, kind) in generators.drain(..) {
            self.select_obligations_where_possible(false, |_| {});
            super::generator_interior::resolve_interior(self, def_id, body_id, interior, kind);
        }
    }

    // Tries to apply a fallback to `ty` if it is an unsolved variable.
    //
    // - Unconstrained ints are replaced with `i32`.
    //
    // - Unconstrained floats are replaced with with `f64`.
    //
    // - Non-numerics get replaced with `!` when `#![feature(never_type_fallback)]`
    //   is enabled. Otherwise, they are replaced with `()`.
    //
    // Fallback becomes very dubious if we have encountered type-checking errors.
    // In that case, fallback to Error.
    // The return value indicates whether fallback has occurred.
    pub(super) fn fallback_if_possible(&self, ty: Ty<'tcx>, mode: FallbackMode) -> bool {
        use rustc_middle::ty::error::UnconstrainedNumeric::Neither;
        use rustc_middle::ty::error::UnconstrainedNumeric::{UnconstrainedFloat, UnconstrainedInt};

        assert!(ty.is_ty_infer());
        let fallback = match self.type_is_unconstrained_numeric(ty) {
            _ if self.is_tainted_by_errors() => self.tcx().ty_error(),
            UnconstrainedInt => self.tcx.types.i32,
            UnconstrainedFloat => self.tcx.types.f64,
            Neither if self.type_var_diverges(ty) => self.tcx.mk_diverging_default(),
            Neither => {
                // This type variable was created from the instantiation of an opaque
                // type. The fact that we're attempting to perform fallback for it
                // means that the function neither constrained it to a concrete
                // type, nor to the opaque type itself.
                //
                // For example, in this code:
                //
                //```
                // type MyType = impl Copy;
                // fn defining_use() -> MyType { true }
                // fn other_use() -> MyType { defining_use() }
                // ```
                //
                // `defining_use` will constrain the instantiated inference
                // variable to `bool`, while `other_use` will constrain
                // the instantiated inference variable to `MyType`.
                //
                // When we process opaque types during writeback, we
                // will handle cases like `other_use`, and not count
                // them as defining usages
                //
                // However, we also need to handle cases like this:
                //
                // ```rust
                // pub type Foo = impl Copy;
                // fn produce() -> Option<Foo> {
                //     None
                //  }
                //  ```
                //
                // In the above snippet, the inference variable created by
                // instantiating `Option<Foo>` will be completely unconstrained.
                // We treat this as a non-defining use by making the inference
                // variable fall back to the opaque type itself.
                if let FallbackMode::All = mode {
                    if let Some(opaque_ty) = self.opaque_types_vars.borrow().get(ty) {
                        debug!(
                            "fallback_if_possible: falling back opaque type var {:?} to {:?}",
                            ty, opaque_ty
                        );
                        *opaque_ty
                    } else {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        };
        debug!("fallback_if_possible: defaulting `{:?}` to `{:?}`", ty, fallback);
        self.demand_eqtype(rustc_span::DUMMY_SP, ty, fallback);
        true
    }

    pub(super) fn select_all_obligations_or_error(&self) {
        debug!("select_all_obligations_or_error");
        if let Err(errors) = self.fulfillment_cx.borrow_mut().select_all_or_error(&self) {
            self.report_fulfillment_errors(&errors, self.inh.body_id, false);
        }
    }

    /// Select as many obligations as we can at present.
    pub(super) fn select_obligations_where_possible(
        &self,
        fallback_has_occurred: bool,
        mutate_fullfillment_errors: impl Fn(&mut Vec<traits::FulfillmentError<'tcx>>),
    ) {
        let result = self.fulfillment_cx.borrow_mut().select_where_possible(self);
        if let Err(mut errors) = result {
            mutate_fullfillment_errors(&mut errors);
            self.report_fulfillment_errors(&errors, self.inh.body_id, fallback_has_occurred);
        }
    }

    /// For the overloaded place expressions (`*x`, `x[3]`), the trait
    /// returns a type of `&T`, but the actual type we assign to the
    /// *expression* is `T`. So this function just peels off the return
    /// type by one layer to yield `T`.
    pub(super) fn make_overloaded_place_return_type(
        &self,
        method: MethodCallee<'tcx>,
    ) -> ty::TypeAndMut<'tcx> {
        // extract method return type, which will be &T;
        let ret_ty = method.sig.output();

        // method returns &T, but the type as visible to user is T, so deref
        ret_ty.builtin_deref(true).unwrap()
    }

    pub(super) fn check_method_argument_types(
        &self,
        sp: Span,
        expr: &'tcx hir::Expr<'tcx>,
        method: Result<MethodCallee<'tcx>, ()>,
        args_no_rcvr: &'tcx [hir::Expr<'tcx>],
        tuple_arguments: TupleArgumentsFlag,
        expected: Expectation<'tcx>,
    ) -> Ty<'tcx> {
        let has_error = match method {
            Ok(method) => method.substs.references_error() || method.sig.references_error(),
            Err(_) => true,
        };
        if has_error {
            let err_inputs = self.err_args(args_no_rcvr.len());

            let err_inputs = match tuple_arguments {
                DontTupleArguments => err_inputs,
                TupleArguments => vec![self.tcx.intern_tup(&err_inputs[..])],
            };

            self.check_argument_types(
                sp,
                expr,
                &err_inputs[..],
                &[],
                args_no_rcvr,
                false,
                tuple_arguments,
                None,
            );
            return self.tcx.ty_error();
        }

        let method = method.unwrap();
        // HACK(eddyb) ignore self in the definition (see above).
        let expected_arg_tys = self.expected_inputs_for_expected_output(
            sp,
            expected,
            method.sig.output(),
            &method.sig.inputs()[1..],
        );
        self.check_argument_types(
            sp,
            expr,
            &method.sig.inputs()[1..],
            &expected_arg_tys[..],
            args_no_rcvr,
            method.sig.c_variadic,
            tuple_arguments,
            self.tcx.hir().span_if_local(method.def_id),
        );
        method.sig.output()
    }

    fn self_type_matches_expected_vid(
        &self,
        trait_ref: ty::PolyTraitRef<'tcx>,
        expected_vid: ty::TyVid,
    ) -> bool {
        let self_ty = self.shallow_resolve(trait_ref.skip_binder().self_ty());
        debug!(
            "self_type_matches_expected_vid(trait_ref={:?}, self_ty={:?}, expected_vid={:?})",
            trait_ref, self_ty, expected_vid
        );
        match *self_ty.kind() {
            ty::Infer(ty::TyVar(found_vid)) => {
                // FIXME: consider using `sub_root_var` here so we
                // can see through subtyping.
                let found_vid = self.root_var(found_vid);
                debug!("self_type_matches_expected_vid - found_vid={:?}", found_vid);
                expected_vid == found_vid
            }
            _ => false,
        }
    }

    pub(super) fn obligations_for_self_ty<'b>(
        &'b self,
        self_ty: ty::TyVid,
    ) -> impl Iterator<Item = (ty::PolyTraitRef<'tcx>, traits::PredicateObligation<'tcx>)>
    + Captures<'tcx>
    + 'b {
        // FIXME: consider using `sub_root_var` here so we
        // can see through subtyping.
        let ty_var_root = self.root_var(self_ty);
        debug!(
            "obligations_for_self_ty: self_ty={:?} ty_var_root={:?} pending_obligations={:?}",
            self_ty,
            ty_var_root,
            self.fulfillment_cx.borrow().pending_obligations()
        );

        self.fulfillment_cx
            .borrow()
            .pending_obligations()
            .into_iter()
            .filter_map(move |obligation| {
                match obligation.predicate.skip_binders() {
                    ty::PredicateAtom::Projection(data) => {
                        Some((ty::Binder::bind(data).to_poly_trait_ref(self.tcx), obligation))
                    }
                    ty::PredicateAtom::Trait(data, _) => {
                        Some((ty::Binder::bind(data).to_poly_trait_ref(), obligation))
                    }
                    ty::PredicateAtom::Subtype(..) => None,
                    ty::PredicateAtom::RegionOutlives(..) => None,
                    ty::PredicateAtom::TypeOutlives(..) => None,
                    ty::PredicateAtom::WellFormed(..) => None,
                    ty::PredicateAtom::ObjectSafe(..) => None,
                    ty::PredicateAtom::ConstEvaluatable(..) => None,
                    ty::PredicateAtom::ConstEquate(..) => None,
                    // N.B., this predicate is created by breaking down a
                    // `ClosureType: FnFoo()` predicate, where
                    // `ClosureType` represents some `Closure`. It can't
                    // possibly be referring to the current closure,
                    // because we haven't produced the `Closure` for
                    // this closure yet; this is exactly why the other
                    // code is looking for a self type of a unresolved
                    // inference variable.
                    ty::PredicateAtom::ClosureKind(..) => None,
                    ty::PredicateAtom::TypeWellFormedFromEnv(..) => None,
                }
            })
            .filter(move |(tr, _)| self.self_type_matches_expected_vid(*tr, ty_var_root))
    }

    pub(super) fn type_var_is_sized(&self, self_ty: ty::TyVid) -> bool {
        self.obligations_for_self_ty(self_ty)
            .any(|(tr, _)| Some(tr.def_id()) == self.tcx.lang_items().sized_trait())
    }

    /// Generic function that factors out common logic from function calls,
    /// method calls and overloaded operators.
    pub(super) fn check_argument_types(
        &self,
        sp: Span,
        expr: &'tcx hir::Expr<'tcx>,
        fn_inputs: &[Ty<'tcx>],
        expected_arg_tys: &[Ty<'tcx>],
        args: &'tcx [hir::Expr<'tcx>],
        c_variadic: bool,
        tuple_arguments: TupleArgumentsFlag,
        def_span: Option<Span>,
    ) {
        let tcx = self.tcx;
        // Grab the argument types, supplying fresh type variables
        // if the wrong number of arguments were supplied
        let supplied_arg_count = if tuple_arguments == DontTupleArguments { args.len() } else { 1 };

        // All the input types from the fn signature must outlive the call
        // so as to validate implied bounds.
        for (&fn_input_ty, arg_expr) in fn_inputs.iter().zip(args.iter()) {
            self.register_wf_obligation(fn_input_ty.into(), arg_expr.span, traits::MiscObligation);
        }

        let expected_arg_count = fn_inputs.len();

        let param_count_error = |expected_count: usize,
                                 arg_count: usize,
                                 error_code: &str,
                                 c_variadic: bool,
                                 sugg_unit: bool| {
            let (span, start_span, args) = match &expr.kind {
                hir::ExprKind::Call(hir::Expr { span, .. }, args) => (*span, *span, &args[..]),
                hir::ExprKind::MethodCall(path_segment, span, args, _) => (
                    *span,
                    // `sp` doesn't point at the whole `foo.bar()`, only at `bar`.
                    path_segment
                        .args
                        .and_then(|args| args.args.iter().last())
                        // Account for `foo.bar::<T>()`.
                        .map(|arg| {
                            // Skip the closing `>`.
                            tcx.sess
                                .source_map()
                                .next_point(tcx.sess.source_map().next_point(arg.span()))
                        })
                        .unwrap_or(*span),
                    &args[1..], // Skip the receiver.
                ),
                k => span_bug!(sp, "checking argument types on a non-call: `{:?}`", k),
            };
            let arg_spans = if args.is_empty() {
                // foo()
                // ^^^-- supplied 0 arguments
                // |
                // expected 2 arguments
                vec![tcx.sess.source_map().next_point(start_span).with_hi(sp.hi())]
            } else {
                // foo(1, 2, 3)
                // ^^^ -  -  - supplied 3 arguments
                // |
                // expected 2 arguments
                args.iter().map(|arg| arg.span).collect::<Vec<Span>>()
            };

            let mut err = tcx.sess.struct_span_err_with_code(
                span,
                &format!(
                    "this function takes {}{} but {} {} supplied",
                    if c_variadic { "at least " } else { "" },
                    potentially_plural_count(expected_count, "argument"),
                    potentially_plural_count(arg_count, "argument"),
                    if arg_count == 1 { "was" } else { "were" }
                ),
                DiagnosticId::Error(error_code.to_owned()),
            );
            let label = format!("supplied {}", potentially_plural_count(arg_count, "argument"));
            for (i, span) in arg_spans.into_iter().enumerate() {
                err.span_label(
                    span,
                    if arg_count == 0 || i + 1 == arg_count { &label } else { "" },
                );
            }

            if let Some(def_s) = def_span.map(|sp| tcx.sess.source_map().guess_head_span(sp)) {
                err.span_label(def_s, "defined here");
            }
            if sugg_unit {
                let sugg_span = tcx.sess.source_map().end_point(expr.span);
                // remove closing `)` from the span
                let sugg_span = sugg_span.shrink_to_lo();
                err.span_suggestion(
                    sugg_span,
                    "expected the unit value `()`; create it with empty parentheses",
                    String::from("()"),
                    Applicability::MachineApplicable,
                );
            } else {
                err.span_label(
                    span,
                    format!(
                        "expected {}{}",
                        if c_variadic { "at least " } else { "" },
                        potentially_plural_count(expected_count, "argument")
                    ),
                );
            }
            err.emit();
        };

        let mut expected_arg_tys = expected_arg_tys.to_vec();

        let formal_tys = if tuple_arguments == TupleArguments {
            let tuple_type = self.structurally_resolved_type(sp, fn_inputs[0]);
            match tuple_type.kind() {
                ty::Tuple(arg_types) if arg_types.len() != args.len() => {
                    param_count_error(arg_types.len(), args.len(), "E0057", false, false);
                    expected_arg_tys = vec![];
                    self.err_args(args.len())
                }
                ty::Tuple(arg_types) => {
                    expected_arg_tys = match expected_arg_tys.get(0) {
                        Some(&ty) => match ty.kind() {
                            ty::Tuple(ref tys) => tys.iter().map(|k| k.expect_ty()).collect(),
                            _ => vec![],
                        },
                        None => vec![],
                    };
                    arg_types.iter().map(|k| k.expect_ty()).collect()
                }
                _ => {
                    struct_span_err!(
                        tcx.sess,
                        sp,
                        E0059,
                        "cannot use call notation; the first type parameter \
                         for the function trait is neither a tuple nor unit"
                    )
                    .emit();
                    expected_arg_tys = vec![];
                    self.err_args(args.len())
                }
            }
        } else if expected_arg_count == supplied_arg_count {
            fn_inputs.to_vec()
        } else if c_variadic {
            if supplied_arg_count >= expected_arg_count {
                fn_inputs.to_vec()
            } else {
                param_count_error(expected_arg_count, supplied_arg_count, "E0060", true, false);
                expected_arg_tys = vec![];
                self.err_args(supplied_arg_count)
            }
        } else {
            // is the missing argument of type `()`?
            let sugg_unit = if expected_arg_tys.len() == 1 && supplied_arg_count == 0 {
                self.resolve_vars_if_possible(&expected_arg_tys[0]).is_unit()
            } else if fn_inputs.len() == 1 && supplied_arg_count == 0 {
                self.resolve_vars_if_possible(&fn_inputs[0]).is_unit()
            } else {
                false
            };
            param_count_error(expected_arg_count, supplied_arg_count, "E0061", false, sugg_unit);

            expected_arg_tys = vec![];
            self.err_args(supplied_arg_count)
        };

        debug!(
            "check_argument_types: formal_tys={:?}",
            formal_tys.iter().map(|t| self.ty_to_string(*t)).collect::<Vec<String>>()
        );

        // If there is no expectation, expect formal_tys.
        let expected_arg_tys =
            if !expected_arg_tys.is_empty() { expected_arg_tys } else { formal_tys.clone() };

        let mut final_arg_types: Vec<(usize, Ty<'_>, Ty<'_>)> = vec![];

        // Check the arguments.
        // We do this in a pretty awful way: first we type-check any arguments
        // that are not closures, then we type-check the closures. This is so
        // that we have more information about the types of arguments when we
        // type-check the functions. This isn't really the right way to do this.
        for &check_closures in &[false, true] {
            debug!("check_closures={}", check_closures);

            // More awful hacks: before we check argument types, try to do
            // an "opportunistic" trait resolution of any trait bounds on
            // the call. This helps coercions.
            if check_closures {
                self.select_obligations_where_possible(false, |errors| {
                    self.point_at_type_arg_instead_of_call_if_possible(errors, expr);
                    self.point_at_arg_instead_of_call_if_possible(
                        errors,
                        &final_arg_types[..],
                        sp,
                        &args,
                    );
                })
            }

            // For C-variadic functions, we don't have a declared type for all of
            // the arguments hence we only do our usual type checking with
            // the arguments who's types we do know.
            let t = if c_variadic {
                expected_arg_count
            } else if tuple_arguments == TupleArguments {
                args.len()
            } else {
                supplied_arg_count
            };
            for (i, arg) in args.iter().take(t).enumerate() {
                // Warn only for the first loop (the "no closures" one).
                // Closure arguments themselves can't be diverging, but
                // a previous argument can, e.g., `foo(panic!(), || {})`.
                if !check_closures {
                    self.warn_if_unreachable(arg.hir_id, arg.span, "expression");
                }

                let is_closure = match arg.kind {
                    ExprKind::Closure(..) => true,
                    _ => false,
                };

                if is_closure != check_closures {
                    continue;
                }

                debug!("checking the argument");
                let formal_ty = formal_tys[i];

                // The special-cased logic below has three functions:
                // 1. Provide as good of an expected type as possible.
                let expected = Expectation::rvalue_hint(self, expected_arg_tys[i]);

                let checked_ty = self.check_expr_with_expectation(&arg, expected);

                // 2. Coerce to the most detailed type that could be coerced
                //    to, which is `expected_ty` if `rvalue_hint` returns an
                //    `ExpectHasType(expected_ty)`, or the `formal_ty` otherwise.
                let coerce_ty = expected.only_has_type(self).unwrap_or(formal_ty);
                // We're processing function arguments so we definitely want to use
                // two-phase borrows.
                self.demand_coerce(&arg, checked_ty, coerce_ty, None, AllowTwoPhase::Yes);
                final_arg_types.push((i, checked_ty, coerce_ty));

                // 3. Relate the expected type and the formal one,
                //    if the expected type was used for the coercion.
                self.demand_suptype(arg.span, formal_ty, coerce_ty);
            }
        }

        // We also need to make sure we at least write the ty of the other
        // arguments which we skipped above.
        if c_variadic {
            fn variadic_error<'tcx>(s: &Session, span: Span, t: Ty<'tcx>, cast_ty: &str) {
                use crate::structured_errors::{StructuredDiagnostic, VariadicError};
                VariadicError::new(s, span, t, cast_ty).diagnostic().emit();
            }

            for arg in args.iter().skip(expected_arg_count) {
                let arg_ty = self.check_expr(&arg);

                // There are a few types which get autopromoted when passed via varargs
                // in C but we just error out instead and require explicit casts.
                let arg_ty = self.structurally_resolved_type(arg.span, arg_ty);
                match arg_ty.kind() {
                    ty::Float(ast::FloatTy::F32) => {
                        variadic_error(tcx.sess, arg.span, arg_ty, "c_double");
                    }
                    ty::Int(ast::IntTy::I8 | ast::IntTy::I16) | ty::Bool => {
                        variadic_error(tcx.sess, arg.span, arg_ty, "c_int");
                    }
                    ty::Uint(ast::UintTy::U8 | ast::UintTy::U16) => {
                        variadic_error(tcx.sess, arg.span, arg_ty, "c_uint");
                    }
                    ty::FnDef(..) => {
                        let ptr_ty = self.tcx.mk_fn_ptr(arg_ty.fn_sig(self.tcx));
                        let ptr_ty = self.resolve_vars_if_possible(&ptr_ty);
                        variadic_error(tcx.sess, arg.span, arg_ty, &ptr_ty.to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    pub(super) fn err_args(&self, len: usize) -> Vec<Ty<'tcx>> {
        vec![self.tcx.ty_error(); len]
    }

    /// Given a vec of evaluated `FulfillmentError`s and an `fn` call argument expressions, we walk
    /// the checked and coerced types for each argument to see if any of the `FulfillmentError`s
    /// reference a type argument. The reason to walk also the checked type is that the coerced type
    /// can be not easily comparable with predicate type (because of coercion). If the types match
    /// for either checked or coerced type, and there's only *one* argument that does, we point at
    /// the corresponding argument's expression span instead of the `fn` call path span.
    fn point_at_arg_instead_of_call_if_possible(
        &self,
        errors: &mut Vec<traits::FulfillmentError<'tcx>>,
        final_arg_types: &[(usize, Ty<'tcx>, Ty<'tcx>)],
        call_sp: Span,
        args: &'tcx [hir::Expr<'tcx>],
    ) {
        // We *do not* do this for desugared call spans to keep good diagnostics when involving
        // the `?` operator.
        if call_sp.desugaring_kind().is_some() {
            return;
        }

        for error in errors {
            // Only if the cause is somewhere inside the expression we want try to point at arg.
            // Otherwise, it means that the cause is somewhere else and we should not change
            // anything because we can break the correct span.
            if !call_sp.contains(error.obligation.cause.span) {
                continue;
            }

            if let ty::PredicateAtom::Trait(predicate, _) =
                error.obligation.predicate.skip_binders()
            {
                // Collect the argument position for all arguments that could have caused this
                // `FulfillmentError`.
                let mut referenced_in = final_arg_types
                    .iter()
                    .map(|&(i, checked_ty, _)| (i, checked_ty))
                    .chain(final_arg_types.iter().map(|&(i, _, coerced_ty)| (i, coerced_ty)))
                    .flat_map(|(i, ty)| {
                        let ty = self.resolve_vars_if_possible(&ty);
                        // We walk the argument type because the argument's type could have
                        // been `Option<T>`, but the `FulfillmentError` references `T`.
                        if ty.walk().any(|arg| arg == predicate.self_ty().into()) {
                            Some(i)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<usize>>();

                // Both checked and coerced types could have matched, thus we need to remove
                // duplicates.

                // We sort primitive type usize here and can use unstable sort
                referenced_in.sort_unstable();
                referenced_in.dedup();

                if let (Some(ref_in), None) = (referenced_in.pop(), referenced_in.pop()) {
                    // We make sure that only *one* argument matches the obligation failure
                    // and we assign the obligation's span to its expression's.
                    error.obligation.cause.make_mut().span = args[ref_in].span;
                    error.points_at_arg_span = true;
                }
            }
        }
    }

    /// Given a vec of evaluated `FulfillmentError`s and an `fn` call expression, we walk the
    /// `PathSegment`s and resolve their type parameters to see if any of the `FulfillmentError`s
    /// were caused by them. If they were, we point at the corresponding type argument's span
    /// instead of the `fn` call path span.
    fn point_at_type_arg_instead_of_call_if_possible(
        &self,
        errors: &mut Vec<traits::FulfillmentError<'tcx>>,
        call_expr: &'tcx hir::Expr<'tcx>,
    ) {
        if let hir::ExprKind::Call(path, _) = &call_expr.kind {
            if let hir::ExprKind::Path(qpath) = &path.kind {
                if let hir::QPath::Resolved(_, path) = &qpath {
                    for error in errors {
                        if let ty::PredicateAtom::Trait(predicate, _) =
                            error.obligation.predicate.skip_binders()
                        {
                            // If any of the type arguments in this path segment caused the
                            // `FullfillmentError`, point at its span (#61860).
                            for arg in path
                                .segments
                                .iter()
                                .filter_map(|seg| seg.args.as_ref())
                                .flat_map(|a| a.args.iter())
                            {
                                if let hir::GenericArg::Type(hir_ty) = &arg {
                                    if let hir::TyKind::Path(hir::QPath::TypeRelative(..)) =
                                        &hir_ty.kind
                                    {
                                        // Avoid ICE with associated types. As this is best
                                        // effort only, it's ok to ignore the case. It
                                        // would trigger in `is_send::<T::AssocType>();`
                                        // from `typeck-default-trait-impl-assoc-type.rs`.
                                    } else {
                                        let ty = AstConv::ast_ty_to_ty(self, hir_ty);
                                        let ty = self.resolve_vars_if_possible(&ty);
                                        if ty == predicate.self_ty() {
                                            error.obligation.cause.make_mut().span = hir_ty.span;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // AST fragment checking
    pub(super) fn check_lit(&self, lit: &hir::Lit, expected: Expectation<'tcx>) -> Ty<'tcx> {
        let tcx = self.tcx;

        match lit.node {
            ast::LitKind::Str(..) => tcx.mk_static_str(),
            ast::LitKind::ByteStr(ref v) => {
                tcx.mk_imm_ref(tcx.lifetimes.re_static, tcx.mk_array(tcx.types.u8, v.len() as u64))
            }
            ast::LitKind::Byte(_) => tcx.types.u8,
            ast::LitKind::Char(_) => tcx.types.char,
            ast::LitKind::Int(_, ast::LitIntType::Signed(t)) => tcx.mk_mach_int(t),
            ast::LitKind::Int(_, ast::LitIntType::Unsigned(t)) => tcx.mk_mach_uint(t),
            ast::LitKind::Int(_, ast::LitIntType::Unsuffixed) => {
                let opt_ty = expected.to_option(self).and_then(|ty| match ty.kind() {
                    ty::Int(_) | ty::Uint(_) => Some(ty),
                    ty::Char => Some(tcx.types.u8),
                    ty::RawPtr(..) => Some(tcx.types.usize),
                    ty::FnDef(..) | ty::FnPtr(_) => Some(tcx.types.usize),
                    _ => None,
                });
                opt_ty.unwrap_or_else(|| self.next_int_var())
            }
            ast::LitKind::Float(_, ast::LitFloatType::Suffixed(t)) => tcx.mk_mach_float(t),
            ast::LitKind::Float(_, ast::LitFloatType::Unsuffixed) => {
                let opt_ty = expected.to_option(self).and_then(|ty| match ty.kind() {
                    ty::Float(_) => Some(ty),
                    _ => None,
                });
                opt_ty.unwrap_or_else(|| self.next_float_var())
            }
            ast::LitKind::Bool(_) => tcx.types.bool,
            ast::LitKind::Err(_) => tcx.ty_error(),
        }
    }

    /// Unifies the output type with the expected type early, for more coercions
    /// and forward type information on the input expressions.
    pub(super) fn expected_inputs_for_expected_output(
        &self,
        call_span: Span,
        expected_ret: Expectation<'tcx>,
        formal_ret: Ty<'tcx>,
        formal_args: &[Ty<'tcx>],
    ) -> Vec<Ty<'tcx>> {
        let formal_ret = self.resolve_vars_with_obligations(formal_ret);
        let ret_ty = match expected_ret.only_has_type(self) {
            Some(ret) => ret,
            None => return Vec::new(),
        };
        let expect_args = self
            .fudge_inference_if_ok(|| {
                // Attempt to apply a subtyping relationship between the formal
                // return type (likely containing type variables if the function
                // is polymorphic) and the expected return type.
                // No argument expectations are produced if unification fails.
                let origin = self.misc(call_span);
                let ures = self.at(&origin, self.param_env).sup(ret_ty, &formal_ret);

                // FIXME(#27336) can't use ? here, Try::from_error doesn't default
                // to identity so the resulting type is not constrained.
                match ures {
                    Ok(ok) => {
                        // Process any obligations locally as much as
                        // we can.  We don't care if some things turn
                        // out unconstrained or ambiguous, as we're
                        // just trying to get hints here.
                        self.save_and_restore_in_snapshot_flag(|_| {
                            let mut fulfill = TraitEngine::new(self.tcx);
                            for obligation in ok.obligations {
                                fulfill.register_predicate_obligation(self, obligation);
                            }
                            fulfill.select_where_possible(self)
                        })
                        .map_err(|_| ())?;
                    }
                    Err(_) => return Err(()),
                }

                // Record all the argument types, with the substitutions
                // produced from the above subtyping unification.
                Ok(formal_args.iter().map(|ty| self.resolve_vars_if_possible(ty)).collect())
            })
            .unwrap_or_default();
        debug!(
            "expected_inputs_for_expected_output(formal={:?} -> {:?}, expected={:?} -> {:?})",
            formal_args, formal_ret, expect_args, expected_ret
        );
        expect_args
    }

    pub fn check_struct_path(
        &self,
        qpath: &QPath<'_>,
        hir_id: hir::HirId,
    ) -> Option<(&'tcx ty::VariantDef, Ty<'tcx>)> {
        let path_span = qpath.qself_span();
        let (def, ty) = self.finish_resolving_struct_path(qpath, path_span, hir_id);
        let variant = match def {
            Res::Err => {
                self.set_tainted_by_errors();
                return None;
            }
            Res::Def(DefKind::Variant, _) => match ty.kind() {
                ty::Adt(adt, substs) => Some((adt.variant_of_res(def), adt.did, substs)),
                _ => bug!("unexpected type: {:?}", ty),
            },
            Res::Def(DefKind::Struct | DefKind::Union | DefKind::TyAlias | DefKind::AssocTy, _)
            | Res::SelfTy(..) => match ty.kind() {
                ty::Adt(adt, substs) if !adt.is_enum() => {
                    Some((adt.non_enum_variant(), adt.did, substs))
                }
                _ => None,
            },
            _ => bug!("unexpected definition: {:?}", def),
        };

        if let Some((variant, did, substs)) = variant {
            debug!("check_struct_path: did={:?} substs={:?}", did, substs);
            self.write_user_type_annotation_from_substs(hir_id, did, substs, None);

            // Check bounds on type arguments used in the path.
            let (bounds, _) = self.instantiate_bounds(path_span, did, substs);
            let cause =
                traits::ObligationCause::new(path_span, self.body_id, traits::ItemObligation(did));
            self.add_obligations_for_parameters(cause, bounds);

            Some((variant, ty))
        } else {
            struct_span_err!(
                self.tcx.sess,
                path_span,
                E0071,
                "expected struct, variant or union type, found {}",
                ty.sort_string(self.tcx)
            )
            .span_label(path_span, "not a struct")
            .emit();
            None
        }
    }

    // Finish resolving a path in a struct expression or pattern `S::A { .. }` if necessary.
    // The newly resolved definition is written into `type_dependent_defs`.
    fn finish_resolving_struct_path(
        &self,
        qpath: &QPath<'_>,
        path_span: Span,
        hir_id: hir::HirId,
    ) -> (Res, Ty<'tcx>) {
        match *qpath {
            QPath::Resolved(ref maybe_qself, ref path) => {
                let self_ty = maybe_qself.as_ref().map(|qself| self.to_ty(qself));
                let ty = AstConv::res_to_ty(self, self_ty, path, true);
                (path.res, ty)
            }
            QPath::TypeRelative(ref qself, ref segment) => {
                let ty = self.to_ty(qself);

                let res = if let hir::TyKind::Path(QPath::Resolved(_, ref path)) = qself.kind {
                    path.res
                } else {
                    Res::Err
                };
                let result =
                    AstConv::associated_path_to_ty(self, hir_id, path_span, ty, res, segment, true);
                let ty = result.map(|(ty, _, _)| ty).unwrap_or_else(|_| self.tcx().ty_error());
                let result = result.map(|(_, kind, def_id)| (kind, def_id));

                // Write back the new resolution.
                self.write_resolution(hir_id, result);

                (result.map(|(kind, def_id)| Res::Def(kind, def_id)).unwrap_or(Res::Err), ty)
            }
            QPath::LangItem(lang_item, span) => {
                self.resolve_lang_item_path(lang_item, span, hir_id)
            }
        }
    }

    pub(super) fn resolve_lang_item_path(
        &self,
        lang_item: hir::LangItem,
        span: Span,
        hir_id: hir::HirId,
    ) -> (Res, Ty<'tcx>) {
        let def_id = self.tcx.require_lang_item(lang_item, Some(span));
        let def_kind = self.tcx.def_kind(def_id);

        let item_ty = if let DefKind::Variant = def_kind {
            self.tcx.type_of(self.tcx.parent(def_id).expect("variant w/out parent"))
        } else {
            self.tcx.type_of(def_id)
        };
        let substs = self.infcx.fresh_substs_for_item(span, def_id);
        let ty = item_ty.subst(self.tcx, substs);

        self.write_resolution(hir_id, Ok((def_kind, def_id)));
        self.add_required_obligations(span, def_id, &substs);
        (Res::Def(def_kind, def_id), ty)
    }

    /// Resolves an associated value path into a base type and associated constant, or method
    /// resolution. The newly resolved definition is written into `type_dependent_defs`.
    pub fn resolve_ty_and_res_ufcs<'b>(
        &self,
        qpath: &'b QPath<'b>,
        hir_id: hir::HirId,
        span: Span,
    ) -> (Res, Option<Ty<'tcx>>, &'b [hir::PathSegment<'b>]) {
        debug!("resolve_ty_and_res_ufcs: qpath={:?} hir_id={:?} span={:?}", qpath, hir_id, span);
        let (ty, qself, item_segment) = match *qpath {
            QPath::Resolved(ref opt_qself, ref path) => {
                return (
                    path.res,
                    opt_qself.as_ref().map(|qself| self.to_ty(qself)),
                    &path.segments[..],
                );
            }
            QPath::TypeRelative(ref qself, ref segment) => (self.to_ty(qself), qself, segment),
            QPath::LangItem(..) => bug!("`resolve_ty_and_res_ufcs` called on `LangItem`"),
        };
        if let Some(&cached_result) = self.typeck_results.borrow().type_dependent_defs().get(hir_id)
        {
            // Return directly on cache hit. This is useful to avoid doubly reporting
            // errors with default match binding modes. See #44614.
            let def =
                cached_result.map(|(kind, def_id)| Res::Def(kind, def_id)).unwrap_or(Res::Err);
            return (def, Some(ty), slice::from_ref(&**item_segment));
        }
        let item_name = item_segment.ident;
        let result = self.resolve_ufcs(span, item_name, ty, hir_id).or_else(|error| {
            let result = match error {
                method::MethodError::PrivateMatch(kind, def_id, _) => Ok((kind, def_id)),
                _ => Err(ErrorReported),
            };
            if item_name.name != kw::Invalid {
                if let Some(mut e) = self.report_method_error(
                    span,
                    ty,
                    item_name,
                    SelfSource::QPath(qself),
                    error,
                    None,
                ) {
                    e.emit();
                }
            }
            result
        });

        // Write back the new resolution.
        self.write_resolution(hir_id, result);
        (
            result.map(|(kind, def_id)| Res::Def(kind, def_id)).unwrap_or(Res::Err),
            Some(ty),
            slice::from_ref(&**item_segment),
        )
    }

    pub fn check_decl_initializer(
        &self,
        local: &'tcx hir::Local<'tcx>,
        init: &'tcx hir::Expr<'tcx>,
    ) -> Ty<'tcx> {
        // FIXME(tschottdorf): `contains_explicit_ref_binding()` must be removed
        // for #42640 (default match binding modes).
        //
        // See #44848.
        let ref_bindings = local.pat.contains_explicit_ref_binding();

        let local_ty = self.local_ty(init.span, local.hir_id).revealed_ty;
        if let Some(m) = ref_bindings {
            // Somewhat subtle: if we have a `ref` binding in the pattern,
            // we want to avoid introducing coercions for the RHS. This is
            // both because it helps preserve sanity and, in the case of
            // ref mut, for soundness (issue #23116). In particular, in
            // the latter case, we need to be clear that the type of the
            // referent for the reference that results is *equal to* the
            // type of the place it is referencing, and not some
            // supertype thereof.
            let init_ty = self.check_expr_with_needs(init, Needs::maybe_mut_place(m));
            self.demand_eqtype(init.span, local_ty, init_ty);
            init_ty
        } else {
            self.check_expr_coercable_to_type(init, local_ty, None)
        }
    }

    /// Type check a `let` statement.
    pub fn check_decl_local(&self, local: &'tcx hir::Local<'tcx>) {
        // Determine and write the type which we'll check the pattern against.
        let ty = self.local_ty(local.span, local.hir_id).decl_ty;
        self.write_ty(local.hir_id, ty);

        // Type check the initializer.
        if let Some(ref init) = local.init {
            let init_ty = self.check_decl_initializer(local, &init);
            self.overwrite_local_ty_if_err(local, ty, init_ty);
        }

        // Does the expected pattern type originate from an expression and what is the span?
        let (origin_expr, ty_span) = match (local.ty, local.init) {
            (Some(ty), _) => (false, Some(ty.span)), // Bias towards the explicit user type.
            (_, Some(init)) => (true, Some(init.span)), // No explicit type; so use the scrutinee.
            _ => (false, None), // We have `let $pat;`, so the expected type is unconstrained.
        };

        // Type check the pattern. Override if necessary to avoid knock-on errors.
        self.check_pat_top(&local.pat, ty, ty_span, origin_expr);
        let pat_ty = self.node_ty(local.pat.hir_id);
        self.overwrite_local_ty_if_err(local, ty, pat_ty);
    }

    fn overwrite_local_ty_if_err(
        &self,
        local: &'tcx hir::Local<'tcx>,
        decl_ty: Ty<'tcx>,
        ty: Ty<'tcx>,
    ) {
        if ty.references_error() {
            // Override the types everywhere with `err()` to avoid knock on errors.
            self.write_ty(local.hir_id, ty);
            self.write_ty(local.pat.hir_id, ty);
            let local_ty = LocalTy { decl_ty, revealed_ty: ty };
            self.locals.borrow_mut().insert(local.hir_id, local_ty);
            self.locals.borrow_mut().insert(local.pat.hir_id, local_ty);
        }
    }

    pub fn check_stmt(&self, stmt: &'tcx hir::Stmt<'tcx>) {
        // Don't do all the complex logic below for `DeclItem`.
        match stmt.kind {
            hir::StmtKind::Item(..) => return,
            hir::StmtKind::Local(..) | hir::StmtKind::Expr(..) | hir::StmtKind::Semi(..) => {}
        }

        self.warn_if_unreachable(stmt.hir_id, stmt.span, "statement");

        // Hide the outer diverging and `has_errors` flags.
        let old_diverges = self.diverges.replace(Diverges::Maybe);
        let old_has_errors = self.has_errors.replace(false);

        match stmt.kind {
            hir::StmtKind::Local(ref l) => {
                self.check_decl_local(&l);
            }
            // Ignore for now.
            hir::StmtKind::Item(_) => {}
            hir::StmtKind::Expr(ref expr) => {
                // Check with expected type of `()`.
                self.check_expr_has_type_or_error(&expr, self.tcx.mk_unit(), |err| {
                    self.suggest_semicolon_at_end(expr.span, err);
                });
            }
            hir::StmtKind::Semi(ref expr) => {
                self.check_expr(&expr);
            }
        }

        // Combine the diverging and `has_error` flags.
        self.diverges.set(self.diverges.get() | old_diverges);
        self.has_errors.set(self.has_errors.get() | old_has_errors);
    }

    pub fn check_block_no_value(&self, blk: &'tcx hir::Block<'tcx>) {
        let unit = self.tcx.mk_unit();
        let ty = self.check_block_with_expected(blk, ExpectHasType(unit));

        // if the block produces a `!` value, that can always be
        // (effectively) coerced to unit.
        if !ty.is_never() {
            self.demand_suptype(blk.span, unit, ty);
        }
    }

    /// If `expr` is a `match` expression that has only one non-`!` arm, use that arm's tail
    /// expression's `Span`, otherwise return `expr.span`. This is done to give better errors
    /// when given code like the following:
    /// ```text
    /// if false { return 0i32; } else { 1u32 }
    /// //                               ^^^^ point at this instead of the whole `if` expression
    /// ```
    fn get_expr_coercion_span(&self, expr: &hir::Expr<'_>) -> rustc_span::Span {
        if let hir::ExprKind::Match(_, arms, _) = &expr.kind {
            let arm_spans: Vec<Span> = arms
                .iter()
                .filter_map(|arm| {
                    self.in_progress_typeck_results
                        .and_then(|typeck_results| {
                            typeck_results.borrow().node_type_opt(arm.body.hir_id)
                        })
                        .and_then(|arm_ty| {
                            if arm_ty.is_never() {
                                None
                            } else {
                                Some(match &arm.body.kind {
                                    // Point at the tail expression when possible.
                                    hir::ExprKind::Block(block, _) => {
                                        block.expr.as_ref().map(|e| e.span).unwrap_or(block.span)
                                    }
                                    _ => arm.body.span,
                                })
                            }
                        })
                })
                .collect();
            if arm_spans.len() == 1 {
                return arm_spans[0];
            }
        }
        expr.span
    }

    pub(super) fn check_block_with_expected(
        &self,
        blk: &'tcx hir::Block<'tcx>,
        expected: Expectation<'tcx>,
    ) -> Ty<'tcx> {
        let prev = {
            let mut fcx_ps = self.ps.borrow_mut();
            let unsafety_state = fcx_ps.recurse(blk);
            replace(&mut *fcx_ps, unsafety_state)
        };

        // In some cases, blocks have just one exit, but other blocks
        // can be targeted by multiple breaks. This can happen both
        // with labeled blocks as well as when we desugar
        // a `try { ... }` expression.
        //
        // Example 1:
        //
        //    'a: { if true { break 'a Err(()); } Ok(()) }
        //
        // Here we would wind up with two coercions, one from
        // `Err(())` and the other from the tail expression
        // `Ok(())`. If the tail expression is omitted, that's a
        // "forced unit" -- unless the block diverges, in which
        // case we can ignore the tail expression (e.g., `'a: {
        // break 'a 22; }` would not force the type of the block
        // to be `()`).
        let tail_expr = blk.expr.as_ref();
        let coerce_to_ty = expected.coercion_target_type(self, blk.span);
        let coerce = if blk.targeted_by_break {
            CoerceMany::new(coerce_to_ty)
        } else {
            let tail_expr: &[&hir::Expr<'_>] = match tail_expr {
                Some(e) => slice::from_ref(e),
                None => &[],
            };
            CoerceMany::with_coercion_sites(coerce_to_ty, tail_expr)
        };

        let prev_diverges = self.diverges.get();
        let ctxt = BreakableCtxt { coerce: Some(coerce), may_break: false };

        let (ctxt, ()) = self.with_breakable_ctxt(blk.hir_id, ctxt, || {
            for s in blk.stmts {
                self.check_stmt(s);
            }

            // check the tail expression **without** holding the
            // `enclosing_breakables` lock below.
            let tail_expr_ty = tail_expr.map(|t| self.check_expr_with_expectation(t, expected));

            let mut enclosing_breakables = self.enclosing_breakables.borrow_mut();
            let ctxt = enclosing_breakables.find_breakable(blk.hir_id);
            let coerce = ctxt.coerce.as_mut().unwrap();
            if let Some(tail_expr_ty) = tail_expr_ty {
                let tail_expr = tail_expr.unwrap();
                let span = self.get_expr_coercion_span(tail_expr);
                let cause = self.cause(span, ObligationCauseCode::BlockTailExpression(blk.hir_id));
                coerce.coerce(self, &cause, tail_expr, tail_expr_ty);
            } else {
                // Subtle: if there is no explicit tail expression,
                // that is typically equivalent to a tail expression
                // of `()` -- except if the block diverges. In that
                // case, there is no value supplied from the tail
                // expression (assuming there are no other breaks,
                // this implies that the type of the block will be
                // `!`).
                //
                // #41425 -- label the implicit `()` as being the
                // "found type" here, rather than the "expected type".
                if !self.diverges.get().is_always() {
                    // #50009 -- Do not point at the entire fn block span, point at the return type
                    // span, as it is the cause of the requirement, and
                    // `consider_hint_about_removing_semicolon` will point at the last expression
                    // if it were a relevant part of the error. This improves usability in editors
                    // that highlight errors inline.
                    let mut sp = blk.span;
                    let mut fn_span = None;
                    if let Some((decl, ident)) = self.get_parent_fn_decl(blk.hir_id) {
                        let ret_sp = decl.output.span();
                        if let Some(block_sp) = self.parent_item_span(blk.hir_id) {
                            // HACK: on some cases (`ui/liveness/liveness-issue-2163.rs`) the
                            // output would otherwise be incorrect and even misleading. Make sure
                            // the span we're aiming at correspond to a `fn` body.
                            if block_sp == blk.span {
                                sp = ret_sp;
                                fn_span = Some(ident.span);
                            }
                        }
                    }
                    coerce.coerce_forced_unit(
                        self,
                        &self.misc(sp),
                        &mut |err| {
                            if let Some(expected_ty) = expected.only_has_type(self) {
                                self.consider_hint_about_removing_semicolon(blk, expected_ty, err);
                            }
                            if let Some(fn_span) = fn_span {
                                err.span_label(
                                    fn_span,
                                    "implicitly returns `()` as its body has no tail or `return` \
                                     expression",
                                );
                            }
                        },
                        false,
                    );
                }
            }
        });

        if ctxt.may_break {
            // If we can break from the block, then the block's exit is always reachable
            // (... as long as the entry is reachable) - regardless of the tail of the block.
            self.diverges.set(prev_diverges);
        }

        let mut ty = ctxt.coerce.unwrap().complete(self);

        if self.has_errors.get() || ty.references_error() {
            ty = self.tcx.ty_error()
        }

        self.write_ty(blk.hir_id, ty);

        *self.ps.borrow_mut() = prev;
        ty
    }

    fn parent_item_span(&self, id: hir::HirId) -> Option<Span> {
        let node = self.tcx.hir().get(self.tcx.hir().get_parent_item(id));
        match node {
            Node::Item(&hir::Item { kind: hir::ItemKind::Fn(_, _, body_id), .. })
            | Node::ImplItem(&hir::ImplItem { kind: hir::ImplItemKind::Fn(_, body_id), .. }) => {
                let body = self.tcx.hir().body(body_id);
                if let ExprKind::Block(block, _) = &body.value.kind {
                    return Some(block.span);
                }
            }
            _ => {}
        }
        None
    }

    /// Given a function block's `HirId`, returns its `FnDecl` if it exists, or `None` otherwise.
    fn get_parent_fn_decl(&self, blk_id: hir::HirId) -> Option<(&'tcx hir::FnDecl<'tcx>, Ident)> {
        let parent = self.tcx.hir().get(self.tcx.hir().get_parent_item(blk_id));
        self.get_node_fn_decl(parent).map(|(fn_decl, ident, _)| (fn_decl, ident))
    }

    /// Given a function `Node`, return its `FnDecl` if it exists, or `None` otherwise.
    pub(super) fn get_node_fn_decl(
        &self,
        node: Node<'tcx>,
    ) -> Option<(&'tcx hir::FnDecl<'tcx>, Ident, bool)> {
        match node {
            Node::Item(&hir::Item { ident, kind: hir::ItemKind::Fn(ref sig, ..), .. }) => {
                // This is less than ideal, it will not suggest a return type span on any
                // method called `main`, regardless of whether it is actually the entry point,
                // but it will still present it as the reason for the expected type.
                Some((&sig.decl, ident, ident.name != sym::main))
            }
            Node::TraitItem(&hir::TraitItem {
                ident,
                kind: hir::TraitItemKind::Fn(ref sig, ..),
                ..
            }) => Some((&sig.decl, ident, true)),
            Node::ImplItem(&hir::ImplItem {
                ident,
                kind: hir::ImplItemKind::Fn(ref sig, ..),
                ..
            }) => Some((&sig.decl, ident, false)),
            _ => None,
        }
    }

    /// Given a `HirId`, return the `FnDecl` of the method it is enclosed by and whether a
    /// suggestion can be made, `None` otherwise.
    pub fn get_fn_decl(&self, blk_id: hir::HirId) -> Option<(&'tcx hir::FnDecl<'tcx>, bool)> {
        // Get enclosing Fn, if it is a function or a trait method, unless there's a `loop` or
        // `while` before reaching it, as block tail returns are not available in them.
        self.tcx.hir().get_return_block(blk_id).and_then(|blk_id| {
            let parent = self.tcx.hir().get(blk_id);
            self.get_node_fn_decl(parent).map(|(fn_decl, _, is_main)| (fn_decl, is_main))
        })
    }

    pub(super) fn note_internal_mutation_in_method(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        expr: &hir::Expr<'_>,
        expected: Ty<'tcx>,
        found: Ty<'tcx>,
    ) {
        if found != self.tcx.types.unit {
            return;
        }
        if let ExprKind::MethodCall(path_segment, _, [rcvr, ..], _) = expr.kind {
            if self
                .typeck_results
                .borrow()
                .expr_ty_adjusted_opt(rcvr)
                .map_or(true, |ty| expected.peel_refs() != ty.peel_refs())
            {
                return;
            }
            let mut sp = MultiSpan::from_span(path_segment.ident.span);
            sp.push_span_label(
                path_segment.ident.span,
                format!(
                    "this call modifies {} in-place",
                    match rcvr.kind {
                        ExprKind::Path(QPath::Resolved(
                            None,
                            hir::Path { segments: [segment], .. },
                        )) => format!("`{}`", segment.ident),
                        _ => "its receiver".to_string(),
                    }
                ),
            );
            sp.push_span_label(
                rcvr.span,
                "you probably want to use this value after calling the method...".to_string(),
            );
            err.span_note(
                sp,
                &format!("method `{}` modifies its receiver in-place", path_segment.ident),
            );
            err.note(&format!("...instead of the `()` output of method `{}`", path_segment.ident));
        }
    }

    pub(super) fn note_need_for_fn_pointer(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        expected: Ty<'tcx>,
        found: Ty<'tcx>,
    ) {
        let (sig, did, substs) = match (&expected.kind(), &found.kind()) {
            (ty::FnDef(did1, substs1), ty::FnDef(did2, substs2)) => {
                let sig1 = self.tcx.fn_sig(*did1).subst(self.tcx, substs1);
                let sig2 = self.tcx.fn_sig(*did2).subst(self.tcx, substs2);
                if sig1 != sig2 {
                    return;
                }
                err.note(
                    "different `fn` items always have unique types, even if their signatures are \
                     the same",
                );
                (sig1, *did1, substs1)
            }
            (ty::FnDef(did, substs), ty::FnPtr(sig2)) => {
                let sig1 = self.tcx.fn_sig(*did).subst(self.tcx, substs);
                if sig1 != *sig2 {
                    return;
                }
                (sig1, *did, substs)
            }
            _ => return,
        };
        err.help(&format!("change the expected type to be function pointer `{}`", sig));
        err.help(&format!(
            "if the expected type is due to type inference, cast the expected `fn` to a function \
             pointer: `{} as {}`",
            self.tcx.def_path_str_with_substs(did, substs),
            sig
        ));
    }

    /// A common error is to add an extra semicolon:
    ///
    /// ```
    /// fn foo() -> usize {
    ///     22;
    /// }
    /// ```
    ///
    /// This routine checks if the final statement in a block is an
    /// expression with an explicit semicolon whose type is compatible
    /// with `expected_ty`. If so, it suggests removing the semicolon.
    fn consider_hint_about_removing_semicolon(
        &self,
        blk: &'tcx hir::Block<'tcx>,
        expected_ty: Ty<'tcx>,
        err: &mut DiagnosticBuilder<'_>,
    ) {
        if let Some(span_semi) = self.could_remove_semicolon(blk, expected_ty) {
            err.span_suggestion(
                span_semi,
                "consider removing this semicolon",
                String::new(),
                Applicability::MachineApplicable,
            );
        }
    }

    pub(super) fn could_remove_semicolon(
        &self,
        blk: &'tcx hir::Block<'tcx>,
        expected_ty: Ty<'tcx>,
    ) -> Option<Span> {
        // Be helpful when the user wrote `{... expr;}` and
        // taking the `;` off is enough to fix the error.
        let last_stmt = blk.stmts.last()?;
        let last_expr = match last_stmt.kind {
            hir::StmtKind::Semi(ref e) => e,
            _ => return None,
        };
        let last_expr_ty = self.node_ty(last_expr.hir_id);
        if matches!(last_expr_ty.kind(), ty::Error(_))
            || self.can_sub(self.param_env, last_expr_ty, expected_ty).is_err()
        {
            return None;
        }
        let original_span = original_sp(last_stmt.span, blk.span);
        Some(original_span.with_lo(original_span.hi() - BytePos(1)))
    }

    // Instantiates the given path, which must refer to an item with the given
    // number of type parameters and type.
    pub fn instantiate_value_path(
        &self,
        segments: &[hir::PathSegment<'_>],
        self_ty: Option<Ty<'tcx>>,
        res: Res,
        span: Span,
        hir_id: hir::HirId,
    ) -> (Ty<'tcx>, Res) {
        debug!(
            "instantiate_value_path(segments={:?}, self_ty={:?}, res={:?}, hir_id={})",
            segments, self_ty, res, hir_id,
        );

        let tcx = self.tcx;

        let path_segs = match res {
            Res::Local(_) | Res::SelfCtor(_) => vec![],
            Res::Def(kind, def_id) => {
                AstConv::def_ids_for_value_path_segments(self, segments, self_ty, kind, def_id)
            }
            _ => bug!("instantiate_value_path on {:?}", res),
        };

        let mut user_self_ty = None;
        let mut is_alias_variant_ctor = false;
        match res {
            Res::Def(DefKind::Ctor(CtorOf::Variant, _), _) => {
                if let Some(self_ty) = self_ty {
                    let adt_def = self_ty.ty_adt_def().unwrap();
                    user_self_ty = Some(UserSelfTy { impl_def_id: adt_def.did, self_ty });
                    is_alias_variant_ctor = true;
                }
            }
            Res::Def(DefKind::AssocFn | DefKind::AssocConst, def_id) => {
                let container = tcx.associated_item(def_id).container;
                debug!("instantiate_value_path: def_id={:?} container={:?}", def_id, container);
                match container {
                    ty::TraitContainer(trait_did) => {
                        callee::check_legal_trait_for_method_call(tcx, span, None, trait_did)
                    }
                    ty::ImplContainer(impl_def_id) => {
                        if segments.len() == 1 {
                            // `<T>::assoc` will end up here, and so
                            // can `T::assoc`. It this came from an
                            // inherent impl, we need to record the
                            // `T` for posterity (see `UserSelfTy` for
                            // details).
                            let self_ty = self_ty.expect("UFCS sugared assoc missing Self");
                            user_self_ty = Some(UserSelfTy { impl_def_id, self_ty });
                        }
                    }
                }
            }
            _ => {}
        }

        // Now that we have categorized what space the parameters for each
        // segment belong to, let's sort out the parameters that the user
        // provided (if any) into their appropriate spaces. We'll also report
        // errors if type parameters are provided in an inappropriate place.

        let generic_segs: FxHashSet<_> = path_segs.iter().map(|PathSeg(_, index)| index).collect();
        let generics_has_err = AstConv::prohibit_generics(
            self,
            segments.iter().enumerate().filter_map(|(index, seg)| {
                if !generic_segs.contains(&index) || is_alias_variant_ctor {
                    Some(seg)
                } else {
                    None
                }
            }),
        );

        if let Res::Local(hid) = res {
            let ty = self.local_ty(span, hid).decl_ty;
            let ty = self.normalize_associated_types_in(span, &ty);
            self.write_ty(hir_id, ty);
            return (ty, res);
        }

        if generics_has_err {
            // Don't try to infer type parameters when prohibited generic arguments were given.
            user_self_ty = None;
        }

        // Now we have to compare the types that the user *actually*
        // provided against the types that were *expected*. If the user
        // did not provide any types, then we want to substitute inference
        // variables. If the user provided some types, we may still need
        // to add defaults. If the user provided *too many* types, that's
        // a problem.

        let mut infer_args_for_err = FxHashSet::default();
        for &PathSeg(def_id, index) in &path_segs {
            let seg = &segments[index];
            let generics = tcx.generics_of(def_id);
            // Argument-position `impl Trait` is treated as a normal generic
            // parameter internally, but we don't allow users to specify the
            // parameter's value explicitly, so we have to do some error-
            // checking here.
            if let GenericArgCountResult {
                correct: Err(GenericArgCountMismatch { reported: Some(ErrorReported), .. }),
                ..
            } = AstConv::check_generic_arg_count_for_call(
                tcx, span, &generics, &seg, false, // `is_method_call`
            ) {
                infer_args_for_err.insert(index);
                self.set_tainted_by_errors(); // See issue #53251.
            }
        }

        let has_self = path_segs
            .last()
            .map(|PathSeg(def_id, _)| tcx.generics_of(*def_id).has_self)
            .unwrap_or(false);

        let (res, self_ctor_substs) = if let Res::SelfCtor(impl_def_id) = res {
            let ty = self.normalize_ty(span, tcx.at(span).type_of(impl_def_id));
            match *ty.kind() {
                ty::Adt(adt_def, substs) if adt_def.has_ctor() => {
                    let variant = adt_def.non_enum_variant();
                    let ctor_def_id = variant.ctor_def_id.unwrap();
                    (
                        Res::Def(DefKind::Ctor(CtorOf::Struct, variant.ctor_kind), ctor_def_id),
                        Some(substs),
                    )
                }
                _ => {
                    let mut err = tcx.sess.struct_span_err(
                        span,
                        "the `Self` constructor can only be used with tuple or unit structs",
                    );
                    if let Some(adt_def) = ty.ty_adt_def() {
                        match adt_def.adt_kind() {
                            AdtKind::Enum => {
                                err.help("did you mean to use one of the enum's variants?");
                            }
                            AdtKind::Struct | AdtKind::Union => {
                                err.span_suggestion(
                                    span,
                                    "use curly brackets",
                                    String::from("Self { /* fields */ }"),
                                    Applicability::HasPlaceholders,
                                );
                            }
                        }
                    }
                    err.emit();

                    return (tcx.ty_error(), res);
                }
            }
        } else {
            (res, None)
        };
        let def_id = res.def_id();

        // The things we are substituting into the type should not contain
        // escaping late-bound regions, and nor should the base type scheme.
        let ty = tcx.type_of(def_id);

        let arg_count = GenericArgCountResult {
            explicit_late_bound: ExplicitLateBound::No,
            correct: if infer_args_for_err.is_empty() {
                Ok(())
            } else {
                Err(GenericArgCountMismatch::default())
            },
        };

        let substs = self_ctor_substs.unwrap_or_else(|| {
            AstConv::create_substs_for_generic_args(
                tcx,
                def_id,
                &[][..],
                has_self,
                self_ty,
                arg_count,
                // Provide the generic args, and whether types should be inferred.
                |def_id| {
                    if let Some(&PathSeg(_, index)) =
                        path_segs.iter().find(|&PathSeg(did, _)| *did == def_id)
                    {
                        // If we've encountered an `impl Trait`-related error, we're just
                        // going to infer the arguments for better error messages.
                        if !infer_args_for_err.contains(&index) {
                            // Check whether the user has provided generic arguments.
                            if let Some(ref data) = segments[index].args {
                                return (Some(data), segments[index].infer_args);
                            }
                        }
                        return (None, segments[index].infer_args);
                    }

                    (None, true)
                },
                // Provide substitutions for parameters for which (valid) arguments have been provided.
                |param, arg| match (&param.kind, arg) {
                    (GenericParamDefKind::Lifetime, GenericArg::Lifetime(lt)) => {
                        AstConv::ast_region_to_region(self, lt, Some(param)).into()
                    }
                    (GenericParamDefKind::Type { .. }, GenericArg::Type(ty)) => {
                        self.to_ty(ty).into()
                    }
                    (GenericParamDefKind::Const, GenericArg::Const(ct)) => {
                        self.const_arg_to_const(&ct.value, param.def_id).into()
                    }
                    _ => unreachable!(),
                },
                // Provide substitutions for parameters for which arguments are inferred.
                |substs, param, infer_args| {
                    match param.kind {
                        GenericParamDefKind::Lifetime => {
                            self.re_infer(Some(param), span).unwrap().into()
                        }
                        GenericParamDefKind::Type { has_default, .. } => {
                            if !infer_args && has_default {
                                // If we have a default, then we it doesn't matter that we're not
                                // inferring the type arguments: we provide the default where any
                                // is missing.
                                let default = tcx.type_of(param.def_id);
                                self.normalize_ty(
                                    span,
                                    default.subst_spanned(tcx, substs.unwrap(), Some(span)),
                                )
                                .into()
                            } else {
                                // If no type arguments were provided, we have to infer them.
                                // This case also occurs as a result of some malformed input, e.g.
                                // a lifetime argument being given instead of a type parameter.
                                // Using inference instead of `Error` gives better error messages.
                                self.var_for_def(span, param)
                            }
                        }
                        GenericParamDefKind::Const => {
                            // FIXME(const_generics:defaults)
                            // No const parameters were provided, we have to infer them.
                            self.var_for_def(span, param)
                        }
                    }
                },
            )
        });
        assert!(!substs.has_escaping_bound_vars());
        assert!(!ty.has_escaping_bound_vars());

        // First, store the "user substs" for later.
        self.write_user_type_annotation_from_substs(hir_id, def_id, substs, user_self_ty);

        self.add_required_obligations(span, def_id, &substs);

        // Substitute the values for the type parameters into the type of
        // the referenced item.
        let ty_substituted = self.instantiate_type_scheme(span, &substs, &ty);

        if let Some(UserSelfTy { impl_def_id, self_ty }) = user_self_ty {
            // In the case of `Foo<T>::method` and `<Foo<T>>::method`, if `method`
            // is inherent, there is no `Self` parameter; instead, the impl needs
            // type parameters, which we can infer by unifying the provided `Self`
            // with the substituted impl type.
            // This also occurs for an enum variant on a type alias.
            let ty = tcx.type_of(impl_def_id);

            let impl_ty = self.instantiate_type_scheme(span, &substs, &ty);
            match self.at(&self.misc(span), self.param_env).sup(impl_ty, self_ty) {
                Ok(ok) => self.register_infer_ok_obligations(ok),
                Err(_) => {
                    self.tcx.sess.delay_span_bug(
                        span,
                        &format!(
                        "instantiate_value_path: (UFCS) {:?} was a subtype of {:?} but now is not?",
                        self_ty,
                        impl_ty,
                    ),
                    );
                }
            }
        }

        self.check_rustc_args_require_const(def_id, hir_id, span);

        debug!("instantiate_value_path: type of {:?} is {:?}", hir_id, ty_substituted);
        self.write_substs(hir_id, substs);

        (ty_substituted, res)
    }

    /// Add all the obligations that are required, substituting and normalized appropriately.
    fn add_required_obligations(&self, span: Span, def_id: DefId, substs: &SubstsRef<'tcx>) {
        let (bounds, spans) = self.instantiate_bounds(span, def_id, &substs);

        for (i, mut obligation) in traits::predicates_for_generics(
            traits::ObligationCause::new(span, self.body_id, traits::ItemObligation(def_id)),
            self.param_env,
            bounds,
        )
        .enumerate()
        {
            // This makes the error point at the bound, but we want to point at the argument
            if let Some(span) = spans.get(i) {
                obligation.cause.make_mut().code = traits::BindingObligation(def_id, *span);
            }
            self.register_predicate(obligation);
        }
    }

    fn check_rustc_args_require_const(&self, def_id: DefId, hir_id: hir::HirId, span: Span) {
        // We're only interested in functions tagged with
        // #[rustc_args_required_const], so ignore anything that's not.
        if !self.tcx.has_attr(def_id, sym::rustc_args_required_const) {
            return;
        }

        // If our calling expression is indeed the function itself, we're good!
        // If not, generate an error that this can only be called directly.
        if let Node::Expr(expr) = self.tcx.hir().get(self.tcx.hir().get_parent_node(hir_id)) {
            if let ExprKind::Call(ref callee, ..) = expr.kind {
                if callee.hir_id == hir_id {
                    return;
                }
            }
        }

        self.tcx.sess.span_err(
            span,
            "this function can only be invoked directly, not through a function pointer",
        );
    }

    /// Resolves `typ` by a single level if `typ` is a type variable.
    /// If no resolution is possible, then an error is reported.
    /// Numeric inference variables may be left unresolved.
    pub fn structurally_resolved_type(&self, sp: Span, ty: Ty<'tcx>) -> Ty<'tcx> {
        let ty = self.resolve_vars_with_obligations(ty);
        if !ty.is_ty_var() {
            ty
        } else {
            if !self.is_tainted_by_errors() {
                self.emit_inference_failure_err((**self).body_id, sp, ty.into(), E0282)
                    .note("type must be known at this point")
                    .emit();
            }
            let err = self.tcx.ty_error();
            self.demand_suptype(sp, err, ty);
            err
        }
    }

    pub(super) fn with_breakable_ctxt<F: FnOnce() -> R, R>(
        &self,
        id: hir::HirId,
        ctxt: BreakableCtxt<'tcx>,
        f: F,
    ) -> (BreakableCtxt<'tcx>, R) {
        let index;
        {
            let mut enclosing_breakables = self.enclosing_breakables.borrow_mut();
            index = enclosing_breakables.stack.len();
            enclosing_breakables.by_id.insert(id, index);
            enclosing_breakables.stack.push(ctxt);
        }
        let result = f();
        let ctxt = {
            let mut enclosing_breakables = self.enclosing_breakables.borrow_mut();
            debug_assert!(enclosing_breakables.stack.len() == index + 1);
            enclosing_breakables.by_id.remove(&id).expect("missing breakable context");
            enclosing_breakables.stack.pop().expect("missing breakable context")
        };
        (ctxt, result)
    }

    /// Instantiate a QueryResponse in a probe context, without a
    /// good ObligationCause.
    pub(super) fn probe_instantiate_query_response(
        &self,
        span: Span,
        original_values: &OriginalQueryValues<'tcx>,
        query_result: &Canonical<'tcx, QueryResponse<'tcx, Ty<'tcx>>>,
    ) -> InferResult<'tcx, Ty<'tcx>> {
        self.instantiate_query_response_and_region_obligations(
            &traits::ObligationCause::misc(span, self.body_id),
            self.param_env,
            original_values,
            query_result,
        )
    }

    /// Returns `true` if an expression is contained inside the LHS of an assignment expression.
    pub(super) fn expr_in_place(&self, mut expr_id: hir::HirId) -> bool {
        let mut contained_in_place = false;

        while let hir::Node::Expr(parent_expr) =
            self.tcx.hir().get(self.tcx.hir().get_parent_node(expr_id))
        {
            match &parent_expr.kind {
                hir::ExprKind::Assign(lhs, ..) | hir::ExprKind::AssignOp(_, lhs, ..) => {
                    if lhs.hir_id == expr_id {
                        contained_in_place = true;
                        break;
                    }
                }
                _ => (),
            }
            expr_id = parent_expr.hir_id;
        }

        contained_in_place
    }
}
