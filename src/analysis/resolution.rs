use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use simplicityhl::error::Span;
use simplicityhl::parse;
use simplicityhl::TemplateProgram;

use super::AnalysisSnapshot;
use crate::text::{get_comments_from_lines, offset_to_position};

impl AnalysisSnapshot {
    /// Add imported functions under the names visible from this root, including aliases.
    pub(super) fn populate_visible_functions(&mut self, template_program: &TemplateProgram) {
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
                    let mut visited = HashSet::new();
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
            let mut visited = HashSet::new();
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

type ResolutionKey = (usize, Vec<String>, String);

fn resolve_function<'a>(
    program: &'a parse::Program,
    file_id: usize,
    module_path: &[simplicityhl::str::Identifier],
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
