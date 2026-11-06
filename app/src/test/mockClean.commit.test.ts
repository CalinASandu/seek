import { describe, expect, it } from "vitest";
import { createMockApi } from "../api/mock";

describe("the mock's cleanCommit", () => {
  it("refuses ids that were not part of the review, like the real engine, and spends the token", async () => {
    const api = createMockApi({ rowCount: 10, latency: [0, 0], state: "connected" });
    const rows = await api.cleanLeftovers();
    const row = rows[0]!;
    const prepared = await api.cleanPrepare({ kind: "folders", why: "leftover", folders: [{ path: row.path, level: row.level, reasons: row.reasons }] });
    const real = prepared.items[0]!.id;
    await expect(api.cleanCommit(prepared.token, [real, 999])).rejects.toThrow("Some of these items were not part of the review.");
    // any bad id spends the token: the real ones can't be committed afterwards either
    await expect(api.cleanCommit(prepared.token, [real])).rejects.toThrow("expired");
    // and a commit of only known ids still works
    const again = await api.cleanPrepare({ kind: "folders", why: "leftover", folders: [{ path: row.path, level: row.level, reasons: row.reasons }] });
    const ok = await api.cleanCommit(again.token, [again.items[0]!.id]);
    expect(ok.items).toHaveLength(1);
  });
});
