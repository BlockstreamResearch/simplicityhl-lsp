mod entry_point;
mod resolution;
mod sources;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ropey::Rope;
use simplicityhl::ast::ElementsJetHinter;
use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, DiagnosticManager, Error as CompilerError, Span,
};
use simplicityhl::parse::ParseFromStrWithErrors;
use simplicityhl::source::CanonSourceFile;
use simplicityhl::{parse, TemplateProgram};
use tower_lsp_server::lsp_types::Uri;
use tower_lsp_server::UriExt;

pub use sources::{Functions, SourceSet};

use self::resolution::collect_use_declarations;
use crate::config::Settings;
use crate::project::ProjectContext;
use crate::text::{get_comments_from_lines, offset_to_position};

/// Immutable result of analyzing one open root document.
#[derive(Debug)]
pub struct AnalysisSnapshot {
    pub functions: Functions,
    pub use_declarations: Vec<parse::UseDecl>,
    pub sources: SourceSet,
    pub text: Rope,
    pub compiler_diagnostics: Vec<CompilerDiagnostic>,
    call_scopes: HashMap<Span, Arc<HashMap<String, parse::Function>>>,
}

impl AnalysisSnapshot {
    pub fn new(uri: Uri, text: Rope) -> Self {
        Self {
            functions: Functions::default(),
            use_declarations: Vec::new(),
            sources: SourceSet::root(uri, text.clone()),
            text,
            compiler_diagnostics: Vec::new(),
            call_scopes: HashMap::new(),
        }
    }

    pub fn from_program(program: &parse::Program, text: &str, path: &Path) -> Self {
        let text = Rope::from_str(text);
        let uri = Uri::from_file_path(path).expect("source path produces a valid file URI");
        let mut analysis = Self::new(uri, text);

        collect_use_declarations(program.items(), &mut analysis.use_declarations);
        for function in program.items().iter().filter_map(|item| match item {
            parse::Item::Function(function) => Some(function),
            _ => None,
        }) {
            let start_line = offset_to_position(function.span().start, &analysis.text)
                .unwrap_or_default()
                .line;
            analysis.functions.insert(
                function.name().to_string(),
                function.clone(),
                get_comments_from_lines(start_line, &analysis.text),
            );
        }

        analysis
    }

    /// Parse, resolve, and type-check one root using the same project context as the compiler.
    /// A snapshot is returned even when parsing fails so editor features retain the latest text.
    pub fn analyze(
        text: &str,
        path: &Path,
        settings: &Settings,
        workspace_roots: &[PathBuf],
    ) -> Self {
        let unstable_features = settings.unstable_features();
        let mut diagnostics = DiagnosticManager::new();
        let shared_text: Arc<str> = Arc::from(text);
        let source_file = simplicityhl::source::SourceFile::new(path, Arc::clone(&shared_text));
        let root_uri = Uri::from_file_path(path).expect("source path produces a valid file URI");

        let Some(program) = parse::Program::parse_from_str_with_errors(
            0,
            shared_text.as_ref(),
            &unstable_features,
            &mut diagnostics,
        ) else {
            let mut snapshot = Self::new(root_uri, Rope::from_str(text));
            snapshot.compiler_diagnostics = diagnostics.diagnostics().to_vec();
            return snapshot;
        };

        let mut snapshot = Self::from_program(&program, shared_text.as_ref(), path);
        let dependencies = match ProjectContext::discover(path, &settings.project, workspace_roots)
            .and_then(|project| project.dependency_map(path))
        {
            Ok(dependencies) => dependencies,
            Err(err) => {
                diagnostics.push(CompilerDiagnostic::new(
                    CompilerError::CannotParse {
                        msg: err.to_string(),
                    },
                    Span::new(0, 0..0),
                ));
                snapshot.compiler_diagnostics = diagnostics.diagnostics().to_vec();
                return snapshot;
            }
        };
        let canonical_source: CanonSourceFile = match source_file.try_into() {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(CompilerDiagnostic::new(
                    CompilerError::CannotParse { msg: error },
                    Span::new(0, 0..0),
                ));
                snapshot.compiler_diagnostics = diagnostics.diagnostics().to_vec();
                return snapshot;
            }
        };

        // The compiler requires an entry point, while editors also analyze library files.
        // Add a synthetic main only to the compiler input; all user spans remain unchanged.
        let analysis_source = entry_point::compiler_source(&program, &canonical_source);

        snapshot.compiler_diagnostics = match TemplateProgram::new_with_dep(
            analysis_source,
            &dependencies,
            &unstable_features,
            Box::new(ElementsJetHinter::new()),
        ) {
            Ok(template_program) => {
                snapshot.populate_visible_functions(&template_program);
                template_program.diagnostics().diagnostics().to_vec()
            }
            Err(diagnostics) => {
                if let Some(source_map) = diagnostics.sources() {
                    snapshot.populate_sources(source_map);
                }
                entry_point::remap_imported_main_diagnostics(
                    &diagnostics,
                    &program,
                    &canonical_source,
                    &dependencies,
                    &unstable_features,
                )
            }
        };

        snapshot
    }
}

#[cfg(test)]
mod tests;
