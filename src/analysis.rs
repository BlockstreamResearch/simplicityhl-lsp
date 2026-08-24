use std::collections::HashMap;
use std::ops::Index;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ropey::Rope;
use simplicityhl::ast::ElementsJetHinter;
use simplicityhl::driver::SourceMap;
use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, DiagnosticManager, Error as CompilerError,
    Severity as CompilerSeverity, Span,
};
use simplicityhl::parse::ParseFromStrWithErrors;
use simplicityhl::resolution::DependencyMap;
use simplicityhl::source::CanonSourceFile;
use simplicityhl::{parse, TemplateProgram, UnstableFeatures};
use tower_lsp_server::lsp_types::Uri;
use tower_lsp_server::UriExt;

use crate::config::Settings;
use crate::function::Functions;
use crate::project::ProjectContext;
use crate::text::{get_comments_from_lines, offset_to_position};

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
    fn root(uri: Uri, text: Rope) -> Self {
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

    fn from_compiler(source_map: &SourceMap, root_text: &Rope) -> Self {
        let by_id = source_map
            .iter()
            .map(|(path, file_id)| {
                (
                    *file_id,
                    SourceDocument {
                        uri: Uri::from_file_path(path.as_path())
                            .expect("compiler source path produces a valid file URI"),
                        text: if *file_id == 0 {
                            root_text.clone()
                        } else {
                            Rope::from_str(
                                source_map
                                    .content(*file_id)
                                    .expect("compiler source map contains every registered file")
                                    .as_ref(),
                            )
                        },
                    },
                )
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

/// Immutable result of analyzing one open root document.
#[derive(Debug)]
pub struct AnalysisSnapshot {
    pub functions: Functions,
    pub use_declarations: Vec<parse::UseDecl>,
    pub sources: SourceSet,
    pub text: Rope,
    pub version: Option<i32>,
    pub compiler_diagnostics: Vec<CompilerDiagnostic>,
    call_scopes: HashMap<FunctionKey, Arc<HashMap<String, parse::Function>>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FunctionKey {
    file_id: usize,
    start: usize,
    end: usize,
}

impl FunctionKey {
    fn new(function: &parse::Function) -> Self {
        let span = function.span();
        Self {
            file_id: span.file_id,
            start: span.start,
            end: span.end,
        }
    }
}

impl AnalysisSnapshot {
    pub fn new(uri: Uri, text: Rope, version: Option<i32>) -> Self {
        Self {
            functions: Functions::new(),
            use_declarations: Vec::new(),
            sources: SourceSet::root(uri, text.clone()),
            text,
            version,
            compiler_diagnostics: Vec::new(),
            call_scopes: HashMap::new(),
        }
    }

    pub fn from_program(program: &parse::Program, text: &str, path: &Path) -> Self {
        let text = Rope::from_str(text);
        let uri = Uri::from_file_path(path).expect("source path produces a valid file URI");
        let mut analysis = Self::new(uri, text, None);

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

    pub fn populate_sources(&mut self, source_map: &SourceMap) {
        self.sources = SourceSet::from_compiler(source_map, &self.text);
    }

    /// Add imported functions under the names visible from this root, including aliases.
    fn populate_visible_functions(&mut self, template_program: &TemplateProgram) {
        let Some(source_map) = template_program.source_map() else {
            return;
        };
        self.populate_sources(source_map);

        let resolved_program = template_program.resolved_program();
        self.populate_call_scopes(resolved_program);
        for item in resolved_program.items() {
            let parse::Item::Module(module) = item else {
                continue;
            };
            let Some(0) = module
                .name()
                .as_inner()
                .strip_prefix("unit_")
                .and_then(|id| id.parse::<usize>().ok())
            else {
                continue;
            };

            for inner_item in module.items() {
                let parse::Item::Use(use_decl) = inner_item else {
                    continue;
                };
                let path = use_decl.path();
                let Some(target_file_id) = path
                    .get(1)
                    .and_then(|segment| segment.as_inner().strip_prefix("unit_"))
                    .and_then(|id| id.parse::<usize>().ok())
                else {
                    continue;
                };
                let items = match use_decl.items() {
                    parse::UseItems::Single(item) => std::slice::from_ref(item),
                    parse::UseItems::List(items) => items.as_slice(),
                };

                for (original_name, alias) in items {
                    let local_name = alias.as_ref().unwrap_or(original_name);
                    let mut visited = std::collections::HashSet::new();
                    let Some(function) = resolve_function(
                        resolved_program,
                        target_file_id,
                        &path[2..],
                        original_name.as_inner(),
                        &mut visited,
                    ) else {
                        continue;
                    };
                    let Some(source) = self.sources.get(function.span().file_id) else {
                        continue;
                    };
                    let start_line = offset_to_position(function.span().start, &source.text)
                        .unwrap_or_default()
                        .line;
                    self.functions.insert(
                        local_name.to_string(),
                        function.clone(),
                        get_comments_from_lines(start_line, &source.text),
                    );
                }
            }
        }
    }

    fn populate_call_scopes(&mut self, program: &parse::Program) {
        for item in program.items() {
            let parse::Item::Module(unit) = item else {
                continue;
            };
            collect_call_scopes(program, unit.items(), &mut self.call_scopes);
        }
    }

    pub(crate) fn resolve_custom_call(
        &self,
        owner: &parse::Function,
        name: &str,
    ) -> Option<&parse::Function> {
        match self.call_scopes.get(&FunctionKey::new(owner)) {
            Some(scope) => scope.get(name),
            None => self.functions.get_func(name),
        }
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
            let mut snapshot = Self::new(root_uri, Rope::from_str(text), None);
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
        let canonical_source: CanonSourceFile = source_file
            .try_into()
            .expect("source file has a canonical name");

        // The compiler requires an entry point, while editors also analyze library files.
        // Add a synthetic main only to the compiler input; all user spans remain unchanged.
        let analysis_source = if items_contain_main(program.items()) {
            canonical_source.clone()
        } else {
            CanonSourceFile::new(
                canonical_source.name().clone(),
                Arc::from(format!("{}\nfn main() {{}}\n", canonical_source.content())),
            )
        };

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
                remap_imported_main_diagnostics(
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

fn collect_call_scopes(
    program: &parse::Program,
    items: &[parse::Item],
    call_scopes: &mut HashMap<FunctionKey, Arc<HashMap<String, parse::Function>>>,
) {
    let mut bindings = items
        .iter()
        .filter_map(|item| match item {
            parse::Item::Function(function) => {
                Some((function.name().to_string(), function.clone()))
            }
            _ => None,
        })
        .collect::<HashMap<_, _>>();

    for use_decl in items.iter().filter_map(|item| match item {
        parse::Item::Use(use_decl) => Some(use_decl),
        _ => None,
    }) {
        let path = use_decl.path();
        let Some(target_file_id) = path
            .get(1)
            .and_then(|segment| segment.as_inner().strip_prefix("unit_"))
            .and_then(|id| id.parse::<usize>().ok())
        else {
            continue;
        };
        let imported_items = match use_decl.items() {
            parse::UseItems::Single(item) => std::slice::from_ref(item),
            parse::UseItems::List(items) => items.as_slice(),
        };
        for (original, alias) in imported_items {
            let mut visited = std::collections::HashSet::new();
            let Some(function) = resolve_function(
                program,
                target_file_id,
                &path[2..],
                original.as_inner(),
                &mut visited,
            ) else {
                continue;
            };
            bindings.insert(
                alias.as_ref().unwrap_or(original).to_string(),
                function.clone(),
            );
        }
    }

    let bindings = Arc::new(bindings);
    for item in items {
        match item {
            parse::Item::Function(function) => {
                call_scopes.insert(FunctionKey::new(function), Arc::clone(&bindings));
            }
            parse::Item::Module(module) => {
                collect_call_scopes(program, module.items(), call_scopes);
            }
            parse::Item::TypeAlias(_)
            | parse::Item::Use(_)
            | parse::Item::EnumDeclaration(_)
            | parse::Item::Ignored => {}
        }
    }
}

fn collect_use_declarations(items: &[parse::Item], declarations: &mut Vec<parse::UseDecl>) {
    for item in items {
        match item {
            parse::Item::Use(use_decl) => declarations.push(use_decl.clone()),
            parse::Item::Module(module) => {
                collect_use_declarations(module.items(), declarations);
            }
            parse::Item::TypeAlias(_)
            | parse::Item::Function(_)
            | parse::Item::EnumDeclaration(_)
            | parse::Item::Ignored => {}
        }
    }
}

fn items_contain_main(items: &[parse::Item]) -> bool {
    items.iter().any(|item| match item {
        parse::Item::Function(function) => function.name().as_inner() == "main",
        parse::Item::Module(module) => items_contain_main(module.items()),
        parse::Item::TypeAlias(_)
        | parse::Item::Use(_)
        | parse::Item::EnumDeclaration(_)
        | parse::Item::Ignored => false,
    })
}

fn imported_main_span(
    items: &[parse::Item],
    current_source: &CanonSourceFile,
    dependencies: &DependencyMap,
    unstable_features: &UnstableFeatures,
) -> Option<Span> {
    for item in items {
        match item {
            parse::Item::Use(use_decl) => {
                let Ok(target) = dependencies.resolve_path(current_source.name(), use_decl) else {
                    continue;
                };
                if &target == current_source.name() {
                    continue;
                }

                let Ok(source) = std::fs::read_to_string(target.as_path()) else {
                    continue;
                };
                let mut diagnostics = DiagnosticManager::new();
                let Some(program) = parse::Program::parse_from_str_with_errors(
                    0,
                    &source,
                    unstable_features,
                    &mut diagnostics,
                ) else {
                    continue;
                };

                if items_contain_main(program.items()) {
                    return Some(*use_decl.span());
                }
            }
            parse::Item::Module(module) => {
                if let Some(span) = imported_main_span(
                    module.items(),
                    current_source,
                    dependencies,
                    unstable_features,
                ) {
                    return Some(span);
                }
            }
            parse::Item::TypeAlias(_)
            | parse::Item::Function(_)
            | parse::Item::EnumDeclaration(_)
            | parse::Item::Ignored => {}
        }
    }
    None
}

fn is_duplicate_main(diagnostic: &CompilerDiagnostic) -> bool {
    matches!(
        diagnostic.error(),
        CompilerError::FunctionRedefined { name } if name.as_inner() == "main"
    )
}

fn remap_imported_main_diagnostics(
    diagnostics: &DiagnosticManager,
    program: &parse::Program,
    current_source: &CanonSourceFile,
    dependencies: &DependencyMap,
    unstable_features: &UnstableFeatures,
) -> Vec<CompilerDiagnostic> {
    if !diagnostics.diagnostics().iter().any(is_duplicate_main) {
        return diagnostics.diagnostics().to_vec();
    }
    let Some(import_span) = imported_main_span(
        program.items(),
        current_source,
        dependencies,
        unstable_features,
    ) else {
        return diagnostics.diagnostics().to_vec();
    };

    diagnostics
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            if !is_duplicate_main(diagnostic) {
                return diagnostic.clone();
            }
            let mut remapped = match diagnostic.severity() {
                CompilerSeverity::Error => {
                    CompilerDiagnostic::new(diagnostic.error().clone(), import_span)
                }
                CompilerSeverity::Warning => {
                    CompilerDiagnostic::warning(diagnostic.error().clone(), import_span)
                }
            };
            for label in diagnostic.secondary() {
                remapped = remapped.with_secondary(label.span, label.message.clone());
            }
            for note in diagnostic.notes() {
                remapped = remapped.with_note(note.clone());
            }
            if let Some(help) = diagnostic.help() {
                remapped = remapped.with_help(help.clone());
            }
            remapped
        })
        .collect()
}

type ResolutionKey = (usize, Vec<String>, String);

fn resolve_function<'a>(
    program: &'a parse::Program,
    file_id: usize,
    module_path: &[simplicityhl::str::Identifier],
    name: &str,
    visited: &mut std::collections::HashSet<ResolutionKey>,
) -> Option<&'a parse::Function> {
    let key = (
        file_id,
        module_path.iter().map(ToString::to_string).collect(),
        name.to_string(),
    );
    if !visited.insert(key) {
        return None;
    }

    let items = module_items(program, file_id, module_path)?;
    if let Some(function) = items.iter().find_map(|item| match item {
        parse::Item::Function(function) if function.name().as_inner() == name => Some(function),
        _ => None,
    }) {
        return Some(function);
    }

    for use_decl in items.iter().filter_map(|item| match item {
        parse::Item::Use(use_decl) => Some(use_decl),
        _ => None,
    }) {
        let imported_items = match use_decl.items() {
            parse::UseItems::Single(item) => std::slice::from_ref(item),
            parse::UseItems::List(items) => items.as_slice(),
        };
        for (original, alias) in imported_items {
            if alias.as_ref().unwrap_or(original).as_inner() != name {
                continue;
            }
            let path = use_decl.path();
            let target_file_id = path
                .get(1)?
                .as_inner()
                .strip_prefix("unit_")?
                .parse::<usize>()
                .ok()?;
            if let Some(function) = resolve_function(
                program,
                target_file_id,
                &path[2..],
                original.as_inner(),
                visited,
            ) {
                return Some(function);
            }
        }
    }
    None
}

fn module_items<'a>(
    program: &'a parse::Program,
    file_id: usize,
    module_path: &[simplicityhl::str::Identifier],
) -> Option<&'a [parse::Item]> {
    let unit_name = format!("unit_{file_id}");
    let unit = program.items().iter().find_map(|item| match item {
        parse::Item::Module(module) if module.name().as_inner() == unit_name => Some(module),
        _ => None,
    })?;
    let mut items = unit.items();
    for segment in module_path {
        let module = items.iter().find_map(|item| match item {
            parse::Item::Module(module) if module.name().as_inner() == segment.as_inner() => {
                Some(module)
            }
            _ => None,
        })?;
        items = module.items();
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_scope_does_not_fall_back_to_root_visible_functions() {
        let source = "fn target() {}\nfn owner() { target(); }\n";
        let mut diagnostics = DiagnosticManager::new();
        let program = parse::Program::parse_from_str_with_errors(
            0,
            source,
            &UnstableFeatures::none(),
            &mut diagnostics,
        )
        .expect("valid program");
        let path = std::env::temp_dir().join("module-scope-test.simf");
        let mut snapshot = AnalysisSnapshot::from_program(&program, source, &path);
        let owner = snapshot
            .functions
            .get_func("owner")
            .expect("owner function")
            .clone();

        assert!(snapshot.resolve_custom_call(&owner, "target").is_some());
        snapshot
            .call_scopes
            .insert(FunctionKey::new(&owner), Arc::new(HashMap::new()));
        assert!(snapshot.resolve_custom_call(&owner, "target").is_none());
    }
}
