import { describe, expect, it } from "vitest";
import { createMockApi } from "../api/mock";
import type { SpaceNode } from "../api/types";

const api = () => createMockApi({ rowCount: 2000, latency: [0, 0] });

function sum(nodes: SpaceNode[]): number {
  return nodes.reduce((s, n) => s + n.bytes, 0);
}

function checkSums(n: SpaceNode) {
  if (n.kind === "folder" && n.children.length > 0) {
    expect(sum(n.children)).toBe(n.bytes);
    n.children.forEach(checkSums);
  }
}

describe("mock Space data", () => {
  it("returns a sorted tree whose children add up to their folder", async () => {
    const root = await api().spaceTree("dev-c", "C:\\", 3);
    expect(root.kind).toBe("folder");
    expect(root.bytes).toBeGreaterThan(100 * 1024 ** 3);
    const sizes = root.children.filter((c) => typeof c.kind === "string").map((c) => c.bytes);
    expect(sizes).toEqual([...sizes].sort((a, b) => b - a));
    checkSums(root);
    const users = root.children.find((c) => c.name === "Users")!;
    expect(users.children.some((c) => c.kind === "hidden")).toBe(true);
  });

  it("never indexes more than a drive holds", async () => {
    const a = api();
    for (const d of await a.listDevices()) {
      const root = await a.spaceTree(d.id, d.lastMountPoint, 1);
      expect(root.bytes, d.displayName).toBeLessThanOrEqual(d.totalBytes - d.freeBytes);
    }
  });

  it("finds folders by path, ignoring case and a trailing backslash", async () => {
    const users = await api().spaceTree("dev-c", "c:\\USERS\\", 1);
    expect(users.name).toBe("Users");
    expect(users.children.length).toBeGreaterThan(0);
  });

  it("rejects missing folders and drives", async () => {
    await expect(api().spaceTree("dev-c", "C:\\Nope", 1)).rejects.toThrow("That folder no longer exists");
    await expect(api().spaceTree("dev-x", "C:\\", 1)).rejects.toThrow("Unknown drive");
  });

  it("lists the largest files inside the folder", async () => {
    const files = await api().spaceLargest("dev-c", "C:\\Users", 10);
    expect(files).toHaveLength(10);
    expect(files.every((f) => f.fullPath.startsWith("C:\\Users\\"))).toBe(true);
    const sizes = files.map((f) => f.size);
    expect(sizes).toEqual([...sizes].sort((a, b) => b - a));
  });

  it("splits a folder by kind of file", async () => {
    const a = api();
    const [types, root] = await Promise.all([a.spaceTypes("dev-d", "D:\\"), a.spaceTree("dev-d", "D:\\", 1)]);
    expect(sum(types.map((t) => ({ ...t, name: "", kind: "file", modified: 0, cloud: false, children: [] })))).toBe(
      root.bytes,
    );
    expect(types.find((t) => t.category === "video")?.bytes).toBeGreaterThan(0);
  });
});
