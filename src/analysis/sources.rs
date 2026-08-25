use std::collections::HashMap;
use std::ops::Index;

use ropey::Rope;
use simplicityhl::driver::SourceMap;
use simplicityhl::parse;
use tower_lsp_server::lsp_types::Uri;
use tower_lsp_server::UriExt;

use super::AnalysisSnapshot;

#[derive(Debug, Clone, Default)]
pub struct Functions(HashMap<String, (parse::Function, String)>);

impl Functions {
    pub(super) fn insert(
        &mut self,
        name: String,
        function: parse::Function,
        documentation: String,
    ) {
        self.0.insert(name, (function, documentation));
    }

    pub fn get(&self, name: &str) -> Option<(&parse::Function, &str)> {
        self.0
            .get(name)
            .map(|(function, documentation)| (function, documentation.as_str()))
    }

    pub fn get_func(&self, name: &str) -> Option<&parse::Function> {
        self.0.get(name).map(|(function, _)| function)
    }

    pub fn iter(&self) -> impl Iterator<Item = &parse::Function> {
        self.0.values().map(|(function, _)| function)
    }

    pub fn functions_and_docs(&self) -> Vec<(&parse::Function, &str)> {
        self.0
            .values()
            .map(|(function, documentation)| (function, documentation.as_str()))
            .collect()
    }
}

/// A compiler source together with the editor identity and text used for LSP ranges.
#[derive(Debug)]
pub struct SourceDocument {
    pub uri: Uri,
    pub text: Rope,
}

/// Stable file-id lookup for every source participating in one compiler analysis.
#[derive(Debug)]
pub struct SourceSet {
    by_id: HashMap<usize, SourceDocument>,
}

impl SourceSet {
    pub(super) fn root(uri: Uri, text: Rope) -> Self {
        Self {
            by_id: HashMap::from([(0, SourceDocument { uri, text })]),
        }
    }

    pub fn get(&self, file_id: usize) -> Option<&SourceDocument> {
        self.by_id.get(&file_id)
    }

    pub fn root_source(&self) -> &SourceDocument {
        self.get(0).expect("every source set contains its root")
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    fn from_compiler(source_map: &SourceMap, root_uri: &Uri, root_text: &Rope) -> Self {
        let by_id = source_map
            .iter()
            .map(|(path, file_id)| {
                let (uri, text) = if *file_id == 0 {
                    (root_uri.clone(), root_text.clone())
                } else {
                    (
                        Uri::from_file_path(path.as_path())
                            .expect("compiler source path produces a valid file URI"),
                        Rope::from_str(
                            source_map
                                .content(*file_id)
                                .expect("compiler source map contains every registered file")
                                .as_ref(),
                        ),
                    )
                };
                (*file_id, SourceDocument { uri, text })
            })
            .collect();
        Self { by_id }
    }
}

impl Index<usize> for SourceSet {
    type Output = SourceDocument;

    fn index(&self, index: usize) -> &Self::Output {
        &self.by_id[&index]
    }
}

impl AnalysisSnapshot {
    pub(super) fn populate_sources(&mut self, source_map: &SourceMap) {
        let root_uri = self.sources.root_source().uri.clone();
        self.sources = SourceSet::from_compiler(source_map, &root_uri, &self.text);
    }
}
