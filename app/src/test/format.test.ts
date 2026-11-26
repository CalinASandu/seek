import { describe, expect, it } from "vitest";
import { formatCount, formatModified, formatRelative, formatSize, splitPath } from "../lib/format";
import { composeQuery, folderFilter, hasToken, highlightTerms, segmentName, toggleFilter, type Filter } from "../lib/query";

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
  const flag = (token: string): Filter => ({ token, kind: "flag", label: token });
  const drive = (name: string): Filter => ({ token: `drive:"${name}"`, kind: "drive", label: name });

  it("toggles filters without touching the typed text", () => {
    const on = toggleFilter({ text: "report", filters: [] }, flag("file:"));
    expect(on).toEqual({ text: "report", filters: [flag("file:")] });
    expect(toggleFilter(on, flag("file:"))).toEqual({ text: "report", filters: [] });
    expect(toggleFilter(on, flag("folder:"), ["file:"]).filters).toEqual([flag("folder:")]);
    expect(hasToken('a device:"Data (D:)" b', 'device:"Data (D:)"')).toBe(true);
  });
  it("takes a filter typed by hand out of the text", () => {
    expect(toggleFilter({ text: "report file:", filters: [] }, flag("file:"))).toEqual({ text: "report", filters: [] });
    expect(toggleFilter({ text: "folder: x", filters: [] }, flag("file:"), ["folder:"])).toEqual({
      text: "x",
      filters: [flag("file:")],
    });
  });
  it("composes filters before the text, with several drives as alternatives", () => {
    expect(composeQuery({ text: "report", filters: [] })).toBe("report");
    expect(composeQuery({ text: "", filters: [flag("file:")] })).toBe("file:");
    expect(composeQuery({ text: 'ab"', filters: [flag("file:"), drive("A"), drive("B")] })).toBe('drive:"A"|drive:"B" file: ab"');
  });
  it("names a folder filter after the folder", () => {
    expect(folderFilter("D:\\Games\\Steam")).toEqual({
      token: 'path:"D:\\Games\\Steam"',
      kind: "folder",
      label: "in Steam",
      title: "D:\\Games\\Steam",
    });
    expect(folderFilter("D:\\").label).toBe("in D:");
  });
  it("derives highlight terms and segments", () => {
    expect(highlightTerms('report ext:pdf device:"Data (D:)" "annual budget"')).toEqual(["report", "annual budget"]);
    expect(segmentName("Report-final.pdf", ["report"])).toEqual([
      { text: "Report", hit: true },
      { text: "-final.pdf", hit: false },
    ]);
  });
});
