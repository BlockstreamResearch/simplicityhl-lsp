use std::collections::HashSet;
use std::sync::Arc;

use simplicityhl::driver::SourceMap;
use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, DiagnosticManager, Error as CompilerError,
    Severity as CompilerSeverity, Span,
};
use simplicityhl::parse::{self, ParseFromStrWithErrors};
use simplicityhl::resolution::DependencyMap;
use simplicityhl::source::CanonSourceFile;
use simplicityhl::UnstableFeatures;

pub(super) fn compiler_source(
    program: &parse::Program,
    canonical_source: &CanonSourceFile,
) -> CanonSourceFile {
    if items_contain_main(program.items()) {
        canonical_source.clone()
    } else {
        CanonSourceFile::new(
            canonical_source.name().clone(),
            Arc::from(format!("{}\nfn main() {{}}\n", canonical_source.content())),
        )
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

fn imports_main(
    items: &[parse::Item],
    current_source: &CanonSourceFile,
    dependencies: &DependencyMap,
    unstable_features: &UnstableFeatures,
    sources: &SourceMap,
    visited: &mut HashSet<simplicityhl::source::CanonPath>,
) -> bool {
    for item in items {
        match item {
            parse::Item::Use(use_decl) => {
                let Ok(target) = dependencies.resolve_path(current_source.name(), use_decl) else {
                    continue;
                };
                if &target == current_source.name() {
                    continue;
                }
                if !visited.insert(target.clone()) {
                    continue;
                }
                let Some(file_id) = sources.id(&target) else {
                    continue;
                };
                let Some(source) = sources.content(file_id) else {
                    continue;
                };
                let mut diagnostics = DiagnosticManager::new();
                let Some(program) = parse::Program::parse_from_str_with_errors(
                    file_id,
                    &source,
                    unstable_features,
                    &mut diagnostics,
                ) else {
                    continue;
                };

                let imported_source = CanonSourceFile::new(target, source);
                if items_contain_main(program.items())
                    || imports_main(
                        program.items(),
                        &imported_source,
                        dependencies,
                        unstable_features,
                        sources,
                        visited,
                    )
                {
                    return true;
                }
            }
            parse::Item::Module(module) => {
                if imports_main(
                    module.items(),
                    current_source,
                    dependencies,
                    unstable_features,
                    sources,
                    visited,
                ) {
                    return true;
                }
            }
            parse::Item::TypeAlias(_)
            | parse::Item::Function(_)
            | parse::Item::EnumDeclaration(_)
            | parse::Item::Ignored => {}
        }
    }
    false
}

fn imported_main_spans(
    items: &[parse::Item],
    current_source: &CanonSourceFile,
    dependencies: &DependencyMap,
    unstable_features: &UnstableFeatures,
    sources: &SourceMap,
    spans: &mut Vec<Span>,
) {
    for item in items {
        match item {
            parse::Item::Use(use_decl) => {
                let Ok(target) = dependencies.resolve_path(current_source.name(), use_decl) else {
                    continue;
                };
                if &target == current_source.name() {
                    continue;
                }
                let Some(file_id) = sources.id(&target) else {
                    continue;
                };
                let Some(source) = sources.content(file_id) else {
                    continue;
                };
                let mut diagnostics = DiagnosticManager::new();
                let Some(program) = parse::Program::parse_from_str_with_errors(
                    file_id,
                    &source,
                    unstable_features,
                    &mut diagnostics,
                ) else {
                    continue;
                };
                let imported_source = CanonSourceFile::new(target.clone(), source);
                if items_contain_main(program.items())
                    || imports_main(
                        program.items(),
                        &imported_source,
                        dependencies,
                        unstable_features,
                        sources,
                        &mut HashSet::from([target]),
                    )
                {
                    spans.push(*use_decl.span());
                }
            }
            parse::Item::Module(module) => imported_main_spans(
                module.items(),
                current_source,
                dependencies,
                unstable_features,
                sources,
                spans,
            ),
            parse::Item::TypeAlias(_)
            | parse::Item::Function(_)
            | parse::Item::EnumDeclaration(_)
            | parse::Item::Ignored => {}
        }
    }
}

fn is_duplicate_main(diagnostic: &CompilerDiagnostic) -> bool {
    matches!(
        diagnostic.error(),
        CompilerError::FunctionRedefined { name } if name.as_inner() == "main"
    )
}

pub(super) fn remap_imported_main_diagnostics(
    diagnostics: &DiagnosticManager,
    program: &parse::Program,
    current_source: &CanonSourceFile,
    dependencies: &DependencyMap,
    unstable_features: &UnstableFeatures,
) -> Vec<CompilerDiagnostic> {
    if !diagnostics.diagnostics().iter().any(is_duplicate_main) {
        return diagnostics.diagnostics().to_vec();
    }
    let Some(sources) = diagnostics.sources() else {
        return diagnostics.diagnostics().to_vec();
    };
    let mut import_spans = Vec::new();
    imported_main_spans(
        program.items(),
        current_source,
        dependencies,
        unstable_features,
        sources,
        &mut import_spans,
    );
    import_spans.sort_unstable_by_key(|span| (span.file_id, span.start, span.end));
    import_spans.dedup();
    if import_spans.is_empty() {
        return diagnostics.diagnostics().to_vec();
    }

    let mut duplicate_index = 0;
    diagnostics
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            if !is_duplicate_main(diagnostic) {
                return diagnostic.clone();
            }
            let primary_index = duplicate_index.min(import_spans.len() - 1);
            duplicate_index += 1;
            let import_span = import_spans[primary_index];
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
            for (index, span) in import_spans.iter().enumerate() {
                if index != primary_index {
                    remapped = remapped
                        .with_secondary(*span, "Another imported `main` enters through this use");
                }
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
