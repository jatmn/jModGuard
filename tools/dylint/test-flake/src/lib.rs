#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use std::collections::HashMap;

use rustc_errors::{Diag, DiagCtxtHandle, Diagnostic};
use rustc_hir::def_id::{DefId, LocalModId};
use rustc_hir::{Expr, ExprKind, ItemKind, Node, StmtKind, find_attr};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::ty;
use rustc_span::Symbol;

dylint_linting::impl_late_lint! {
    pub TEST_FLAKE,
    Warn,
    "real clocks, fixed waits and assertion retries in tests",
    TestFlake::default()
}

#[derive(Default)]
pub struct TestFlake {
    tests: HashMap<LocalModId, HashMap<Symbol, bool>>,
}

impl<'tcx> LateLintPass<'tcx> for TestFlake {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let Some(def_id) = function(cx, expr) else {
            return;
        };
        if def_id.is_local()
            || !matches!(
                cx.tcx.item_name(def_id).as_str(),
                "now" | "sleep" | "catch_unwind"
            )
        {
            return;
        }
        let path = ty::print::with_no_trimmed_paths!(cx.tcx.def_path_str(def_id));
        let (kind, help) = match path.as_str() {
            "std::thread::sleep" | "tokio::time::sleep" => {
                ("sleep", "wait on a signal or use paused Tokio time")
            }
            "std::time::SystemTime::now"
            | "std::time::Instant::now"
            | "tokio::time::Instant::now"
            | "chrono::Utc::now" => ("clock", "use an injected or paused clock"),
            "std::panic::catch_unwind" => ("retry", "let the first failed assertion fail the test"),
            _ => return,
        };

        // rustc expands both test attributes into a harness entry with a test marker.
        // Index those entries per module, then associate expressions with their test body.
        let module = cx.tcx.parent_module(expr.hir_id);
        let tests = self.tests.entry(module).or_insert_with(|| {
            cx.tcx
                .hir_module_free_items(module)
                .filter_map(|id| {
                    let item = cx.tcx.hir_item(id);
                    if !find_attr!(cx.tcx, item.hir_id(), RustcTestMarker(..)) {
                        return None;
                    }
                    match item.kind {
                        ItemKind::Const(ident, ..) | ItemKind::Static(_, ident, ..) => {
                            Some((ident.name, starts_paused(cx, item.span)))
                        }
                        _ => None,
                    }
                })
                .collect()
        });
        if tests.is_empty() {
            return;
        }
        let Some((test, starts_paused)) =
            cx.tcx.hir_parent_iter(expr.hir_id).find_map(|(_, node)| {
                if let Node::Item(item) = node
                    && let ItemKind::Fn { ident, .. } = item.kind
                    && let Some(&starts_paused) = tests.get(&ident.name)
                {
                    return Some((item, starts_paused));
                }
                None
            })
        else {
            return;
        };

        if path.starts_with("tokio::") && paused(cx, expr, starts_paused) {
            return;
        }
        if kind == "retry"
            && !cx.tcx.hir_parent_iter(expr.hir_id).any(|(_, node)| {
                matches!(
                    node,
                    Node::Expr(Expr {
                        kind: ExprKind::Loop(..),
                        ..
                    })
                )
            })
        {
            return;
        }

        let qualified = cx.tcx.def_path_str(test.owner_id.def_id);
        cx.emit_span_lint(
            TEST_FLAKE,
            expr.span.source_callsite(),
            FlakeDiagnostic(format!("{kind} in test {qualified}: {help} ({path})")),
        );
    }
}

struct FlakeDiagnostic(String);

impl<'a> Diagnostic<'a, ()> for FlakeDiagnostic {
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: rustc_errors::Level) -> Diag<'a, ()> {
        Diag::new(dcx, level, self.0)
    }
}

fn function(cx: &LateContext<'_>, expr: &Expr<'_>) -> Option<DefId> {
    if !matches!(expr.kind, ExprKind::Path(_)) {
        return None;
    }
    match *cx.typeck_results().expr_ty(expr).kind() {
        ty::FnDef(id, _) => Some(id),
        _ => None,
    }
}

fn paused(cx: &LateContext<'_>, expr: &Expr<'_>, starts_paused: bool) -> bool {
    // Inspect preceding statements in the enclosing blocks. A conditional pause
    // in another branch or later in this block cannot license this use.
    for (_, node) in cx.tcx.hir_parent_iter(expr.hir_id) {
        let Node::Block(block) = node else {
            continue;
        };
        for statement in block.stmts.iter().rev() {
            if statement.span.hi() > expr.span.lo() {
                continue;
            }
            if let StmtKind::Semi(statement) | StmtKind::Expr(statement) = statement.kind
                && let ExprKind::Call(callee, _) = statement.kind
                && let Some(id) = function(cx, callee)
            {
                match ty::print::with_no_trimmed_paths!(cx.tcx.def_path_str(id)).as_str() {
                    "tokio::time::pause" => return true,
                    "tokio::time::resume" => return false,
                    _ => {}
                }
            }
        }
    }

    starts_paused
}

fn starts_paused(cx: &LateContext<'_>, test_span: rustc_span::Span) -> bool {
    // The harness entry retains the attribute expansion even though the test
    // function body itself has an ordinary source span.
    test_span.macro_backtrace().any(|expansion| {
        cx.sess()
            .source_map()
            .span_to_snippet(expansion.call_site)
            .is_ok_and(|attr| {
                attr.split_whitespace()
                    .collect::<String>()
                    .contains("start_paused=true")
            })
    })
}
