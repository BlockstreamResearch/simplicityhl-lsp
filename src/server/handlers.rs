use serde_json::Value;

use std::str::FromStr;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, Diagnostic, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
    InitializeResult, InitializedParams, Location, MarkupContent, MarkupKind, MessageType, Range,
    ReferenceParams, Registration, SemanticTokens, SemanticTokensParams, SemanticTokensResult,
    SignatureHelp, SignatureHelpParams, SymbolKind, Uri,
};
use tower_lsp_server::{LanguageServer, UriExt};

use simplicityhl::parse;

use crate::analysis::AnalysisSnapshot;
use crate::completion;
use crate::completion::imports::{self, ImportCompletionContext};
use crate::config::Settings;
use crate::project::{ProjectContext, SIMPLEX_MANIFEST};
use crate::text::{
    get_call_span, position_to_offset, position_to_span, span_contains, span_to_positions,
};
use crate::utils::find_key_position;
use crate::workspace::{AnalysisInput, DiagnosticUpdate, WorkspaceState};

use super::capabilities::{initialize_result, watched_files, workspace_roots};
use super::Backend;

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let workspace_roots = workspace_roots(&params);
        let watched_files_registration = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|capability| capability.dynamic_registration)
            .unwrap_or(false);
        let settings = params
            .initialization_options
            .and_then(|value| Settings::from_json(value).ok())
            .unwrap_or_default();
        {
            let mut config = self.config.write().await;
            config.workspace_roots = workspace_roots;
            config.watched_files_registration = watched_files_registration;
            config.settings = settings;
        }

        Ok(initialize_result())
    }

    async fn initialized(&self, _: InitializedParams) {
        if !self.config.read().await.watched_files_registration {
            return;
        }

        let watchers = watched_files();
        let registration = Registration {
            id: "simplicityhl-lsp-watched-files".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers,
            })
            .ok(),
        };
        if let Err(error) = self.client.register_capability(vec![registration]).await {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("Unable to register file watchers: {error}"),
                )
                .await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        {
            let mut config = self.config.write().await;
            for removed in params.event.removed {
                if let Some(path) = removed.uri.to_file_path() {
                    config.workspace_roots.retain(|root| root != path.as_ref());
                }
            }
            for added in params.event.added {
                if let Some(path) = added.uri.to_file_path() {
                    let path = path.into_owned();
                    if !config.workspace_roots.contains(&path) {
                        config.workspace_roots.push(path);
                    }
                }
            }
        }
        self.reanalyze_open_documents().await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        match Settings::from_json(params.settings) {
            Ok(settings) => {
                self.config.write().await.settings = settings;
                self.reanalyze_open_documents().await;
            }
            Err(err) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Invalid SimplicityHL settings: {err}"),
                    )
                    .await;
            }
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // A manifest or a file elsewhere in the dependency graph changed, so results
        // cached for the open documents may no longer be correct.
        let relevant = params.changes.iter().any(|change| {
            change.uri.to_file_path().is_some_and(|path| {
                path.extension().is_some_and(|ext| ext == "simf")
                    || path
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case(SIMPLEX_MANIFEST))
            })
        });
        if relevant {
            self.reanalyze_open_documents().await;
        }
    }

    async fn execute_command(&self, _: ExecuteCommandParams) -> Result<Option<Value>> {
        Ok(None)
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if params.text_document.uri.to_file_path().is_none() {
            return;
        }
        let input = self.workspace.write().await.begin_open(
            &params.text_document.uri,
            &params.text_document.text,
            Some(params.text_document.version),
        );
        self.on_change(input).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Sync is `FULL`, so the last change holds the whole document. Indexing the first
        // element instead would panic on the empty list some clients send, and would use
        // stale text whenever a client batches several changes into one notification.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let Some(input) = self.workspace.write().await.begin_change(
            &params.text_document.uri,
            &change.text,
            Some(params.text_document.version),
        ) else {
            return;
        };
        self.on_change(input).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(text) = params.text {
            let Some(input) =
                self.workspace
                    .write()
                    .await
                    .begin_change(&params.text_document.uri, &text, None)
            else {
                return;
            };
            self.on_change(input).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(generation) = self.workspace.write().await.begin_close(&uri) else {
            return;
        };
        // Without this the parsed document is retained for the rest of the session and the
        // editor keeps showing the diagnostics published for a file that is no longer open.
        self.run_diagnostic_transaction(|workspace| workspace.remove_if_current(&uri, generation))
            .await;
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        // .wit files don't have semantic tokens
        if std::path::Path::new(uri.path().as_str())
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wit"))
        {
            return Ok(None);
        }

        let documents = self.workspace.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: crate::semantic_tokens::tokens(doc),
        })))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        // .wit files don't have symbols
        if std::path::Path::new(uri.path().as_str())
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wit"))
        {
            return Ok(None);
        }

        let documents = self.workspace.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        let functions = doc.functions.functions();

        let symbols: Vec<DocumentSymbol> = functions
            .iter()
            .filter_map(|func| {
                if func.span().file_id != 0 {
                    return None;
                }

                // Get the full function range
                let (start, end) = span_to_positions(func.span(), &doc.text).ok()?;
                let full_range = Range { start, end };

                // Get the function name range for selection
                let selection_range = doc.find_function_name_range(func).ok()?;

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
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let documents = self.workspace.read().await;
        let uri = &params.text_document_position_params.text_document.uri;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        Ok(crate::signature_help::at(
            doc,
            params.text_document_position_params.position,
        )?)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let (source_prefix, functions) = {
            let documents = self.workspace.read().await;
            let Some(doc) = documents.get(uri) else {
                return Ok(None);
            };
            let Ok(offset) = position_to_offset(pos, &doc.text) else {
                return Ok(None);
            };
            let Some(prefix) = doc.text.get_byte_slice(..offset) else {
                return Ok(None);
            };
            (prefix.to_string(), doc.functions.clone())
        };

        if let Some(context) = ImportCompletionContext::at(&source_prefix, source_prefix.len()) {
            return Ok(self
                .import_completion(uri, &source_prefix, &context)
                .await
                .map(CompletionResponse::Array));
        }

        // The extra trigger characters above exist solely for import completion. Avoid opening
        // the generic function list after every space, comma, or block brace in normal code.
        if params
            .context
            .as_ref()
            .and_then(|context| context.trigger_character.as_deref())
            .is_some_and(|character| matches!(character, " " | "{" | ","))
        {
            return Ok(None);
        }

        let prefix = source_prefix
            .rsplit_once('\n')
            .map_or(source_prefix.as_str(), |(_, line)| line);
        let completions = self
            .completion_provider
            .process_completions(prefix, &functions.functions_and_docs())
            .map(CompletionResponse::Array);

        Ok(completions)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;

        // .wit files don't have hover info
        if std::path::Path::new(uri.path().as_str())
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wit"))
        {
            return Ok(None);
        }

        let documents = self.workspace.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        let token_pos = params.text_document_position_params.position;

        let token_span = position_to_span(token_pos, &doc.text)?;
        let Some(call) = doc.find_related_call(token_span) else {
            return Ok(None);
        };

        let call_span = get_call_span(call);
        let (start, end) = span_to_positions(&call_span, &doc.text)?;

        let description = match call.name() {
            parse::CallName::Jet(jet) => {
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
            parse::CallName::Custom(func) => {
                let Some((function, function_doc)) = doc.functions.get(func.as_inner()) else {
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
            contents: tower_lsp_server::lsp_types::HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: description,
            }),
            range: Some(Range { start, end }),
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let documents = self.workspace.read().await;
        let uri = &params.text_document_position_params.text_document.uri;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };
        Ok(doc.definition_at(uri, params.text_document_position_params.position)?)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let documents = self.workspace.read().await;
        let uri = &params.text_document_position.text_document.uri;

        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };
        let functions = doc.functions.functions();

        let token_position = params.text_document_position.position;

        let token_span = position_to_span(token_position, &doc.text)?;

        let call_name = doc
            .find_related_call(token_span)
            .map(simplicityhl::parse::Call::name);

        match call_name {
            Some(parse::CallName::Custom(_)) | None => {}
            Some(name) => {
                return Ok(Some(doc.find_all_references(name)?));
            }
        }

        let Some(func) = (match call_name {
            Some(parse::CallName::Custom(name)) => doc.functions.get_func(name.as_inner()),
            _ => functions
                .iter()
                .find(|func| span_contains(func.span(), &token_span))
                .copied(),
        }) else {
            return Ok(None);
        };

        if call_name.is_none() {
            let range = doc.find_function_name_range(func)?;
            if !(range.start..=range.end).contains(&token_position) {
                return Ok(None);
            }
        }

        let Some(identity) = doc.function_identity(func) else {
            return Ok(None);
        };

        Ok(Some(documents.find_references_to(&identity)))
    }
}

impl Backend {
    async fn run_diagnostic_transaction<F>(&self, transition: F)
    where
        F: FnOnce(&mut WorkspaceState) -> Option<Vec<DiagnosticUpdate>>,
    {
        self.diagnostic_transaction
            .run(&self.workspace, transition, |updates| {
                self.publish_diagnostic_updates(updates)
            })
            .await;
    }

    async fn publish_diagnostic_updates(&self, updates: Vec<DiagnosticUpdate>) {
        for update in updates {
            self.client
                .publish_diagnostics(update.uri, update.diagnostics, update.version)
                .await;
        }
    }

    async fn import_completion(
        &self,
        uri: &Uri,
        source: &str,
        context: &ImportCompletionContext,
    ) -> Option<Vec<CompletionItem>> {
        let path = uri.to_file_path()?;
        let (project_settings, workspace_roots) = {
            let config = self.config.read().await;
            if !config.settings.experimental_features.imports {
                return None;
            }
            (
                config.settings.project.clone(),
                config.workspace_roots.clone(),
            )
        };

        Some(
            ProjectContext::discover(path.as_ref(), &project_settings, &workspace_roots)
                .map(|project| imports::complete_import(context, source, path.as_ref(), &project))
                .unwrap_or_default(),
        )
    }

    /// Re-run analysis for every open document, after configuration that affects
    /// dependency resolution has changed.
    async fn reanalyze_open_documents(&self) {
        let documents = self.workspace.write().await.begin_reanalysis();
        for input in documents {
            self.on_change(input).await;
        }
    }

    /// Function which executed on change of file (`did_save`, `did_open` or `did_change` methods)
    async fn on_change(&self, params: AnalysisInput) {
        let Some(path_buf) = params.uri.to_file_path() else {
            return;
        };
        let path = path_buf.as_ref();

        // Check if this is a witness file
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wit"))
        {
            self.on_change_witness(params).await;
            return;
        }

        let (settings, workspace_roots) = {
            let config = self.config.read().await;
            (config.settings.clone(), config.workspace_roots.clone())
        };
        let snapshot = AnalysisSnapshot::analyze(&params.text, path, &settings, &workspace_roots);
        self.run_diagnostic_transaction(|workspace| {
            workspace.replace_if_current(&params.uri, snapshot, params.version, params.generation)
        })
        .await;
    }

    /// Validate witness (.wit) files
    async fn on_change_witness(&self, params: AnalysisInput) {
        let diagnostics = validate_witness_file(&params.text);
        self.run_diagnostic_transaction(|workspace| {
            workspace.diagnostics_if_current(
                params.uri.clone(),
                diagnostics,
                params.version,
                params.generation,
            )
        })
        .await;
    }
}

/// Validate a witness (.wit) file and return diagnostics.
fn validate_witness_file(text: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let json: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            let line = u32::try_from(e.line().saturating_sub(1)).unwrap_or(0);
            let col = u32::try_from(e.column().saturating_sub(1)).unwrap_or(0);
            diagnostics.push(Diagnostic::new_simple(
                Range::new(
                    tower_lsp_server::lsp_types::Position::new(line, col),
                    tower_lsp_server::lsp_types::Position::new(line, col + 1),
                ),
                format!("JSON syntax error: {e}"),
            ));
            return diagnostics;
        }
    };

    let Some(obj) = json.as_object() else {
        diagnostics.push(Diagnostic::new_simple(
            Range::new(
                tower_lsp_server::lsp_types::Position::new(0, 0),
                tower_lsp_server::lsp_types::Position::new(0, 1),
            ),
            "Witness file must be a JSON object".to_string(),
        ));
        return diagnostics;
    };

    for (name, value) in obj {
        let Some(witness_obj) = value.as_object() else {
            // Find approximate position for this key
            if let Some(pos) = find_key_position(text, name) {
                diagnostics.push(Diagnostic::new_simple(
                    Range::new(pos, pos),
                    format!("Witness '{name}' must be an object with 'value' and 'type' fields"),
                ));
            }
            continue;
        };

        if !witness_obj.contains_key("value") {
            if let Some(pos) = find_key_position(text, name) {
                diagnostics.push(Diagnostic::new_simple(
                    Range::new(pos, pos),
                    format!("Witness '{name}' is missing required 'value' field"),
                ));
            }
        }

        if !witness_obj.contains_key("type") {
            if let Some(pos) = find_key_position(text, name) {
                diagnostics.push(Diagnostic::new_simple(
                    Range::new(pos, pos),
                    format!("Witness '{name}' is missing required 'type' field"),
                ));
            }
        }
    }

    diagnostics
}
