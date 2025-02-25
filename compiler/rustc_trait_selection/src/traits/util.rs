use std::collections::BTreeMap;

use super::NormalizeExt;
use super::{ObligationCause, PredicateObligation, SelectionContext};
use rustc_data_structures::fx::FxHashSet;
use rustc_errors::Diagnostic;
use rustc_hir::def_id::DefId;
use rustc_infer::infer::{InferCtxt, InferOk};
use rustc_middle::ty::GenericArgsRef;
use rustc_middle::ty::{self, ImplSubject, ToPredicate, Ty, TyCtxt, TypeVisitableExt};
use rustc_middle::ty::{TypeFoldable, TypeFolder, TypeSuperFoldable};
use rustc_span::Span;
use smallvec::SmallVec;

pub use rustc_infer::traits::util::*;

///////////////////////////////////////////////////////////////////////////
// `TraitAliasExpander` iterator
///////////////////////////////////////////////////////////////////////////

/// "Trait alias expansion" is the process of expanding a sequence of trait
/// references into another sequence by transitively following all trait
/// aliases. e.g. If you have bounds like `Foo + Send`, a trait alias
/// `trait Foo = Bar + Sync;`, and another trait alias
/// `trait Bar = Read + Write`, then the bounds would expand to
/// `Read + Write + Sync + Send`.
/// Expansion is done via a DFS (depth-first search), and the `visited` field
/// is used to avoid cycles.
pub struct TraitAliasExpander<'tcx> {
    tcx: TyCtxt<'tcx>,
    stack: Vec<TraitAliasExpansionInfo<'tcx>>,
}

/// Stores information about the expansion of a trait via a path of zero or more trait aliases.
#[derive(Debug, Clone)]
pub struct TraitAliasExpansionInfo<'tcx> {
    pub path: SmallVec<[(ty::PolyTraitRef<'tcx>, Span); 4]>,
}

impl<'tcx> TraitAliasExpansionInfo<'tcx> {
    fn new(trait_ref: ty::PolyTraitRef<'tcx>, span: Span) -> Self {
        Self { path: smallvec![(trait_ref, span)] }
    }

    /// Adds diagnostic labels to `diag` for the expansion path of a trait through all intermediate
    /// trait aliases.
    pub fn label_with_exp_info(
        &self,
        diag: &mut Diagnostic,
        top_label: &'static str,
        use_desc: &str,
    ) {
        diag.span_label(self.top().1, top_label);
        if self.path.len() > 1 {
            for (_, sp) in self.path.iter().rev().skip(1).take(self.path.len() - 2) {
                diag.span_label(*sp, format!("referenced here ({use_desc})"));
            }
        }
        if self.top().1 != self.bottom().1 {
            // When the trait object is in a return type these two spans match, we don't want
            // redundant labels.
            diag.span_label(
                self.bottom().1,
                format!("trait alias used in trait object type ({use_desc})"),
            );
        }
    }

    pub fn trait_ref(&self) -> ty::PolyTraitRef<'tcx> {
        self.top().0
    }

    pub fn top(&self) -> &(ty::PolyTraitRef<'tcx>, Span) {
        self.path.last().unwrap()
    }

    pub fn bottom(&self) -> &(ty::PolyTraitRef<'tcx>, Span) {
        self.path.first().unwrap()
    }

    fn clone_and_push(&self, trait_ref: ty::PolyTraitRef<'tcx>, span: Span) -> Self {
        let mut path = self.path.clone();
        path.push((trait_ref, span));

        Self { path }
    }
}

pub fn expand_trait_aliases<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_refs: impl Iterator<Item = (ty::PolyTraitRef<'tcx>, Span)>,
) -> TraitAliasExpander<'tcx> {
    let items: Vec<_> =
        trait_refs.map(|(trait_ref, span)| TraitAliasExpansionInfo::new(trait_ref, span)).collect();
    TraitAliasExpander { tcx, stack: items }
}

impl<'tcx> TraitAliasExpander<'tcx> {
    /// If `item` is a trait alias and its predicate has not yet been visited, then expands `item`
    /// to the definition, pushes the resulting expansion onto `self.stack`, and returns `false`.
    /// Otherwise, immediately returns `true` if `item` is a regular trait, or `false` if it is a
    /// trait alias.
    /// The return value indicates whether `item` should be yielded to the user.
    fn expand(&mut self, item: &TraitAliasExpansionInfo<'tcx>) -> bool {
        let tcx = self.tcx;
        let trait_ref = item.trait_ref();
        let pred = trait_ref.to_predicate(tcx);

        debug!("expand_trait_aliases: trait_ref={:?}", trait_ref);

        // Don't recurse if this bound is not a trait alias.
        let is_alias = tcx.is_trait_alias(trait_ref.def_id());
        if !is_alias {
            return true;
        }

        // Don't recurse if this trait alias is already on the stack for the DFS search.
        let anon_pred = anonymize_predicate(tcx, pred);
        if item
            .path
            .iter()
            .rev()
            .skip(1)
            .any(|&(tr, _)| anonymize_predicate(tcx, tr.to_predicate(tcx)) == anon_pred)
        {
            return false;
        }

        // Get components of trait alias.
        let predicates = tcx.implied_predicates_of(trait_ref.def_id());
        debug!(?predicates);

        let items = predicates.predicates.iter().rev().filter_map(|(pred, span)| {
            pred.instantiate_supertrait(tcx, &trait_ref)
                .as_trait_clause()
                .map(|trait_ref| item.clone_and_push(trait_ref.map_bound(|t| t.trait_ref), *span))
        });
        debug!("expand_trait_aliases: items={:?}", items.clone().collect::<Vec<_>>());

        self.stack.extend(items);

        false
    }
}

impl<'tcx> Iterator for TraitAliasExpander<'tcx> {
    type Item = TraitAliasExpansionInfo<'tcx>;

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.stack.len(), None)
    }

    fn next(&mut self) -> Option<TraitAliasExpansionInfo<'tcx>> {
        while let Some(item) = self.stack.pop() {
            if self.expand(&item) {
                return Some(item);
            }
        }
        None
    }
}

///////////////////////////////////////////////////////////////////////////
// Iterator over def-IDs of supertraits
///////////////////////////////////////////////////////////////////////////

pub struct SupertraitDefIds<'tcx> {
    tcx: TyCtxt<'tcx>,
    stack: Vec<DefId>,
    visited: FxHashSet<DefId>,
}

pub fn supertrait_def_ids(tcx: TyCtxt<'_>, trait_def_id: DefId) -> SupertraitDefIds<'_> {
    SupertraitDefIds {
        tcx,
        stack: vec![trait_def_id],
        visited: Some(trait_def_id).into_iter().collect(),
    }
}

impl Iterator for SupertraitDefIds<'_> {
    type Item = DefId;

    fn next(&mut self) -> Option<DefId> {
        let def_id = self.stack.pop()?;
        let predicates = self.tcx.super_predicates_of(def_id);
        let visited = &mut self.visited;
        self.stack.extend(
            predicates
                .predicates
                .iter()
                .filter_map(|(pred, _)| pred.as_trait_clause())
                .map(|trait_ref| trait_ref.def_id())
                .filter(|&super_def_id| visited.insert(super_def_id)),
        );
        Some(def_id)
    }
}

///////////////////////////////////////////////////////////////////////////
// Other
///////////////////////////////////////////////////////////////////////////

/// Instantiate all bound parameters of the impl subject with the given args,
/// returning the resulting subject and all obligations that arise.
/// The obligations are closed under normalization.
pub fn impl_subject_and_oblig<'a, 'tcx>(
    selcx: &mut SelectionContext<'a, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    impl_def_id: DefId,
    impl_args: GenericArgsRef<'tcx>,
    cause: impl Fn(usize, Span) -> ObligationCause<'tcx>,
) -> (ImplSubject<'tcx>, impl Iterator<Item = PredicateObligation<'tcx>>) {
    let subject = selcx.tcx().impl_subject(impl_def_id);
    let subject = subject.instantiate(selcx.tcx(), impl_args);

    let InferOk { value: subject, obligations: normalization_obligations1 } =
        selcx.infcx.at(&ObligationCause::dummy(), param_env).normalize(subject);

    let predicates = selcx.tcx().predicates_of(impl_def_id);
    let predicates = predicates.instantiate(selcx.tcx(), impl_args);
    let InferOk { value: predicates, obligations: normalization_obligations2 } =
        selcx.infcx.at(&ObligationCause::dummy(), param_env).normalize(predicates);
    let impl_obligations = super::predicates_for_generics(cause, param_env, predicates);

    let impl_obligations =
        impl_obligations.chain(normalization_obligations1).chain(normalization_obligations2);

    (subject, impl_obligations)
}

/// Casts a trait reference into a reference to one of its super
/// traits; returns `None` if `target_trait_def_id` is not a
/// supertrait.
pub fn upcast_choices<'tcx>(
    tcx: TyCtxt<'tcx>,
    source_trait_ref: ty::PolyTraitRef<'tcx>,
    target_trait_def_id: DefId,
) -> Vec<ty::PolyTraitRef<'tcx>> {
    if source_trait_ref.def_id() == target_trait_def_id {
        return vec![source_trait_ref]; // Shortcut the most common case.
    }

    supertraits(tcx, source_trait_ref).filter(|r| r.def_id() == target_trait_def_id).collect()
}

/// Given an upcast trait object described by `object`, returns the
/// index of the method `method_def_id` (which should be part of
/// `object.upcast_trait_ref`) within the vtable for `object`.
pub fn get_vtable_index_of_object_method<'tcx>(
    tcx: TyCtxt<'tcx>,
    vtable_base: usize,
    method_def_id: DefId,
) -> Option<usize> {
    // Count number of methods preceding the one we are selecting and
    // add them to the total offset.
    tcx.own_existential_vtable_entries(tcx.parent(method_def_id))
        .iter()
        .copied()
        .position(|def_id| def_id == method_def_id)
        .map(|index| vtable_base + index)
}

pub fn closure_trait_ref_and_return_type<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_trait_def_id: DefId,
    self_ty: Ty<'tcx>,
    sig: ty::PolyFnSig<'tcx>,
    tuple_arguments: TupleArgumentsFlag,
    fn_host_effect: ty::Const<'tcx>,
) -> ty::Binder<'tcx, (ty::TraitRef<'tcx>, Ty<'tcx>)> {
    assert!(!self_ty.has_escaping_bound_vars());
    let arguments_tuple = match tuple_arguments {
        TupleArgumentsFlag::No => sig.skip_binder().inputs()[0],
        TupleArgumentsFlag::Yes => Ty::new_tup(tcx, sig.skip_binder().inputs()),
    };
    let trait_ref = if tcx.has_host_param(fn_trait_def_id) {
        ty::TraitRef::new(
            tcx,
            fn_trait_def_id,
            [
                ty::GenericArg::from(self_ty),
                ty::GenericArg::from(arguments_tuple),
                ty::GenericArg::from(fn_host_effect),
            ],
        )
    } else {
        ty::TraitRef::new(tcx, fn_trait_def_id, [self_ty, arguments_tuple])
    };
    sig.map_bound(|sig| (trait_ref, sig.output()))
}

pub fn coroutine_trait_ref_and_outputs<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_trait_def_id: DefId,
    self_ty: Ty<'tcx>,
    sig: ty::GenSig<'tcx>,
) -> (ty::TraitRef<'tcx>, Ty<'tcx>, Ty<'tcx>) {
    assert!(!self_ty.has_escaping_bound_vars());
    let trait_ref = ty::TraitRef::new(tcx, fn_trait_def_id, [self_ty, sig.resume_ty]);
    (trait_ref, sig.yield_ty, sig.return_ty)
}

pub fn future_trait_ref_and_outputs<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_trait_def_id: DefId,
    self_ty: Ty<'tcx>,
    sig: ty::GenSig<'tcx>,
) -> (ty::TraitRef<'tcx>, Ty<'tcx>) {
    assert!(!self_ty.has_escaping_bound_vars());
    let trait_ref = ty::TraitRef::new(tcx, fn_trait_def_id, [self_ty]);
    (trait_ref, sig.return_ty)
}

pub fn iterator_trait_ref_and_outputs<'tcx>(
    tcx: TyCtxt<'tcx>,
    iterator_def_id: DefId,
    self_ty: Ty<'tcx>,
    sig: ty::GenSig<'tcx>,
) -> (ty::TraitRef<'tcx>, Ty<'tcx>) {
    assert!(!self_ty.has_escaping_bound_vars());
    let trait_ref = ty::TraitRef::new(tcx, iterator_def_id, [self_ty]);
    (trait_ref, sig.yield_ty)
}

pub fn async_iterator_trait_ref_and_outputs<'tcx>(
    tcx: TyCtxt<'tcx>,
    async_iterator_def_id: DefId,
    self_ty: Ty<'tcx>,
    sig: ty::GenSig<'tcx>,
) -> (ty::TraitRef<'tcx>, Ty<'tcx>) {
    assert!(!self_ty.has_escaping_bound_vars());
    let trait_ref = ty::TraitRef::new(tcx, async_iterator_def_id, [self_ty]);
    (trait_ref, sig.yield_ty)
}

pub fn impl_item_is_final(tcx: TyCtxt<'_>, assoc_item: &ty::AssocItem) -> bool {
    assoc_item.defaultness(tcx).is_final()
        && tcx.defaultness(assoc_item.container_id(tcx)).is_final()
}

pub enum TupleArgumentsFlag {
    Yes,
    No,
}

// Verify that the trait item and its implementation have compatible args lists
pub fn check_args_compatible<'tcx>(
    tcx: TyCtxt<'tcx>,
    assoc_item: ty::AssocItem,
    args: ty::GenericArgsRef<'tcx>,
) -> bool {
    fn check_args_compatible_inner<'tcx>(
        tcx: TyCtxt<'tcx>,
        generics: &'tcx ty::Generics,
        args: &'tcx [ty::GenericArg<'tcx>],
    ) -> bool {
        if generics.count() != args.len() {
            return false;
        }

        let (parent_args, own_args) = args.split_at(generics.parent_count);

        if let Some(parent) = generics.parent
            && let parent_generics = tcx.generics_of(parent)
            && !check_args_compatible_inner(tcx, parent_generics, parent_args)
        {
            return false;
        }

        for (param, arg) in std::iter::zip(&generics.params, own_args) {
            match (&param.kind, arg.unpack()) {
                (ty::GenericParamDefKind::Type { .. }, ty::GenericArgKind::Type(_))
                | (ty::GenericParamDefKind::Lifetime, ty::GenericArgKind::Lifetime(_))
                | (ty::GenericParamDefKind::Const { .. }, ty::GenericArgKind::Const(_)) => {}
                _ => return false,
            }
        }

        true
    }

    let generics = tcx.generics_of(assoc_item.def_id);
    // Chop off any additional args (RPITIT) args
    let args = &args[0..generics.count().min(args.len())];
    check_args_compatible_inner(tcx, generics, args)
}

/// Executes `f` on `value` after replacing all escaping bound variables with placeholders
/// and then replaces these placeholders with the original bound variables in the result.
///
/// In most places, bound variables should be replaced right when entering a binder, making
/// this function unnecessary. However, normalization currently does not do that, so we have
/// to do this lazily.
///
/// You should not add any additional uses of this function, at least not without first
/// discussing it with t-types.
///
/// FIXME(@lcnr): We may even consider experimenting with eagerly replacing bound vars during
/// normalization as well, at which point this function will be unnecessary and can be removed.
pub fn with_replaced_escaping_bound_vars<
    'a,
    'tcx,
    T: TypeFoldable<TyCtxt<'tcx>>,
    R: TypeFoldable<TyCtxt<'tcx>>,
>(
    infcx: &'a InferCtxt<'tcx>,
    universe_indices: &'a mut Vec<Option<ty::UniverseIndex>>,
    value: T,
    f: impl FnOnce(T) -> R,
) -> R {
    if value.has_escaping_bound_vars() {
        let (value, mapped_regions, mapped_types, mapped_consts) =
            BoundVarReplacer::replace_bound_vars(infcx, universe_indices, value);
        let result = f(value);
        PlaceholderReplacer::replace_placeholders(
            infcx,
            mapped_regions,
            mapped_types,
            mapped_consts,
            universe_indices,
            result,
        )
    } else {
        f(value)
    }
}

pub struct BoundVarReplacer<'me, 'tcx> {
    infcx: &'me InferCtxt<'tcx>,
    // These three maps track the bound variable that were replaced by placeholders. It might be
    // nice to remove these since we already have the `kind` in the placeholder; we really just need
    // the `var` (but we *could* bring that into scope if we were to track them as we pass them).
    mapped_regions: BTreeMap<ty::PlaceholderRegion, ty::BoundRegion>,
    mapped_types: BTreeMap<ty::PlaceholderType, ty::BoundTy>,
    mapped_consts: BTreeMap<ty::PlaceholderConst, ty::BoundVar>,
    // The current depth relative to *this* folding, *not* the entire normalization. In other words,
    // the depth of binders we've passed here.
    current_index: ty::DebruijnIndex,
    // The `UniverseIndex` of the binding levels above us. These are optional, since we are lazy:
    // we don't actually create a universe until we see a bound var we have to replace.
    universe_indices: &'me mut Vec<Option<ty::UniverseIndex>>,
}

impl<'me, 'tcx> BoundVarReplacer<'me, 'tcx> {
    /// Returns `Some` if we *were* able to replace bound vars. If there are any bound vars that
    /// use a binding level above `universe_indices.len()`, we fail.
    pub fn replace_bound_vars<T: TypeFoldable<TyCtxt<'tcx>>>(
        infcx: &'me InferCtxt<'tcx>,
        universe_indices: &'me mut Vec<Option<ty::UniverseIndex>>,
        value: T,
    ) -> (
        T,
        BTreeMap<ty::PlaceholderRegion, ty::BoundRegion>,
        BTreeMap<ty::PlaceholderType, ty::BoundTy>,
        BTreeMap<ty::PlaceholderConst, ty::BoundVar>,
    ) {
        let mapped_regions: BTreeMap<ty::PlaceholderRegion, ty::BoundRegion> = BTreeMap::new();
        let mapped_types: BTreeMap<ty::PlaceholderType, ty::BoundTy> = BTreeMap::new();
        let mapped_consts: BTreeMap<ty::PlaceholderConst, ty::BoundVar> = BTreeMap::new();

        let mut replacer = BoundVarReplacer {
            infcx,
            mapped_regions,
            mapped_types,
            mapped_consts,
            current_index: ty::INNERMOST,
            universe_indices,
        };

        let value = value.fold_with(&mut replacer);

        (value, replacer.mapped_regions, replacer.mapped_types, replacer.mapped_consts)
    }

    fn universe_for(&mut self, debruijn: ty::DebruijnIndex) -> ty::UniverseIndex {
        let infcx = self.infcx;
        let index =
            self.universe_indices.len() + self.current_index.as_usize() - debruijn.as_usize() - 1;
        let universe = self.universe_indices[index].unwrap_or_else(|| {
            for i in self.universe_indices.iter_mut().take(index + 1) {
                *i = i.or_else(|| Some(infcx.create_next_universe()))
            }
            self.universe_indices[index].unwrap()
        });
        universe
    }
}

impl<'tcx> TypeFolder<TyCtxt<'tcx>> for BoundVarReplacer<'_, 'tcx> {
    fn interner(&self) -> TyCtxt<'tcx> {
        self.infcx.tcx
    }

    fn fold_binder<T: TypeFoldable<TyCtxt<'tcx>>>(
        &mut self,
        t: ty::Binder<'tcx, T>,
    ) -> ty::Binder<'tcx, T> {
        self.current_index.shift_in(1);
        let t = t.super_fold_with(self);
        self.current_index.shift_out(1);
        t
    }

    fn fold_region(&mut self, r: ty::Region<'tcx>) -> ty::Region<'tcx> {
        match *r {
            ty::ReBound(debruijn, _)
                if debruijn.as_usize()
                    >= self.current_index.as_usize() + self.universe_indices.len() =>
            {
                bug!(
                    "Bound vars {r:#?} outside of `self.universe_indices`: {:#?}",
                    self.universe_indices
                );
            }
            ty::ReBound(debruijn, br) if debruijn >= self.current_index => {
                let universe = self.universe_for(debruijn);
                let p = ty::PlaceholderRegion { universe, bound: br };
                self.mapped_regions.insert(p, br);
                ty::Region::new_placeholder(self.infcx.tcx, p)
            }
            _ => r,
        }
    }

    fn fold_ty(&mut self, t: Ty<'tcx>) -> Ty<'tcx> {
        match *t.kind() {
            ty::Bound(debruijn, _)
                if debruijn.as_usize() + 1
                    > self.current_index.as_usize() + self.universe_indices.len() =>
            {
                bug!(
                    "Bound vars {t:#?} outside of `self.universe_indices`: {:#?}",
                    self.universe_indices
                );
            }
            ty::Bound(debruijn, bound_ty) if debruijn >= self.current_index => {
                let universe = self.universe_for(debruijn);
                let p = ty::PlaceholderType { universe, bound: bound_ty };
                self.mapped_types.insert(p, bound_ty);
                Ty::new_placeholder(self.infcx.tcx, p)
            }
            _ if t.has_vars_bound_at_or_above(self.current_index) => t.super_fold_with(self),
            _ => t,
        }
    }

    fn fold_const(&mut self, ct: ty::Const<'tcx>) -> ty::Const<'tcx> {
        match ct.kind() {
            ty::ConstKind::Bound(debruijn, _)
                if debruijn.as_usize() + 1
                    > self.current_index.as_usize() + self.universe_indices.len() =>
            {
                bug!(
                    "Bound vars {ct:#?} outside of `self.universe_indices`: {:#?}",
                    self.universe_indices
                );
            }
            ty::ConstKind::Bound(debruijn, bound_const) if debruijn >= self.current_index => {
                let universe = self.universe_for(debruijn);
                let p = ty::PlaceholderConst { universe, bound: bound_const };
                self.mapped_consts.insert(p, bound_const);
                ty::Const::new_placeholder(self.infcx.tcx, p, ct.ty())
            }
            _ => ct.super_fold_with(self),
        }
    }

    fn fold_predicate(&mut self, p: ty::Predicate<'tcx>) -> ty::Predicate<'tcx> {
        if p.has_vars_bound_at_or_above(self.current_index) { p.super_fold_with(self) } else { p }
    }
}

/// The inverse of [`BoundVarReplacer`]: replaces placeholders with the bound vars from which they came.
pub struct PlaceholderReplacer<'me, 'tcx> {
    infcx: &'me InferCtxt<'tcx>,
    mapped_regions: BTreeMap<ty::PlaceholderRegion, ty::BoundRegion>,
    mapped_types: BTreeMap<ty::PlaceholderType, ty::BoundTy>,
    mapped_consts: BTreeMap<ty::PlaceholderConst, ty::BoundVar>,
    universe_indices: &'me [Option<ty::UniverseIndex>],
    current_index: ty::DebruijnIndex,
}

impl<'me, 'tcx> PlaceholderReplacer<'me, 'tcx> {
    pub fn replace_placeholders<T: TypeFoldable<TyCtxt<'tcx>>>(
        infcx: &'me InferCtxt<'tcx>,
        mapped_regions: BTreeMap<ty::PlaceholderRegion, ty::BoundRegion>,
        mapped_types: BTreeMap<ty::PlaceholderType, ty::BoundTy>,
        mapped_consts: BTreeMap<ty::PlaceholderConst, ty::BoundVar>,
        universe_indices: &'me [Option<ty::UniverseIndex>],
        value: T,
    ) -> T {
        let mut replacer = PlaceholderReplacer {
            infcx,
            mapped_regions,
            mapped_types,
            mapped_consts,
            universe_indices,
            current_index: ty::INNERMOST,
        };
        value.fold_with(&mut replacer)
    }
}

impl<'tcx> TypeFolder<TyCtxt<'tcx>> for PlaceholderReplacer<'_, 'tcx> {
    fn interner(&self) -> TyCtxt<'tcx> {
        self.infcx.tcx
    }

    fn fold_binder<T: TypeFoldable<TyCtxt<'tcx>>>(
        &mut self,
        t: ty::Binder<'tcx, T>,
    ) -> ty::Binder<'tcx, T> {
        if !t.has_placeholders() && !t.has_infer() {
            return t;
        }
        self.current_index.shift_in(1);
        let t = t.super_fold_with(self);
        self.current_index.shift_out(1);
        t
    }

    fn fold_region(&mut self, r0: ty::Region<'tcx>) -> ty::Region<'tcx> {
        let r1 = match *r0 {
            ty::ReVar(vid) => self
                .infcx
                .inner
                .borrow_mut()
                .unwrap_region_constraints()
                .opportunistic_resolve_var(self.infcx.tcx, vid),
            _ => r0,
        };

        let r2 = match *r1 {
            ty::RePlaceholder(p) => {
                let replace_var = self.mapped_regions.get(&p);
                match replace_var {
                    Some(replace_var) => {
                        let index = self
                            .universe_indices
                            .iter()
                            .position(|u| matches!(u, Some(pu) if *pu == p.universe))
                            .unwrap_or_else(|| bug!("Unexpected placeholder universe."));
                        let db = ty::DebruijnIndex::from_usize(
                            self.universe_indices.len() - index + self.current_index.as_usize() - 1,
                        );
                        ty::Region::new_bound(self.interner(), db, *replace_var)
                    }
                    None => r1,
                }
            }
            _ => r1,
        };

        debug!(?r0, ?r1, ?r2, "fold_region");

        r2
    }

    fn fold_ty(&mut self, ty: Ty<'tcx>) -> Ty<'tcx> {
        let ty = self.infcx.shallow_resolve(ty);
        match *ty.kind() {
            ty::Placeholder(p) => {
                let replace_var = self.mapped_types.get(&p);
                match replace_var {
                    Some(replace_var) => {
                        let index = self
                            .universe_indices
                            .iter()
                            .position(|u| matches!(u, Some(pu) if *pu == p.universe))
                            .unwrap_or_else(|| bug!("Unexpected placeholder universe."));
                        let db = ty::DebruijnIndex::from_usize(
                            self.universe_indices.len() - index + self.current_index.as_usize() - 1,
                        );
                        Ty::new_bound(self.infcx.tcx, db, *replace_var)
                    }
                    None => {
                        if ty.has_infer() {
                            ty.super_fold_with(self)
                        } else {
                            ty
                        }
                    }
                }
            }

            _ if ty.has_placeholders() || ty.has_infer() => ty.super_fold_with(self),
            _ => ty,
        }
    }

    fn fold_const(&mut self, ct: ty::Const<'tcx>) -> ty::Const<'tcx> {
        let ct = self.infcx.shallow_resolve(ct);
        if let ty::ConstKind::Placeholder(p) = ct.kind() {
            let replace_var = self.mapped_consts.get(&p);
            match replace_var {
                Some(replace_var) => {
                    let index = self
                        .universe_indices
                        .iter()
                        .position(|u| matches!(u, Some(pu) if *pu == p.universe))
                        .unwrap_or_else(|| bug!("Unexpected placeholder universe."));
                    let db = ty::DebruijnIndex::from_usize(
                        self.universe_indices.len() - index + self.current_index.as_usize() - 1,
                    );
                    ty::Const::new_bound(self.infcx.tcx, db, *replace_var, ct.ty())
                }
                None => {
                    if ct.has_infer() {
                        ct.super_fold_with(self)
                    } else {
                        ct
                    }
                }
            }
        } else {
            ct.super_fold_with(self)
        }
    }
}
