// Pure, VS Code-free helpers for locating and naming the `tfls` release asset.
// Kept separate from binaryManager.ts (which imports `vscode`) so they can be
// unit-tested under plain Node. These encode the contract with the release
// workflow's asset naming — keep them in sync with .github/workflows/release.yml.
import * as os from "node:os";
import * as path from "node:path";

const REPO = "alisonjenkins/terraform-ls-rs";

/** Maps a Node `platform-arch` pair to the release asset's Rust target triple. */
export const TARGETS: Record<string, string> = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
};

/** Binary file name for a platform (`tfls.exe` on Windows, else `tfls`). */
export function binaryName(platform: NodeJS.Platform = process.platform): string {
  return platform === "win32" ? "tfls.exe" : "tfls";
}

/** Rust target triple for a platform/arch, or `undefined` if unsupported. */
export function targetTriple(
  platform: NodeJS.Platform = process.platform,
  arch: string = process.arch,
): string | undefined {
  return TARGETS[`${platform}-${arch}`];
}

/** Expand a leading `~` to the home directory. */
export function expandHome(p: string, home: string = os.homedir()): string {
  return p.startsWith("~") ? path.join(home, p.slice(1)) : p;
}

/** Release asset stem, e.g. `tfls-v0.4.0-x86_64-unknown-linux-gnu`. */
export function assetStem(version: string, triple: string): string {
  return `tfls-v${version}-${triple}`;
}

/** Base download URL for a release tag. */
export function releaseBaseUrl(version: string): string {
  return `https://github.com/${REPO}/releases/download/v${version}`;
}

/** First whitespace-delimited field of a `sha256sum` file (the hex digest). */
export function parseChecksum(text: string): string {
  return text.trim().split(/\s+/)[0];
}
