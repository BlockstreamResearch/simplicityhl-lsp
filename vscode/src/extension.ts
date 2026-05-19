// SimplicityHL VSCode Extension entry point.
// Initializes LSP client and registers all extension features.

import { LspClient } from "./client";
import { registerRestartCommand } from "./commands";
import { registerCompileCommands } from "./compile_commands";
import { registerTaskProvider } from "./tasks";
import { disposeCompiler } from "./compile";
import { disposeStatusBar } from "./statusBar";
import { ExtensionContext } from "vscode";

let client: LspClient;

export function activate(context: ExtensionContext) {
  // Initialize LSP client for language intelligence (also shows status bar)
  client = new LspClient();
  void client.start();

  // Register all commands and providers
  registerRestartCommand(context, client);
  registerCompileCommands(context);  // Compile commands (Cmd+Shift+B, etc.)
  registerTaskProvider(context);      // Task integration (Tasks: Run Task)
}

export function deactivate(): Thenable<void> | undefined {
  // Clean up all resources
  disposeCompiler();
  disposeStatusBar();

  if (!client) {
    return undefined;
  }
  return client.stop();
}
