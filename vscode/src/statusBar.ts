// Status bar integration for SimplicityHL.
// Shows LSP connection status and provides quick access to commands.

import * as vscode from "vscode";

// Connection states for the status bar
export type LspStatus = "starting" | "connected" | "disconnected" | "error";

// Manages the status bar item showing LSP state
export class StatusBar {
  private statusBarItem: vscode.StatusBarItem;
  private status: LspStatus = "disconnected";

  constructor() {
    // Create status bar item on the left side
    this.statusBarItem = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Left,
      100
    );
    // Clicking restarts the LSP server
    this.statusBarItem.command = "simplicityhl.restartServer";
    this.statusBarItem.tooltip = "SimplicityHL Language Server - Click to restart";
    this.update("disconnected");
  }

  // Update the status bar display based on LSP state
  public update(status: LspStatus): void {
    this.status = status;

    switch (status) {
      case "starting":
        this.statusBarItem.text = "$(sync~spin) SimplicityHL";
        this.statusBarItem.backgroundColor = undefined;
        this.statusBarItem.tooltip = "SimplicityHL LSP starting...";
        break;
      case "connected":
        this.statusBarItem.text = "$(check) SimplicityHL";
        this.statusBarItem.backgroundColor = undefined;
        this.statusBarItem.tooltip = "SimplicityHL LSP connected - Click to restart";
        break;
      case "disconnected":
        this.statusBarItem.text = "$(circle-slash) SimplicityHL";
        this.statusBarItem.backgroundColor = new vscode.ThemeColor(
          "statusBarItem.warningBackground"
        );
        this.statusBarItem.tooltip = "SimplicityHL LSP disconnected - Click to restart";
        break;
      case "error":
        this.statusBarItem.text = "$(error) SimplicityHL";
        this.statusBarItem.backgroundColor = new vscode.ThemeColor(
          "statusBarItem.errorBackground"
        );
        this.statusBarItem.tooltip = "SimplicityHL LSP error - Click to restart";
        break;
    }
  }

  // Show the status bar item
  public show(): void {
    this.statusBarItem.show();
  }

  // Hide the status bar item
  public hide(): void {
    this.statusBarItem.hide();
  }

  // Get current status
  public getStatus(): LspStatus {
    return this.status;
  }

  // Clean up resources
  public dispose(): void {
    this.statusBarItem.dispose();
  }
}

// Singleton instance
let statusBar: StatusBar | undefined;

export function getStatusBar(): StatusBar {
  if (!statusBar) {
    statusBar = new StatusBar();
  }
  return statusBar;
}

export function disposeStatusBar(): void {
  statusBar?.dispose();
  statusBar = undefined;
}
