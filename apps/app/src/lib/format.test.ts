import { describe, expect, it } from "vitest";
import { fileKind, formatBytes, formatEta, isUrl, plural } from "./format";

describe("format", () => {
  it("formats bytes with SI units", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1_000)).toBe("1.0 KB");
    expect(formatBytes(2_400_000_000)).toBe("2.4 GB");
    expect(formatBytes(512_000_000)).toBe("512 MB");
  });
  it("formats eta", () => {
    expect(formatEta(3)).toBe("a few seconds left");
    expect(formatEta(42)).toBe("42 s left");
    expect(formatEta(3600 + 1800)).toBe("1 h 30 min left");
    expect(formatEta(null)).toBe("");
  });
  it("classifies files", () => {
    expect(fileKind("IMG_1.HEIC")).toBe("image");
    expect(fileKind("a.unknown", "video/mp4")).toBe("video");
    expect(fileKind("Album", undefined, true)).toBe("folder");
    expect(fileKind("setup.exe")).toBe("app");
  });
  it("detects links and plurals", () => {
    expect(isUrl("https://example.com/x")).toBe(true);
    expect(isUrl("not a link")).toBe(false);
    expect(plural(1, "file")).toBe("1 file");
    expect(plural(3, "file")).toBe("3 files");
  });
});

describe("firstUrl", () => {
  it("finds links inside text", async () => {
    const { firstUrl } = await import("./format");
    expect(firstUrl("see https://example.com/a?b=1, thanks")).toBe("https://example.com/a?b=1");
    expect(firstUrl("no link here")).toBeNull();
  });
});
