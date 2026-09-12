use rustc_hir as hir;
use rustc_hir::intravisit::Visitor;

use super::Encoder;

impl<'tcx> Encoder<'tcx> {
    pub(super) fn assembly(&mut self, assembly: &'tcx hir::InlineAsm<'tcx>) {
        self.text("assembly");
        self.scalar(assembly.asm_macro);
        self.scalar(assembly.options);
        for piece in assembly.template {
            self.tag(piece);
            match piece {
                rustc_ast::InlineAsmTemplatePiece::String(text) => self.text(text),
                rustc_ast::InlineAsmTemplatePiece::Placeholder {
                    operand_idx,
                    modifier,
                    ..
                } => {
                    self.scalar(operand_idx);
                    self.scalar(modifier);
                }
            }
        }
        self.end();
        for operand in assembly.operands {
            self.tag(&operand.0);
            self.scalar(operand.0.reg());
            match &operand.0 {
                hir::InlineAsmOperand::In { expr, .. } | hir::InlineAsmOperand::SymFn { expr } => {
                    self.visit_expr(expr)
                }
                hir::InlineAsmOperand::Out { late, expr, .. } => {
                    self.scalar(late);
                    self.scalar(expr.is_some());
                    if let Some(expr) = expr {
                        self.visit_expr(expr);
                    }
                }
                hir::InlineAsmOperand::InOut { late, expr, .. } => {
                    self.scalar(late);
                    self.visit_expr(expr);
                }
                hir::InlineAsmOperand::SplitInOut {
                    late,
                    in_expr,
                    out_expr,
                    ..
                } => {
                    self.scalar(late);
                    self.visit_expr(in_expr);
                    self.scalar(out_expr.is_some());
                    if let Some(expr) = out_expr {
                        self.visit_expr(expr);
                    }
                }
                hir::InlineAsmOperand::Const { anon_const } => self.visit_inline_const(anon_const),
                hir::InlineAsmOperand::SymStatic { def_id, .. } => self.reference(*def_id),
                hir::InlineAsmOperand::Label { block } => self.visit_block(block),
            }
            self.end();
        }
        self.end();
    }
}
