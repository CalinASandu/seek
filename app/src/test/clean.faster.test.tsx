import { render, screen, within } from "@testing-library/react";
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
  it("Scan says what it remembers, and the head keeps one button", async () => {
    const user = await openClean(makeApi());
    const scan = screen.getByRole("button", { name: "Scan" });
    expect(scan.title).toMatch(/remembers files it has already compared, in your temp folder/);
    await user.click(scan);
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
