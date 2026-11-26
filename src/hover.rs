use std::str::FromStr;

use simplicityhl::parse::CallName;
use tower_lsp_server::lsp_types::{
    Hover, HoverContents, MarkupContent, MarkupKind, Position, Range,
};

use crate::analysis::AnalysisSnapshot;
use crate::completion;
use crate::error::LspError;
use crate::text::{get_call_span, position_to_span, span_to_positions};

/// Signature and documentation of the function called at `position`.
pub fn at(snapshot: &AnalysisSnapshot, position: Position) -> Result<Option<Hover>, LspError> {
    let token_span = position_to_span(position, &snapshot.text)?;
    let Some(call) = snapshot.find_related_call(token_span) else {
        return Ok(None);
    };

    let call_span = get_call_span(call);
    let (start, end) = span_to_positions(&call_span, &snapshot.text)?;

    let description = match call.name() {
        CallName::Jet(jet) => {
            let Ok(element) =
                simplicityhl::simplicity::jet::Elements::from_str(format!("{jet}").as_str())
            else {
                return Ok(None);
            };

            let template = completion::jet::jet_to_template(element);
            format!(
                "Jet function\n```simplicityhl\nfn {}({}) -> {}\n```\n---\n\n{}",
                template.display_name,
                template.args.join(", "),
                template.return_type,
                template.description
            )
        }
        CallName::Custom(func) => {
            let Some((function, function_doc)) = snapshot.functions.get(func.as_str()) else {
                return Ok(None);
            };

            let template = completion::function_to_template(function, function_doc);
            format!(
                "```simplicityhl\nfn {}({}) -> {}\n```\n---\n{}",
                template.display_name,
                template.args.join(", "),
                template.return_type,
                template.description
            )
        }
        other => {
            let Some(template) = completion::builtin::match_callname(other) else {
                return Ok(None);
            };
            format!(
                "Built-in function\n```simplicityhl\nfn {}({}) -> {}\n```\n---\n{}",
                template.display_name,
                template.args.join(", "),
                template.return_type,
                template.description
            )
        }
    };

    Ok(Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: description,
        }),
        range: Some(Range { start, end }),
    }))
}
