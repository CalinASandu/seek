import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { HistoryEntry, SeekApi } from "../api/types";

const base = (): SeekApi => createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected" });

const entry = (over: Partial<HistoryEntry>): HistoryEntry => ({
  id: "h1",
  timeMs: Date.now() - 3_600_000,
  originalPath: "C:\\Users\\me\\Docs\\report.docx",
  size: 1000,
  why: "duplicate",
  binPath: "C:\\$Recycle.Bin\\S\\$RAAA.docx",
  leftReason: null,
  putBackMs: null,
  ...over,
});

async function openRemoved(api: SeekApi) {
  const user = userEvent.setup();
  render(<App api={api} />);
  await user.click(await screen.findByRole("radio", { name: "Decimator" }));
  await user.click(screen.getByRole("button", { name: /Recently removed/ }));
  const dialog = await screen.findByRole("dialog", { name: "Recently removed" });
  return { user, dialog };
}

describe("Recently removed", () => {
  it("lets an uninstall leftover be put back like anything else", async () => {
    const cleanPutBack = vi.fn(async (ids: string[]) => ids.map((id) => ({ id, restored: true, reason: null })));
    const api: SeekApi = {
      ...base(),
      cleanHistory: async () => [entry({ id: "u1", why: "uninstall", originalPath: "C:\\Program Files (x86)\\Contoso Updater" })],
      cleanPutBack,
    };
    const { user, dialog } = await openRemoved(api);
    const button = await within(dialog).findByRole("button", { name: "Put back Contoso Updater" });
    expect(within(dialog).queryByText(/can't be put back/)).toBeNull();
    await user.click(button);
    expect(cleanPutBack).toHaveBeenCalledWith(["u1"]);
    expect(await within(dialog).findByText("Put back")).toBeInTheDocument();
  });

  it("shows a put-back entry as Put back, with no button", async () => {
    const api: SeekApi = { ...base(), cleanHistory: async () => [entry({ putBackMs: Date.now() - 1000 })] };
    const { dialog } = await openRemoved(api);
    expect(await within(dialog).findByText("Put back")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: /^Put back/ })).toBeNull();
  });

  it("says the reason when a put-back worked but could not be recorded", async () => {
    const note = "Recently removed couldn't record it. Access is denied.";
    const api: SeekApi = {
      ...base(),
      cleanHistory: async () => [entry({})],
      cleanPutBack: async (ids) => ids.map((id) => ({ id, restored: true, reason: note })),
    };
    const { user, dialog } = await openRemoved(api);
    await user.click(await within(dialog).findByRole("button", { name: "Put back report.docx" }));
    expect(await within(dialog).findByText("Put back")).toBeInTheDocument();
    expect(within(dialog).getByText(note)).toBeInTheDocument();
  });

  it("says in the result when the history could not be written, and offers the Recycle Bin instead of Put back", async () => {
    const said = "Moved to the Recycle Bin, but they weren't recorded. Access is denied. Put them back from the Recycle Bin if needed.";
    const b = base();
    const openRecycleBin = vi.fn(async () => {});
    const api: SeekApi = {
      ...b,
      openRecycleBin,
      cleanCommit: async (token, ids) => ({ ...(await b.cleanCommit(token, ids)), historyError: said }),
    };
    const user = userEvent.setup();
    render(<App api={api} />);
    await user.click(await screen.findByRole("radio", { name: "Decimator" }));
    await user.click(screen.getByRole("tab", { name: "Leftovers" }));
    await screen.findByText("Battle.net Launcher");
    await user.click(screen.getByRole("checkbox", { name: /Select .*ZoomUpdater-Cache/ }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const sheet = await screen.findByRole("dialog");
    await within(sheet).findAllByRole("row");
    await user.click(within(sheet).getByRole("button", { name: /^Move 1 item/ }));
    const done = await screen.findByRole("dialog");
    await within(done).findByText(/^Moved 1 item/);
    expect(within(done).getByText(said)).toBeInTheDocument();
    expect(within(done).queryByRole("button", { name: "Put back" })).toBeNull();
    expect(within(done).queryByText(/or here/)).toBeNull();
    await user.click(within(done).getByRole("button", { name: "Open Recycle Bin" }));
    expect(openRecycleBin).toHaveBeenCalled();
  });
});
