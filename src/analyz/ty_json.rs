use std::convert::TryFrom;
use rustc_data_structures::stable_hasher::{HashStable, StableHasher};
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_index::vec::{IndexVec, Idx};
use rustc_middle::mir;
use rustc_const_eval::interpret::{self, InterpCx, InterpResult, MPlaceTy, Provenance};
use rustc_const_eval::const_eval::CheckAlignment;
use rustc_middle::bug;
use rustc_middle::ty;
use rustc_middle::ty::{AdtKind, DynKind, TyCtxt, TypeVisitable};
use rustc_middle::ty::util::{IntTypeExt};
use rustc_query_system::ich::StableHashingContext;
use rustc_target::abi::{Align, FieldsShape, HasDataLayout, Size};
use rustc_span::DUMMY_SP;
use serde_json;
use std::usize;

use analyz::to_json::*;

impl<'tcx, T> ToJson<'tcx> for ty::List<T>
    where
    T: ToJson<'tcx>,
{
    fn to_json(&self, mir: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let mut j = Vec::new();
        for v in self.iter() {
            j.push(v.to_json(mir));
        }
        json!(j)
    }
}

impl ToJson<'_> for mir::BorrowKind {
    fn to_json(&self, _mir: &mut MirState) -> serde_json::Value {
        match self {
            &mir::BorrowKind::Shared => json!("Shared"),
            &mir::BorrowKind::Shallow => json!("Shallow"),
            &mir::BorrowKind::Unique => json!("Unique"),
            &mir::BorrowKind::Mut{..} => json!("Mut"),
        }
    }
}

impl ToJson<'_> for ty::VariantDiscr {
    fn to_json(&self, mir: &mut MirState) -> serde_json::Value {
        match self {
            &ty::VariantDiscr::Relative(i) => {
                json!({"kind": "Relative", "index" : json!(i)})
            }
            &ty::VariantDiscr::Explicit(def_id) => {
                json!({
                    "kind": "Explicit",
                    "name" : get_fn_def_name(mir, def_id, ty::List::empty()),
                })
            }
        }
    }
}

pub fn def_id_str(tcx: TyCtxt, def_id: hir::def_id::DefId) -> String {
    // Based on rustc/ty/context.rs.html TyCtxt::def_path_debug_str
    let crate_name = tcx.crate_name(def_id.krate);
    let disambig = tcx.crate_hash(def_id.krate);
    let defpath = tcx.def_path(def_id);
    format!(
        "{}/{}{}",
        crate_name,
        &disambig.to_string()[..8],
        defpath.to_string_no_crate_verbose(),
    )
}

pub fn ext_def_id_str<'tcx, T>(
    tcx: TyCtxt<'tcx>,
    def_id: hir::def_id::DefId,
    prefix: &str,
    extra: T,
) -> String
where T: for<'a> HashStable<StableHashingContext<'a>> {
    let base = def_id_str(tcx, def_id);

    // Based on librustc_codegen_utils/symbol_names/legacy.rs get_symbol_hash
    let hash: u64 = tcx.with_stable_hashing_context(|mut hcx| {
        let mut hasher = StableHasher::new();
        extra.hash_stable(&mut hcx, &mut hasher);
        hasher.finish()
    });
    format!("{}::{}{:016x}[0]", base, prefix, hash)
}

pub fn adt_inst_id_str<'tcx>(
    tcx: TyCtxt<'tcx>,
    ai: AdtInst<'tcx>,
) -> String {
    // Erase all early-bound regions.
    let substs = tcx.erase_regions(ai.substs);
    ext_def_id_str(tcx, ai.def_id(), "_adt", substs)
}

pub fn inst_id_str<'tcx>(
    tcx: TyCtxt<'tcx>,
    inst: ty::Instance<'tcx>,
) -> String {
    let substs = tcx.normalize_erasing_regions(
        ty::ParamEnv::reveal_all(),
        inst.substs,
    );
    assert!(!substs.has_erasable_regions());
    assert!(!substs.needs_subst());

    match inst.def {
        ty::InstanceDef::Item(ty::WithOptConstParam { did: def_id, .. }) |
        ty::InstanceDef::Intrinsic(def_id) => {
            if substs.len() == 0 {
                def_id_str(tcx, def_id)
            } else {
                ext_def_id_str(tcx, def_id, "_inst", substs)
            }
        },
        ty::InstanceDef::VTableShim(def_id) =>
            ext_def_id_str(tcx, def_id, "_vtshim", substs),
        ty::InstanceDef::ReifyShim(def_id) =>
            ext_def_id_str(tcx, def_id, "_reify", substs),
        ty::InstanceDef::Virtual(def_id, idx) =>
            ext_def_id_str(tcx, def_id, &format!("_virt{}_", idx), substs),
        ty::InstanceDef::DropGlue(def_id, _) =>
            ext_def_id_str(tcx, def_id, "_drop", substs),
        ty::InstanceDef::FnPtrShim(def_id, _) |
        ty::InstanceDef::ClosureOnceShim { call_once: def_id, .. } =>
            ext_def_id_str(tcx, def_id, "_callonce", substs),
        ty::InstanceDef::CloneShim(def_id, _) =>
            ext_def_id_str(tcx, def_id, "_shim", substs),
    }
}

pub fn trait_inst_id_str<'tcx>(
    tcx: TyCtxt<'tcx>,
    ti: &TraitInst<'tcx>,
) -> String {
    if let Some(trait_ref) = ti.trait_ref {
        let dyn_ty = ti.dyn_ty(tcx)
            .expect("dyn_ty should only return None when self.trait_ref is None");
        ext_def_id_str(tcx, trait_ref.def_id, "_trait", dyn_ty)
    } else {
        "trait/0::empty[0]".to_owned()
    }
}

/// Get the mangled name of a monomorphic function.  As a side effect, this marks the function as
/// "used", so its body will be emitted too.
pub fn get_fn_def_name<'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    defid: DefId,
    substs: ty::subst::SubstsRef<'tcx>,
) -> String {
    let inst = ty::Instance::resolve(
        mir.state.tcx,
        ty::ParamEnv::reveal_all(),
        defid,
        substs,
    );

    // Compute the mangled name of the monomorphized instance being called.
    if let Ok(Some(inst)) = inst {
        mir.used.instances.insert(inst);
        inst_id_str(mir.state.tcx, inst)
    } else {
        eprintln!(
            "error: failed to resolve FnDef Instance: {:?}, {:?}",
            defid, substs,
        );
        def_id_str(mir.state.tcx, defid)
    }
}

pub fn get_drop_fn_name<'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    ty: ty::Ty<'tcx>,
) -> Option<String> {
    let inst = ty::Instance::resolve_drop_in_place(mir.state.tcx, ty);
    if let ty::InstanceDef::DropGlue(_, None) = inst.def {
        // `None` instead of a `Ty` indicates this drop glue is a no-op.
        return None;
    }
    mir.used.instances.insert(inst);
    Some(inst_id_str(mir.state.tcx, inst))
}

impl ToJson<'_> for hir::def_id::DefId {
    fn to_json(&self, mir: &mut MirState) -> serde_json::Value {
        json!(def_id_str(mir.state.tcx, *self))
    }
}

/// rustc's vtables have null entries for non-object-safe methods (those with `Where Self: Sized`).
/// We omit such methods from our vtables.  This function adjusts vtable indices from rustc's way
/// of counting to ours.
fn adjust_method_index<'tcx>(
    tcx: TyCtxt<'tcx>,
    tref: ty::Binder<'tcx, ty::TraitRef<'tcx>>,
    raw_idx: usize,
) -> usize {
    let methods = tcx.vtable_entries(tref);
    methods.iter().take(raw_idx)
        .filter(|m| matches!(m, ty::vtable::VtblEntry::Method(_)))
        .count()
}

impl<'tcx> ToJson<'tcx> for ty::Instance<'tcx> {
    fn to_json(&self, mir: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let substs = mir.state.tcx.normalize_erasing_regions(
            ty::ParamEnv::reveal_all(),
            self.substs,
        );

        match self.def {
            ty::InstanceDef::Item(did) => json!({
                "kind": "Item",
                "def_id": did.did.to_json(mir),
                "substs": substs.to_json(mir),
            }),
            ty::InstanceDef::Intrinsic(did) => json!({
                "kind": "Intrinsic",
                "def_id": did.to_json(mir),
                "substs": substs.to_json(mir),
            }),
            ty::InstanceDef::VTableShim(did) => json!({
                "kind": "VTableShim",
                "def_id": did.to_json(mir),
                "substs": substs.to_json(mir),
            }),
            ty::InstanceDef::ReifyShim(did) => json!({
                "kind": "ReifyShim",
                "def_id": did.to_json(mir),
                "substs": substs.to_json(mir),
            }),
            ty::InstanceDef::FnPtrShim(did, ty) => json!({
                "kind": "FnPtrShim",
                "def_id": did.to_json(mir),
                "substs": substs.to_json(mir),
                "ty": ty.to_json(mir),
            }),
            ty::InstanceDef::Virtual(did, idx) => {
                let self_ty = substs.types().next()
                    .unwrap_or_else(|| panic!("expected self type in substs for {:?}", self));
                let preds = match *self_ty.kind() {
                    ty::TyKind::Dynamic(ref preds, _region, _dynkind) => preds,
                    _ => panic!("expected `dyn` self type, but got {:?}", self_ty),
                };
                let ex_tref = match preds.principal() {
                    Some(x) => x,
                    None => panic!("no principal trait for {:?}?", self_ty),
                };
                let tref = ex_tref.with_self_ty(mir.state.tcx, self_ty);

                let erased_tref = mir.state.tcx.erase_late_bound_regions(tref);
                let ti = TraitInst::from_trait_ref(mir.state.tcx, erased_tref);
                let trait_name = trait_inst_id_str(mir.state.tcx, &ti);
                mir.used.traits.insert(ti);

                json!({
                    "kind": "Virtual",
                    "trait_id": trait_name,
                    "item_id": did.to_json(mir),
                    "index": adjust_method_index(mir.state.tcx, tref, idx),
                })
            },
            ty::InstanceDef::ClosureOnceShim { call_once, .. } => json!({
                "kind": "ClosureOnceShim",
                "call_once": call_once.to_json(mir),
                "substs": substs.to_json(mir),
            }),
            ty::InstanceDef::DropGlue(did, ty) => json!({
                "kind": "DropGlue",
                "def_id": did.to_json(mir),
                "substs": substs.to_json(mir),
                "ty": ty.to_json(mir),
            }),
            ty::InstanceDef::CloneShim(did, ty) => {
                let sub_tys = match *ty.kind() {
                    ty::TyKind::Array(t, _) => vec![t],
                    ty::TyKind::Tuple(ts) => ts[..].to_owned(),
                    ty::TyKind::Closure(_closure_did, substs) =>
                        substs.as_closure().upvar_tys().collect(),
                    _ => {
                        eprintln!("warning: don't know how to build clone shim for {:?}", ty);
                        vec![]
                    },
                };
                let callees = sub_tys.into_iter()
                    .map(|ty| {
                        let inst = ty::Instance::resolve(
                            mir.state.tcx,
                            ty::ParamEnv::reveal_all(),
                            did,
                            mir.state.tcx.intern_substs(&[ty.into()]),
                        ).unwrap_or_else(|_| {
                            panic!("failed to resolve instance: {:?}, {:?}", did, ty);
                        });
                        if let Some(inst) = inst {
                            // Add the callee to `used.insances`, so we'll emit code for it even if
                            // it's otherwise unused.  If `inst` is itself a `CloneShim`, its own
                            // callees will be visited when generating the "intrinsics" entry for
                            // `inst`.
                            mir.used.instances.insert(inst.clone());
                        }
                        inst.map(|i| inst_id_str(mir.state.tcx, i))
                    }).collect::<Vec<_>>();
                json!({
                    "kind": "CloneShim",
                    "def_id": did.to_json(mir),
                    "substs": substs.to_json(mir),
                    "ty": ty.to_json(mir),
                    "callees": callees.to_json(mir),
                })
            },
        }
    }
}

// For type _references_. To translate ADT defintions, do it explicitly.
impl<'tcx> ToJson<'tcx> for ty::Ty<'tcx> {
    fn to_json(&self, mir: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let tcx = mir.state.tcx;

        // If this type has already been interned, just return its ID.
        if let Some(id) = mir.tys.get(*self) {
            return json!(id);
        }

        // Otherwise, convert the type to JSON and add the new entry to the interning table.
        let j = match self.kind() {
            &ty::TyKind::Bool => {
                json!({"kind": "Bool"})
            }
            &ty::TyKind::Char => {
                json!({"kind": "Char"})
            }
            &ty::TyKind::Int(t) => {
                json!({"kind": "Int", "intkind": t.to_json(mir)})
            }
            &ty::TyKind::Uint(t) => {
                json!({"kind": "Uint", "uintkind": t.to_json(mir)})
            }
            &ty::TyKind::Tuple(ref sl) => {
                json!({"kind": "Tuple", "tys": sl.to_json(mir)})
            }
            &ty::TyKind::Slice(ref f) => {
                json!({"kind": "Slice", "ty": f.to_json(mir)})
            }
            &ty::TyKind::Str => {
                json!({"kind": "Str"})
            }
            &ty::TyKind::Float(sz) => {
                json!({"kind": "Float", "size": sz.to_json(mir)})
            }
            &ty::TyKind::Array(ref t, ref size) => {
                json!({"kind": "Array", "ty": t.to_json(mir), "size": size.to_json(mir)})
            }
            &ty::TyKind::Ref(ref _region, ref ty, ref mtbl) => {
                json!({
                    "kind": "Ref",
                    "ty": ty.to_json(mir),
                    "mutability": mtbl.to_json(mir)
                })
            }
            &ty::TyKind::RawPtr(ref tm) => {
                json!({
                    "kind": "RawPtr",
                    "ty": tm.ty.to_json(mir),
                    "mutability": tm.mutbl.to_json(mir)
                })
            }
            &ty::TyKind::Adt(adtdef, substs) => {
                let ai = AdtInst::new(adtdef, substs);
                mir.used.types.insert(ai);
                json!({
                    "kind": "Adt",
                    "name": adt_inst_id_str(mir.state.tcx, ai),
                    "orig_def_id": adtdef.did().to_json(mir),
                    "substs": substs.to_json(mir),
                })
            }
            &ty::TyKind::FnDef(defid, ref substs) => {
                let name = get_fn_def_name(mir, defid, substs);
                json!({
                    "kind": "FnDef",
                    "defid": name,
                })
            }
            &ty::TyKind::Param(..) => unreachable!(
                "no TyKind::Param should remain after monomorphization"
            ),
            &ty::TyKind::Closure(_defid, ref substs) => {
                json!({
                    "kind": "Closure",
                    "upvar_tys": substs.as_closure().upvar_tys()
                        .collect::<Vec<_>>().to_json(mir),
                    // crucible-mir uses the same representation for closures as it does for
                    // tuples, so no additional information is needed.
                })
            }
            &ty::TyKind::Dynamic(preds, _region, dynkind) => {
                match dynkind {
                    DynKind::Dyn => {
                        let ti = TraitInst::from_dynamic_predicates(mir.state.tcx, preds);
                        let trait_name = trait_inst_id_str(mir.state.tcx, &ti);
                        mir.used.traits.insert(ti);
                        json!({
                            "kind": "Dynamic",
                            "trait_id": trait_name,
                            "predicates": preds.iter().map(|p|{
                                let p = tcx.erase_late_bound_regions(p);
                                p.to_json(mir)
                            }).collect::<Vec<_>>(),
                        })
                    },
                    DynKind::DynStar =>
                        json!({
                            "kind": "DynamicStar",
                        }),
                }
            }
            &ty::TyKind::Alias(ty::AliasKind::Projection, _) => unreachable!(
                "no TyKind::Alias with AliasKind Projection should remain after monomorphization"
            ),
            &ty::TyKind::FnPtr(ref sig) => {
                json!({"kind": "FnPtr", "signature": sig.to_json(mir)})
            }
            &ty::TyKind::Never => {
                json!({"kind": "Never"})
            }
            &ty::TyKind::Error(_) => {
                json!({"kind": "Error"})
            }
            &ty::TyKind::Infer(_) => {
                // TODO
                json!({"kind": "Infer"})
            }
            &ty::TyKind::Bound(_, _) => {
                // TODO
                json!({"kind": "Bound"})
            }
            &ty::TyKind::Placeholder(_) => {
                // TODO
                json!({"kind": "Placeholder"})
            }
            &ty::TyKind::Foreign(_) => {
                // TODO
                json!({"kind": "Foreign"})
            }
            &ty::TyKind::Generator(_, _, _) => {
                // TODO
                json!({"kind": "Generator"})
            }
            &ty::TyKind::GeneratorWitness(_) => {
                // TODO
                json!({"kind": "GeneratorWitness"})
            }
            &ty::TyKind::Alias(ty::AliasKind::Opaque, _) => {
                // TODO
                json!({"kind": "Alias"})
            }
        };

        let id = mir.tys.insert(*self, j);
        json!(id)
    }
}

impl ToJson<'_> for ty::ParamTy {
    fn to_json(&self, _mir: &mut MirState) -> serde_json::Value {
        json!(self.index)
    }
}

impl<'tcx> ToJson<'tcx> for ty::PolyFnSig<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let sig = ms.state.tcx.erase_late_bound_regions(*self);
        sig.to_json(ms)
    }
}

impl<'tcx> ToJson<'tcx> for ty::FnSig<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let input_jsons : Vec<serde_json::Value> =
            self.inputs().iter().map(|i| i.to_json(ms)).collect();
        json!({
            "inputs": input_jsons,
            "output": self.output().to_json(ms),
            "abi": self.abi.to_json(ms),
        })
    }
}

impl<'tcx> ToJson<'tcx> for ty::TraitRef<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        json!({
            "trait":  self.def_id.to_json(ms),
            "substs":  self.substs.to_json(ms)
        })
    }
}

impl<'tcx> ToJson<'tcx> for ty::AliasTy<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        json!({
            "substs": self.substs.to_json(ms),
            "def_id": self.def_id.to_json(ms)
        })
    }
}

// Predicate (static / `where` clause)

impl<'tcx> ToJson<'tcx> for ty::Predicate<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        match self.kind().skip_binder() {
            ty::PredicateKind::Clause(ty::Clause::Trait(tp)) => {
                json!({
                    "trait_pred": tp.trait_ref.to_json(ms)
                })
            }
            ty::PredicateKind::Clause(ty::Clause::Projection(pp)) => match pp.term.unpack() {
                ty::TermKind::Ty(ty) => json!({
                    "projection_ty": pp.projection_ty.to_json(ms),
                    "ty": ty.to_json(ms),
                }),
                ty::TermKind::Const(_) => json!("unknown_const_projection"),
            }
            _ => {
                json!("unknown_pred")
            }
        }
    }
}

// Existential predicate (dynamic / trait object version of `ty::Predicate`)

impl<'tcx> ToJson<'tcx> for ty::ExistentialPredicate<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        match self {
            &ty::ExistentialPredicate::Trait(ref trait_ref) => {
                json!({
                    "kind": "Trait",
                    "trait": trait_ref.def_id.to_json(ms),
                    "substs": trait_ref.substs.to_json(ms),
                })
            },
            &ty::ExistentialPredicate::Projection(ref proj) => match proj.term.unpack() {
                ty::TermKind::Ty(ty) => json!({
                    "kind": "Projection",
                    "proj": proj.def_id.to_json(ms),
                    "substs": proj.substs.to_json(ms),
                    "rhs_ty": ty.to_json(ms),
                }),
                ty::TermKind::Const(_) => json!({
                    "kind": "Projection_Const",
                }),
            },
            &ty::ExistentialPredicate::AutoTrait(ref did) => {
                json!({
                    "kind": "AutoTrait",
                    "trait": did.to_json(ms),
                })
            },
        }
    }
}


impl<'tcx> ToJson<'tcx> for ty::GenericPredicates<'tcx> {
    fn to_json(&self, ms: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        fn gather_preds<'tcx>(
            ms: &mut MirState<'_, 'tcx>,
            preds: &ty::GenericPredicates<'tcx>,
            dest: &mut Vec<serde_json::Value>,
        ) {
            dest.extend(preds.predicates.iter().map(|p| p.0.to_json(ms)));
            if let Some(parent_id) = preds.parent {
                let parent_preds = ms.state.tcx.predicates_of(parent_id);
                gather_preds(ms, &parent_preds, dest);
            }
        }

        let mut json_preds: Vec<serde_json::Value> = Vec::new();
        gather_preds(ms, self, &mut json_preds);
        json!({ "predicates": json_preds })
    }
}

impl ToJson<'_> for ty::GenericParamDef {
    fn to_json(&self, ms: &mut MirState) -> serde_json::Value {
        json!({
            "param_def": *(self.name.as_str()),
            "def_id": self.def_id.to_json(ms),
        }) // TODO
    }
}

impl ToJson<'_> for ty::Generics {
    fn to_json(&self, ms: &mut MirState) -> serde_json::Value {
        fn gather_params(
            ms: &mut MirState,
            generics: &ty::Generics,
            dest: &mut Vec<serde_json::Value>,
        ) {
            if let Some(parent_id) = generics.parent {
                let parent_generics = ms.state.tcx.generics_of(parent_id);
                gather_params(ms, &parent_generics, dest);
            }
            dest.extend(generics.params.iter().map(|p| p.to_json(ms)));
        }

        let mut json_params: Vec<serde_json::Value> = Vec::new();
        gather_params(ms, self, &mut json_params);
        json!({
            "params": json_params
        }) // TODO
    }
}

pub trait ToJsonAg {
    fn tojson<'tcx>(
        &self,
        mir: &mut MirState<'_, 'tcx>,
        substs: ty::subst::SubstsRef<'tcx>,
    ) -> serde_json::Value;
}

impl<'tcx> ToJson<'tcx> for ty::subst::GenericArg<'tcx> {
    fn to_json(&self, mir: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        match self.unpack() {
            ty::subst::GenericArgKind::Type(ref ty) => ty.to_json(mir),
            // In crucible-mir, all substs entries are considered "types", and there are dummy
            // TyLifetime and TyConst variants to handle non-type entries.  We emit something that
            // looks vaguely like an interned type's ID here, and handle it specially in MIR.JSON.
            ty::subst::GenericArgKind::Lifetime(_) => json!("nonty::Lifetime"),
            ty::subst::GenericArgKind::Const(_) => json!("nonty::Const"),
        }
    }
}

use self::machine::RenderConstMachine;
mod machine {
    use std::borrow::Cow;
    use super::*;
    use rustc_const_eval::interpret::*;
    use rustc_data_structures::fx::FxIndexMap;
    use rustc_middle::ty::*;
    use rustc_target::abi::Size;
    use rustc_target::spec::abi::Abi;
    pub struct RenderConstMachine<'mir, 'tcx> {
        stack: Vec<Frame<'mir, 'tcx, AllocId, ()>>,
    }

    impl<'mir, 'tcx> RenderConstMachine<'mir, 'tcx> {
        pub fn new() -> RenderConstMachine<'mir, 'tcx> {
            RenderConstMachine {
                stack: Vec::new()
            }
        }
    }

    impl<'mir, 'tcx> Machine<'mir, 'tcx> for RenderConstMachine<'mir, 'tcx> {
        type MemoryKind = !;
        type Provenance = AllocId;
        type ProvenanceExtra = ();
        type ExtraFnVal = !;
        type FrameExtra = ();
        type AllocExtra = ();
        type MemoryMap = FxIndexMap<
            AllocId,
            (MemoryKind<!>, Allocation),
        >;

        const GLOBAL_KIND: Option<Self::MemoryKind> = None;
        const PANIC_ON_ALLOC_FAIL: bool = false;

        fn enforce_alignment(_ecx: &InterpCx<'mir, 'tcx, Self>) -> CheckAlignment {
            CheckAlignment::No
        }

        fn alignment_check_failed(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            _has: Align,
            _required: Align,
            _check: CheckAlignment,
        ) -> InterpResult<'tcx, ()> {
            panic!("not implemented: alignment_check_failed");
        }

        fn use_addr_for_alignment_check(_ecx: &InterpCx<'mir, 'tcx, Self>) -> bool {
            false
        }

        #[inline(always)]
        fn checked_binop_checks_overflow(_ecx: &InterpCx<'mir, 'tcx, Self>) -> bool {
            true
        }

        fn enforce_validity(_ecx: &InterpCx<'mir, 'tcx, Self>) -> bool {
            false
        }

        fn find_mir_or_eval_fn(
            _ecx: &mut InterpCx<'mir, 'tcx, Self>,
            _instance: ty::Instance<'tcx>,
            _abi: Abi,
            _args: &[OpTy<'tcx, Self::Provenance>],
            _destination: &PlaceTy<'tcx, Self::Provenance>,
            _target: Option<mir::BasicBlock>,
            _unwind: StackPopUnwind,
        ) -> InterpResult<'tcx, Option<(&'mir mir::Body<'tcx>, ty::Instance<'tcx>)>> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "find_mir_or_eval_fn".into(),
                ),
            ).into())
        }

        fn call_extra_fn(
            _ecx: &mut InterpCx<'mir, 'tcx, Self>,
            _fn_val: Self::ExtraFnVal,
            _abi: Abi,
            _args: &[OpTy<'tcx, Self::Provenance>],
            _destination: &PlaceTy<'tcx, Self::Provenance>,
            _target: Option<mir::BasicBlock>,
            _unwind: StackPopUnwind,
        ) -> InterpResult<'tcx> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "call_extra_fn".into(),
                ),
            ).into())
        }

        fn call_intrinsic(
            _ecx: &mut InterpCx<'mir, 'tcx, Self>,
            _instance: ty::Instance<'tcx>,
            _args: &[OpTy<'tcx, Self::Provenance>],
            _destination: &PlaceTy<'tcx, Self::Provenance>,
            _target: Option<mir::BasicBlock>,
            _unwind: StackPopUnwind,
        ) -> InterpResult<'tcx> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "call_intrinsic".into(),
                ),
            ).into())
        }

        fn assert_panic(
            _ecx: &mut InterpCx<'mir, 'tcx, Self>,
            _msg: &mir::AssertMessage<'tcx>,
            _unwind: Option<mir::BasicBlock>,
        ) -> InterpResult<'tcx> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "assert_panic".into(),
                ),
            ).into())
        }

        fn binary_ptr_op(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            _bin_op: mir::BinOp,
            _left: &ImmTy<'tcx, Self::Provenance>,
            _right: &ImmTy<'tcx, Self::Provenance>,
        ) -> InterpResult<'tcx, (Scalar<Self::Provenance>, bool, Ty<'tcx>)> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "binary_ptr_op".into(),
                ),
            ).into())
        }

        fn extern_static_base_pointer(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            _def_id: DefId,
        ) -> InterpResult<'tcx, Pointer<Self::Provenance>> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "extern_static_base_pointer".into(),
                ),
            ).into())
        }

        fn adjust_alloc_base_pointer(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            ptr: Pointer,
        ) -> Pointer<Self::Provenance> {
            ptr
        }

        fn ptr_from_addr_cast(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            _addr: u64,
        ) -> InterpResult<'tcx, Pointer<Option<Self::Provenance>>> {
            unimplemented!("ptr_from_addr_cast")
        }

        fn expose_ptr(
            _ecx: &mut InterpCx<'mir, 'tcx, Self>,
            _ptr: Pointer<Self::Provenance>,
        ) -> InterpResult<'tcx> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "expose_ptr".into(),
                ),
            ).into())
        }

        fn ptr_get_alloc(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            ptr: Pointer<Self::Provenance>,
        ) -> Option<(AllocId, Size, Self::ProvenanceExtra)> {
            let (prov, offset) = ptr.into_parts();
            Some((prov, offset, ()))
        }

        fn adjust_allocation<'b>(
            _ecx: &InterpCx<'mir, 'tcx, Self>,
            _id: AllocId,
            alloc: Cow<'b, Allocation>,
            _kind: Option<MemoryKind<Self::MemoryKind>>,
        ) -> InterpResult<'tcx, Cow<'b, Allocation<Self::Provenance, Self::AllocExtra>>> {
            Ok(alloc)
        }

        fn init_frame_extra(
            _ecx: &mut InterpCx<'mir, 'tcx, Self>,
            _frame: Frame<'mir, 'tcx, Self::Provenance>,
        ) -> InterpResult<'tcx, Frame<'mir, 'tcx, Self::Provenance, Self::FrameExtra>> {
            Err(InterpError::Unsupported(
                UnsupportedOpInfo::Unsupported(
                    "init_frame_extra".into(),
                ),
            ).into())
        }

        fn stack<'a>(
            ecx: &'a InterpCx<'mir, 'tcx, Self>,
        ) -> &'a [Frame<'mir, 'tcx, Self::Provenance, Self::FrameExtra>] {
            &ecx.machine.stack
            // unimplemented!("stack")
        }

        fn stack_mut<'a>(
            _ecx: &'a mut InterpCx<'mir, 'tcx, Self>,
        ) -> &'a mut Vec<Frame<'mir, 'tcx, Self::Provenance, Self::FrameExtra>> {
            unimplemented!("stack_mut")
        }
    }
}

impl<'tcx> ToJson<'tcx> for ty::Const<'tcx> {
    fn to_json(&self, mir: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("ty".to_owned(), self.ty().to_json(mir));

        match self.kind() {
            // remove? should probably not show up?
            ty::ConstKind::Unevaluated(un) => {
                map.insert("initializer".to_owned(), json!({
                    "def_id": get_fn_def_name(mir, un.def.did, un.substs),
                }));
            },
            _ => {},
        }

        let rendered = match self.kind() {
            ty::ConstKind::Value(ty::ValTree::Leaf(val)) => {
                let sz = val.size();
                Some(json!({
                    "kind": "usize",
                    "size": sz.bytes(),
                    "val": get_const_usize(mir.state.tcx, *self).to_string(),
                }))
            }
            _ => panic!("don't know how to translate ConstKind::{:?}", self.kind())
        };
        if let Some(rendered) = rendered {
            map.insert("rendered".to_owned(), rendered);
        }

        map.into()
    }
}

impl<'tcx> ToJson<'tcx> for (interpret::ConstValue<'tcx>, ty::Ty<'tcx>) {
    fn to_json(&self, mir: &mut MirState<'_, 'tcx>) -> serde_json::Value {
        let (val, ty) = *self;
        let op_ty = as_opty(mir.state.tcx, val, ty);
        let mut icx = interpret::InterpCx::new(
            mir.state.tcx,
            DUMMY_SP,
            ty::ParamEnv::reveal_all(),
            RenderConstMachine::new(),
        );

        json!({
            "ty": ty.to_json(mir),
            "rendered": render_opty(mir, &mut icx, &op_ty),
        })
    }
}

pub fn get_const_usize<'tcx>(tcx: ty::TyCtxt<'tcx>, c: ty::Const<'tcx>) -> usize {
    match c.kind() {
        ty::ConstKind::Value(ty::ValTree::Leaf(val)) => {
            let v = val.try_to_machine_usize(tcx).unwrap();
            v as usize
        }
        _ => panic!("don't know how to translate ConstKind::{:?}", c.kind())
    }
}

pub fn render_opty<'mir, 'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    icx: &mut interpret::InterpCx<'mir, 'tcx, RenderConstMachine<'mir, 'tcx>>,
    op_ty: &interpret::OpTy<'tcx>,
) -> serde_json::Value {
    try_render_opty(mir, icx, op_ty).unwrap_or_else(|| {
        json!({
            "kind": "unsupported_const",
            "debug_val": format!("{:?}", op_ty),
        })
    })
}

pub fn try_render_opty<'mir, 'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    icx: &mut interpret::InterpCx<'mir, 'tcx, RenderConstMachine<'mir, 'tcx>>,
    op_ty: &interpret::OpTy<'tcx>,
) -> Option<serde_json::Value> {
    let ty = op_ty.layout.ty;
    let layout = op_ty.layout.layout;
    let tcx = mir.state.tcx;

    Some(match *ty.kind() {
        ty::TyKind::Bool |
        ty::TyKind::Char |
        ty::TyKind::Uint(_) =>
        {
            let s = icx.read_immediate(op_ty).unwrap().to_scalar();
            let size = layout.size();
            let bits = s.to_bits(size).unwrap();

            json!({
                "kind": match *ty.kind() {
                    ty::TyKind::Bool => "bool",
                    ty::TyKind::Char => "char",
                    ty::TyKind::Uint(ty::UintTy::Usize) => "usize",
                    ty::TyKind::Uint(_) => "uint",
                    _ => unreachable!(),
                },
                "size": size.bytes(),
                "val": bits.to_string(),
            })
        }
        ty::TyKind::Int(_i) => {
            let s = icx.read_immediate(op_ty).unwrap().to_scalar();
            let size = layout.size();
            let bits = s.to_bits(size).unwrap();
            let mut val = bits as i128;
            if bits & (1 << (size.bits() - 1)) != 0 && size.bits() < 128 {
                // Sign-extend to 128 bits
                val |= -1_i128 << size.bits();
            }

            json!({
                "kind": match *ty.kind() {
                    ty::TyKind::Int(ty::IntTy::Isize) => "isize",
                    ty::TyKind::Int(_) => "int",
                    _ => unreachable!(),
                },
                "size": size.bytes(),
                "val": val.to_string(),
            })
        }
        ty::TyKind::Float(fty) => {
            let s = icx.read_immediate(op_ty).unwrap().to_scalar();
            let size = layout.size();
            let val_str = match fty {
                ty::FloatTy::F32 => s.to_f32().unwrap().to_string(),
                ty::FloatTy::F64 => s.to_f64().unwrap().to_string(),
            };

            json!({
                "kind": "float",
                "size": size.bytes(),
                "val": val_str,
            })
        }
        ty::TyKind::Adt(adt_def, _substs) if adt_def.is_struct() => {
            let variant = adt_def.non_enum_variant();
            let mut field_vals = Vec::new();
            for field_idx in 0..variant.fields.len() {
                let field = icx.operand_field(&op_ty, field_idx).unwrap();
                field_vals.push(try_render_opty(mir, icx, &field)?)
            }

            let val: serde_json::Value = field_vals.into();

            json!({
                "kind": "struct",
                "fields": val,
            })
        },
        ty::TyKind::Adt(adt_def, _substs) if adt_def.is_enum() => {
            let (_, variant_idx) = icx.read_discriminant(&op_ty).unwrap();
            let val = icx.operand_downcast(op_ty, variant_idx).unwrap();
            let mut field_vals = Vec::with_capacity(val.layout.fields.count());
            for idx in 0 .. val.layout.fields.count() {
                let field_opty = icx.operand_field(&val, idx).unwrap();
                field_vals.push(try_render_opty(mir, icx,  &field_opty)?);
            }

            json!({
                "kind": "enum",
                "variant": variant_idx.as_u32(),
                "fields": field_vals,
            })
        },

        ty::TyKind::Adt(_, _) => {
            panic!("Adt is not enum or struct!")
        },

        ty::TyKind::Foreign(_) => todo!("foreign is unimplemented"), // can't do this
        ty::TyKind::Str => unreachable!("str type should not occur here"),
        ty::TyKind::Array(ety, sz) => {
            let sz = get_const_usize(tcx, sz);
            let mut vals = Vec::with_capacity(sz);
            for field in icx.operand_array_fields(op_ty).unwrap() {
                let f_json = try_render_opty(mir, icx, &field.unwrap());
                vals.push(f_json);
            }

            json!({
                "kind": "array",
                "element_ty": ety.to_json(mir),
                "elements": vals
            })

        }
        ty::TyKind::Slice(_) => unreachable!("slice type should not occur here"),

        // similar to ref in some ways
        ty::TyKind::RawPtr(m_ty) =>
            try_render_ref_opty(mir, icx, op_ty, m_ty.ty, m_ty.mutbl)?,

        ty::TyKind::Ref(_, rty, mutability) =>
            try_render_ref_opty(mir, icx, op_ty, rty, mutability)?,

        ty::TyKind::FnDef(_, _) => json!({"kind": "zst"}),
        ty::TyKind::FnPtr(_sig) => {
            let ptr = icx.read_pointer(op_ty).unwrap();
            let (prov, _offset) = ptr.into_parts();
            let alloc = tcx.try_get_global_alloc(prov?)?;
            match alloc {
                interpret::GlobalAlloc::Function(i) => {
                    mir.used.instances.insert(i);
                    json!({
                        "kind": "fn_ptr",
                        "instance": i.to_json(mir),

                    })
                },
                _ => unreachable!("Function pointer doesn't point to a function"),
            }
        }
        ty::TyKind::Dynamic(_, _, _) => unreachable!("dynamic should not occur here"),

        ty::TyKind::Closure(_defid, substs) => {
            let upvars_count = substs.as_closure().upvar_tys().count();
            let mut upvar_vals = Vec::with_capacity(upvars_count);
            for idx in 0 .. upvars_count {
                let upvar_opty = icx.operand_field(&op_ty, idx).unwrap();
                upvar_vals.push(try_render_opty(mir, icx, &upvar_opty)?);
            }

            json!({
                "kind": "closure",
                "upvars": upvar_vals,
            })
        }
        ty::TyKind::Generator(_, _, _) => todo!("generator not supported yet"), // not supported in haskell
        ty::TyKind::GeneratorWitness(_) => todo!("generatorwitness not supported yet"), // not supported in haskell
        ty::TyKind::Never => unreachable!("never type should be uninhabited"),

        ty::TyKind::Tuple(elts) => {
            let mut vals = Vec::with_capacity(elts.len());
            for i in 0..elts.len() {
                let fld: interpret::OpTy<'tcx> = icx.operand_field(&op_ty, i).unwrap();
                vals.push(render_opty(mir, icx, &fld));
            }
            json!({
                "kind": "tuple",
                "elements": vals
            })
        }

        // should go away during monomorphiszation but could in theory be resolvable to a real type
        ty::TyKind::Alias(_, _) => unreachable!("alias should not occur after monomorphization"),
        ty::TyKind::Param(_) => unreachable!("param should not occur after monomorphization"),

        ty::TyKind::Bound(_, _) => unreachable!("bound is not a real type?"),
        ty::TyKind::Placeholder(_) => unreachable!("placeholder is not a real type?"),
        ty::TyKind::Infer(_) => unreachable!("infer is not a real type?"),
        ty::TyKind::Error(_) => unreachable!("error is not a real type?"),
    })
}

fn make_allocation_body<'mir, 'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    icx: &mut interpret::InterpCx<'mir, 'tcx, RenderConstMachine<'mir, 'tcx>>,
    rty: ty::Ty<'tcx>,
    d: MPlaceTy<'tcx>,
    is_mut: bool,
) -> serde_json::Value {
    let tcx = mir.state.tcx;

    if !is_mut {
        match *rty.kind() {
            // Special cases for &str and &[T]
            //
            // These and the ones in try_render_ref_opty below should be
            // kept in sync.
            ty::TyKind::Str => {
                let len = mplace_ty_len(&d, icx).unwrap();
                let mem = icx.read_bytes_ptr_strip_provenance(d.ptr, Size::from_bytes(len)).unwrap();
                // corresponding array type for contents
                let elem_ty = tcx.mk_ty(ty::TyKind::Uint(ty::UintTy::U8));
                let aty = tcx.mk_array(elem_ty, len);
                let rendered = json!({
                    "kind": "strbody",
                    "elements": mem,
                    "len": len
                });
                return json!({
                    "kind": "constant",
                    "mutable": false,
                    "ty": aty.to_json(mir),
                    "rendered": rendered,
                });
            },
            ty::TyKind::Slice(slice_ty) => {
                let slice_len = mplace_ty_len(&d, icx).unwrap();
                let mut elt_values = Vec::with_capacity(slice_len as usize);
                for idx in 0..slice_len {
                    let elt = icx.operand_index(&d.into(), idx).unwrap();
                    elt_values.push(try_render_opty(mir, icx, &elt));
                }
                // corresponding array type for contents
                let aty = tcx.mk_array(slice_ty, slice_len);
                let rendered = json!({
                    // this can now be the same as an ordinary array
                    "kind": "array",
                    "elements": elt_values,
                    "len": slice_len
                });
                return json!({
                    "kind": "constant",
                    "mutable": false,
                    "ty": aty.to_json(mir),
                    "rendered": rendered,
                });
            },
            _ => ()
        }
    }

    // Default case
    let rlayout = tcx.layout_of(ty::ParamEnv::reveal_all().and(rty)).unwrap();
    let mpty = interpret::MPlaceTy::from_aligned_ptr_with_meta(d.ptr, rlayout, d.meta);
    let rendered = try_render_opty(mir, icx, &mpty.into());

    return json!({
        "kind": "constant",
        "mutable": false,
        "ty": rty.to_json(mir),
        "rendered": rendered,
    });
}

fn try_render_ref_opty<'mir, 'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    icx: &mut interpret::InterpCx<'mir, 'tcx, RenderConstMachine<'mir, 'tcx>>,
    op_ty: &interpret::OpTy<'tcx>,
    rty: ty::Ty<'tcx>,
    mutability: hir::Mutability,
) -> Option<serde_json::Value> {
    let tcx = mir.state.tcx;

    // Special case for nullptr
    let val = icx.read_immediate(op_ty).unwrap();
    let mplace = icx.ref_to_mplace(&val).unwrap();
    let (prov, offset) = mplace.ptr.into_parts();
    if prov.is_none() {
        assert!(!mplace.meta.has_meta(), "not expecting meta for nullptr");

        return Some(json!({
            "kind": "raw_ptr",
            "val": offset.bytes().to_string(),
        }));
    }

    let d = icx.deref_operand(op_ty).unwrap();
    let is_mut = mutability == hir::Mutability::Mut;

    let (prov, d_offset) = d.ptr.into_parts();
    assert!(d_offset == Size::ZERO, "cannot handle nonzero reference offsets");
    let alloc = tcx.try_get_global_alloc(prov?)?;

    let def_id_json = match alloc {
        interpret::GlobalAlloc::Static(def_id) =>
            def_id.to_json(mir),
        interpret::GlobalAlloc::Memory(ca) => {
            let ty = op_ty.layout.ty;
            let def_id_str = match mir.allocs.get(ca, ty) {
                Some(alloc_id) => alloc_id.to_owned(),
                None => {
                    // create the allocation
                    let body = make_allocation_body(mir, icx, rty, d, is_mut);
                    mir.allocs.insert(tcx, ca, ty, body)
                }
            };
            def_id_str.to_json(mir)
        }
        _ =>
            // Give up
            return None
    };

    if !is_mut {
        match *rty.kind() {
            // Special cases for &str and &[T]
            //
            // These and the ones in make_allocation_body above should be
            // kept in sync.
            ty::TyKind::Str | ty::TyKind::Slice(_) => {
                let len = mplace_ty_len(&d, icx).unwrap();
                return Some(json!({
                    "kind": "slice",
                    "def_id": def_id_json,
                    "len": len
                }))
            },
            _ => ()
        }
    }

    return Some(json!({
        "kind": "static_ref",
        "def_id": def_id_json,
    }));
}

// A copied version of MPlaceTy::len, which (sadly) isn't exported. See
// https://github.com/rust-lang/rust/blob/56ee85274e5a3a4dda92f3bf73d1664c74ff9c15/compiler/rustc_const_eval/src/interpret/place.rs#L227C5-L243
#[inline]
pub fn mplace_ty_len<'tcx, Tag: Provenance>(mplace_ty: &MPlaceTy<'tcx, Tag>, cx: &impl HasDataLayout) -> InterpResult<'tcx, u64> {
    if mplace_ty.layout.is_unsized() {
        // We need to consult `meta` metadata
        match mplace_ty.layout.ty.kind() {
            ty::Slice(..) | ty::Str => mplace_ty.meta.unwrap_meta().to_machine_usize(cx),
            _ => bug!("len not supported on unsized type {:?}", mplace_ty.layout.ty),
        }
    } else {
        // Go through the layout.  There are lots of types that support a length,
        // e.g., SIMD types. (But not all repr(simd) types even have FieldsShape::Array!)
        match mplace_ty.layout.fields {
            FieldsShape::Array { count, .. } => Ok(count),
            _ => bug!("len not supported on sized type {:?}", mplace_ty.layout.ty),
        }
    }
}

pub fn as_opty<'tcx>(tcx: TyCtxt<'tcx>, cv: interpret::ConstValue<'tcx>, ty: ty::Ty<'tcx>)
    -> interpret::OpTy<'tcx, interpret::AllocId>
{
    use rustc_const_eval::interpret::{Operand, Pointer, MemPlace, ConstValue, Immediate, Scalar, ImmTy};
    let op = match cv {
        ConstValue::ByRef { alloc, offset } => {
            let id = tcx.create_memory_alloc(alloc);
            // We rely on mutability being set correctly in that allocation to prevent writes
            // where none should happen.
            let ptr = Pointer::new(id, offset);
            Operand::Indirect(MemPlace::from_ptr(ptr.into()))
        }
        ConstValue::Scalar(x) => Operand::Immediate(x.into()),
        ConstValue::ZeroSized => Operand::Immediate(Immediate::Uninit),
        ConstValue::Slice { data, start, end } => {
            // We rely on mutability being set correctly in `data` to prevent writes
            // where none should happen.
            let ptr = Pointer::new(
                tcx.create_memory_alloc(data),
                Size::from_bytes(start), // offset: `start`
            );
            Operand::Immediate(Immediate::new_slice(
                Scalar::from_pointer(ptr, &tcx),
                u64::try_from(end.checked_sub(start).unwrap()).unwrap(), // len: `end - start`
                &tcx,
            ))
        }
    };

    let layout = tcx.layout_of(ty::ParamEnv::reveal_all().and(ty)).unwrap();

    match op {
        Operand::Immediate(imm) => ImmTy::from_immediate(imm, layout).into() ,
        Operand::Indirect(ind) => MPlaceTy::from_aligned_ptr(ind.ptr, layout).into(),
    }
}

fn iter_tojson<'a, 'tcx, I, V: 'a>(
    it: I,
    mir: &mut MirState<'_, 'tcx>,
    substs: ty::subst::SubstsRef<'tcx>,
) -> serde_json::Value
where I: Iterator<Item = &'a V>, V: ToJsonAg {
    let mut j = Vec::with_capacity(it.size_hint().0);
    for v in it {
        j.push(v.tojson(mir, substs));
    }
    json!(j)
}

impl<T> ToJsonAg for [T]
where
    T: ToJsonAg,
{
    fn tojson<'tcx>(
        &self,
        mir: &mut MirState<'_, 'tcx>,
        substs: ty::subst::SubstsRef<'tcx>,
    ) -> serde_json::Value {
        iter_tojson(self.iter(), mir, substs)
    }
}

impl<T> ToJsonAg for Vec<T>
where
    T: ToJsonAg,
{
    fn tojson<'tcx>(
        &self,
        mir: &mut MirState<'_, 'tcx>,
        substs: ty::subst::SubstsRef<'tcx>,
    ) -> serde_json::Value {
        <[T] as ToJsonAg>::tojson(self, mir, substs)
    }
}

impl<I, T> ToJsonAg for IndexVec<I, T>
where
    I: Idx,
    T: ToJsonAg,
{
    fn tojson<'tcx>(
        &self,
        mir: &mut MirState<'_, 'tcx>,
        substs: ty::subst::SubstsRef<'tcx>,
    ) -> serde_json::Value {
        iter_tojson(self.iter(), mir, substs)
    }
}

pub fn is_adt_ak(ak: &mir::AggregateKind) -> bool {
    match ak {
        &mir::AggregateKind::Adt(_, _, _, _, _) => true,
        _ => false,
    }
}

impl<'tcx> ToJson<'tcx> for AdtInst<'tcx> {
    fn to_json(
        &self,
        mir: &mut MirState<'_, 'tcx>,
    ) -> serde_json::Value {
        let ty = mir.state.tcx.mk_adt(self.adt, self.substs);
        let tyl = mir.state.tcx.layout_of(ty::ParamEnv::reveal_all().and(ty))
            .unwrap_or_else(|e| panic!("failed to get layout of {:?}: {}", ty, e));

        let kind = match self.adt.adt_kind() {
            AdtKind::Struct => json!({"kind": "Struct"}),
            AdtKind::Enum =>
                json!({
                    "kind": "Enum",
                    "discr_ty": self.adt
                                    .repr()
                                    .discr_type()
                                    .to_ty(mir.state.tcx)
                                    .to_json(mir)
                }),
            AdtKind::Union => json!({"kind": "Union"}),
        };

        let variants =
            if self.adt.is_enum() {
                render_enum_variants(mir, &self)
            } else {
                self.adt.variants()
                        .iter()
                        .map(|v| render_variant(mir, &self, v, &None))
                        .collect::<Vec<serde_json::Value>>()
                        .into()
            };

        json!({
            "name": adt_inst_id_str(mir.state.tcx, *self),
            "kind": kind,
            "variants": variants,
            "size": tyl.size.bytes(),
            "repr_transparent": self.adt.repr().transparent(),
            "orig_def_id": self.adt.did().to_json(mir),
            "orig_substs": self.substs.to_json(mir),
        })
    }
}

fn render_enum_variants<'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    adt: &AdtInst<'tcx>,
) -> serde_json::Value {
    let mut variants = Vec::with_capacity(adt.adt.variants().len());
    for (idx, d_value) in adt.adt.discriminants(mir.state.tcx) {
        let v = adt.adt.variant(idx);
        let rendered = render_variant(mir, adt, v, &Some(d_value.to_string()));
        variants.push(rendered);
    }

    variants.into()
}

fn render_variant<'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    adt: &AdtInst<'tcx>,
    v: &ty::VariantDef,
    mb_discr: &Option<String>
) -> serde_json::Value {
    let tcx = mir.state.tcx;
    let inhabited = v.inhabited_predicate(tcx, adt.adt)
                     .subst(tcx, adt.substs)
                     .apply_ignore_module(tcx, ty::ParamEnv::reveal_all());

    json!({
        "name": v.def_id.to_json(mir),
        "discr": v.discr.to_json(mir),
        "fields": v.fields.tojson(mir, adt.substs),
        "ctor_kind": v.ctor_kind().to_json(mir),
        "discr_value": mb_discr,
        "inhabited": inhabited,
    })
}

impl ToJsonAg for ty::FieldDef {
    fn tojson<'tcx>(
        &self,
        mir: &mut MirState<'_, 'tcx>,
        substs: ty::subst::SubstsRef<'tcx>,
    ) -> serde_json::Value {
        let unsubst_ty = mir.state.tcx.type_of(self.did);
        let ty = mir.state.tcx.subst_and_normalize_erasing_regions(
            substs, ty::ParamEnv::reveal_all(), unsubst_ty);
        json!({
            "name": self.did.to_json(mir),
            "ty": ty.to_json(mir),
        })
    }
}

pub fn handle_adt_ag<'tcx>(
    mir: &mut MirState<'_, 'tcx>,
    ak: &mir::AggregateKind<'tcx>,
    opv: &Vec<mir::Operand<'tcx>>,
) -> serde_json::Value {
    match ak {
        &mir::AggregateKind::Adt(adt_did, variant, substs, _, _) => {
            let adt = mir.state.tcx.adt_def(adt_did);
            json!({
                "adt": AdtInst::new(adt, substs).to_json(mir),
                "variant": variant.to_json(mir),
                "ops": opv.to_json(mir)
            })
        }
        _ => unreachable!("bad"),
    }
}

// Based on `rustc_codegen_ssa::mir::FunctionCx::eval_mir_constant`
pub fn eval_mir_constant<'tcx>(
    tcx: TyCtxt<'tcx>,
    constant: &mir::Constant<'tcx>,
) -> interpret::ConstValue<'tcx> {
    let uv = match constant.literal {
        mir::ConstantKind::Ty(ct) => match ct.kind() {
            ty::ConstKind::Unevaluated(uv) => uv.expand(),
            ty::ConstKind::Value(val) => {
                return tcx.valtree_to_const_val((ct.ty(), val));
            }
            err => panic!(
                "encountered bad ConstKind after monomorphizing: {:?} span:{:?}",
                err, constant.span
            ),
        },
        mir::ConstantKind::Unevaluated(uv, _) => uv,
        mir::ConstantKind::Val(val, _) => return val,
    };

    tcx.const_eval_resolve(ty::ParamEnv::reveal_all(), uv, None).unwrap()
}
