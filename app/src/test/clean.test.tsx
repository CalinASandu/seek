import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { CleanEvent, DupGroup, SeekApi } from "../api/types";
import { createCleanController } from "../hooks/useCleanup";
import { lockText } from "../lib/clean";

type UE = ReturnType<typeof userEvent.setup>;

function makeApi(over: Partial<SeekApi> = {}, opts: Parameters<typeof createMockApi>[0] = {}): SeekApi {
  return { ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected", ...opts }), ...over };
}

/** An API whose cleanCommit and setSettings can be watched. */
function spied(opts: Parameters<typeof createMockApi>[0] = {}) {
  const base = makeApi({}, opts);
  const commit = vi.fn(base.cleanCommit);
  const setSettings = vi.fn(base.setSettings);
  const uninstallStart = vi.fn(base.uninstallStart);
  const api: SeekApi = { ...base, cleanCommit: commit, setSettings, uninstallStart };
  return { base, api, commit, setSettings, uninstallStart };
}

async function openClean(api: SeekApi = makeApi()) {
  const user = userEvent.setup();
  render(<App api={api} />);
  await user.click(await screen.findByRole("radio", { name: "Decimator" }));
  return user;
}

const tab = (name: string) => screen.getByRole("tab", { name });
const groupButtons = () => screen.queryAllByRole("button", { name: /, \d+ copies$/ });
const groupButton = (name: string) => screen.getByRole("button", { name: new RegExp(`^${name.replace(/[.()]/g, "\\$&")}, \\d+ copies$`) });

async function scan(user: UE) {
  await user.click(screen.getByRole("button", { name: "Scan" }));
  await screen.findByText(/Largest|duplicates/i, { selector: ".clean-caption" });
  await waitFor(() => expect(groupButtons().length).toBeGreaterThan(5));
  // wait for the end of the scan
  await waitFor(() => expect(screen.getByRole("button", { name: "Rescan" })).toBeEnabled());
}

async function expand(user: UE, name: string) {
  const b = groupButton(name);
  if (b.getAttribute("aria-expanded") !== "true") await user.click(b);
  return screen.getByRole("list", { name: new RegExp(`^Copies of ${name.replace(/[.()]/g, "\\$&")}$`) });
}

const checkedBoxes = () => screen.queryAllByRole("checkbox", { checked: true });

async function toLeftovers(user: UE) {
  await user.click(tab("Leftovers"));
  await screen.findByText("Battle.net Launcher");
}

describe("Decimator view", () => {
  it("shows Decimator in the view switch with its tooltip", async () => {
    render(<App api={makeApi()} />);
    const radio = await screen.findByRole("radio", { name: "Decimator" });
    expect(radio).toHaveAttribute("title", "Cleans up files (Ctrl+Shift+D)");
    expect(screen.queryByRole("radio", { name: "History" })).toBeNull();
  });

  it("Ctrl+Shift+D toggles it and the view is kept across switching", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    await screen.findByRole("searchbox");
    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    await scan(user);
    await expand(user, "vc_redist.x64.exe");
    await user.click(screen.getByRole("checkbox", { name: /^Select C:\\Users\\Calin\\Downloads\\vc_redist/ }));
    expect(checkedBoxes()).toHaveLength(1);
    const groups = groupButtons().length;

    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    expect(await screen.findByRole("searchbox")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Rescan" })).toBeNull(); // hidden, not gone

    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    expect(await screen.findByRole("button", { name: "Rescan" })).toBeVisible();
    expect(groupButtons()).toHaveLength(groups);
    expect(checkedBoxes()).toHaveLength(1);
  });

  it("does no work while hidden: it stops listening and starts again on return", async () => {
    const base = makeApi();
    const stop = vi.fn();
    const onCleanEvent = vi.fn((cb: (e: CleanEvent) => void) => {
      const off = base.onCleanEvent(cb);
      return () => {
        stop();
        off();
      };
    });
    const user = userEvent.setup();
    render(<App api={{ ...base, onCleanEvent }} />);
    await screen.findByRole("searchbox");
    expect(onCleanEvent).not.toHaveBeenCalled(); // never opened: nothing runs
    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    await waitFor(() => expect(onCleanEvent).toHaveBeenCalledTimes(1));
    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    await waitFor(() => expect(stop).toHaveBeenCalledTimes(1));
    await user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
    await waitFor(() => expect(onCleanEvent).toHaveBeenCalledTimes(2));
  });

  it("starts with nothing ticked at any confidence level, in every tab", async () => {
    const user = await openClean();
    // before and after a scan, even with every group open
    expect(checkedBoxes()).toHaveLength(0);
    await scan(user);
    for (const b of groupButtons().slice(0, 4)) await user.click(b);
    expect(screen.getAllByRole("checkbox").length).toBeGreaterThan(4);
    expect(checkedBoxes()).toHaveLength(0);
    expect(screen.queryByRole("region", { name: "Selection" })).toBeNull();
    // after Use suggestions and Clear it is back to nothing
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    expect(checkedBoxes().length).toBeGreaterThan(0);
    await user.click(screen.getByRole("button", { name: "Clear" }));
    expect(checkedBoxes()).toHaveLength(0);

    await toLeftovers(user);
    await user.click(screen.getByRole("button", { name: /^Probably still in use/ }));
    for (const level of ["Very likely left over", "Likely left over", "Not sure", "Probably still in use"]) {
      expect(screen.getAllByText(level, { selector: ".meter-label" }).length).toBeGreaterThan(0);
    }
    expect(screen.getAllByRole("checkbox").length).toBeGreaterThan(10);
    expect(checkedBoxes()).toHaveLength(0);

    await user.click(tab("Apps"));
    await screen.findByText("Google Chrome");
    expect(screen.queryAllByRole("checkbox")).toHaveLength(0); // Apps has nothing to tick
    expect(screen.queryByRole("region", { name: "Selection" })).toBeNull();
  });

  it("Use suggestions ticks the others but never the suggested keep", async () => {
    const { base, api } = spied();
    const user = await openClean(api);
    await scan(user);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    const truth = (await base.cleanScanState()).found;
    for (const g of truth.slice(0, 5)) {
      const list = await expand(user, g.name);
      const rows = within(list).getAllByRole("listitem");
      expect(rows).toHaveLength(Math.min(g.copies.length, 40));
      for (const row of rows.filter((r) => within(r).queryByText("Suggested keep"))) {
        const box = within(row).queryByRole("checkbox");
        expect(box === null || !(box as HTMLInputElement).checked).toBe(true);
      }
    }
    // every ticked box belongs to a copy that is not the suggestion
    const keeps = new Set(truth.flatMap((g) => g.copies.filter((c) => c.suggestedKeep).map((c) => `Select ${c.path}`)));
    for (const box of checkedBoxes()) expect(keeps.has(box.getAttribute("aria-label")!)).toBe(false);
    expect(checkedBoxes().length).toBeGreaterThan(0);
    expect(screen.getByRole("region", { name: "Selection" })).toHaveTextContent(/\d+ items?, /);
  });

  it("a group's last unlocked copy can't be ticked when the others are all ticked", async () => {
    const { base, api } = spied();
    const user = await openClean(api);
    await scan(user);
    const truth: DupGroup[] = (await base.cleanScanState()).found;
    const g = truth.find((x) => x.copies.length >= 2 && x.copies.length <= 4 && x.copies.every((c) => c.lock === null))!;
    const list = await expand(user, g.name);
    const boxes = within(list).getAllByRole("checkbox");
    for (const b of boxes.slice(0, -1)) await user.click(b);
    expect(boxes.slice(0, -1).every((b) => (b as HTMLInputElement).checked)).toBe(true);
    const last = boxes[boxes.length - 1]!;
    expect(last).toBeDisabled();
    expect(last).toHaveAttribute("title", "One copy always stays");
    await user.click(last);
    expect(checkedBoxes()).toHaveLength(boxes.length - 1);
    // untick one and it can be ticked again
    await user.click(boxes[0]!);
    expect(last).toBeEnabled();
  });

  it("lets every unlocked copy be ticked when a locked copy stays", async () => {
    const user = await openClean();
    await scan(user);
    const list = await expand(user, "vc_redist.x64.exe");
    const boxes = within(list).getAllByRole("checkbox");
    expect(boxes).toHaveLength(2); // the Program Files copy has none
    for (const b of boxes) await user.click(b);
    expect(checkedBoxes()).toHaveLength(2);
  });

  it("a locked copy has no tick and says why", async () => {
    const user = await openClean();
    await scan(user);
    const pf = within(await expand(user, "vc_redist.x64.exe"));
    expect(pf.queryByRole("checkbox", { name: /Program Files/ })).toBeNull();
    const row = pf.getByText(lockText("programFiles")).closest("li")!;
    expect(within(row).queryByRole("checkbox")).toBeNull();
    expect(row).toHaveTextContent("Program Files");

    const usb = within(await expand(user, "IMG_4412.CR3"));
    expect(usb.getByText(lockText("noRecycleBin"))).toBeInTheDocument();
    expect(usb.getAllByRole("checkbox")).toHaveLength(2);
  });

  it("keeps a very long path legible: the name stays, the full path is on hover", async () => {
    const user = await openClean();
    await scan(user);
    const long = "Quarterly-financial-statement-consolidated-draft-for-review-by-the-board-of-directors-FINAL-v7-signed.xlsx";
    const list = within(await expand(user, long));
    const deep = list.getAllByTitle(/Drafts for review\\Quarterly-financial/)[0]!;
    expect(deep).toHaveAttribute("title", expect.stringContaining("D:\\Backups\\Old laptop\\Users\\Calin"));
    expect(deep.querySelector(".mid-tail")!.textContent).toContain("signed.xlsx");
    expect(deep.querySelector(".mid-head")!.textContent!.startsWith("D:\\")).toBe(true);
  });

  it("narrows Duplicates to a drive when its sidebar entry is clicked", async () => {
    const user = await openClean();
    await scan(user);
    const all = groupButtons().length;
    await user.click(screen.getByRole("button", { name: /^Data \(D:\)/ }));
    await waitFor(() => expect(groupButtons().length).toBeLessThan(all));
    expect(screen.queryByRole("button", { name: /^vc_redist.x64.exe, / })).toBeNull(); // only on C:
    expect(screen.getByText(/on Data \(D:\)/, { selector: ".clean-caption" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /^Data \(D:\)/ }));
    await waitFor(() => expect(groupButtons()).toHaveLength(all));
  });

  it("lists matches on a linked PC apart, read-only, as not compared", async () => {
    const user = await openClean();
    await scan(user);
    const region = await screen.findByRole("region", { name: "On your other PCs" });
    expect(region).toHaveTextContent("Same name and size, contents not compared");
    expect(within(region).queryAllByRole("checkbox")).toHaveLength(0);
    expect(within(region).getByText("Holiday-Photos-2023.zip")).toBeInTheDocument();
  });

  it("explains scanning and zero results instead of showing a blank list", async () => {
    const user = await openClean(makeApi({}, { clean: "empty" }));
    expect(screen.getByText(/compares what's inside files that share a name/i)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Scan" }));
    expect(await screen.findByText("No duplicates found")).toBeInTheDocument();
    await user.click(tab("Leftovers"));
    expect(await screen.findByText("No leftovers found")).toBeInTheDocument();
    await user.click(tab("Apps"));
    expect(await screen.findByText("No apps found")).toBeInTheDocument();
  });
});

describe("Review sheet", () => {
  async function reviewDuplicates(api: SeekApi) {
    const user = await openClean(api);
    await scan(user);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog", { name: /^Move \d/ });
    await within(dialog).findAllByRole("row");
    return { user, dialog };
  }

  it("does not focus the action button, and Enter on the list removes nothing", async () => {
    const { api, commit } = spied();
    const { user, dialog } = await reviewDuplicates(api);
    const list = within(dialog).getByRole("grid", { name: "Items to move" });
    expect(list).toHaveFocus();
    expect(list).toHaveAttribute("data-autofocus");
    const move = within(dialog).getByRole("button", { name: /^Move \d/ });
    expect(move).not.toHaveFocus();
    await user.keyboard("{Enter}");
    await user.keyboard("{ArrowDown}{Enter}");
    expect(commit).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog", { name: /^Move \d/ })).toBeInTheDocument();
  });

  it("follows unticking: the title and the total change", async () => {
    const { api } = spied();
    const { user, dialog } = await reviewDuplicates(api);
    const count = () => Number(/Move ([\d,]+) item/.exec(dialog.querySelector("h2")!.textContent!)![1]!.replace(/,/g, ""));
    const total = () => dialog.querySelector(".review-total")!.textContent!;
    const n = count();
    const before = total();
    expect(n).toBeGreaterThan(3);
    await user.click(within(dialog).getAllByRole("checkbox")[1]!);
    expect(count()).toBe(n - 1);
    expect(total()).not.toBe(before);
    expect(within(dialog).getByRole("button", { name: new RegExp(`^Move ${n - 1} items?`) })).toBeInTheDocument();
  });

  it("lets every tab reach removal only through the sheet", async () => {
    const { api, commit } = spied();
    const user = await openClean(api);
    // Duplicates
    await scan(user);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    await screen.findByRole("dialog");
    expect(commit).not.toHaveBeenCalled();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(commit).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Clear" }));
    // Leftovers
    await toLeftovers(user);
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const sheet = await screen.findByRole("dialog");
    await within(sheet).findAllByRole("row");
    expect(commit).not.toHaveBeenCalled();
    await user.click(within(sheet).getByRole("button", { name: /^Move 1 item/ }));
    await within(await screen.findByRole("dialog")).findByText(/^Moved 1 item/);
    expect(commit).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("button", { name: "Done" }));
    // Apps: the uninstall leftovers go through the same sheet
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Audacity/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    const left = await screen.findByRole("list", { name: "Left behind" });
    expect(within(left).queryAllByRole("checkbox", { checked: true })).toHaveLength(0);
    await user.click(within(left).getAllByRole("checkbox")[0]!);
    await user.click(screen.getByRole("button", { name: "Review…" }));
    expect(commit).toHaveBeenCalledTimes(1);
    const sheet2 = await screen.findAllByRole("dialog");
    const top = sheet2[sheet2.length - 1]!;
    await within(top).findAllByRole("row");
    await user.click(within(top).getByRole("button", { name: /^Move 1 item/ }));
    await waitFor(() => expect(commit).toHaveBeenCalledTimes(2));
  });

  it("marks Program Files rows with a shield, and the button says it asks for admin", async () => {
    const user = await openClean();
    await toLeftovers(user);
    const row = screen.getByText("Battle.net Launcher").closest("li")!;
    expect(within(row).getByRole("img", { name: "Asks for admin" })).toBeInTheDocument();
    await user.click(within(row).getByRole("checkbox"));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog");
    const sheetRow = (await within(dialog).findAllByRole("row")).find((r) => r.textContent!.includes("Battle.net Launcher"))!;
    expect(within(sheetRow).getByRole("img", { name: "Asks for admin" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Move 1 item… (asks for admin)" })).toBeInTheDocument();
  });

  it("replaces the list with the result in the same sheet, with what stayed and why", async () => {
    const user = await openClean();
    await toLeftovers(user);
    await user.click(screen.getByRole("button", { name: /^Probably still in use/ }));
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await user.click(screen.getByRole("checkbox", { name: /Select .*\\Code$/ }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findAllByRole("row");
    await user.click(within(dialog).getByRole("button", { name: /^Move 2 items/ }));
    await within(dialog).findByText(/^Moved 1 item/);
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    expect(within(dialog).queryByRole("grid", { name: "Items to move" })).toBeNull();
    const stayed = within(dialog).getByRole("list", { name: "Left in place" });
    expect(within(stayed).getByText("An app or a running program is using this folder, so it was left in place.")).toBeInTheDocument();
    expect(within(stayed).getByTitle(/\\Code$/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Put back" })).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Put back" }));
    expect(await within(dialog).findByText(/Put back 1 item/)).toBeInTheDocument();
  });

  it("lets any listed item be protected from the sheet, Program Files included", async () => {
    const { api, commit, setSettings } = spied();
    const user = await openClean(api);
    await toLeftovers(user);
    const pfPath = "C:\\Program Files (x86)\\Battle.net Launcher";
    await user.click(screen.getByRole("checkbox", { name: `Select ${pfPath}` }));
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog", { name: /^Move 2 items/ });
    await within(dialog).findAllByRole("row");
    await user.click(within(dialog).getByRole("button", { name: `Always protect ${pfPath}` }));
    expect(within(dialog).queryByRole("row", { name: new RegExp("Battle.net") })).toBeNull();
    expect(dialog.querySelector("h2")).toHaveTextContent("Move 1 item to the Recycle Bin?");
    await waitFor(() =>
      expect(setSettings).toHaveBeenLastCalledWith(expect.objectContaining({ protectedPaths: [pfPath] })),
    );
    await user.click(within(dialog).getByRole("button", { name: /^Move 1 item/ }));
    await within(dialog).findByText(/^Moved 1 item/);
    expect(commit).toHaveBeenCalledTimes(1);
    const [, ids] = commit.mock.calls[0]!;
    expect(ids).toHaveLength(1);
    const result = await commit.mock.results[0]!.value;
    expect(result.items.map((i: { path: string }) => i.path)).not.toContain(pfPath);
    await user.click(screen.getByRole("button", { name: "Done" }));
    // the row stays, locked, and can't be ticked any more
    const row = screen.getByText("Battle.net Launcher").closest("li")!;
    expect(within(row).queryByRole("checkbox")).toBeNull();
    expect(row).toHaveTextContent("Protected");
  });

  it("renders only the rows in view for thousands of items", async () => {
    const { api } = spied({ clean: "big" });
    const user = await openClean(api);
    await scan(user);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog", { name: /^Move [\d,]+ items/ });
    const n = Number(/Move ([\d,]+) items/.exec(dialog.querySelector("h2")!.textContent!)![1]!.replace(/,/g, ""));
    expect(n).toBeGreaterThan(3000);
    await within(dialog).findAllByRole("row");
    expect(within(dialog).getAllByRole("row").length).toBeLessThan(80);
  }, 30_000);
});

describe("Leftovers and the meter", () => {
  it("always writes the level out, and shows the reasons", async () => {
    const user = await openClean();
    await toLeftovers(user);
    const row = screen.getByText("OldVPNClient").closest("li")!;
    expect(within(row).getByText("Very likely left over")).toBeInTheDocument();
    await user.click(within(row).getByRole("button", { name: /OldVPNClient/ }));
    const reasons = within(row).getByRole("list", { name: "Why Seek thinks so" });
    expect(within(reasons).getByText("Nothing inside changed for a year")).toBeInTheDocument();
    expect(within(reasons).getByText("Nothing inside changed for a year").closest("li")).toHaveTextContent("+2");
    const pf = screen.getByText("Battle.net Launcher").closest("li")!;
    expect(within(pf).getByText("Likely left over")).toBeInTheDocument();
    await user.hover(within(pf).getByText("Likely left over"));
    const tip = await screen.findByRole("tooltip");
    expect(within(tip).getByText("Contains programs (.exe, .dll)")).toBeInTheDocument();
    expect(within(tip).getByText("−4")).toBeInTheDocument();
  });

  it("describes a stale Seek file by what it is, never as empty", async () => {
    const user = await openClean();
    await toLeftovers(user);
    const row = screen.getByText(/5e1f0a52.*.seekidx/).closest("li")!;
    await user.click(within(row).getByRole("button", { name: /5e1f0a52/ }));
    expect(within(row).getByText("Seek's index for a drive it no longer knows")).toBeInTheDocument();
    expect(within(row).queryByText("Empty.")).toBeNull();
  });

  it("starts 'Probably still in use' collapsed under its own heading", async () => {
    const user = await openClean();
    await toLeftovers(user);
    expect(screen.queryByText("Code")).toBeNull();
    const heading = screen.getByRole("button", { name: /^Probably still in use/ });
    expect(heading).toHaveAttribute("aria-expanded", "false");
    await user.click(heading);
    expect(await screen.findByText("Code")).toBeInTheDocument();
  });
});

describe("Apps, Recently removed and Settings", () => {
  it("the uninstall sheet says it can't be undone before anything runs", async () => {
    const { api, uninstallStart } = spied();
    const user = await openClean(api);
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Spotify/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    const dialog = await screen.findByRole("dialog", { name: "Uninstall Spotify?" });
    expect(dialog).toHaveTextContent(/can't be undone/);
    expect(dialog).toHaveTextContent(/running/i);
    expect(uninstallStart).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(uninstallStart).not.toHaveBeenCalled();
  });

  it("disables Uninstall and says why when Seek can't start the uninstaller", async () => {
    const user = await openClean();
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Odd Portable Tool/ }));
    expect(screen.getByRole("button", { name: /^Uninstall…/ })).toBeDisabled();
    expect(screen.getAllByText(/can't start this app's uninstaller/).length).toBeGreaterThan(0);
    await user.click(await screen.findByRole("button", { name: /^Google Chrome/ }));
    expect(screen.getByRole("button", { name: /^Uninstall…/ })).toBeEnabled();
  });

  it("marks a folder Seek only guessed, and not one the app registered", async () => {
    const user = await openClean();
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Spotify/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    const dialog = await screen.findByRole("dialog", { name: "Uninstall Spotify?" });
    expect(dialog).toHaveTextContent(/Install folder.*\(Seek's best guess\)/);
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await user.click(await screen.findByRole("button", { name: /^Google Chrome/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    const chrome = await screen.findByRole("dialog", { name: "Uninstall Google Chrome?" });
    expect(chrome).not.toHaveTextContent(/best guess/);
  });

  it("offers 'It's finished' when an uninstaller never settles", async () => {
    const user = await openClean();
    await user.click(tab("Apps"));
    await user.click(await screen.findByRole("button", { name: /^Legacy Backup Suite/ }));
    await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    expect(await screen.findByRole("button", { name: "It's finished" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeInTheDocument();
  });

  it("Recently removed is a button in the head, not a tab", async () => {
    const user = await openClean();
    expect(screen.queryByRole("tab", { name: /Recently removed/ })).toBeNull();
    expect(within(screen.getByRole("tablist", { name: "Cleanup" })).getAllByRole("tab")).toHaveLength(3);
    await user.click(screen.getByRole("button", { name: /Recently removed/ }));
    const dialog = await screen.findByRole("dialog", { name: "Recently removed" });
    expect(await within(dialog).findByText(/invoice-0382/)).toBeInTheDocument();
    expect(within(dialog).getByText("This changed after you reviewed it, so it was left in place.")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Open Recycle Bin" })).toBeInTheDocument();
    expect(within(dialog).getAllByRole("button", { name: /^Put back/ }).length).toBeGreaterThan(0);
  });

  it("lists what is protected in Settings and lets it go", async () => {
    const { api, setSettings } = spied();
    await api.setSettings({ startWithWindows: true, theme: "system", excludeFolders: [], protectedPaths: ["D:\\Keep\\this"] });
    setSettings.mockClear();
    const user = userEvent.setup();
    render(<App api={api} />);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    const section = (await screen.findByRole("heading", { name: "Protected from Cleanup" })).closest("section")!;
    expect(await within(section).findByText("D:\\Keep\\this")).toBeInTheDocument();
    await user.click(within(section).getByRole("button", { name: "Stop protecting D:\\Keep\\this" }));
    expect(setSettings).toHaveBeenLastCalledWith(expect.objectContaining({ protectedPaths: [] }));
    expect(within(section).queryByText("D:\\Keep\\this")).toBeNull();
  });

  it("never uses the word safe in a tab", async () => {
    const user = await openClean();
    const scanText = () => {
      const labels = [...document.querySelectorAll("[title],[aria-label]")].map((e) => `${e.getAttribute("title")} ${e.getAttribute("aria-label")}`);
      return `${document.body.textContent} ${labels.join(" ")}`;
    };
    expect(scanText()).not.toMatch(/\bsafe/i);
    await scan(user);
    for (const b of groupButtons().slice(0, 6)) await user.click(b);
    expect(scanText()).not.toMatch(/\bsafe/i);
    await toLeftovers(user);
    await user.click(screen.getByRole("button", { name: /^Probably still in use/ }));
    await user.click(within(screen.getByText("OldVPNClient").closest("li")!).getByRole("button", { name: /OldVPNClient/ }));
    expect(scanText()).not.toMatch(/\bsafe/i);
    await user.click(tab("Apps"));
    await screen.findByText("Google Chrome");
    expect(scanText()).not.toMatch(/\bsafe/i);
    await user.click(screen.getByRole("button", { name: /Recently removed/ }));
    await screen.findByRole("dialog", { name: "Recently removed" });
    expect(scanText()).not.toMatch(/\bsafe/i);
  });
});

describe("Scan arrivals", () => {
  it("applies a burst of groups together and keeps the rows that were already there", async () => {
    const base = makeApi();
    const found: DupGroup[] = (await (async () => {
      await base.cleanScanStart([]);
      await new Promise((r) => setTimeout(r, 20));
      return (await base.cleanScanState()).found;
    })());
    let emit: (e: CleanEvent) => void = () => {};
    const api: SeekApi = {
      ...base,
      onCleanEvent: (cb) => {
        emit = cb;
        return () => {};
      },
      cleanScanState: async () => ({ running: true, found: [], otherPcs: [], checkedBytes: 0, totalBytes: 100, current: null }),
      cleanScanStart: async () => {},
    };
    const ctl = createCleanController(api);
    ctl.attach();
    await new Promise((r) => setTimeout(r, 0));
    ctl.scanStart([]);
    let notified = 0;
    ctl.dup.subscribe(() => notified++);
    const groups = found.slice(0, 30);
    emit({ type: "group", group: groups[0]! });
    emit({ type: "group", group: groups[1]! });
    await new Promise((r) => setTimeout(r, 200));
    const first = ctl.dup.get().groups[0];
    for (const g of groups.slice(2)) emit({ type: "group", group: g });
    emit({ type: "progress", checkedBytes: 50, totalBytes: 100, current: "D:\\x" });
    await new Promise((r) => setTimeout(r, 200));
    expect(ctl.dup.get().groups).toHaveLength(30);
    expect(ctl.dup.get().groups[0]).toBe(first); // same object: its row doesn't render again
    expect(notified).toBeLessThanOrEqual(3);
  });
});

describe("Always protect and Reveal from the row menu", () => {
  const PF = "C:\\Program Files (x86)\\Battle.net Launcher";
  const rowOf = (name: string) => screen.getByText(name).closest("li")!;
  const rightClick = (user: UE, el: Element) => user.pointer({ keys: "[MouseRight]", target: el });
  const openSettingsList = async (user: UE) => {
    await user.click(screen.getByRole("button", { name: "Settings" }));
    return (await screen.findByRole("heading", { name: "Protected from Cleanup" })).closest("section")!;
  };
  /** The first duplicate copy in Downloads, opened. */
  async function firstCopy(user: UE) {
    await scan(user);
    const list = within(await expand(user, "vc_redist.x64.exe"));
    const box = list.getByRole("checkbox", { name: /^Select C:\\Users\\Calin\\Downloads\\vc_redist/ });
    return { box, li: box.closest("li")!, path: box.getAttribute("aria-label")!.slice("Select ".length) };
  }

  it("always_protect_from_the_menu_locks_the_row_and_appears_in_settings", async () => {
    const { api, setSettings } = spied();
    const user = await openClean(api);
    await toLeftovers(user);
    await rightClick(user, rowOf("Battle.net Launcher"));
    expect(screen.getByRole("menu", { name: "Folder actions" })).toBeInTheDocument();
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    // saved first, then the row is judged again
    await waitFor(() => expect(setSettings).toHaveBeenLastCalledWith(expect.objectContaining({ protectedPaths: [PF] })));
    await waitFor(() => expect(rowOf("Battle.net Launcher")).toHaveTextContent("Protected"));
    expect(within(rowOf("Battle.net Launcher")).queryByRole("checkbox")).toBeNull();

    // it stays locked after a rescan
    await user.click(screen.getByRole("button", { name: "Rescan" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Rescan" })).toBeEnabled());
    expect(rowOf("Battle.net Launcher")).toHaveTextContent("Protected");
    expect(within(rowOf("Battle.net Launcher")).queryByRole("checkbox")).toBeNull();

    const section = await openSettingsList(user);
    expect(await within(section).findByText(PF)).toBeInTheDocument();
  });

  it("removing_it_in_settings_unlocks_the_row", async () => {
    const { api } = spied();
    const user = await openClean(api);
    await toLeftovers(user);
    await rightClick(user, rowOf("Battle.net Launcher"));
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    await waitFor(() => expect(rowOf("Battle.net Launcher")).toHaveTextContent("Protected"));

    const section = await openSettingsList(user);
    await user.click(within(section).getByRole("button", { name: `Stop protecting ${PF}` }));
    await user.click(screen.getByRole("button", { name: "Close settings" }));
    await waitFor(() => expect(within(rowOf("Battle.net Launcher")).getByRole("checkbox")).toBeInTheDocument());
    expect(rowOf("Battle.net Launcher")).not.toHaveTextContent("Protected");
  });

  it("protects a duplicate copy by its file, shows the protected reason, and Stop protecting undoes it", async () => {
    const { api, setSettings } = spied();
    const user = await openClean(api);
    const { li, path } = await firstCopy(user);
    await rightClick(user, li);
    expect(screen.getByRole("menu", { name: "Copy actions" })).toBeInTheDocument();
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    await waitFor(() => expect(setSettings).toHaveBeenLastCalledWith(expect.objectContaining({ protectedPaths: [path] })));
    const locked = (await screen.findByText(lockText("protected"), { selector: ".copy-why" })).closest("li")!;
    expect(locked).toHaveTextContent(path.slice(path.lastIndexOf("\\") + 1));
    expect(within(locked).queryByRole("checkbox")).toBeNull();

    await rightClick(user, locked);
    await user.click(screen.getByRole("menuitem", { name: "Stop protecting" }));
    await waitFor(() => expect(setSettings).toHaveBeenLastCalledWith(expect.objectContaining({ protectedPaths: [] })));
    await waitFor(() => expect(screen.queryByText(lockText("protected"), { selector: ".copy-why" })).toBeNull());
  });

  it("a protected folder locks what is inside it, and a copy's menu can't undo the folder's protection", async () => {
    const { api } = spied();
    await api.setSettings({ ...(await api.getSettings()), protectedPaths: ["C:\\Users\\Calin\\Downloads"] });
    const user = await openClean(api);
    await scan(user);
    const list = within(await expand(user, "vc_redist.x64.exe"));
    const locked = (await list.findByText(lockText("protected"), { selector: ".copy-why" })).closest("li")!;
    expect(within(locked).queryByRole("checkbox")).toBeNull();
    await rightClick(user, locked);
    const item = screen.getByRole("menuitem", { name: "Stop protecting" });
    expect(item).toBeDisabled();
    expect(item).toHaveAttribute("title", expect.stringContaining("C:\\Users\\Calin\\Downloads"));
    // the reason is visible text under the item, not only a tooltip
    const note = item.parentElement!.querySelector(".menu-note")!;
    expect(note).toHaveTextContent("Protected through");
    expect(note).toHaveTextContent("C:\\Users\\Calin\\Downloads");
    expect(note).toHaveTextContent("Stop protecting that folder instead.");
    expect(screen.getByRole("menuitem", { name: "Reveal in Explorer" })).toBeEnabled();
  });

  it("when the list can't be saved it says so and the row is not shown as protected", async () => {
    const base = makeApi();
    let fail: (e: Error) => void = () => undefined;
    const setSettings = vi.fn(() => new Promise<void>((_, reject) => void (fail = reject)));
    const user = await openClean({ ...base, setSettings });
    await toLeftovers(user);
    await rightClick(user, rowOf("Battle.net Launcher"));
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    // while the save is still running the row is not judged protected yet
    await waitFor(() => expect(setSettings).toHaveBeenCalled());
    expect(rowOf("Battle.net Launcher")).not.toHaveTextContent("Protected");
    fail(new Error("disk is full"));
    expect(await screen.findByText(/"Always protect" could not be saved\. disk is full/)).toBeInTheDocument();
    expect(setSettings).toHaveBeenCalled();
    // an error stays longer than the 1.8 s of an ordinary message
    await new Promise((r) => setTimeout(r, 2200));
    expect(screen.getByText(/could not be saved\. disk is full/)).toBeInTheDocument();
    const row = rowOf("Battle.net Launcher");
    expect(row).not.toHaveTextContent("Protected");
    expect(within(row).getByRole("checkbox")).toBeInTheDocument();
  });

  it("opens on the context-menu key and on Shift+F10 for the focused row", async () => {
    const user = await openClean();
    await toLeftovers(user);
    const li = rowOf("Battle.net Launcher");
    within(li).getByRole("checkbox").focus();
    await user.keyboard("{ContextMenu}");
    expect(screen.getByRole("menu", { name: "Folder actions" })).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).toBeNull();
    expect(within(li).getByRole("checkbox")).toHaveFocus();

    await user.keyboard("{Shift>}{F10}{/Shift}");
    const items = within(screen.getByRole("menu", { name: "Folder actions" })).getAllByRole("menuitem");
    expect(items.map((i) => i.textContent)).toEqual(["Always protect", "Reveal in Explorer"]);
    // the menu works from the keyboard alone: Enter chooses the first item
    await user.keyboard("{Enter}");
    await waitFor(() => expect(rowOf("Battle.net Launcher")).toHaveTextContent("Protected"));

    // a duplicate copy opens its own menu the same way
    await user.click(tab("Duplicates"));
    const { box } = await firstCopy(user);
    box.focus();
    await user.keyboard("{ContextMenu}");
    expect(screen.getByRole("menu", { name: "Copy actions" })).toBeInTheDocument();
  });

  it("after a mouse right-click, Escape puts focus back on the row", async () => {
    const user = await openClean();
    await toLeftovers(user);
    const li = rowOf("Battle.net Launcher");
    await rightClick(user, li);
    expect(screen.getByRole("menu")).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).toBeNull();
    expect(li).toHaveFocus();
  });

  it("two rows protected before the first save finishes both end up protected, in the file and on screen", async () => {
    const base = makeApi();
    const gates: Array<() => void> = [];
    const setSettings = vi.fn(
      (s: Parameters<SeekApi["setSettings"]>[0]) =>
        new Promise<void>((resolve) => gates.push(resolve)).then(() => base.setSettings(s)),
    );
    const user = await openClean({ ...base, setSettings });
    await toLeftovers(user);
    const VPN = "OldVPNClient";
    await rightClick(user, rowOf("Battle.net Launcher"));
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    await rightClick(user, rowOf(VPN));
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    await waitFor(() => expect(gates).toHaveLength(1));
    gates[0]!();
    await waitFor(() => expect(gates).toHaveLength(2)); // the second save starts after the first
    gates[1]!();
    await waitFor(() => expect(rowOf(VPN)).toHaveTextContent("Protected"));
    expect(rowOf("Battle.net Launcher")).toHaveTextContent("Protected");
    const saved = setSettings.mock.calls.at(-1)![0].protectedPaths;
    expect(saved).toHaveLength(2);
    expect(saved).toContain(PF);
    expect(saved.some((p) => p.endsWith("\\" + VPN))).toBe(true);
    expect((await base.getSettings()).protectedPaths).toEqual(saved);
  });

  it("Reveal in Explorer uses revealPath with the row's path", async () => {
    const base = makeApi();
    const revealPath = vi.fn(async () => undefined);
    const user = await openClean({ ...base, revealPath });
    await toLeftovers(user);
    await rightClick(user, rowOf("Battle.net Launcher"));
    await user.click(screen.getByRole("menuitem", { name: "Reveal in Explorer" }));
    expect(revealPath).toHaveBeenCalledWith(PF);
  });

  it("a row protected after it was ticked drops out and is not sent for review", async () => {
    const base = makeApi();
    const cleanPrepare = vi.fn(base.cleanPrepare);
    const user = await openClean({ ...base, cleanPrepare });
    await toLeftovers(user);
    await user.click(screen.getByRole("checkbox", { name: `Select ${PF}` }));
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await rightClick(user, rowOf("Battle.net Launcher"));
    await user.click(screen.getByRole("menuitem", { name: "Always protect" }));
    await waitFor(() => expect(rowOf("Battle.net Launcher")).toHaveTextContent("Protected"));
    expect(screen.getByRole("region", { name: "Selection" })).toHaveTextContent(/^1 item, /);
    await user.click(screen.getByRole("button", { name: "Review…" }));
    await screen.findByRole("dialog", { name: /^Move 1 item/ });
    expect(cleanPrepare).toHaveBeenCalledTimes(1);
    const sent = JSON.stringify(cleanPrepare.mock.calls[0]![0]);
    expect(sent).not.toContain("Battle.net");
    expect(sent).toContain("ZoomUpdater-Cache");
  });
});
