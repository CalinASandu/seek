import { describe, expect, it } from "vitest";
import { formatCount, formatModified, formatRelative, formatSize, splitPath } from "../lib/format";
import { hasToken, highlightTerms, segmentName, toggleToken } from "../lib/query";

describe("formatSize", () => {
  it("formats bytes with binary units", () => {
    expect(formatSize(1536)).toBe("1.5 KB");
    expect(formatSize(0)).toBe("0 B");
    expect(formatSize(512)).toBe("512 B");
    expect(formatSize(1024 * 1024 * 12)).toBe("12 MB");
    expect(formatSize(1024)).toBe("1 KB");
  });
});

describe("formatCount / dates / paths", () => {
  it("abbreviates counts", () => {
    expect(formatCount(950)).toBe("950");
    expect(formatCount(48210)).toBe("48k");
    expect(formatCount(1_200_000)).toBe("1.2M");
  });
  it("uses relative time inside a week and dates after", () => {
    const now = Date.now();
    expect(formatRelative(now - 3 * 86_400_000, now)).toBe("3 days ago");
    expect(formatModified(now - 2 * 3_600_000, now)).toBe("2 h ago");
    expect(formatModified(now - 30 * 86_400_000, now)).not.toMatch(/ago/);
  });
  it("splits long folders for middle ellipsis", () => {
    expect(splitPath("D:\\Uni\\Sem2\\Lectures")).toEqual({ head: "D:\\Uni\\", tail: "Sem2\\Lectures" });
    expect(splitPath("D:\\Uni")).toEqual({ head: "", tail: "D:\\Uni" });
  });
});

describe("query helpers", () => {
  it("toggles filter tokens", () => {
    expect(toggleToken("report", "file:")).toBe("report file: ");
    expect(toggleToken("report file: ", "file:")).toBe("report");
    expect(toggleToken("folder: x", "file:", ["folder:"])).toBe("x file: ");
    expect(hasToken('a device:"Data (D:)" b', 'device:"Data (D:)"')).toBe(true);
  });
  it("derives highlight terms and segments", () => {
    expect(highlightTerms('report ext:pdf device:"Data (D:)" "annual budget"')).toEqual(["report", "annual budget"]);
    expect(segmentName("Report-final.pdf", ["report"])).toEqual([
      { text: "Report", hit: true },
      { text: "-final.pdf", hit: false },
    ]);
  });
});
