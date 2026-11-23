use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use simplicityhl::error::Span;
use simplicityhl::parse;
use simplicityhl::str::{Identifier, SymbolName};
use simplicityhl::TemplateAst;

use super::{documentation, AnalysisSnapshot};

impl AnalysisSnapshot {
    /// Add imported functions under the names visible from this root, including aliases.
    pub(super) fn populate_visible_functions(&mut self, template_ast: &TemplateAst) {
        let Some(source_map) = template_ast.source_map() else {
            return;
        };
        self.populate_sources(source_map);

        let program = template_ast.resolved_program();
        self.populate_call_scopes(program);

        let Some(root) =
            modules(program.items()).find(|module| unit_id(module.name().as_str()) == Some(0))
        else {
            return;
        };

        for (local_name, function) in imports(program, root.items()) {
            let Some(source) = self.sources.get(function.span().file_id) else {
                continue;
            };
            self.functions.insert(
                local_name.to_string(),
                function.clone(),
                documentation(function, &source.text),
            );
        }
    }

    fn populate_call_scopes(&mut self, program: &parse::Program) {
        for unit in modules(program.items()) {
            collect_call_scopes(program, unit.items(), &mut self.call_scopes);
        }
    }

    pub(crate) fn resolve_custom_call(
        &self,
        owner: &parse::Function,
        name: &str,
    ) -> Option<&parse::Function> {
        match self.call_scopes.get(owner.span()) {
            Some(scope) => scope.get(name),
            None => self.functions.get_func(name),
        }
    }
}

fn collect_call_scopes(
    program: &parse::Program,
    items: &[parse::Item],
    call_scopes: &mut HashMap<Span, Arc<HashMap<String, parse::Function>>>,
) {
    let mut bindings = functions(items)
        .map(|function| (function.name().to_string(), function.clone()))
        .collect::<HashMap<_, _>>();
    bindings.extend(
        imports(program, items).map(|(name, function)| (name.to_string(), function.clone())),
    );

    let bindings = Arc::new(bindings);
    for item in items {
        match item {
            parse::Item::Function(function) => {
                call_scopes.insert(*function.span(), Arc::clone(&bindings));
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

pub(super) fn collect_use_declarations(
    items: &[parse::Item],
    declarations: &mut Vec<parse::UseDecl>,
) {
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

/// Functions imported by the `use` declarations in `items`, under their local names.
fn imports<'a>(
    program: &'a parse::Program,
    items: &'a [parse::Item],
) -> impl Iterator<Item = (&'a SymbolName, &'a parse::Function)> + 'a {
    uses(items).flat_map(move |use_decl| {
        let path = use_decl.path();
        let target = path.get(1).and_then(|segment| unit_id(segment.as_str()));

        use_items(use_decl)
            .iter()
            .filter_map(move |(original, alias)| {
                let function = resolve_function(
                    program,
                    target?,
                    &path[2..],
                    original.as_str(),
                    &mut HashSet::new(),
                )?;
                Some((alias.as_ref().unwrap_or(original), function))
            })
    })
}

type ResolutionKey = (usize, Vec<String>, String);

fn resolve_function<'a>(
    program: &'a parse::Program,
    file_id: usize,
    module_path: &[Identifier],
    name: &str,
    visited: &mut HashSet<ResolutionKey>,
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
    if let Some(function) = functions(items).find(|function| function.name().as_str() == name) {
        return Some(function);
    }

    for use_decl in uses(items) {
        for (original, alias) in use_items(use_decl) {
            if alias.as_ref().unwrap_or(original).as_str() != name {
                continue;
            }

            let path = use_decl.path();
            let target_file_id = unit_id(path.get(1)?.as_str())?;
            if let Some(function) = resolve_function(
                program,
                target_file_id,
                &path[2..],
                original.as_str(),
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
    module_path: &[Identifier],
) -> Option<&'a [parse::Item]> {
    let unit =
        modules(program.items()).find(|module| unit_id(module.name().as_str()) == Some(file_id))?;

    let mut items = unit.items();
    for segment in module_path {
        items = modules(items)
            .find(|module| module.name().as_str() == segment.as_str())?
            .items();
    }

    Some(items)
}

/// The file id of a module the compiler driver named `unit_<id>` when flattening the program.
fn unit_id(name: &str) -> Option<usize> {
    name.strip_prefix("unit_")?.parse().ok()
}

pub(crate) fn use_items(use_decl: &parse::UseDecl) -> &[parse::AliasedSymbolName] {
    match use_decl.items() {
        parse::UseItems::Single(item) => std::slice::from_ref(item),
        parse::UseItems::List(items) => items,
    }
}

fn uses(items: &[parse::Item]) -> impl Iterator<Item = &parse::UseDecl> {
    items.iter().filter_map(|item| match item {
        parse::Item::Use(use_decl) => Some(use_decl),
        _ => None,
    })
}

fn functions(items: &[parse::Item]) -> impl Iterator<Item = &parse::Function> {
    items.iter().filter_map(|item| match item {
        parse::Item::Function(function) => Some(function),
        _ => None,
    })
}

fn modules(items: &[parse::Item]) -> impl Iterator<Item = &parse::Module> {
    items.iter().filter_map(|item| match item {
        parse::Item::Module(module) => Some(module),
        _ => None,
    })
}
