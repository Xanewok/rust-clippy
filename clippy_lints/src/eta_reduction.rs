use crate::utils::{is_adjusted, iter_input_pats, snippet_opt, span_lint_and_then, type_is_unsafe_function};
use if_chain::if_chain;
use rustc::hir::*;
use rustc::lint::{in_external_macro, LateContext, LateLintPass, LintArray, LintContext, LintPass};
use rustc::ty;
use rustc::{declare_tool_lint, lint_array};
use rustc_errors::Applicability;

pub struct EtaPass;

/// **What it does:** Checks for closures which just call another function where
/// the function can be called directly. `unsafe` functions or calls where types
/// get adjusted are ignored.
///
/// **Why is this bad?** Needlessly creating a closure adds code for no benefit
/// and gives the optimizer more work.
///
/// **Known problems:** If creating the closure inside the closure has a side-
/// effect then moving the closure creation out will change when that side-
/// effect runs.
/// See https://github.com/rust-lang/rust-clippy/issues/1439 for more
/// details.
///
/// **Example:**
/// ```rust
/// xs.map(|x| foo(x))
/// ```
/// where `foo(_)` is a plain function that takes the exact argument type of
/// `x`.
declare_clippy_lint! {
    pub REDUNDANT_CLOSURE,
    style,
    "redundant closures, i.e. `|a| foo(a)` (which can be written as just `foo`)"
}

impl LintPass for EtaPass {
    fn get_lints(&self) -> LintArray {
        lint_array!(REDUNDANT_CLOSURE)
    }

    fn name(&self) -> &'static str {
        "EtaReduction"
    }
}

impl<'a, 'tcx> LateLintPass<'a, 'tcx> for EtaPass {
    fn check_expr(&mut self, cx: &LateContext<'a, 'tcx>, expr: &'tcx Expr) {
        if in_external_macro(cx.sess(), expr.span) {
            return;
        }

        match expr.node {
            ExprKind::Call(_, ref args) | ExprKind::MethodCall(_, _, ref args) => {
                for arg in args {
                    check_closure(cx, arg)
                }
            },
            _ => (),
        }
    }
}

fn check_closure(cx: &LateContext<'_, '_>, expr: &Expr) {
    if let ExprKind::Closure(_, ref decl, eid, _, _) = expr.node {
        let body = cx.tcx.hir().body(eid);
        let ex = &body.value;

        if_chain!(
            if let ExprKind::Call(ref caller, ref args) = ex.node;

            // Not the same number of arguments, there is no way the closure is the same as the function return;
            if args.len() == decl.inputs.len();

            // Are the expression or the arguments type-adjusted? Then we need the closure
            if !(is_adjusted(cx, ex) || args.iter().any(|arg| is_adjusted(cx, arg)));

            let fn_ty = cx.tables.expr_ty(caller);
            if !type_is_unsafe_function(cx, fn_ty);

            if compare_inputs(&mut iter_input_pats(decl, body), &mut args.into_iter());

            then {
                span_lint_and_then(cx, REDUNDANT_CLOSURE, expr.span, "redundant closure found", |db| {
                    if let Some(snippet) = snippet_opt(cx, caller.span) {
                        db.span_suggestion(
                            expr.span,
                            "remove closure as shown",
                            snippet,
                            Applicability::MachineApplicable,
                        );
                    }
                });
            }
        );

        if_chain!(
            if let ExprKind::MethodCall(ref path, _, ref args) = ex.node;

            // Not the same number of arguments, there is no way the closure is the same as the function return;
            if args.len() == decl.inputs.len();

            // Are the expression or the arguments type-adjusted? Then we need the closure
            if !(is_adjusted(cx, ex) || args.iter().skip(1).any(|arg| is_adjusted(cx, arg)));

            let method_def_id = cx.tables.type_dependent_defs()[ex.hir_id].def_id();
            if !type_is_unsafe_function(cx, cx.tcx.type_of(method_def_id));

            if compare_inputs(&mut iter_input_pats(decl, body), &mut args.into_iter());

            if let Some(name) = get_ufcs_type_name(cx, method_def_id, &args[0]);

            then {
                span_lint_and_then(cx, REDUNDANT_CLOSURE, expr.span, "redundant closure found", |db| {
                    db.span_suggestion(
                        expr.span,
                        "remove closure as shown",
                        format!("{}::{}", name, path.ident.name),
                        Applicability::MachineApplicable,
                    );
                });
            }
        );
    }
}

/// Tries to determine the type for universal function call to be used instead of the closure
fn get_ufcs_type_name(
    cx: &LateContext<'_, '_>,
    method_def_id: def_id::DefId,
    self_arg: &Expr,
) -> std::option::Option<String> {
    let expected_type_of_self = &cx.tcx.fn_sig(method_def_id).inputs_and_output().skip_binder()[0].sty;
    let actual_type_of_self = &cx.tables.node_type(self_arg.hir_id).sty;

    if let Some(trait_id) = cx.tcx.trait_of_item(method_def_id) {
        if match_borrow_depth(expected_type_of_self, actual_type_of_self) {
            return Some(cx.tcx.item_path_str(trait_id));
        }
    }

    cx.tcx.impl_of_method(method_def_id).and_then(|_| {
        //a type may implicitly implement other type's methods (e.g. Deref)
        if match_types(expected_type_of_self, actual_type_of_self) {
            return Some(get_type_name(cx, &actual_type_of_self));
        }
        None
    })
}

fn match_borrow_depth(lhs: &ty::TyKind<'_>, rhs: &ty::TyKind<'_>) -> bool {
    match (lhs, rhs) {
        (ty::Ref(_, t1, _), ty::Ref(_, t2, _)) => match_borrow_depth(&t1.sty, &t2.sty),
        (l, r) => match (l, r) {
            (ty::Ref(_, _, _), _) | (_, ty::Ref(_, _, _)) => false,
            (_, _) => true,
        },
    }
}

fn match_types(lhs: &ty::TyKind<'_>, rhs: &ty::TyKind<'_>) -> bool {
    match (lhs, rhs) {
        (ty::Bool, ty::Bool)
        | (ty::Char, ty::Char)
        | (ty::Int(_), ty::Int(_))
        | (ty::Uint(_), ty::Uint(_))
        | (ty::Str, ty::Str) => true,
        (ty::Ref(_, t1, _), ty::Ref(_, t2, _))
        | (ty::Array(t1, _), ty::Array(t2, _))
        | (ty::Slice(t1), ty::Slice(t2)) => match_types(&t1.sty, &t2.sty),
        (ty::Adt(def1, _), ty::Adt(def2, _)) => def1 == def2,
        (_, _) => false,
    }
}

fn get_type_name(cx: &LateContext<'_, '_>, kind: &ty::TyKind<'_>) -> String {
    match kind {
        ty::Adt(t, _) => cx.tcx.item_path_str(t.did),
        ty::Ref(_, r, _) => get_type_name(cx, &r.sty),
        _ => kind.to_string(),
    }
}

fn compare_inputs(closure_inputs: &mut dyn Iterator<Item = &Arg>, call_args: &mut dyn Iterator<Item = &Expr>) -> bool {
    for (closure_input, function_arg) in closure_inputs.zip(call_args) {
        if let PatKind::Binding(_, _, _, ident, _) = closure_input.pat.node {
            // XXXManishearth Should I be checking the binding mode here?
            if let ExprKind::Path(QPath::Resolved(None, ref p)) = function_arg.node {
                if p.segments.len() != 1 {
                    // If it's a proper path, it can't be a local variable
                    return false;
                }
                if p.segments[0].ident.name != ident.name {
                    // The two idents should be the same
                    return false;
                }
            } else {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}
