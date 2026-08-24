#![warn(clippy::all, clippy::pedantic)]

mod analysis;
mod completion;
mod config;
mod error;
mod navigation;
mod project;
mod semantic_tokens;
mod server;
mod signature_help;
mod text;
mod utils;
mod workspace;

use server::Backend;
use tower_lsp_server::{LspService, Server};

/// Serve the SimplicityHL language server over the process standard streams.
pub async fn run_stdio() {
    let (service, socket) = LspService::new(Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
