mod support;

use std::fs;

use serde_json::json;
use tempfile::TempDir;

use support::{file_uri, LspProcess};

#[test]
fn renamed_configured_target_is_recoverable_and_clears_stale_diagnostics() {
    let root = TempDir::new().expect("workspace");
    let source_directory = root.path().join("simf");
    fs::create_dir(&source_directory).expect("source directory");
    fs::write(root.path().join("Simplex.toml"), "").expect("manifest");
    let original = source_directory.join("verifier.simf");
    let renamed = source_directory.join("verifier.moved.simf");
    let source = "fn main() {}\n";
    fs::write(&original, source).expect("configured source");

    let root_uri = file_uri(root.path());
    let document_uri = file_uri(&original);
    let mut server = LspProcess::spawn();
    server.initialize(&root_uri, &json!({}));
    fs::rename(&original, &renamed).expect("rename configured target");

    server.notify(
        "textDocument/didOpen",
        &json!({
            "textDocument": {
                "uri": document_uri,
                "languageId": "simplicityhl",
                "version": 1,
                "text": source
            }
        }),
    );
    for version in 1..=6 {
        if version > 1 {
            server.notify(
                "textDocument/didChange",
                &json!({
                    "textDocument": { "uri": document_uri, "version": version },
                    "contentChanges": [{ "text": source }]
                }),
            );
        }
        let publication = server.diagnostics(&document_uri, version);
        assert!(publication["params"]["diagnostics"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(
                    |item| item["message"].as_str().is_some_and(|message| message
                        .contains("Failed to find library target path")
                        && message.contains("verifier.simf"))
                )));
    }

    fs::rename(&renamed, &original).expect("restore configured target");
    server.notify(
        "textDocument/didChange",
        &json!({
            "textDocument": { "uri": document_uri, "version": 7 },
            "contentChanges": [{ "text": source }]
        }),
    );
    let recovered = server.diagnostics(&document_uri, 7);
    assert_eq!(recovered["params"]["diagnostics"], json!([]));

    // A successful shutdown response proves the repeated filesystem failures did not panic or
    // leave the process in the extension's restart loop failure mode.
    server.shutdown();
}
