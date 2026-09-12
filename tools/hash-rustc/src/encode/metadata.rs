use rustc_hir::def::DefKind;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;

use super::Encoder;

impl Encoder<'_> {
    pub(super) fn metadata(&mut self, id: LocalDefId) {
        match self.tcx.def_kind(id) {
            DefKind::Fn | DefKind::AssocFn | DefKind::Static { .. } => {
                let attributes = self.tcx.codegen_fn_attrs(id);
                self.text("definition-attributes");
                self.scalar(attributes.flags);
                self.scalar(attributes.symbol_name);
                self.scalar(attributes.link_ordinal);
                self.scalar(&attributes.target_features);
                self.scalar(attributes.instruction_set);
                self.scalar(attributes.linkage);
                self.scalar(attributes.import_linkage);
                self.scalar(attributes.link_section);
                self.scalar(attributes.sanitizers);
                self.scalar(attributes.alignment);
                self.scalar(attributes.patchable_function_entry);
                self.scalar(attributes.objc_class);
                self.scalar(attributes.objc_selector);
                self.scalar(attributes.instrument_fn);
                if attributes.symbol_name.is_none()
                    && attributes.flags.intersects(
                        CodegenFnAttrFlags::FOREIGN_ITEM | CodegenFnAttrFlags::NO_MANGLE,
                    )
                {
                    self.text(self.tcx.item_name(id).as_str());
                }
                for alias in &attributes.foreign_item_symbol_aliases {
                    self.reference(alias.0);
                    self.scalar(alias.1);
                    self.scalar(alias.2);
                }
            }
            DefKind::Struct | DefKind::Union | DefKind::Enum => {
                let repr = self.tcx.adt_def(id).repr();
                self.text("representation");
                self.scalar(repr.int);
                self.scalar(repr.align);
                self.scalar(repr.pack);
                self.scalar(repr.flags);
                self.scalar(repr.scalable);
                // Layout randomization is a build setting, not definition
                // identity. Its seed includes rustc's name-based identity.
            }
            _ => {}
        }
    }
}
