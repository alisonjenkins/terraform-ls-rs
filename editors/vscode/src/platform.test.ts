import { describe, expect, it } from "vitest";

import {
  assetStem,
  binaryName,
  expandHome,
  parseChecksum,
  releaseBaseUrl,
  targetTriple,
} from "./platform";

describe("targetTriple", () => {
  it("maps supported platform/arch pairs to release triples", () => {
    expect(targetTriple("linux", "x64")).toBe("x86_64-unknown-linux-musl");
    expect(targetTriple("darwin", "arm64")).toBe("aarch64-apple-darwin");
    expect(targetTriple("win32", "x64")).toBe("x86_64-pc-windows-msvc");
  });

  it("returns undefined for unsupported pairs", () => {
    expect(targetTriple("darwin", "x64")).toBeUndefined(); // intel mac not built
    expect(targetTriple("linux", "arm64")).toBeUndefined();
    expect(targetTriple("aix", "ppc64")).toBeUndefined();
  });
});

describe("binaryName", () => {
  it("uses tfls.exe on Windows and tfls elsewhere", () => {
    expect(binaryName("win32")).toBe("tfls.exe");
    expect(binaryName("linux")).toBe("tfls");
    expect(binaryName("darwin")).toBe("tfls");
  });
});

describe("expandHome", () => {
  it("expands a leading ~ to the home directory", () => {
    expect(expandHome("~/bin/tfls", "/home/me")).toBe("/home/me/bin/tfls");
  });

  it("leaves absolute and relative paths untouched", () => {
    expect(expandHome("/usr/local/bin/tfls", "/home/me")).toBe(
      "/usr/local/bin/tfls",
    );
    expect(expandHome("bin/tfls", "/home/me")).toBe("bin/tfls");
  });
});

describe("asset naming (must match release.yml)", () => {
  it("builds the asset stem", () => {
    expect(assetStem("0.4.0", "x86_64-unknown-linux-gnu")).toBe(
      "tfls-v0.4.0-x86_64-unknown-linux-gnu",
    );
  });

  it("builds the release base URL", () => {
    expect(releaseBaseUrl("0.4.0")).toBe(
      "https://github.com/alisonjenkins/terraform-ls-rs/releases/download/v0.4.0",
    );
  });
});

describe("parseChecksum", () => {
  it("extracts the hex digest from a sha256sum line", () => {
    expect(parseChecksum("deadbeef  tfls-v0.4.0-x.tar.gz\n")).toBe("deadbeef");
  });

  it("handles single-space and tab separators and trailing whitespace", () => {
    expect(parseChecksum("  abc123 file  ")).toBe("abc123");
    expect(parseChecksum("abc123\tfile")).toBe("abc123");
  });
});
