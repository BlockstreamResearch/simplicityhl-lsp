use ropey::Rope;
use serde_json::Value;

use std::sync::Arc;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentFormattingParams, DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
    InitializeResult, InitializedParams, Location, MessageType, Position, Range, ReferenceParams,
    Registration, SemanticTokens, SemanticTokensParams, SemanticTokensResult, SignatureHelp,
    SignatureHelpParams, TextEdit, Uri,
};
use tower_lsp_server::{LanguageServer, UriExt};

use smplx_build::CONFIG_FILENAME;

use crate::analysis::AnalysisSnapshot;
use crate::completion::imports::{self, ImportCompletionContext};
use crate::config::Settings;
use crate::error::LspError;
use crate::format::{self, FormatError};
use crate::navigation::ReferenceTarget;
use crate::project::ProjectContext;
use crate::text::{offset_to_position, position_to_offset};
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
        self.config.write().await.apply_folder_change(params.event);
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
                        .is_some_and(|name| name.eq_ignore_ascii_case(CONFIG_FILENAME))
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

    /// Semantic highlighting: marks function names and the `jet::` namespace on top of the
    /// editor's grammar-based colours. Answers from the stored snapshot.
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;
        if is_witness(uri) {
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

    /// Structure of file, used by the editor's Outline panel, breadcrumbs and
    /// "Go to Symbol". Lists only top-level functions defined in this file.
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;
        if is_witness(uri) {
            return Ok(None);
        }

        let documents = self.workspace.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        Ok(Some(DocumentSymbolResponse::Nested(doc.document_symbols())))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = &params.text_document.uri;
        let Some(path) = uri.to_file_path() else {
            return Ok(None);
        };

        if path.extension().is_none_or(|ext| ext != "simf") {
            return Ok(None);
        }

        let Some(text) = self.workspace.read().await.text(uri) else {
            return Ok(None);
        };

        let binary = self.config.read().await.settings.formatter.path.clone();

        let path = path.into_owned();
        let source = Arc::clone(&text);
        let result = tokio::task::spawn_blocking(move || format::format(&source, &path, &binary))
            .await
            .map_err(|error| LspError::Internal(error.to_string()))?;

        match result {
            Ok(Some(formatted)) => Ok(Some(vec![TextEdit {
                range: Range {
                    start: Position::new(0, 0),
                    end: offset_to_position(text.len(), &Rope::from_str(&text))?,
                },
                new_text: formatted,
            }])),
            Ok(None) => Ok(Some(Vec::new())),
            // Syntax errors are already published as diagnostics.
            // Do not show them again on every format-on-save.
            Err(error @ FormatError::Failed(_)) => {
                self.client
                    .log_message(MessageType::WARNING, error.to_string())
                    .await;
                Ok(None)
            }
            Err(error) => {
                self.client
                    .show_message(MessageType::ERROR, error.to_string())
                    .await;
                Ok(None)
            }
        }
    }

    /// Add list of functions which we can use during typing it's name
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
        if is_witness(uri) {
            return Ok(None);
        }

        let documents = self.workspace.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        Ok(crate::hover::at(
            doc,
            params.text_document_position_params.position,
        )?)
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

        match doc.reference_target(params.text_document_position.position)? {
            None => Ok(None),
            Some(ReferenceTarget::Name(name)) => Ok(Some(doc.find_all_references(name)?)),
            Some(ReferenceTarget::Function(identity)) => {
                Ok(Some(documents.find_references_to(&identity)))
            }
        }
    }
}

fn is_witness(uri: &Uri) -> bool {
    std::path::Path::new(uri.path().as_str())
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("wit"))
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

        // NOTE: `analyze` is synchronous, and tower-lsp-server polls every handler in one task,
        // so hover, completion, etc. wait until it returns. Small contracts analyze quickly
        // enough today. If this becomes slow, run it in `spawn_blocking` and debounce
        // `did_change`.
        let snapshot = AnalysisSnapshot::analyze(&params.text, path, &settings, &workspace_roots);
        self.run_diagnostic_transaction(|workspace| {
            workspace.replace_if_current(&params.uri, snapshot, params.version, params.generation)
        })
        .await;
    }

    /// Validate witness (.wit) files
    async fn on_change_witness(&self, params: AnalysisInput) {
        // Parsing is input-dependent work. Keep it outside the global diagnostic gate and
        // workspace write lock; the generation check below safely discards stale results.
        let diagnostics = crate::witness::validate(&params.text);
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
