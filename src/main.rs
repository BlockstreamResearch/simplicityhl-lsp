#[tokio::main]
async fn main() {
    env_logger::init();
    simplicityhl_lsp::run_stdio().await;
}
