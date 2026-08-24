#![warn(clippy::all, clippy::pedantic)]

mod analysis;
mod backend;
mod completion;
mod config;
mod error;
mod imports;
mod navigation;
mod project;
mod semantic_tokens;
mod text;
mod utils;
mod workspace;

use backend::Backend;
use tower_lsp_server::{LspService, Server};

#[tokio::main]
async fn main() {
    env_logger::init();
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
