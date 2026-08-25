use std::path::PathBuf;

use tower_lsp_server::lsp_types::{
    CompletionOptions, FileSystemWatcher, GlobPattern, HoverProviderCapability, InitializeParams,
    InitializeResult, OneOf, SaveOptions, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, WorkDoneProgressOptions, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};
use tower_lsp_server::UriExt;

/// Collect the workspace folders the client opened with, falling back to the
/// deprecated `root_uri` for clients that do not send folders.
pub(super) fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
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

pub(super) fn initialize_result() -> InitializeResult {
    InitializeResult {
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
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                    legend: crate::semantic_tokens::legend(),
                    range: Some(false),
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                }),
            ),
            ..ServerCapabilities::default()
        },
    }
}

pub(super) fn watched_files() -> Vec<FileSystemWatcher> {
    ["**/*.simf", "**/Simplex.toml", "**/simplex.toml"]
        .into_iter()
        .map(|glob| FileSystemWatcher {
            glob_pattern: GlobPattern::String(glob.to_string()),
            kind: None,
        })
        .collect()
}
