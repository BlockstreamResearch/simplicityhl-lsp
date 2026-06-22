use crate::completion::types;

use simplicityhl::docs::jet::JetInfo;
use simplicityhl::jet;
use simplicityhl::simplicity::jet::Elements;

/// Convert all jets to [`types::FunctionTemplate`].
pub fn get_jets_completions() -> Vec<types::FunctionTemplate> {
    Elements::ALL.iter().copied().map(jet_to_template).collect()
}

/// Convert [`Elements`] to [`types::FunctionTemplate`]
pub fn jet_to_template(jet: Elements) -> types::FunctionTemplate {
    types::FunctionTemplate::simple(
        jet.to_string(),
        jet::source_type(&jet)
            .iter()
            .map(|item| format!("{item}"))
            .collect::<Vec<String>>(),
        jet::target_type(&jet).to_string().as_str(),
        jet.documentation(),
    )
}
