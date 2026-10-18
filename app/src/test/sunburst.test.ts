import { describe, expect, it } from "vitest";
import type { SpaceNode } from "../api/types";
import { liveHover } from "../components/space/Sunburst";
import { arcPath, layoutSunburst, lerpArcs, type Arc } from "../lib/sunburst";

const TAU = Math.PI * 2;

function n(name: string, bytes: number, children: SpaceNode[] = [], kind: SpaceNode["kind"] = children.length ? "folder" : "file"): SpaceNode {
  return { name, kind, bytes, files: 1, modified: 0, cloud: false, children };
}

const tree = n("root", 100, [
  n("A", 60, [n("a1", 40, [n("x", 40)]), n("a2.bin", 20)]),
  n("B", 30, [n("b1", 30, [], "folder")], "folder"),
  n("c.iso", 10),
]);

const span = (a: Arc) => a.a1 - a.a0;

describe("layoutSunburst", () => {
  it("gives every child its share of the parent's angle, ring by ring", () => {
    const arcs = layoutSunburst(tree, "", 3, 100);
    const ring1 = arcs.filter((a) => a.depth === 1);
    expect(ring1.map((a) => a.node.name)).toEqual(["A", "B", "c.iso"]);
    expect(ring1.reduce((s, a) => s + span(a), 0)).toBeCloseTo(TAU);
    const a = ring1[0]!;
    expect(span(a)).toBeCloseTo(TAU * 0.6);
    const kidsOfA = arcs.filter((x) => x.key.startsWith("A\\") && x.depth === 2);
    expect(kidsOfA.reduce((s, x) => s + span(x), 0)).toBeCloseTo(span(a));
    expect(arcs.find((x) => x.key === "A\\a1\\x")!.depth).toBe(3);
    const r = ring1.map((x) => [x.r0, x.r1]);
    expect(new Set(r.map(String)).size).toBe(1);
    expect(arcs.every((x) => x.r1 <= 100 && x.r0 < x.r1)).toBe(true);
  });

  it("draws nothing for an empty folder", () => {
    expect(layoutSunburst(n("empty", 0, [], "folder"), "", 3, 100)).toEqual([]);
  });

  it("merges slivers into one smaller-items arc", () => {
    const tiny = Array.from({ length: 50 }, (_, i) => n(`t${i}`, 1));
    const arcs = layoutSunburst(n("r", 10_050, [n("big", 10_000), ...tiny]), "", 1, 100, (0.5 * Math.PI) / 180);
    expect(arcs).toHaveLength(2);
    expect(arcs[1]!.kind).toBe("smaller");
    expect(arcs[1]!.node.bytes).toBe(50);
  });

  it("colours first-ring folders in order and keeps the hue further out", () => {
    const arcs = layoutSunburst(tree, "", 3, 100);
    const hue = (k: string) => arcs.find((a) => a.key === k)!.hue;
    expect([hue("A"), hue("B"), hue("c.iso")]).toEqual([0, 1, -1]);
    expect(hue("A\\a1")).toBe(0);
    expect(hue("B\\b1")).toBe(1);
    const tone = (k: string) => arcs.find((a) => a.key === k)!.tone;
    expect(tone("A")).toBeLessThan(tone("A\\a1"));
  });

  it("keys arcs by their path from the drive root", () => {
    const arcs = layoutSunburst(tree, "Users\\Calin", 1, 100);
    expect(arcs[0]!.key).toBe("Users\\Calin\\A");
  });
});

describe("liveHover", () => {
  it("ignores a hover left over from another folder", () => {
    const arcs = layoutSunburst(tree.children[0]!, "A", 3, 100);
    expect(liveHover("B", arcs)).toBeNull();
    expect(liveHover("A\\a1", arcs)).toBe("A\\a1");
    expect(liveHover("A", arcs)).toBe("A");
  });
});

describe("arcPath", () => {
  it("draws a full ring as two half arcs", () => {
    const [only] = layoutSunburst(n("r", 5, [n("all", 5)]), "", 1, 100);
    const d = arcPath(only!, 100, 100);
    expect(d).not.toContain("NaN");
    expect((d.match(/A/g) ?? []).length).toBe(4);
  });
});

describe("lerpArcs", () => {
  it("moves shared arcs from their old place to the new one", () => {
    const from = layoutSunburst(tree, "", 3, 100);
    const to = layoutSunburst(tree.children[0]!, "A", 3, 100);
    const at0 = lerpArcs(from, to, 0).find((a) => a.key === "A\\a1")!;
    const old = from.find((a) => a.key === "A\\a1")!;
    expect([at0.a0, at0.a1, at0.r0, at0.r1]).toEqual([old.a0, old.a1, old.r0, old.r1]);
    const at1 = lerpArcs(from, to, 1).find((a) => a.key === "A\\a1")!;
    const nu = to.find((a) => a.key === "A\\a1")!;
    expect([at1.a0, at1.a1, at1.r0, at1.r1, at1.opacity]).toEqual([nu.a0, nu.a1, nu.r0, nu.r1, 1]);
  });

  it("grows new arcs out of their nearest old ancestor", () => {
    const deep = n("root", 100, [n("A", 100, [n("a1", 100, [n("x", 100, [n("y", 100)])])])]);
    const from = layoutSunburst(deep, "", 3, 100);
    const to = layoutSunburst(deep.children[0]!, "A", 3, 100);
    const y = lerpArcs(from, to, 0).find((a) => a.key === "A\\a1\\x\\y")!;
    const x = from.find((a) => a.key === "A\\a1\\x")!;
    expect([y.a0, y.a1, y.r0, y.r1]).toEqual([x.a0, x.a1, x.r1, x.r1]);
  });
});
