import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as stream from "node:stream";
import { promisify } from "node:util";
import * as tar from "tar";
import * as vscode from "vscode";

import {
  assetStem,
  binaryName,
  expandHome,
  parseChecksum,
  releaseBaseUrl,
  targetTriple,
} from "./platform";

const pipeline = promisify(stream.pipeline);

async function exists(p: string): Promise<boolean> {
  try {
    await fs.promises.access(p);
    return true;
  } catch {
    return false;
  }
}

/**
 * Resolve the `tfls` binary to launch.
 *
 * Order: (1) the `terraform-ls-rs.serverPath` setting, (2) a previously
 * downloaded binary cached for this extension version, (3) download the
 * matching release asset, verify its checksum, and cache it.
 */
export async function resolveServerBinary(
  context: vscode.ExtensionContext,
  output: vscode.OutputChannel,
): Promise<string> {
  const configured = vscode.workspace
    .getConfiguration("terraform-ls-rs")
    .get<string>("serverPath", "")
    .trim();
  if (configured) {
    const resolved = expandHome(configured);
    if (!(await exists(resolved))) {
      throw new Error(
        `terraform-ls-rs.serverPath points at "${resolved}", which does not exist.`,
      );
    }
    output.appendLine(`Using configured server binary: ${resolved}`);
    return resolved;
  }

  const version = context.extension.packageJSON.version as string;
  const triple = targetTriple();
  if (!triple) {
    throw new Error(
      `No prebuilt tfls binary for ${process.platform}/${process.arch}. ` +
        `Build tfls yourself and set "terraform-ls-rs.serverPath".`,
    );
  }

  const destDir = vscode.Uri.joinPath(
    context.globalStorageUri,
    "server",
    version,
    triple,
  ).fsPath;
  const destBin = path.join(destDir, binaryName());
  if (await exists(destBin)) {
    output.appendLine(`Using cached server binary: ${destBin}`);
    return destBin;
  }

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `Downloading tfls ${version} (${triple})…`,
      cancellable: false,
    },
    () => downloadAndExtract(version, triple, destDir, output),
  );

  return destBin;
}

async function downloadAndExtract(
  version: string,
  triple: string,
  destDir: string,
  output: vscode.OutputChannel,
): Promise<void> {
  const stem = assetStem(version, triple);
  const base = releaseBaseUrl(version);
  const archiveUrl = `${base}/${stem}.tar.gz`;
  const checksumUrl = `${archiveUrl}.sha256`;

  await fs.promises.mkdir(destDir, { recursive: true });
  const archivePath = path.join(destDir, `${stem}.tar.gz`);

  output.appendLine(`Downloading ${archiveUrl}`);
  await downloadFile(archiveUrl, archivePath);

  output.appendLine(`Verifying checksum ${checksumUrl}`);
  const expected = parseChecksum(await fetchText(checksumUrl));
  const actual = await sha256(archivePath);
  if (expected.toLowerCase() !== actual.toLowerCase()) {
    await fs.promises.rm(archivePath, { force: true });
    throw new Error(
      `Checksum mismatch for ${stem}.tar.gz (expected ${expected}, got ${actual}).`,
    );
  }

  output.appendLine(`Extracting to ${destDir}`);
  await tar.x({ file: archivePath, cwd: destDir, strip: 1 });
  await fs.promises.rm(archivePath, { force: true });

  if (process.platform !== "win32") {
    await fs.promises.chmod(path.join(destDir, binaryName()), 0o755);
  }
  output.appendLine("Server binary ready.");
}

async function downloadFile(url: string, dest: string): Promise<void> {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok || !res.body) {
    throw new Error(`Download failed (${res.status} ${res.statusText}): ${url}`);
  }
  await pipeline(
    stream.Readable.fromWeb(res.body as never),
    fs.createWriteStream(dest),
  );
}

async function fetchText(url: string): Promise<string> {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`Download failed (${res.status} ${res.statusText}): ${url}`);
  }
  return res.text();
}

function sha256(file: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash("sha256");
    const rs = fs.createReadStream(file);
    rs.on("error", reject);
    rs.on("data", (chunk) => hash.update(chunk));
    rs.on("end", () => resolve(hash.digest("hex")));
  });
}
