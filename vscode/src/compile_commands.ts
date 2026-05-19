// Command handlers for SimplicityHL compilation.
// Registers commands accessible via Command Palette and keybindings.

import * as vscode from "vscode";
import * as path from "path";
import { getCompiler } from "./compile";

// Validates that the active editor contains a SimplicityHL file
async function getSimplicityHLDocument(): Promise<vscode.TextDocument | undefined> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showWarningMessage("No active file to compile");
    return undefined;
  }

  const document = editor.document;
  if (document.languageId !== "simplicityhl") {
    vscode.window.showWarningMessage("Current file is not a SimplicityHL file (.simf)");
    return undefined;
  }

  // Auto-save before compile if enabled
  const config = vscode.workspace.getConfiguration("simplicityhl");
  const autoSave = config.get<boolean>("build.autoSaveBeforeCompile", true);
  if (autoSave && document.isDirty) {
    await document.save();
  }

  return document;
}

// Register all compile-related commands
export function registerCompileCommands(context: vscode.ExtensionContext): void {
  // Basic compile - compiles the current .simf file
  const compileFileCommand = vscode.commands.registerCommand(
    "simplicityhl.compileFile",
    async () => {
      const document = await getSimplicityHLDocument();
      if (!document) return;

      const compiler = getCompiler();
      const result = await compiler.compileFile(document.uri.fsPath);

      if (result.success) {
        // Offer to copy output to clipboard
        const action = await vscode.window.showInformationMessage(
          "SimplicityHL compiled successfully!",
          "Copy Program",
          "Dismiss"
        );

        if (action === "Copy Program" && result.program) {
          await vscode.env.clipboard.writeText(result.program);
          vscode.window.showInformationMessage("Program copied to clipboard");
        }
      } else {
        vscode.window.showErrorMessage(
          "Compilation failed. See Problems panel for details."
        );
      }
    }
  );

  // Compile with debug symbols - includes debug info in output
  const compileDebugCommand = vscode.commands.registerCommand(
    "simplicityhl.compileFileDebug",
    async () => {
      const document = await getSimplicityHLDocument();
      if (!document) return;

      const compiler = getCompiler();
      const result = await compiler.compileFile(document.uri.fsPath, { debug: true });

      if (result.success) {
        vscode.window.showInformationMessage("Compiled with debug symbols!");
      } else {
        vscode.window.showErrorMessage(
          "Compilation failed. See Problems panel for details."
        );
      }
    }
  );

  // Compile with witness - satisfies the program with witness data
  const compileWithWitnessCommand = vscode.commands.registerCommand(
    "simplicityhl.compileWithWitness",
    async () => {
      const document = await getSimplicityHLDocument();
      if (!document) return;

      const simfPath = document.uri.fsPath;
      // Default: look for .wit file with same name
      const defaultWitPath = simfPath.replace(/\.simf$/, ".wit");

      let witnessFile: string | undefined;
      try {
        await vscode.workspace.fs.stat(vscode.Uri.file(defaultWitPath));
        witnessFile = defaultWitPath;
      } catch {
        // No default witness file, prompt user to select one
        const selected = await vscode.window.showOpenDialog({
          canSelectFiles: true,
          canSelectFolders: false,
          canSelectMany: false,
          filters: { "Witness Files": ["wit", "json"] },
          defaultUri: vscode.Uri.file(path.dirname(simfPath)),
          title: "Select Witness File",
        });

        if (selected?.[0]) {
          witnessFile = selected[0].fsPath;
        }
      }

      if (!witnessFile) {
        vscode.window.showWarningMessage("No witness file selected");
        return;
      }

      const compiler = getCompiler();
      const result = await compiler.compileFile(simfPath, { witnessFile });

      if (result.success) {
        const action = await vscode.window.showInformationMessage(
          "Program satisfied with witness!",
          "Copy Program",
          "Copy Witness",
          "Dismiss"
        );

        if (action === "Copy Program" && result.program) {
          await vscode.env.clipboard.writeText(result.program);
          vscode.window.showInformationMessage("Program copied to clipboard");
        } else if (action === "Copy Witness" && result.witness) {
          await vscode.env.clipboard.writeText(result.witness);
          vscode.window.showInformationMessage("Witness copied to clipboard");
        }
      } else {
        vscode.window.showErrorMessage(
          "Compilation failed. See Problems panel for details."
        );
      }
    }
  );

  // Compile to JSON - outputs result in JSON format in a new editor
  const compileJsonCommand = vscode.commands.registerCommand(
    "simplicityhl.compileJson",
    async () => {
      const document = await getSimplicityHLDocument();
      if (!document) return;

      const compiler = getCompiler();
      const result = await compiler.compileFile(document.uri.fsPath, { json: true });

      if (result.success && result.program) {
        // Show JSON output in a new untitled document
        const jsonOutput = JSON.stringify(
          {
            program: result.program,
            witness: result.witness ?? null,
          },
          null,
          2
        );
        const doc = await vscode.workspace.openTextDocument({
          content: jsonOutput,
          language: "json",
        });
        await vscode.window.showTextDocument(doc);
      } else {
        vscode.window.showErrorMessage(
          "Compilation failed. See Problems panel for details."
        );
      }
    }
  );

  context.subscriptions.push(
    compileFileCommand,
    compileDebugCommand,
    compileWithWitnessCommand,
    compileJsonCommand
  );
}
