use ropey::Rope;
use serde_json::Value;
use simplicityhl::ast::ElementsJetHinter;
use simplicityhl::parse::ParseFromStrWithErrors;
use simplicityhl::TemplateProgram;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    Location, MarkupContent, MarkupKind, MessageType, OneOf, Range, ReferenceParams, Registration,
    SaveOptions, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelp,
    SignatureHelpOptions, SignatureHelpParams, SymbolKind, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
    WorkDoneProgressOptions, WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};
use tower_lsp_server::{Client, LanguageServer, UriExt};

use miniscript::iter::TreeLike;
use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, DiagnosticManager, Error as CompilerError,
    Location as CompilerLocation, Severity as CompilerSeverity, Span,
};
use simplicityhl::parse;

use crate::completion::{self, CompletionProvider};
use crate::config::Settings;
use crate::error::LspError;
use crate::function::Functions;
use crate::project::{ProjectContext, SIMPLEX_MANIFEST};
use crate::utils::{
    create_signature_info, find_builtin_signature, find_function_call_context, find_key_position,
    get_call_span, get_comments_from_lines, offset_to_position, position_to_span, span_contains,
    span_to_positions,
};

/// Semantic token type indices - must match the legend order
mod semantic_token_types {
    pub const FUNCTION: u32 = 0;
    pub const NAMESPACE: u32 = 5;
}

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

/// Get the semantic token legend for this server
fn get_semantic_token_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::FUNCTION,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::TYPE,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::NAMESPACE,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
        ],
    }
}

#[derive(Debug)]
pub struct SourceFile {
    pub uri: Uri,
    pub text: Rope,
}

#[derive(Debug)]
pub struct Document {
    /// Functions defined in file and imported modules.
    pub functions: Functions,

    /// Mapping from module id to its Uri.
    pub linearization_map: Vec<SourceFile>,

    /// Source of given document.
    pub text: Rope,

    /// Version of the text this document was built from, when the client supplied one.
    ///
    /// Notifications are served concurrently and complete out of order, so a slow
    /// analysis must not overwrite the result of a newer edit.
    pub version: Option<i32>,
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

#[derive(Debug)]
pub struct Backend {
    client: Client,

    document_map: Arc<RwLock<HashMap<Uri, Document>>>,

    config: Arc<RwLock<ServerConfig>>,

    completion_provider: CompletionProvider,
}

struct TextDocumentItem<'a> {
    uri: Uri,
    text: &'a str,
    version: Option<i32>,
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
                    trigger_characters: Some(vec![":".to_string(), "<".to_string()]),
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
                            legend: get_semantic_token_legend(),
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
        self.on_change(TextDocumentItem {
            uri: params.text_document.uri,
            text: &params.text_document.text,
            version: Some(params.text_document.version),
        })
        .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Sync is `FULL`, so the last change holds the whole document. Indexing the first
        // element instead would panic on the empty list some clients send, and would use
        // stale text whenever a client batches several changes into one notification.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        self.on_change(TextDocumentItem {
            text: &change.text,
            uri: params.text_document.uri,
            version: Some(params.text_document.version),
        })
        .await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(text) = params.text {
            self.on_change(TextDocumentItem {
                uri: params.text_document.uri,
                text: &text,
                version: None,
            })
            .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        // Without this the parsed document is retained for the rest of the session and the
        // editor keeps showing the diagnostics published for a file that is no longer open.
        self.document_map.write().await.remove(&uri);
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
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

        let documents = self.document_map.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        let functions = doc.functions.functions();
        let mut raw_tokens: Vec<(u32, u32, u32, u32, u32)> = Vec::new(); // (line, col, len, type, modifiers)

        for func in &functions {
            if func.span().file_id != 0 {
                continue;
            }

            // Add function name token (declaration)
            if let Ok(name_range) = doc.find_function_name_range(func) {
                let len = u32::try_from(func.name().as_inner().len()).map_err(LspError::from)?;
                raw_tokens.push((
                    name_range.start.line,
                    name_range.start.character,
                    len,
                    semantic_token_types::FUNCTION,
                    0b11, // DECLARATION | DEFINITION
                ));
            }

            // Add function call tokens by walking the expression tree
            let calls = parse::ExprTree::Expression(func.body())
                .pre_order_iter()
                .filter_map(|expr| {
                    if let parse::ExprTree::Call(call) = expr {
                        Some((call, get_call_span(call)))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            for (call, span) in calls {
                if let Ok((start, _end)) = span_to_positions(&span, &doc.text) {
                    let name = call.name();
                    let name_str = name.to_string();

                    // Determine token type based on call name
                    let (token_type, prefix_len) = match name {
                        parse::CallName::Jet(_) => {
                            // jet::xxx - add namespace token for "jet" and function for xxx
                            // First add "jet" as namespace
                            raw_tokens.push((
                                start.line,
                                start.character,
                                3, // "jet"
                                semantic_token_types::NAMESPACE,
                                0,
                            ));
                            // The function name starts after "jet::"
                            (semantic_token_types::FUNCTION, 5)
                        }
                        _ => (semantic_token_types::FUNCTION, 0),
                    };

                    // Add the function name token
                    let func_name_len = if prefix_len > 0 {
                        name_str.len().saturating_sub(prefix_len)
                    } else {
                        name_str.len()
                    };

                    if func_name_len > 0 {
                        raw_tokens.push((
                            start.line,
                            start.character + u32::try_from(prefix_len).map_err(LspError::from)?,
                            u32::try_from(func_name_len).map_err(LspError::from)?,
                            token_type,
                            0,
                        ));
                    }
                }
            }
        }

        // Sort tokens by position (line, then column)
        raw_tokens.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        // Convert to delta-encoded semantic tokens
        let mut semantic_tokens = Vec::new();
        let mut prev_line = 0u32;
        let mut prev_char = 0u32;

        for (line, col, len, token_type, modifiers) in raw_tokens {
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                col - prev_char
            } else {
                col
            };

            semantic_tokens.push(SemanticToken {
                delta_line,
                delta_start,
                length: len,
                token_type,
                token_modifiers_bitset: modifiers,
            });

            prev_line = line;
            prev_char = col;
        }

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: semantic_tokens,
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

        let documents = self.document_map.read().await;

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
        let documents = self.document_map.read().await;
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
        let documents = self.document_map.read().await;
        let uri = &params.text_document_position.text_document.uri;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        let pos = params.text_document_position.position;

        let Some(line) = doc.text.lines().nth(pos.line as usize) else {
            return Ok(None);
        };

        let Some(slice) = line.get_slice(..pos.character as usize) else {
            return Ok(None);
        };

        let Some(prefix) = slice.as_str() else {
            return Ok(None);
        };

        let completions = self
            .completion_provider
            .process_completions(prefix, &doc.functions.functions_and_docs())
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

        let documents = self.document_map.read().await;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };

        let token_pos = params.text_document_position_params.position;

        let token_span = position_to_span(token_pos, &doc.text)?;
        let Ok(Some(call)) = doc.find_related_call(token_span) else {
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
        let documents = self.document_map.read().await;
        let uri = &params.text_document_position_params.text_document.uri;

        // Return None if document not found (e.g., file has parse errors)
        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };
        let functions = doc.functions.functions();

        let token_position = params.text_document_position_params.position;
        let token_span = position_to_span(token_position, &doc.text)?;

        let Ok(Some(call)) = doc.find_related_call(token_span) else {
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

                let Some(source_file) = doc.linearization_map.get(function.span().file_id) else {
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
        let documents = self.document_map.read().await;
        let uri = &params.text_document_position.text_document.uri;

        let Some(doc) = documents.get(uri) else {
            return Ok(None);
        };
        let functions = doc.functions.functions();

        let token_position = params.text_document_position.position;

        let token_span = position_to_span(token_position, &doc.text)?;

        let call_name = doc
            .find_related_call(token_span)?
            .map(simplicityhl::parse::Call::name);

        match call_name {
            Some(parse::CallName::Custom(_)) | None => {}
            Some(name) => {
                return Ok(Some(doc.find_all_references(name)?));
            }
        }

        let Some(func) = functions.iter().find(|func| match call_name {
            Some(parse::CallName::Custom(name)) => func.name() == name,
            _ => span_contains(func.span(), &token_span),
        }) else {
            return Ok(None);
        };

        let range = doc.find_function_name_range(func)?;

        if (token_position <= range.end && token_position >= range.start) || call_name.is_some() {
            Ok(Some(
                documents
                    .values()
                    .filter_map(|document| {
                        document
                            .find_all_references(&parse::CallName::Custom(func.name().clone()))
                            .ok()
                    })
                    .flatten()
                    .collect(),
            ))
        } else {
            Ok(None)
        }
    }
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            document_map: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(RwLock::new(ServerConfig::default())),
            completion_provider: CompletionProvider::new(),
        }
    }

    /// Re-run analysis for every open document, after configuration that affects
    /// dependency resolution has changed.
    async fn reanalyze_open_documents(&self) {
        let documents = {
            let documents = self.document_map.read().await;
            documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), doc.text.to_string(), doc.version))
                .collect::<Vec<_>>()
        };
        for (uri, text, version) in documents {
            self.on_change(TextDocumentItem {
                uri,
                text: &text,
                version,
            })
            .await;
        }
    }

    /// Function which executed on change of file (`did_save`, `did_open` or `did_change` methods)
    async fn on_change(&self, params: TextDocumentItem<'_>) {
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
        let (err, document) = parse_program(params.text, path, &settings, &workspace_roots);
        let rope = Rope::from_str(params.text);
        let mut documents = self.document_map.write().await;

        // Analysis above runs without the lock, so a concurrent notification for a newer
        // version may already have stored its result. Dropping the stale write keeps the
        // map consistent with the latest text the client sent.
        let stored_version = documents.get(&params.uri).and_then(|doc| doc.version);
        if matches!((params.version, stored_version), (Some(incoming), Some(stored)) if incoming < stored)
        {
            return;
        }
        // `did_save` carries no version; keep the one already recorded rather than
        // clearing it, so later edits can still be ordered against it.
        let version = params.version.or(stored_version);

        if let Some(mut doc) = document {
            doc.version = version;
            documents.insert(params.uri.clone(), doc);
        } else if let Some(doc) = documents.get_mut(&params.uri) {
            doc.text = rope.clone();
            doc.version = version;
        }
        let diagnostics = err
            .iter()
            .filter_map(|err| {
                // HACK: We ignoring MainRequired error because right now we cannot parse file as a
                // library
                match err.error() {
                    simplicityhl::error::Error::MainRequired => return None,
                    simplicityhl::error::Error::CannotParse { msg }
                        if msg.clone()
                            == simplicityhl::error::Error::MainOutOfEntryFile.to_string() =>
                    {
                        return None;
                    }
                    _ => {}
                }

                // This merged backend owns one open document at a time. Compiler 0.7
                // diagnostics can point into imported files;
                // TODO: until multi-document publication is implemented, keep those visible on the
                // root document without pretending their byte offsets belong to the root source.
                let range = match err.location() {
                    CompilerLocation::Code(span) if span.file_id == 0 => {
                        let Ok((start, end)) = span_to_positions(span, &rope) else {
                            return None;
                        };
                        Range::new(start, end)
                    }
                    CompilerLocation::Code(_)
                    | CompilerLocation::File(_)
                    | CompilerLocation::Global => Range::default(),
                };
                let severity = match err.severity() {
                    CompilerSeverity::Error => DiagnosticSeverity::ERROR,
                    CompilerSeverity::Warning => DiagnosticSeverity::WARNING,
                };

                Some(Diagnostic {
                    range,
                    severity: Some(severity),
                    source: Some("simplicityhl".to_string()),
                    message: err.error().to_string(),
                    ..Diagnostic::default()
                })
            })
            .collect();

        self.client
            .publish_diagnostics(params.uri.clone(), diagnostics, params.version)
            .await;
    }

    /// Validate witness (.wit) files
    async fn on_change_witness(&self, params: TextDocumentItem<'_>) {
        let diagnostics = validate_witness_file(params.text);
        self.client
            .publish_diagnostics(params.uri.clone(), diagnostics, params.version)
            .await;
    }
}

/// Create [`Document`] using parsed program and code.
fn create_document(program: &simplicityhl::parse::Program, text: &str) -> Document {
    let mut document = Document {
        functions: Functions::new(),
        text: Rope::from_str(text),
        linearization_map: Vec::new(),
        version: None,
    };

    program
        .items()
        .iter()
        .filter_map(|item| {
            if let parse::Item::Function(func) = item {
                Some(func)
            } else {
                None
            }
        })
        .for_each(|func| {
            let start_line = offset_to_position(func.span().start, &document.text)
                .unwrap_or_default()
                .line;
            document.functions.insert(
                func.name().to_string(),
                func.to_owned(),
                get_comments_from_lines(start_line, &document.text),
            );
        });

    document
}

/// Parse and analyze a program using the [`simplicityhl`] compiler.
/// Also create a [`Document`] when parsing succeeds.
fn parse_program(
    text: &str,
    path: &Path,
    settings: &Settings,
    workspace_roots: &[PathBuf],
) -> (Vec<CompilerDiagnostic>, Option<Document>) {
    let unstable_features = settings.unstable_features();
    let mut diagnostics = DiagnosticManager::new();
    let text: Arc<str> = Arc::from(text);
    let source_file = simplicityhl::source::SourceFile::new(path, Arc::clone(&text));
    let Some(program) = parse::Program::parse_from_str_with_errors(
        0,
        text.as_ref(),
        &unstable_features,
        &mut diagnostics,
    ) else {
        return (diagnostics.diagnostics().to_vec(), None);
    };

    let mut document = create_document(&program, text.as_ref());

    // Import roots come from the Simplex manifest and the client's settings rather than
    // from the containing directory, so dependencies resolve the way `simplex` builds them.
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

            return (diagnostics.diagnostics().to_vec(), Some(document));
        }
    };
    let compiler_diagnostics = match TemplateProgram::new_with_dep(
        source_file.try_into().expect("name was defined above"),
        &dependencies,
        &unstable_features,
        Box::new(ElementsJetHinter::new()),
    ) {
        Ok(template_program) => {
            document.populate_visible_functions(&template_program);
            template_program.diagnostics().diagnostics().to_vec()
        }
        Err(diagnostics) => diagnostics.diagnostics().to_vec(),
    };

    (compiler_diagnostics, Some(document))
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
    use simplicityhl::error::Error;
    use tempfile::TempDir;

    use super::*;

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
        assert_eq!(doc.functions.map.len(), 2);
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
