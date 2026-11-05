import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
// @ts-expect-error node types are not installed; vitest runs in node
import { readFileSync } from "node:fs";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { CleanEvent, DupGroup, SeekApi } from "../api/types";
import { canTick, createCleanController } from "../hooks/useCleanup";
import { normPath } from "../lib/clean";

type UE = ReturnType<typeof userEvent.setup>;

function makeApi(over: Partial<SeekApi> = {}, opts: Parameters<typeof createMockApi>[0] = {}): SeekApi {
  return { ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected", ...opts }), ...over };
}
function spied(opts: Parameters<typeof createMockApi>[0] = {}) {
  const base = makeApi({}, opts);
  const setSettings = vi.fn(base.setSettings);
  const api: SeekApi = { ...base, setSettings };
  return { base, api, setSettings };
}
async function openClean(api: SeekApi = makeApi()) {
  const user = userEvent.setup();
  render(<App api={api} />);
  await user.click(await screen.findByRole("radio", { name: "Decimator" }));
  return user;
}
const tab = (name: string) => screen.getByRole("tab", { name });
const groupButtons = () => screen.queryAllByRole("button", { name: /, \d+ copies$/ });
async function scan(user: UE) {
  await user.click(screen.getByRole("button", { name: "Scan" }));
  await waitFor(() => expect(groupButtons().length).toBeGreaterThan(5));
  await waitFor(() => expect(screen.getByRole("button", { name: "Rescan" })).toBeEnabled());
}
async function toLeftovers(user: UE) {
  await user.click(tab("Leftovers"));
  await screen.findByText("Battle.net Launcher");
}
const tick = () => new Promise((r) => setTimeout(r, 10));

async function leftoverSheet(api: SeekApi, names: RegExp[]) {
  const user = await openClean(api);
  await toLeftovers(user);
  for (const n of names) await user.click(screen.getByRole("checkbox", { name: n }));
  await user.click(screen.getByRole("button", { name: "Review…" }));
  const dialog = await screen.findByRole("dialog", { name: /^Move \d/ });
  await within(dialog).findAllByRole("row");
  const list = within(dialog).getByRole("grid", { name: "Items to move" });
  const itemRows = () => within(list).getAllByRole("row").filter((r) => within(r).queryByRole("checkbox", { name: /^Move [A-Z]:/ }));
  return { user, dialog, list, itemRows };
}
const boxOf = (row: HTMLElement) => within(row).getByRole("checkbox", { name: /^Move [A-Z]:/ }) as HTMLInputElement;
const THREE = [/Select .*ZoomUpdater-Cache/, /Select .*Skype for Desktop/, /Select .*OldVPNClient/];
const nameOf = (row: HTMLElement) => row.querySelector(".rv-name-text") as HTMLElement;

describe("Entrances run once", () => {
  it("clears is-new on animationend and doesn't bring it back when the tab changes", async () => {
    const user = await openClean();
    await scan(user);
    const fresh = () => [...document.querySelectorAll<HTMLElement>(".cl-row.is-new")];
    expect(fresh().length).toBeGreaterThan(5);
    const one = fresh()[0]!;
    fireEvent.animationEnd(one);
    expect(one.classList.contains("is-new")).toBe(false);
    fresh().forEach((el) => fireEvent.animationEnd(el));
    expect(fresh()).toHaveLength(0);
    await user.click(tab("Leftovers"));
    await screen.findByText("Battle.net Launcher");
    await user.click(tab("Duplicates"));
    expect(fresh()).toHaveLength(0);
    expect(document.querySelector(".sel-bar.is-entering, .clean-empty.is-entering")).toBeNull();
  });
});

describe("Review sheet keyboard follows the mouse", () => {
  it("Space acts on the row that was clicked, not a stale one", async () => {
    const { user, list, itemRows } = await leftoverSheet(makeApi(), THREE);
    const rows = itemRows();
    const target = rows[1]!;
    const first = within(list).getAllByRole("checkbox")[0] as HTMLInputElement;
    await user.click(nameOf(target));
    expect(boxOf(target).checked).toBe(false);
    expect(list).toHaveFocus();
    await user.keyboard(" ");
    expect(boxOf(target).checked).toBe(true);
    expect(first.checked).toBe(true);
    await user.click(boxOf(rows[2]!));
    expect(list).toHaveFocus();
    expect(boxOf(rows[2]!).checked).toBe(false);
  });

  it("P protects the clicked row, once, and focus lands on the row that takes its place", async () => {
    const { api, setSettings } = spied();
    const { user, list, itemRows } = await leftoverSheet(api, THREE);
    const target = itemRows()[1]!;
    const path = boxOf(target).getAttribute("aria-label")!.replace(/^Move /, "");
    await user.click(nameOf(target));
    fireEvent.keyDown(list, { key: "p" });
    for (let i = 0; i < 3; i++) fireEvent.keyDown(list, { key: "p", repeat: true });
    await waitFor(() => expect(setSettings).toHaveBeenCalled());
    const last = setSettings.mock.calls[setSettings.mock.calls.length - 1]![0];
    expect(last.protectedPaths).toEqual([path]);
    expect(itemRows()).toHaveLength(2);
    expect(within(list).queryByRole("checkbox", { name: `Move ${path}` })).toBeNull();
    expect(list.querySelector(".vrow.is-focus")).not.toBeNull();
  });
});

describe("Always protect that cannot be saved", () => {
  it("says so in the sheet and leaves the row where it was", async () => {
    const why = "Your settings file (X:\\Seek\\settings.json) couldn't be read. It wasn't overwritten; fix or delete it first.";
    const { api } = spied();
    const failing: SeekApi = {
      ...api,
      setSettings: async () => {
        throw why;
      },
    };
    const { user, list, itemRows } = await leftoverSheet(failing, THREE);
    const before = itemRows().length;
    await user.click(nameOf(itemRows()[1]!));
    fireEvent.keyDown(list, { key: "p" });
    const dialog = screen.getAllByRole("dialog").pop()!;
    expect(await within(dialog).findByText(new RegExp("wasn't overwritten"))).toBeInTheDocument();
    expect(itemRows()).toHaveLength(before);
  });
});

describe("Focus after a sheet closes", () => {
  it("lands on the active tab when the opener is gone, and the result's Done is focused", async () => {
    const user = await openClean();
    await toLeftovers(user);
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findAllByRole("row");
    await user.click(within(dialog).getByRole("button", { name: /^Move 1 item/ }));
    await within(dialog).findByText(/^Moved 1 item/);
    const done = within(dialog).getByRole("button", { name: "Done" });
    expect(done).toHaveFocus();
    expect(done).toHaveAttribute("data-autofocus");
    expect(within(dialog).getByRole("button", { name: "Put back" })).not.toHaveFocus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(tab("Leftovers")).toHaveFocus();
  });
});

describe("Scan cancel", () => {
  function held() {
    let emit: (e: CleanEvent) => void = () => {};
    let running = false;
    const starts = vi.fn(async () => {
      running = true;
    });
    const base = makeApi();
    const api: SeekApi = {
      ...base,
      onCleanEvent: (cb) => {
        emit = cb;
        return () => {};
      },
      cleanScanStart: starts,
      cleanScanCancel: async () => {},
      cleanScanState: async () => ({ running, found: [], otherPcs: [], checkedBytes: 0, totalBytes: 0, current: null }),
    };
    return { api, starts, emit: (e: CleanEvent) => emit(e), stop: () => (running = false) };
  }

  it("blocks a new scan until done arrives, and then the new scan starts clean", async () => {
    const h = held();
    const ctl = createCleanController(h.api);
    ctl.attach();
    await tick();
    ctl.scanStart([]);
    ctl.scanCancel();
    expect(ctl.dup.get().status).toBe("cancelling");
    ctl.scanStart([]); // refused
    expect(h.starts).toHaveBeenCalledTimes(1);
    expect(ctl.dup.get().status).toBe("cancelling");
    h.emit({ type: "done" });
    expect(ctl.dup.get().status).toBe("cancelled");
    ctl.scanStart([]);
    expect(h.starts).toHaveBeenCalledTimes(2);
    expect(ctl.dup.get().status).toBe("scanning");
  });

  it("stops waiting when the backend says nothing is running", async () => {
    const h = held();
    const ctl = createCleanController(h.api);
    ctl.attach();
    await tick();
    ctl.scanStart([]);
    h.stop();
    ctl.scanCancel();
    await waitFor(() => expect(ctl.dup.get().status).toBe("cancelled"));
  });

  it("does not stay on 'Stopping' when the cancel call itself fails", async () => {
    const h = held();
    const api: SeekApi = {
      ...h.api,
      cleanScanCancel: async () => {
        throw new Error("the pipe closed");
      },
    };
    const ctl = createCleanController(api);
    ctl.attach();
    await tick();
    ctl.scanStart([]);
    ctl.scanCancel();
    expect(ctl.dup.get().status).toBe("cancelling");
    await waitFor(() => expect(ctl.dup.get().status).toBe("cancelled"));
    // and a failed cancel after the scan already ended changes nothing
    h.emit({ type: "done" });
    expect(ctl.dup.get().status).toBe("cancelled");
  });

  it("disables Rescan while it is stopping", async () => {
    const h = held();
    const user = await openClean(h.api);
    await user.click(screen.getByRole("button", { name: "Scan" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(screen.getByRole("button", { name: "Rescan" })).toBeDisabled();
    h.emit({ type: "done" });
    await waitFor(() => expect(screen.getByRole("button", { name: "Rescan" })).toBeEnabled());
  });
});

describe("Paths from the engine", () => {
  it("normalises case, separators and trailing slashes", () => {
    expect(normPath("D:/Photos/A.JPG/")).toBe("d:\\photos\\a.jpg");
  });

  it("lists items whose paths come back in other case and slashes", async () => {
    const base = makeApi();
    const api = {
      ...base,
      cleanPrepare: async (req: Parameters<SeekApi["cleanPrepare"]>[0]) => {
        const p = await base.cleanPrepare(req);
        return { ...p, items: p.items.map((i) => ({ ...i, path: i.path.toUpperCase().replace(/\\/g, "/") })) };
      },
    };
    const user = await openClean(api);
    await scan(user);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog", { name: /^Move [\d,]+ items/ });
    expect(within(dialog).getAllByRole("row").length).toBeGreaterThan(2);
  });

  it("moveOut matches moved paths in any spelling", async () => {
    const ctl = createCleanController(makeApi());
    ctl.loadLeftovers();
    await waitFor(() => expect(ctl.leftovers.get().rows.length).toBeGreaterThan(3));
    const row = ctl.leftovers.get().rows[0]!;
    ctl.moveOut([row.path.toUpperCase().replace(/\\/g, "/")], null);
    expect(ctl.leftovers.get().rows.find((r) => r.path === row.path)).toBeUndefined();
  });
});

describe("Ticks stay true to the data", () => {
  it("adopt drops ticks of vanished copies and of groups that are gone", async () => {
    const base = makeApi();
    await base.cleanScanStart([]);
    await tick();
    const found = (await base.cleanScanState()).found;
    const plain = found.filter((g) => g.copies.length === 2 && g.copies.every((c) => c.lock === null));
    const [a, b] = plain as [DupGroup, DupGroup];
    let state = { running: false, found: [a, b], otherPcs: [], checkedBytes: 0, totalBytes: 0, current: null as string | null };
    const api: SeekApi = { ...base, cleanScanState: async () => state, onCleanEvent: () => () => {} };
    const ctl = createCleanController(api);
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().groups).toHaveLength(2));
    ctl.toggleCopy(a, a.copies[0]!.path);
    ctl.toggleCopy(b, b.copies[0]!.path);
    expect(ctl.dupTicks.get().size).toBe(2);
    ctl.detach();
    // b is gone; a lost its ticked copy
    state = { ...state, found: [{ ...a, copies: [a.copies[1]!] }] };
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().groups).toHaveLength(1));
    expect(ctl.dupTicks.get().size).toBe(0);
  });

  it("adopt re-runs the last-copy rule for a group whose copies changed", async () => {
    const base = makeApi();
    const mk = (path: string) => ({ path, deviceId: "d", size: 10, modified: 1, lock: null, suggestedKeep: false });
    const g3: DupGroup = { id: 1, name: "f", size: 10, freeable: 20, copies: [mk("D:\\a"), mk("D:\\b"), mk("D:\\c")] };
    let state = { running: false, found: [g3], otherPcs: [], checkedBytes: 0, totalBytes: 0, current: null as string | null };
    const ctl = createCleanController({ ...base, cleanScanState: async () => state, onCleanEvent: () => () => {} });
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().groups).toHaveLength(1));
    ctl.toggleCopy(g3, "D:\\a");
    ctl.toggleCopy(g3, "D:\\b");
    ctl.detach();
    const g2: DupGroup = { ...g3, copies: [g3.copies[0]!, g3.copies[1]!] };
    state = { ...state, found: [g2] };
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().groups[0]!.copies).toHaveLength(2));
    expect(ctl.dupTicks.get().size).toBe(1); // both ticked would remove every copy
  });

  it("clears the tab's ticks once a commit finishes, and keeps them on Cancel", async () => {
    const user = await openClean();
    await toLeftovers(user);
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await user.click(screen.getByRole("checkbox", { name: /Select .*Skype for Desktop/ }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    let dialog = await screen.findByRole("dialog");
    await within(dialog).findAllByRole("row");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.getByRole("region", { name: "Selection" })).toHaveTextContent("2 items");
    await user.click(screen.getByRole("button", { name: "Review…" }));
    dialog = await screen.findByRole("dialog");
    await within(dialog).findAllByRole("row");
    await user.click(within(dialog).getAllByRole("checkbox", { name: /^Move [A-Z]:/ })[0]!);
    await user.click(within(dialog).getByRole("button", { name: /^Move 1 item/ }));
    await within(dialog).findByText(/^Moved 1 item/);
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    expect(screen.queryByRole("region", { name: "Selection" })).toBeNull();
  });
});

describe("Use suggestions, the last-copy rule and protection", () => {
  const copy = (path: string, over: Partial<DupGroup["copies"][number]> = {}) => ({
    path, deviceId: "d", size: 10, modified: 1, lock: null, suggestedKeep: false, ...over,
  });
  const group = (id: number, copies: DupGroup["copies"]): DupGroup => ({ id, name: "f", size: 10, copies, freeable: 10 });

  it("ticks all but the keep in every group, whatever the group looks like", async () => {
    const base = makeApi();
    await base.cleanScanStart([]);
    await tick();
    const real = (await base.cleanScanState()).found;
    const noKey = group(9001, [copy("D:\\a\\x"), copy("D:\\b\\x"), copy("D:\\c\\x")]);
    const noKeyLocked = group(9002, [copy("D:\\a\\y"), copy("D:\\b\\y"), copy("C:\\Program Files\\y", { lock: "programFiles" })]);
    const ctl = createCleanController({ ...base, onCleanEvent: () => () => {} });
    const all = [...real, noKey, noKeyLocked];
    ctl.dup.set((s) => ({ ...s, status: "done", groups: all }));
    ctl.applySuggestions(all);
    const ticked = ctl.dupTicks.get();
    for (const g of real) {
      const keep = g.copies.find((c) => c.suggestedKeep)!;
      const expected = g.copies.filter((c) => c !== keep && c.lock === null).map((c) => c.path).sort();
      expect(g.copies.filter((c) => ticked.has(c.path)).map((c) => c.path).sort()).toEqual(expected);
      expect(ticked.has(keep.path)).toBe(false);
    }
    // No suggested keep and nothing locked: the code ticks all but the last, so one copy always stays
    // (it does not leave the whole group unticked).
    expect(noKey.copies.map((c) => ticked.has(c.path))).toEqual([true, true, false]);
    // No suggested keep, but a locked copy stays: every unlocked copy is ticked.
    expect(noKeyLocked.copies.map((c) => ticked.has(c.path))).toEqual([true, true, false]);
  });

  it("counts a protected copy as the survivor", () => {
    const g = group(1, [copy("D:\\a"), copy("D:\\b"), copy("D:\\c")]);
    const ticked = new Set(["D:\\b"]);
    expect(canTick(g, "D:\\c", ticked, [])).toBe(true);
    expect(canTick(g, "D:\\c", new Set(["D:\\a", "D:\\b"]), [])).toBe(false);
    expect(canTick(g, "D:\\c", ticked, ["D:\\a"])).toBe(true);
    expect(canTick(g, "D:\\a", ticked, ["D:\\a"])).toBe(false);
    const ctl = createCleanController(makeApi());
    ctl.setProtected(["D:\\a"]);
    ctl.toggleCopy(g, "D:\\b");
    ctl.toggleCopy(g, "D:\\c");
    expect([...ctl.dupTicks.get()].sort()).toEqual(["D:\\b", "D:\\c"]);
  });
});

describe("Hotkeys leave dialogs alone", () => {
  it("ignores Ctrl+Shift+D and Ctrl+Shift+S while a dialog is open", async () => {
    const user = await openClean();
    await user.click(screen.getByRole("button", { name: /Recently removed/ }));
    await screen.findByRole("dialog", { name: "Recently removed" });
    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    await user.keyboard("{Control>}{Shift>}s{/Shift}{/Control}");
    expect(screen.getByRole("radio", { name: "Decimator" })).toBeChecked();
    expect(screen.getByRole("dialog", { name: "Recently removed" })).toBeInTheDocument();
  });
});

describe("Uninstall sheet and protection", () => {
  it("drops a ticked leftover that becomes protected", async () => {
    const user = await openClean();
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Audacity/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    const left = await screen.findByRole("list", { name: "Left behind" });
    await user.click(within(left).getAllByRole("checkbox")[0]!);
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialogs = await screen.findAllByRole("dialog");
    const review = dialogs[dialogs.length - 1]!;
    const rows = await within(review).findAllByRole("row");
    const withProtect = rows.find((r) => within(r).queryByRole("button", { name: /^Always protect/ }))!;
    await user.click(within(withProtect).getByRole("button", { name: /^Always protect/ }));
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.getByRole("button", { name: "Review…" })).toBeDisabled());
  });
});

describe("Design fixes", () => {
  it("keeps copy rows inside their list (the CSS guards it; jsdom has no layout to measure)", () => {
    const css: string = readFileSync("src/styles/clean.css", "utf8");

    expect(css).toMatch(/\.copy \{[^}]*grid-template-columns: minmax\(0, 1fr\)/);
    expect(css).toMatch(/\.cl-row \{[^}]*grid-template-columns: minmax\(0, 1fr\)/);
    expect(css).toMatch(/\.clean-scroll \{[^}]*padding: [^;]*96px/);
    expect(css).toMatch(/\.cl-head:active/);
  });

  it("keeps the extension when a file name is cut", async () => {
    const user = await openClean();
    await scan(user);
    const b = screen.getByRole("button", {
      name: /^Quarterly-financial-statement-consolidated-draft-for-review-by-the-board-of-directors-FINAL-v7-signed\.xlsx, /,
    });
    expect(b.querySelector(".mid-tail")!.textContent).toMatch(/\.xlsx$/);
  });

  it("sizes the review list to its rows and keeps its height while unticking", async () => {
    const { user, list, itemRows } = await leftoverSheet(makeApi(), THREE.slice(0, 2));
    const h = list.style.height;
    expect(h).not.toBe("");
    expect(parseInt(h)).toBeLessThan(300);
    await user.click(boxOf(itemRows()[0]!));
    expect(list.style.height).toBe(h);
  });

  it("doesn't show the meter's tooltip over an expanded row", async () => {
    const user = await openClean();
    await toLeftovers(user);
    const row = screen.getByText("OldVPNClient").closest("li")!;
    await user.click(within(row).getByRole("button", { name: /OldVPNClient/ }));
    await user.hover(within(row).getByText("Very likely left over"));
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("uses the title form for an empty result", async () => {
    const user = await openClean(makeApi({}, { clean: "empty" }));
    await user.click(screen.getByRole("button", { name: "Scan" }));
    await screen.findByText("No duplicates found");
    expect(document.querySelector(".clean-figure")).toHaveTextContent("Duplicate files");
    expect(document.querySelector(".clean-caption")).toHaveTextContent("None found in the last scan");
    await user.click(tab("Leftovers"));
    await screen.findByText("No leftovers found");
    expect(document.querySelector(".clean-figure")).toHaveTextContent("Leftovers");
    expect(document.querySelector(".clean-caption")).toHaveTextContent("None found in the last scan");
    await user.click(tab("Apps"));
    await screen.findByText("No apps found");
    expect(document.querySelector(".clean-figure")).toHaveTextContent("Apps");
  });

  it("puts Uninstall where the date was, so nothing shifts", async () => {
    const user = await openClean();
    await user.click(tab("Apps"));
    const pick = await screen.findByRole("button", { name: /^Audacity/ });
    const row = pick.closest("li")!;
    expect(row.querySelector(".app-date")!.textContent).not.toBe("");
    await user.click(pick);
    expect(row).toHaveClass("is-selected");
    const slot = row.querySelector(".app-slot")!;
    expect(within(row).getByRole("button", { name: /^Uninstall…/ }).closest(".app-slot")).toBe(slot);
    expect(row.querySelector(".app-date")).toBeNull();
  });

  it("scans every surface for the word safe, sheets and Settings included", async () => {
    const text = () =>
      `${document.body.textContent} ${[...document.querySelectorAll("[title],[aria-label]")].map((e) => `${e.getAttribute("title")} ${e.getAttribute("aria-label")}`).join(" ")}`;
    const { api } = spied();
    const user = await openClean(api);
    await scan(user);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const review = await screen.findByRole("dialog", { name: /^Move/ });
    await within(review).findAllByRole("row");
    expect(text()).not.toMatch(/\bsafe/i);
    await user.click(within(review).getByRole("button", { name: /^Move/ }));
    await within(review).findByText(/^Moved/);
    expect(text()).not.toMatch(/\bsafe/i);
    await user.click(within(review).getByRole("button", { name: "Done" }));
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Spotify/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    await screen.findByRole("dialog", { name: "Uninstall Spotify?" });
    expect(text()).not.toMatch(/\bsafe/i);
    await user.keyboard("{Escape}");
    await user.click(screen.getByRole("button", { name: /Recently removed/ }));
    await screen.findByRole("dialog", { name: "Recently removed" });
    expect(text()).not.toMatch(/\bsafe/i);
    await user.keyboard("{Escape}");
    await user.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("heading", { name: "Protected from Cleanup" });
    expect(text()).not.toMatch(/\bsafe/i);
  });
});
