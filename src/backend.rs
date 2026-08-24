use serde_json::Value;

use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::lsp_types::{
    CompletionItem, CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    Location, MarkupContent, MarkupKind, MessageType, OneOf, Range, ReferenceParams, Registration,
    SaveOptions, SemanticTokens, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, SignatureHelp, SignatureHelpOptions, SignatureHelpParams, SymbolKind,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri, WorkDoneProgressOptions, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};
use tower_lsp_server::{Client, LanguageServer, UriExt};

use simplicityhl::parse;

use crate::analysis::AnalysisSnapshot;
use crate::completion::{self, CompletionProvider};
use crate::config::Settings;
use crate::error::LspError;
use crate::imports::{self, ImportCompletionContext};
use crate::project::{ProjectContext, SIMPLEX_MANIFEST};
use crate::text::{
    get_call_span, position_to_offset, position_to_span, span_contains, span_to_positions,
};
use crate::utils::{
    create_signature_info, find_builtin_signature, find_function_call_context, find_key_position,
};
use crate::workspace::{AnalysisInput, DiagnosticUpdate, WorkspaceState};

/// Collect the workspace folders the client opened with, falling back to the
/// deprecated `root_uri` for clients that do not send folders.
fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let mut roots = params
        .workspace_folders
        .as_ref()
        .into_iter()
        .flatten()
        .filter_map(|folder| folder.uri.to_file_path().map(std::borrow::Cow::into_owned))
        .collect::<Vec<_>>();
    #[allow(deprecated)]
    if roots.is_empty() {
        if let Some(path) = params
            .root_uri
            .as_ref()
            .and_then(UriExt::to_file_path)
            .map(std::borrow::Cow::into_owned)
        {
            roots.push(path);
        }
    }
    roots
}

/// Client-supplied configuration, kept separate from the document cache so a
/// settings change does not need the document lock.
#[derive(Debug, Default)]
struct ServerConfig {
    settings: Settings,

    /// Workspace folders, used to resolve relative paths in [`Settings`].
    workspace_roots: Vec<PathBuf>,

    /// Whether the client supports server-requested file watchers.
    watched_files_registration: bool,
}

#[derive(Debug, Default)]
struct DiagnosticTransaction {
    gate: Mutex<()>,
}

impl DiagnosticTransaction {
    async fn run<F, P, Fut>(&self, workspace: &RwLock<WorkspaceState>, transition: F, publish: P)
    where
        F: FnOnce(&mut WorkspaceState) -> Option<Vec<DiagnosticUpdate>>,
        P: FnOnce(Vec<DiagnosticUpdate>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let _transaction = self.gate.lock().await;
        let updates = {
            let mut workspace = workspace.write().await;
            transition(&mut workspace)
        };
        if let Some(updates) = updates {
            publish(updates).await;
        }
    }
}

#[derive(Debug)]
pub struct Backend {
    client: Client,

    workspace: Arc<RwLock<WorkspaceState>>,

    /// Serializes each workspace diagnostic transition with its complete publication batch.
    diagnostic_transaction: DiagnosticTransaction,

    config: Arc<RwLock<ServerConfig>>,

    completion_provider: CompletionProvider,
}

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

        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    // `:`, space, `{`, and `,` cover the useful stages of a `use`
                    // declaration. `<` remains the trigger for type-cast completion.
                    trigger_characters: Some(vec![
                        ":".to_string(),
                        "<".to_string(),
                        " ".to_string(),
                        "{".to_string(),
                        ",".to_string(),
                    ]),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: crate::semantic_tokens::legend(),
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if !self.config.read().await.watched_files_registration {
            return;
        }

        let watchers = ["**/*.simf", "**/Simplex.toml", "**/simplex.toml"]
            .into_iter()
            .map(|glob| FileSystemWatcher {
                glob_pattern: GlobPattern::String(glob.to_string()),
                kind: None,
            })
            .collect();
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

        let token_pos = params.text_document_position_params.position;

        // Get the current line up to cursor position
        let line = doc
            .text
            .lines()
            .nth(token_pos.line as usize)
            .ok_or(LspError::Internal("Line not found".into()))?;

        let line_str = line
            .get_slice(..token_pos.character as usize)
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Find function call context: look for unclosed '(' and count commas
        let Some((func_name, active_param)) = find_function_call_context(&line_str) else {
            return Ok(None);
        };

        // Try to find the function signature
        let signature_info = if func_name.starts_with("jet::") {
            // It's a jet function
            let jet_name = func_name.strip_prefix("jet::").unwrap_or(&func_name);
            match simplicityhl::simplicity::jet::Elements::from_str(jet_name) {
                Ok(element) => {
                    let template = completion::jet::jet_to_template(element);
                    Some(create_signature_info(&template))
                }
                Err(_) => None,
            }
        } else if let Some((function, function_doc)) = doc.functions.get(&func_name) {
            // It's a custom function
            let template = completion::function_to_template(function, function_doc);
            Some(create_signature_info(&template))
        } else {
            // Try builtin functions
            find_builtin_signature(&func_name)
        };

        match signature_info {
            Some(sig) => Ok(Some(SignatureHelp {
                signatures: vec![sig],
                active_signature: Some(0),
                active_parameter: Some(active_param),
            })),
            None => Ok(None),
        }
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
        let functions = doc.functions.functions();

        let token_position = params.text_document_position_params.position;
        let token_span = position_to_span(token_position, &doc.text)?;

        if let Some(function) = doc.find_imported_function(token_span) {
            let Some(source_file) = doc.sources.get(function.span().file_id) else {
                return Ok(None);
            };
            let (start, end) = span_to_positions(function.as_ref(), &source_file.text)?;

            return Ok(Some(GotoDefinitionResponse::from(Location::new(
                source_file.uri.clone(),
                Range::new(start, end),
            ))));
        }

        let Some(call) = doc.find_related_call(token_span) else {
            let Some(func) = functions
                .iter()
                .find(|func| span_contains(func.span(), &token_span))
            else {
                return Ok(None);
            };
            let range = doc.find_function_name_range(func)?;

            if token_position <= range.end && token_position >= range.start {
                return Ok(Some(GotoDefinitionResponse::from(Location::new(
                    uri.clone(),
                    range,
                ))));
            }
            return Ok(None);
        };

        match call.name() {
            simplicityhl::parse::CallName::Custom(func) => {
                let Some(function) = doc.functions.get_func(func.as_inner()) else {
                    return Ok(None);
                };

                let Some(source_file) = doc.sources.get(function.span().file_id) else {
                    return Ok(None);
                };

                let (start, end) = span_to_positions(function.as_ref(), &source_file.text)?;

                Ok(Some(GotoDefinitionResponse::from(Location::new(
                    source_file.uri.clone(),
                    Range::new(start, end),
                ))))
            }
            _ => Ok(None),
        }
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
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(RwLock::new(WorkspaceState::default())),
            diagnostic_transaction: DiagnosticTransaction::default(),
            config: Arc::new(RwLock::new(ServerConfig::default())),
            completion_provider: CompletionProvider::new(),
        }
    }

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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    use ropey::Rope;
    use simplicityhl::error::{
        Diagnostic as CompilerDiagnostic, DiagnosticManager, Error, Location as CompilerLocation,
        Span,
    };
    use simplicityhl::parse::ParseFromStrWithErrors;
    use simplicityhl::UnstableFeatures;
    use tempfile::TempDir;
    use tokio::sync::Notify;
    use tower_lsp_server::lsp_types::SemanticToken;

    use super::*;
    use crate::text::offset_to_position;

    /// `parse_program` resolves imports from the project the file lives in, so tests
    /// need a real path on disk rather than a placeholder.
    fn in_temp_project(source: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("temp dir");
        std::fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");
        std::fs::create_dir(temp.path().join("simf")).expect("create source dir");
        let path = temp.path().join("simf/main.simf");
        std::fs::write(&path, source).expect("write source");
        (temp, path)
    }

    const IMPORTING_ROOT: &str = "use crate::shared::broken;\nfn main() { broken(); }\n";

    struct DependencyProject {
        temp: TempDir,
        root_path: PathBuf,
        dependency_path: PathBuf,
        root_uri: Uri,
        dependency_uri: Uri,
        settings: Settings,
    }

    impl DependencyProject {
        fn new(dependency_source: &str) -> Self {
            let temp = TempDir::new().expect("temp dir");
            std::fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");
            std::fs::create_dir(temp.path().join("simf")).expect("create source dir");
            let dependency_path = temp.path().join("simf/shared.simf");
            let root_path = temp.path().join("simf/main.simf");
            std::fs::write(&dependency_path, dependency_source).expect("write dependency");
            std::fs::write(&root_path, IMPORTING_ROOT).expect("write root");
            let root_uri = Uri::from_file_path(
                std::fs::canonicalize(&root_path).expect("canonical root path"),
            )
            .expect("root URI");
            let dependency_uri = Uri::from_file_path(
                std::fs::canonicalize(&dependency_path).expect("canonical dependency path"),
            )
            .expect("dependency URI");
            let settings = Settings::from_json(serde_json::json!({
                "experimentalFeatures": { "imports": true }
            }))
            .expect("valid settings");
            Self {
                temp,
                root_path,
                dependency_path,
                root_uri,
                dependency_uri,
                settings,
            }
        }

        fn root_snapshot(&self) -> AnalysisSnapshot {
            AnalysisSnapshot::analyze(
                IMPORTING_ROOT,
                &self.root_path,
                &self.settings,
                &[self.temp.path().to_path_buf()],
            )
        }

        fn dependency_snapshot(&self, source: &str) -> AnalysisSnapshot {
            AnalysisSnapshot::analyze(
                source,
                &self.dependency_path,
                &self.settings,
                &[self.temp.path().to_path_buf()],
            )
        }

        fn write_dependency(&self, source: &str) {
            std::fs::write(&self.dependency_path, source).expect("write dependency");
        }
    }

    fn diagnostic_count(updates: &[DiagnosticUpdate], uri: &Uri) -> usize {
        updates
            .iter()
            .find(|update| &update.uri == uri)
            .expect("diagnostic update")
            .diagnostics
            .len()
    }

    fn record_count(
        events: &StdMutex<Vec<(&'static str, usize)>>,
        label: &'static str,
        updates: &[DiagnosticUpdate],
        uri: &Uri,
    ) {
        events
            .lock()
            .expect("event lock")
            .push((label, diagnostic_count(updates, uri)));
    }

    fn sample_program() -> &'static str {
        "fn add(a: u32, b: u32) -> u32 { let (_, res): (bool, u32) = jet::add_32(a, b); res }
         fn main() {}"
    }
    fn invalid_program_on_ast() -> &'static str {
        "fn add(a: u32, b: u32) -> u32 {}
         fn main() {}"
    }

    fn invalid_program_on_parsing() -> &'static str {
        "fn add(a: u32, b: u32) -> u32 "
    }

    type RawSemanticToken = (u32, u32, u32, u32, u32);
    const FUNCTION_TOKEN: u32 = 0;
    const NAMESPACE_TOKEN: u32 = 5;

    fn parse_program(
        source: &str,
        path: &Path,
        settings: &Settings,
        workspace_roots: &[PathBuf],
    ) -> (Vec<CompilerDiagnostic>, Option<AnalysisSnapshot>) {
        let snapshot = AnalysisSnapshot::analyze(source, path, settings, workspace_roots);
        let mut syntax_diagnostics = DiagnosticManager::new();
        let parsed = parse::Program::parse_from_str_with_errors(
            0,
            source,
            &settings.unstable_features(),
            &mut syntax_diagnostics,
        )
        .is_some();
        let diagnostics = snapshot.compiler_diagnostics.clone();
        (diagnostics, parsed.then_some(snapshot))
    }

    fn document_from_source(source: &str) -> AnalysisSnapshot {
        let mut diagnostics = DiagnosticManager::new();
        let program = parse::Program::parse_from_str_with_errors(
            0,
            source,
            &UnstableFeatures::none(),
            &mut diagnostics,
        )
        .unwrap_or_else(|| panic!("source should parse: {diagnostics:?}"));
        let path = std::env::temp_dir().join("semantic_tokens.simf");
        AnalysisSnapshot::from_program(&program, source, &path)
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn diagnostic_transactions_do_not_interleave_state_and_publication() {
        let project = DependencyProject::new("pub fn broken() -> u32 { false }\n");
        let old_snapshot = project.root_snapshot();
        project.write_dependency("pub fn broken() -> u32 { 0 }\n");
        let new_snapshot = project.root_snapshot();
        let root_uri = project.root_uri.clone();
        let dependency_uri = project.dependency_uri.clone();

        let transaction = StdArc::new(DiagnosticTransaction::default());
        let workspace = StdArc::new(RwLock::new(WorkspaceState::default()));
        let old_input = workspace
            .write()
            .await
            .begin_open(&root_uri, IMPORTING_ROOT, Some(1));
        let first_started = StdArc::new(Notify::new());
        let release_first = StdArc::new(Notify::new());
        let second_started = StdArc::new(Notify::new());
        let second_transition_ran = StdArc::new(AtomicBool::new(false));
        let events = StdArc::new(StdMutex::new(Vec::new()));

        let first_wait = first_started.notified();
        let first = {
            let transaction = StdArc::clone(&transaction);
            let workspace = StdArc::clone(&workspace);
            let first_started = StdArc::clone(&first_started);
            let release_first = StdArc::clone(&release_first);
            let events = StdArc::clone(&events);
            let root_uri = root_uri.clone();
            let dependency_uri = dependency_uri.clone();
            tokio::spawn(async move {
                transaction
                    .run(
                        &workspace,
                        |workspace| {
                            workspace.replace_if_current(
                                &root_uri,
                                old_snapshot,
                                old_input.version,
                                old_input.generation,
                            )
                        },
                        |updates| async move {
                            record_count(
                                &events,
                                "first publication started",
                                &updates,
                                &dependency_uri,
                            );
                            first_started.notify_one();
                            release_first.notified().await;
                            record_count(
                                &events,
                                "first publication finished",
                                &updates,
                                &dependency_uri,
                            );
                        },
                    )
                    .await;
            })
        };
        first_wait.await;
        let new_input = workspace
            .write()
            .await
            .begin_change(&root_uri, IMPORTING_ROOT, Some(2))
            .expect("new analysis ticket");

        let second_wait = second_started.notified();
        let second = {
            let transaction = StdArc::clone(&transaction);
            let workspace = StdArc::clone(&workspace);
            let second_started = StdArc::clone(&second_started);
            let second_transition_ran = StdArc::clone(&second_transition_ran);
            let events = StdArc::clone(&events);
            let root_uri = root_uri.clone();
            let dependency_uri = dependency_uri.clone();
            tokio::spawn(async move {
                second_started.notify_one();
                transaction
                    .run(
                        &workspace,
                        |workspace| {
                            second_transition_ran.store(true, Ordering::SeqCst);
                            workspace.replace_if_current(
                                &root_uri,
                                new_snapshot,
                                new_input.version,
                                new_input.generation,
                            )
                        },
                        |updates| async move {
                            record_count(&events, "second publication", &updates, &dependency_uri);
                        },
                    )
                    .await;
            })
        };

        second_wait.await;
        tokio::task::yield_now().await;
        assert!(!second_transition_ran.load(Ordering::SeqCst));

        release_first.notify_one();
        first.await.expect("first transaction");
        second.await.expect("second transaction");
        assert_eq!(
            *events.lock().expect("event lock"),
            [
                ("first publication started", 1),
                ("first publication finished", 1),
                ("second publication", 0)
            ]
        );

        let workspace = workspace.read().await;
        let final_snapshot = workspace.get(&root_uri).expect("latest root snapshot");
        let final_diagnostics =
            crate::workspace::diagnostics::DiagnosticBundle::from_snapshot(final_snapshot);
        assert!(final_diagnostics.get(&dependency_uri).is_none());
    }

    #[tokio::test]
    async fn closing_document_restores_dependency_diagnostics_through_transaction() {
        let broken_dependency = "pub fn broken() -> u32 { false }\n";
        let clean_dependency = "pub fn broken() -> u32 { 0 }\n";
        let project = DependencyProject::new(broken_dependency);
        let root_snapshot = project.root_snapshot();
        let clean_snapshot = project.dependency_snapshot(clean_dependency);
        let root_uri = project.root_uri.clone();
        let dependency_uri = project.dependency_uri.clone();

        let transaction = DiagnosticTransaction::default();
        let workspace = RwLock::new(WorkspaceState::default());
        let root_input = workspace
            .write()
            .await
            .begin_open(&root_uri, IMPORTING_ROOT, Some(1));
        workspace
            .write()
            .await
            .replace_if_current(
                &root_uri,
                root_snapshot,
                root_input.version,
                root_input.generation,
            )
            .expect("root diagnostic update");
        let open = workspace
            .write()
            .await
            .begin_open(&dependency_uri, clean_dependency, Some(7));
        let publications = StdArc::new(StdMutex::new(Vec::new()));

        transaction
            .run(
                &workspace,
                |workspace| {
                    workspace.replace_if_current(
                        &dependency_uri,
                        clean_snapshot,
                        open.version,
                        open.generation,
                    )
                },
                {
                    let publications = StdArc::clone(&publications);
                    let dependency_uri = dependency_uri.clone();
                    move |updates| async move {
                        let update = updates
                            .iter()
                            .find(|update| update.uri == dependency_uri)
                            .expect("direct dependency update");
                        publications
                            .lock()
                            .expect("publication lock")
                            .push((update.diagnostics.len(), update.version));
                    }
                },
            )
            .await;

        let close_generation = workspace
            .write()
            .await
            .begin_close(&dependency_uri)
            .expect("close ticket");
        transaction
            .run(
                &workspace,
                |workspace| workspace.remove_if_current(&dependency_uri, close_generation),
                {
                    let publications = StdArc::clone(&publications);
                    let dependency_uri = dependency_uri.clone();
                    move |updates| async move {
                        let update = updates
                            .iter()
                            .find(|update| update.uri == dependency_uri)
                            .expect("restored dependency update");
                        publications
                            .lock()
                            .expect("publication lock")
                            .push((update.diagnostics.len(), update.version));
                    }
                },
            )
            .await;

        assert_eq!(
            *publications.lock().expect("publication lock"),
            [(0, Some(7)), (1, None)]
        );
    }

    fn decode_semantic_tokens(tokens: &[SemanticToken]) -> Vec<RawSemanticToken> {
        let mut line = 0;
        let mut character = 0;
        tokens
            .iter()
            .map(|token| {
                line += token.delta_line;
                character = if token.delta_line == 0 {
                    character + token.delta_start
                } else {
                    token.delta_start
                };
                (
                    line,
                    character,
                    token.length,
                    token.token_type,
                    token.token_modifiers_bitset,
                )
            })
            .collect()
    }

    fn expected_token(
        source: &str,
        offset: usize,
        text: &str,
        token_type: u32,
    ) -> RawSemanticToken {
        let rope = Rope::from_str(source);
        let start = offset_to_position(offset, &rope).expect("valid token start");
        let end = offset_to_position(offset + text.len(), &rope).expect("valid token end");
        (
            start.line,
            start.character,
            end.character - start.character,
            token_type,
            0,
        )
    }

    #[test]
    fn semantic_tokens_split_generic_callable_components() {
        let source = "// 😀 keeps UTF-16 columns honest\nfn consume_budget(acc: u32, item: u32) -> u32 { acc }\nfn main() { array_fold::<consume_budget, 320>(witness::PADDING, true); }\n";
        let document = document_from_source(source);
        let decoded = decode_semantic_tokens(&crate::semantic_tokens::tokens(&document));
        let builtin_offset = source.find("array_fold").expect("array_fold call");
        let callback_offset = source.rfind("consume_budget").expect("callback argument");

        assert!(decoded.contains(&expected_token(
            source,
            builtin_offset,
            "array_fold",
            FUNCTION_TOKEN,
        )));
        assert!(decoded.contains(&expected_token(
            source,
            callback_offset,
            "consume_budget",
            FUNCTION_TOKEN,
        )));

        let bound_offset = source.find("320").expect("array bound");
        let bound_position = offset_to_position(bound_offset, &document.text).unwrap();
        assert!(!decoded.iter().any(|token| {
            token.0 == bound_position.line
                && token.1 <= bound_position.character
                && token.1 + token.2 > bound_position.character
        }));
    }

    #[test]
    fn semantic_tokens_bound_each_builtin_to_its_identifier() {
        let source = "fn step(acc: u32, item: u32) -> u32 { acc }\nfn main() {\nfold::<step, 2>(0, 0);\nfor_while::<step>(0);\nunwrap_left::<u32>(0);\n<u32>::into(0);\njet::add_32(0, 0);\nassert!(true);\n}\n";
        let document = document_from_source(source);
        let decoded = decode_semantic_tokens(&crate::semantic_tokens::tokens(&document));

        for name in ["fold", "for_while", "unwrap_left", "into", "assert!"] {
            let offset = source
                .find(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(decoded.contains(&expected_token(source, offset, name, FUNCTION_TOKEN,)));
        }

        let callback_offsets = [
            source.find("step, 2").unwrap(),
            source.rfind("step").unwrap(),
        ];
        for offset in callback_offsets {
            assert!(decoded.contains(&expected_token(source, offset, "step", FUNCTION_TOKEN,)));
        }

        let jet_offset = source.find("jet::add_32").unwrap();
        assert!(decoded.contains(&expected_token(source, jet_offset, "jet", NAMESPACE_TOKEN,)));
        assert!(decoded.contains(&expected_token(
            source,
            jet_offset + "jet::".len(),
            "add_32",
            FUNCTION_TOKEN,
        )));
    }

    #[test]
    fn test_parse_program_valid() {
        let (temp, path) = in_temp_project(sample_program());
        let (err, doc) = parse_program(
            sample_program(),
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        assert!(err.is_empty(), "Expected no parsing error, got {err:?}");
        let doc = doc.expect("Expected Some(Document)");
        assert_eq!(doc.functions.iter().count(), 2);
    }

    #[test]
    fn library_file_without_main_keeps_definition_metadata() {
        let source = "fn helper() {}\nfn caller() { helper() }\n";
        let (temp, path) = in_temp_project(source);
        let (errors, doc) = parse_program(
            source,
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let doc = doc.expect("library document");
        let call_start = source.rfind("helper").expect("helper call");
        let call = doc
            .find_related_call(Span::new(0, call_start + 1..call_start + 1))
            .expect("helper call should remain navigable");
        let function = doc
            .functions
            .get_func(call.name().to_string().as_str())
            .expect("helper definition");
        let source_file = doc
            .sources
            .get(function.span().file_id)
            .expect("current file should always have source metadata");

        assert_eq!(function.name().as_inner(), "helper");
        assert_eq!(function.span().file_id, 0);
        assert_eq!(
            source_file.uri,
            Uri::from_file_path(std::fs::canonicalize(&path).expect("canonical path"))
                .expect("file URI")
        );
    }

    #[test]
    fn nested_main_does_not_conflict_with_a_synthetic_entry_point() {
        let source = "mod nested { fn main() {} }\n";
        let (temp, path) = in_temp_project(source);
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        let (errors, doc) = parse_program(source, &path, &settings, &[temp.path().to_path_buf()]);

        assert!(
            doc.is_some(),
            "the source should remain available to the LSP"
        );
        assert!(
            !errors.iter().any(|diagnostic| {
                matches!(
                    diagnostic.error(),
                    Error::FunctionRedefined { name } if name.as_inner() == "main"
                )
            }),
            "a nested main must not collide with an injected main: {errors:?}"
        );
        let expected = Error::MainOutOfEntryFile.to_string();
        assert!(
            errors.iter().any(|diagnostic| {
                matches!(diagnostic.error(), Error::CannotParse { msg } if msg == &expected)
            }),
            "the compiler should still report that main is outside the entry scope: {errors:?}"
        );
    }

    #[test]
    fn use_items_and_aliases_resolve_to_the_imported_definition() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        std::fs::write(root.join("Simplex.toml"), "").expect("write manifest");
        std::fs::create_dir(root.join("simf")).expect("create source dir");
        let dependency_path = root.join("simf/math.simf");
        std::fs::write(&dependency_path, "pub fn add() {}\npub fn subtract() {}\n")
            .expect("write module");
        let source =
            "use crate::math::{add as plus, subtract};\nfn main() { plus(); subtract() }\n";
        let path = root.join("simf/main.simf");
        std::fs::write(&path, source).expect("write entry file");
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        let (errors, doc) = parse_program(source, &path, &settings, &[root.to_path_buf()]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let doc = doc.expect("document");
        let imported_at = |needle: &str| {
            let start = source.find(needle).expect("import token");
            doc.find_imported_function(Span::new(0, start + 1..start + 1))
                .expect("imported function")
        };

        let original = imported_at("add as");
        let alias = imported_at("plus,");
        let grouped_item = imported_at("subtract}");
        assert_eq!(original.name().as_inner(), "add");
        assert_eq!(alias.name().as_inner(), "add");
        assert_eq!(grouped_item.name().as_inner(), "subtract");
        assert!(doc
            .find_imported_function(Span::new(
                0,
                source.find("math").unwrap() + 1..source.find("math").unwrap() + 1,
            ))
            .is_none());

        let source_file = doc
            .sources
            .get(original.span().file_id)
            .expect("imported source metadata");
        assert_eq!(
            source_file.uri,
            Uri::from_file_path(
                std::fs::canonicalize(&dependency_path).expect("canonical dependency path"),
            )
            .expect("file URI")
        );
    }

    #[test]
    fn nested_and_transitive_reexports_resolve_to_original_definitions() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        let write = |path: PathBuf, source: &str| {
            std::fs::create_dir_all(path.parent().expect("has parent")).expect("create dir");
            std::fs::write(path, source).expect("write file");
        };

        write(
            root.join("Simplex.toml"),
            "[dependencies]\nmerkle = { path = 'deps/merkle' }\nfacade = { path = 'deps/facade' }\n",
        );
        write(root.join("deps/merkle/Simplex.toml"), "");
        let merkle_path = root.join("deps/merkle/simf/build_root.simf");
        write(
            merkle_path.clone(),
            "pub mod wrapper {\n    pub fn get_root() {}\n    pub fn hash() {}\n}\npub use crate::wrapper::{get_root, hash};\n",
        );
        write(
            root.join("deps/facade/Simplex.toml"),
            "[dependencies]\nleaf = { path = '../leaf' }\n",
        );
        write(
            root.join("deps/facade/simf/smth.simf"),
            "pub use leaf::ops::hash;\n",
        );
        write(root.join("deps/leaf/Simplex.toml"), "");
        let leaf_path = root.join("deps/leaf/simf/ops.simf");
        write(leaf_path.clone(), "pub fn hash() {}\n");

        let source = "use merkle::build_root::{get_root, hash as and_hash};\nuse facade::smth::hash as or_hash;\nfn main() { get_root(); and_hash(); or_hash(); }\n";
        let path = root.join("simf/main.simf");
        write(path.clone(), source);
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        let (errors, doc) = parse_program(source, &path, &settings, &[root.to_path_buf()]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let doc = doc.expect("document");
        let imported_at = |offset: usize| {
            doc.find_imported_function(Span::new(0, offset + 1..offset + 1))
                .expect("imported function")
        };

        let nested = imported_at(source.find("get_root").expect("nested import"));
        let nested_alias = imported_at(source.find("and_hash").expect("nested alias"));
        let transitive = imported_at(source.rfind("hash as").expect("transitive import"));
        let transitive_alias = imported_at(source.find("or_hash").expect("transitive alias"));

        assert_eq!(nested.name().as_inner(), "get_root");
        assert_eq!(nested_alias.name().as_inner(), "hash");
        assert_eq!(transitive.name().as_inner(), "hash");
        assert_eq!(transitive_alias.name().as_inner(), "hash");

        let definition_uri = |function: &parse::Function| &doc.sources[function.span().file_id].uri;
        let merkle_uri =
            Uri::from_file_path(std::fs::canonicalize(merkle_path).expect("canonical merkle path"))
                .expect("merkle URI");
        let leaf_uri =
            Uri::from_file_path(std::fs::canonicalize(leaf_path).expect("canonical leaf path"))
                .expect("leaf URI");
        assert_eq!(definition_uri(nested), &merkle_uri);
        assert_eq!(definition_uri(nested_alias), &merkle_uri);
        assert_eq!(definition_uri(transitive), &leaf_uri);
        assert_eq!(definition_uri(transitive_alias), &leaf_uri);
    }

    #[test]
    fn parse_program_respects_the_enum_feature_setting() {
        let source = "enum Choice { Yes, No, }\nfn main() {}\n";
        let (temp, path) = in_temp_project(source);

        let (disabled, _) = parse_program(
            source,
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        assert!(disabled.iter().any(|diagnostic| {
            matches!(
                diagnostic.error(),
                Error::UnstableFeature {
                    feature: simplicityhl::UnstableFeature::Enums
                }
            )
        }));

        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": false, "enums": true }
        }))
        .expect("valid settings");
        let (enabled, document) =
            parse_program(source, &path, &settings, &[temp.path().to_path_buf()]);

        assert!(enabled.is_empty(), "enum should be enabled: {enabled:?}");
        assert!(document.is_some());
    }

    #[test]
    fn function_selection_range_is_inside_its_document_symbol_range() {
        let source = "/* 😀 */ fn main() {}";
        let (temp, path) = in_temp_project(source);
        let (errors, doc) = parse_program(
            source,
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let doc = doc.expect("document");
        let function = doc
            .functions
            .functions()
            .into_iter()
            .find(|function| function.name().as_inner() == "main")
            .expect("main function");
        let (start, end) = span_to_positions(function.span(), &doc.text).unwrap();
        let full_range = Range::new(start, end);
        let selection_range = doc.find_function_name_range(function).unwrap();

        assert!(selection_range.start >= full_range.start);
        assert!(selection_range.end <= full_range.end);
        let name_start = source.find("main").expect("function name");
        assert_eq!(
            selection_range,
            Range::new(
                offset_to_position(name_start, &doc.text).unwrap(),
                offset_to_position(name_start + "main".len(), &doc.text).unwrap(),
            )
        );
    }

    #[test]
    fn stale_analysis_cannot_produce_an_out_of_bounds_selection_range() {
        let source = "fn main() {}";
        let (temp, path) = in_temp_project(source);
        let (_, doc) = parse_program(
            source,
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        let mut doc = doc.expect("document");
        let function = doc
            .functions
            .functions()
            .into_iter()
            .find(|function| function.name().as_inner() == "main")
            .expect("main function")
            .clone();

        doc.text = Rope::from_str(&format!("// {}\n{source}", "x".repeat(100)));

        assert!(doc.find_function_name_range(&function).is_err());
    }

    #[test]
    fn looking_for_a_call_outside_a_function_is_an_empty_result() {
        let source = "/* heading */\nfn main() {}";
        let (temp, path) = in_temp_project(source);
        let (errors, doc) = parse_program(
            source,
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let doc = doc.expect("document");

        assert!(doc
            .find_related_call(simplicityhl::error::Span::new(0, 0..0))
            .is_none());
    }

    #[test]
    #[ignore = "TODO we need to also create a file with a path so that could work"]
    fn test_parse_program_invalid_ast() {
        let (temp, path) = in_temp_project(invalid_program_on_ast());
        let (err, doc) = parse_program(
            invalid_program_on_ast(),
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        assert!(
            err.first()
                .expect("program should produce an error")
                .to_string()
                .contains("Expected expression of type `u32`, found type `()`"),
            "Expected error on return type"
        );
        assert!(doc.is_some(), "Expected problem in AST build, not parse");
    }

    #[test]
    fn parse_program_resolves_a_manifest_dependency() {
        // End-to-end check that the manifest drives import resolution: `math` is only
        // reachable because Simplex.toml declares it, and `src_dir` points the package
        // root at `contracts` rather than the default `simf`.
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        let write = |path: PathBuf, source: &str| {
            std::fs::create_dir_all(path.parent().expect("has parent")).expect("create dir");
            std::fs::write(path, source).expect("write file");
        };
        write(
            root.join("Simplex.toml"),
            "[build]\nsrc_dir = 'contracts'\n[dependencies]\nmath = { path = 'vendor/math' }\n",
        );
        write(root.join("vendor/math/Simplex.toml"), "");
        write(
            root.join("vendor/math/simf/ops.simf"),
            "pub fn double(a: u32) -> u32 {\n    let (_, n): (bool, u32) = jet::add_32(a, a);\n    n\n}\n",
        );

        let source = "use math::ops::double;\nfn main() {\n    let _: u32 = double(2);\n}\n";
        let path = root.join("contracts/main.simf");
        write(path.clone(), source);

        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        let (err, doc) = parse_program(source, &path, &settings, &[root.to_path_buf()]);

        assert!(
            err.is_empty(),
            "expected the import to resolve, got {err:?}"
        );
        assert!(doc.is_some(), "expected a document");
    }

    #[test]
    fn duplicate_imported_main_points_to_the_import() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        std::fs::write(root.join("Simplex.toml"), "").expect("write manifest");
        std::fs::create_dir(root.join("simf")).expect("create source dir");
        std::fs::write(
            root.join("simf/library.simf"),
            "pub fn helper() {}\nfn main() {}\n",
        )
        .expect("write imported module");

        let import = "use crate::library::helper;";
        let source = format!("{import}\nfn main() {{}}\n");
        let path = root.join("simf/main.simf");
        std::fs::write(&path, &source).expect("write entry file");
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        let (errors, _) = parse_program(&source, &path, &settings, &[root.to_path_buf()]);
        let duplicate_main = errors
            .iter()
            .find(|error| {
                matches!(
                    error.error(),
                    Error::FunctionRedefined { name } if name.as_inner() == "main"
                )
            })
            .expect("duplicate main diagnostic");

        let CompilerLocation::Code(span) = duplicate_main.location() else {
            panic!("duplicate main should point to source code");
        };
        assert_eq!(span.to_slice(&source), Some(import));
    }

    #[test]
    fn imported_diagnostic_points_to_its_real_file() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        std::fs::write(root.join("Simplex.toml"), "").expect("write manifest");
        std::fs::create_dir(root.join("simf")).expect("create source dir");
        let library_path = root.join("simf/library.simf");
        let library_source = "pub fn broken() -> (u1, u256) { 0 }\n";
        std::fs::write(&library_path, library_source).expect("write imported module");

        let source = "use crate::library::broken;\nfn main() { broken(); }\n";
        let path = root.join("simf/main.simf");
        std::fs::write(&path, source).expect("write entry file");
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        let (_errors, document) = parse_program(source, &path, &settings, &[root.to_path_buf()]);
        let document = document.expect("document remains available after analysis errors");
        assert!(document.sources.len() > 1);
        let bundle = crate::workspace::diagnostics::DiagnosticBundle::from_snapshot(&document);
        let library_uri = Uri::from_file_path(
            std::fs::canonicalize(&library_path).expect("canonical library path"),
        )
        .expect("library URI");
        let root_uri = Uri::from_file_path(std::fs::canonicalize(&path).expect("canonical root"))
            .expect("root URI");

        let imported_error = bundle
            .get(&library_uri)
            .and_then(|diagnostics| {
                diagnostics
                    .iter()
                    .find(|diagnostic| diagnostic.message.contains("Expected expression"))
            })
            .unwrap_or_else(|| panic!("expected imported diagnostic, got {bundle:?}"));
        assert_ne!(imported_error.range, Range::default());
        assert!(!bundle.get(&root_uri).is_some_and(|diagnostics| {
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == imported_error.message)
        }));
    }

    #[test]
    fn lsp_diagnostics_preserve_secondary_labels_notes_and_help() {
        let source = "fn main() {}\n";
        let mut document = document_from_source(source);
        let diagnostic = CompilerDiagnostic::new(
            Error::CannotParse {
                msg: "primary".to_string(),
            },
            Span::new(0, 3..7),
        )
        .with_secondary(Span::new(0, 0..2), "secondary")
        .with_note("context")
        .with_help("fix it");
        document.compiler_diagnostics = vec![diagnostic];
        let bundle = crate::workspace::diagnostics::DiagnosticBundle::from_snapshot(&document);
        let uri = &document.sources[0].uri;
        let published = &bundle.get(uri).expect("root diagnostic")[0];

        assert!(published.message.contains("Note: context"));
        assert!(published.message.contains("Help: fix it"));
        let related = published
            .related_information
            .as_ref()
            .expect("secondary label becomes related information");
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "secondary");
        assert_eq!(related[0].location.uri, document.sources[0].uri);
    }

    #[test]
    fn parse_program_reports_a_missing_configured_manifest() {
        let (temp, path) = in_temp_project(sample_program());
        let mut settings = Settings::default();
        settings.project.simplex.manifest_path = "nowhere/Simplex.toml".to_string();

        let (err, _) = parse_program(
            sample_program(),
            &path,
            &settings,
            &[temp.path().to_path_buf()],
        );

        assert!(
            err.iter()
                .any(|e| e.to_string().contains("Simplex manifest was not found")),
            "a misconfigured manifest path should surface as a diagnostic, got {err:?}"
        );
    }

    #[test]
    fn test_parse_program_invalid_parse() {
        let (temp, path) = in_temp_project(invalid_program_on_parsing());
        let (err, doc) = parse_program(
            invalid_program_on_parsing(),
            &path,
            &Settings::default(),
            &[temp.path().to_path_buf()],
        );
        match err
            .first()
            .expect("program should produce an error")
            .error()
            .clone()
        {
            Error::Syntax { .. } => {}
            _ => panic!("Expected `Syntax` error"),
        }

        assert!(doc.is_none(), "Expected no document to return");
    }
}
