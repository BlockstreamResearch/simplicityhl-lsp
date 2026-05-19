import * as os from "os";
import * as fs from "fs";
import * as path from "path";

import process from "node:process";
import * as cp from "child_process";

import { env, ProgressLocation, Uri, window, workspace } from "vscode";

// Searches for an executable in PATH and common installation directories.
// Used by both LSP client and compiler to locate binaries (simplicityhl-lsp, simc).
export function findExecutable(command: string): string | null {
  try {
    const resolved = cp
      .execSync(
        process.platform === "win32" ? `where ${command}` : `which ${command}`,
      )
      .toString()
      .split(/\r?\n/)[0]
      .trim();
    if (resolved && fs.existsSync(resolved)) {
      return resolved;
    }
  } catch {
    // Not found in PATH
  }

  const commonDirs: string[] = [];

  if (process.platform === "win32") {
    commonDirs.push(
      path.join(
        process.env["USERPROFILE"] ?? "C:\\Users\\Default",
        ".cargo",
        "bin",
      ),
    );
  } else {
    commonDirs.push(path.join(os.homedir(), ".cargo", "bin"));

    commonDirs.push(
      "/usr/local/bin",
      "/usr/bin",
      path.join(os.homedir(), ".local", "bin"),
    );
  }

  for (const dir of commonDirs) {
    const candidate = path.join(dir, command);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  return null;
}

async function installServer(command: string) {
  const cargoPath = findExecutable("cargo");
  if (!cargoPath) {
    throw new Error("Unable to find 'cargo'. Please ensure Rust is installed and in your PATH.");
  }

  const action = findExecutable(command) ? "Updating" : "Installing";

  return window.withProgress({
    location: ProgressLocation.Notification,
    title: `${action} ${command}`,
    cancellable: true
  }, async (progress, token) => {
    return new Promise<void>((resolve, reject) => {
      const installProcess = cp.spawn(cargoPath!, ["install", "--color", "never", command]);

      token.onCancellationRequested(() => {
        installProcess.kill("SIGTERM");
        reject(new Error("Installation canceled"));
      });

      const reportProgress = (data: Buffer) => {
        const lines = data.toString()
          .split(/\r?\n/)
          .map(l => l.trim())

        for (const line of lines) {
          if (line.startsWith("Compiling") && line !== "Compiling") {
            progress.report({ message: line });
          }
        }
      };

      installProcess.stderr?.on('data', reportProgress);

      installProcess.on('close', (code) => {
        if (code === 0) {
          resolve();
        } else {
          reject(new Error(`Installation failed with exit code ${code}`));
        }
      });

      installProcess.on('error', (err) => {
        reject(new Error(`Failed to start cargo process: ${err.message}`));
      });
    });
  });
}

export async function ensureExecutable(
  command: string,
): Promise<string | null> {
  const cargoPath = findExecutable("cargo");
  const config = workspace.getConfiguration("simplicityhl");

  let serverPath = findExecutable(command);

  if (!cargoPath && !serverPath) {
    const suppressWarning = config.get<boolean>(
      "suppressMissingLspWarning",
      false,
    );
    if (suppressWarning) {
      return null;
    }

    const choice = await window.showWarningMessage(
      `To use SimplicityHL language server, please install cargo`,
      "Learn more",
      "Don't show again",
    );

    if (choice === "Learn more") {
      const url = "https://rust-lang.org/tools/install";
      await env.openExternal(Uri.parse(url));
    } else if (choice === "Don't show again") {
      const config = workspace.getConfiguration("simplicityhl");
      await config.update("suppressMissingLspWarning", true, true);
    }

    return null;
  }

  if (!cargoPath) {
    return serverPath;
  }

  const disableAutoupdate = config.get<boolean>("disableAutoupdate", false);

  if (serverPath && disableAutoupdate) {
    return serverPath;
  }

  try {
    await installServer(command);

    serverPath = findExecutable(command);
  } catch (err) {
    window.showErrorMessage(err);
    return null;
  }

  return serverPath;
}
