import * as path from "node:path";
import * as vscode from "vscode";
import {
  DidChangeConfigurationNotification,
  LanguageClient,
  type LanguageClientOptions,
  type ServerOptions,
} from "vscode-languageclient/node";
import { resolveServerBinary } from "./binaryManager";

const CONFIG_SECTION = "terraform-ls-rs";

let client: LanguageClient | undefined;
let output: vscode.LogOutputChannel;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  output = vscode.window.createOutputChannel("Terraform (tfls)", { log: true });
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.commands.registerCommand("terraform-ls-rs.toggleFormatStyle", toggleFormatStyle),
    vscode.commands.registerCommand("terraform-ls-rs.restartServer", restartServer),
    vscode.commands.registerCommand("terraform-ls-rs.showOutputChannel", () => output.show()),
  );

  // Push configuration changes to the running server; it applies them live
  // (re-publishing diagnostics) without a restart.
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration(CONFIG_SECTION) && client) {
        void client.sendNotification(DidChangeConfigurationNotification.type, {
          settings: { [CONFIG_SECTION]: serverSettings() },
        });
      }
    }),
  );

  await startClient(context);
}

export async function deactivate(): Promise<void> {
  await client?.stop();
  client = undefined;
}

/** Collect the `terraform-ls-rs.*` settings the server understands. */
function serverSettings(): Record<string, unknown> {
  const cfg = vscode.workspace.getConfiguration(CONFIG_SECTION);
  return {
    formatStyle: cfg.get("formatStyle", "minimal"),
    cliEnabled: cfg.get("cliEnabled", true),
    cliBinary: cfg.get("cliBinary", "tofu"),
    cliTimeoutSecs: cfg.get("cliTimeoutSecs", 60),
    watchDebounceMs: cfg.get("watchDebounceMs", 150),
    staleVersionDays: cfg.get("staleVersionDays", 180),
    styleRules: cfg.get("styleRules", false),
    rules: cfg.get("rules", {}),
  };
}

async function startClient(context: vscode.ExtensionContext): Promise<void> {
  let command: string;
  try {
    command = await resolveServerBinary(context, output);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    void vscode.window.showErrorMessage(
      `terraform-ls-rs: could not obtain the tfls server. ${message}`,
    );
    output.appendLine(`Failed to resolve server binary: ${message}`);
    return;
  }

  const logFile = path.join(context.globalStorageUri.fsPath, "tfls.log");

  const serverOptions: ServerOptions = {
    command,
    args: [],
    options: {
      env: { ...process.env, TFLS_LOG_FILE: logFile },
    },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "terraform" },
      { scheme: "file", language: "terraform-vars" },
    ],
    initializationOptions: { [CONFIG_SECTION]: serverSettings() },
    outputChannel: output,
  };

  client = new LanguageClient(
    "terraform-ls-rs",
    "Terraform Language Server (tfls)",
    serverOptions,
    clientOptions,
  );

  await client.start();
  output.appendLine(`Language server started: ${command}`);
}

async function toggleFormatStyle(): Promise<void> {
  const cfg = vscode.workspace.getConfiguration(CONFIG_SECTION);
  const current = cfg.get<string>("formatStyle", "minimal");
  const next = current === "minimal" ? "opinionated" : "minimal";
  // Writing the setting fires onDidChangeConfiguration, which pushes the new
  // config to the server.
  await cfg.update("formatStyle", next, vscode.ConfigurationTarget.Global);
  void vscode.window.showInformationMessage(`Terraform format style: ${next}`);
}

async function restartServer(): Promise<void> {
  if (!client) {
    return;
  }
  output.appendLine("Restarting language server…");
  await client.restart();
}
