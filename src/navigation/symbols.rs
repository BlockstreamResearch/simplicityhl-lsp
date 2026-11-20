use tower_lsp_server::lsp_types::{DocumentSymbol, Range, SymbolKind};

use crate::analysis::AnalysisSnapshot;
use crate::text::span_to_positions;

impl AnalysisSnapshot {
    /// Top-level functions defined in this file, with their `fn(params) -> ret` signature.
    // TODO: items inside inline `mod` blocks are missing; see the TODO on
    // `AnalysisSnapshot::from_program`.
    pub fn document_symbols(&self) -> Vec<DocumentSymbol> {
        self.functions
            .iter()
            .filter_map(|func| {
                if func.span().file_id != 0 {
                    return None;
                }

                // Get the full function range
                let (start, end) = span_to_positions(func.span(), &self.text).ok()?;
                let full_range = Range { start, end };

                // Get the function name range for selection
                let selection_range = self.find_function_name_range(func).ok()?;

                // Build parameters detail string
                let params_str = func
                    .params()
                    .iter()
                    .map(|p| format!("{p}"))
                    .collect::<Vec<_>>()
                    .join(", ");

                let return_type = match func.ret() {
                    Some(ret) => format!("{ret}"),
                    None => "()".to_string(),
                };

                #[allow(deprecated)]
                Some(DocumentSymbol {
                    name: func.name().to_string(),
                    detail: Some(format!("fn({params_str}) -> {return_type}")),
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    range: full_range,
                    selection_range,
                    children: None,
                    deprecated: None,
                })
            })
            .collect()
    }
}
