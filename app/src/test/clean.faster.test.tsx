import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { SeekApi } from "../api/types";

const makeApi = (over: Partial<SeekApi> = {}): SeekApi => ({
  ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected" }),
  ...over,
});

async function openClean(api: SeekApi) {
  const user = userEvent.setup();
  render(<App api={api} />);
  await user.click(await screen.findByRole("radio", { name: "Decimator" }));
  return user;
}

describe("Faster scans", () => {
  it("scans same-name copies by default, and Rescan repeats a renamed-copies scan", async () => {
    const base = makeApi();
    const start = vi.fn(base.cleanScanStart);
    const user = await openClean({ ...base, cleanScanStart: start });
    await user.click(screen.getByRole("button", { name: "Include renamed copies" }));
    expect(start).toHaveBeenLastCalledWith(expect.any(Array), true);
    await user.click(await screen.findByRole("button", { name: "Rescan" }, { timeout: 3000 }));
    await waitFor(() => expect(start).toHaveBeenCalledTimes(2));
    expect(start).toHaveBeenLastCalledWith(expect.any(Array), true);
  });

  it("Scan starts a quick scan, and the head keeps one button", async () => {
    const base = makeApi();
    const start = vi.fn(base.cleanScanStart);
    const user = await openClean({ ...base, cleanScanStart: start });
    await user.click(screen.getByRole("button", { name: "Scan" }));
    expect(start).toHaveBeenLastCalledWith(expect.any(Array), false);
    const head = document.querySelector(".clean-hero")!;
    expect(within(head as HTMLElement).getAllByRole("button")).toHaveLength(1);
  });

  it("Scan faster has the exact tooltip and toggles while scanning", async () => {
    const base = makeApi();
    const faster = vi.fn(async () => {});
    const api: SeekApi = {
      ...base,
      cleanScanStart: async () => {},
      cleanScanFaster: faster,
      cleanScanState: async () => ({ running: true, found: [], otherPcs: [], checkedBytes: 0, totalBytes: 100, current: null }),
    };
    // the view picks up the scan that is already running
    const user = await openClean(api);
    const btn = await screen.findByRole("button", { name: "Scan faster" });
    expect(btn).toHaveAttribute("title", "Uses more of your PC's disk and processor while it scans.");
    expect(btn).toHaveAttribute("aria-pressed", "false");
    await user.click(btn);
    expect(faster).toHaveBeenCalledWith(true);
    expect(btn).toHaveAttribute("aria-pressed", "true");
  });

  it("Settings shows what scans remember, and Clear forgets it", async () => {
    const base = makeApi();
    const forget = vi.fn(base.cleanForget);
    const user = userEvent.setup();
    render(<App api={{ ...base, cleanForget: forget }} />);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    const dialog = await screen.findByRole("dialog", { name: "Settings" });
    expect(await within(dialog).findByText(/^Uses \d/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Clear" }));
    expect(forget).toHaveBeenCalled();
    expect(await within(dialog).findByText("Nothing remembered yet")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Clear" })).toBeDisabled();
  });
});
