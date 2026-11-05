import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { DupScanState, SeekApi } from "../api/types";
import { createCleanController } from "../hooks/useCleanup";

type UE = ReturnType<typeof userEvent.setup>;

function makeApi(over: Partial<SeekApi> = {}): SeekApi {
  return { ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected" }), ...over };
}
const groupButtons = () => screen.queryAllByRole("button", { name: /, \d+ copies$/ });
const toggleView = (user: UE) => user.keyboard("{Control>}{Shift>}d{/Shift}{/Control}");
const EMPTY: DupScanState = { running: false, found: [], otherPcs: [], checkedBytes: 0, totalBytes: 0, current: null };

async function scanned(api: SeekApi) {
  const user = userEvent.setup();
  render(<App api={api} />);
  await screen.findByRole("searchbox");
  await toggleView(user);
  await user.click(await screen.findByRole("button", { name: "Scan" }));
  await waitFor(() => expect(groupButtons().length).toBeGreaterThan(5));
  await waitFor(() => expect(screen.getByRole("button", { name: "Rescan" })).toBeEnabled());
  return user;
}

describe("Results dropped while the window was hidden", () => {
  it("shows the Scan offer again and releases the groups when the backend reports nothing left", async () => {
    const base = makeApi();
    let dropped = false;
    const api: SeekApi = { ...base, cleanScanState: async () => (dropped ? EMPTY : base.cleanScanState()) };
    const user = await scanned(api);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    expect(screen.getByRole("region", { name: "Selection" })).toBeInTheDocument();

    await toggleView(user); // away from the Decimator
    await screen.findByRole("searchbox");
    dropped = true;
    await toggleView(user); // back

    await waitFor(() => expect(screen.getByRole("button", { name: "Scan" })).toBeEnabled());
    expect(screen.queryByRole("button", { name: "Rescan" })).toBeNull();
    expect(screen.getByText("Find files you have twice")).toBeInTheDocument();
    expect(groupButtons()).toHaveLength(0);
    expect(document.querySelectorAll(".cl-row")).toHaveLength(0);
    expect(screen.queryByRole("region", { name: "Selection" })).toBeNull();
  });

  it("releases the controller's groups, ticks and fresh-row marks", async () => {
    const base = makeApi();
    await base.cleanScanStart([]);
    await new Promise((r) => setTimeout(r, 10));
    const found = (await base.cleanScanState()).found;
    let state: DupScanState = { ...EMPTY, found };
    const ctl = createCleanController({ ...base, cleanScanState: async () => state, onCleanEvent: () => () => {} });
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().groups.length).toBe(found.length));
    ctl.applySuggestions(found);
    ctl.fresh.set(found[0]!.id, Date.now());
    expect(ctl.dupTicks.get().size).toBeGreaterThan(0);
    ctl.detach();
    state = EMPTY;
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().status).toBe("idle"));
    expect(ctl.dup.get().groups).toHaveLength(0);
    expect(ctl.dupTicks.get().size).toBe(0);
    expect(ctl.fresh.size).toBe(0);
  });
});

describe("The scan's notice", () => {
  const NOTICE = "Left out, still being indexed for the first time: Data.";

  it("is said in the scan line and kept after the scan ends", async () => {
    const base = makeApi();
    const api: SeekApi = { ...base, cleanScanState: async () => ({ ...(await base.cleanScanState()), notice: NOTICE }) };
    await scanned(api);
    expect(screen.getByText(NOTICE)).toBeInTheDocument();
  });

  it("is adopted when the view comes back to a scan that went on", async () => {
    const base = makeApi();
    const ctl = createCleanController({
      ...base,
      cleanScanState: async () => ({ ...EMPTY, running: true, notice: NOTICE }),
      onCleanEvent: () => () => {},
    });
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().notice).toBe(NOTICE));
  });

  it("clears when a new scan starts", async () => {
    const base = makeApi();
    let notice: string | null = NOTICE;
    const ctl = createCleanController({
      ...base,
      cleanScanState: async () => ({ ...EMPTY, running: true, notice }),
      onCleanEvent: () => () => {},
    });
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().notice).toBe(NOTICE));
    ctl.dup.set((s) => ({ ...s, status: "done" }));
    notice = null;
    ctl.scanStart([]);
    expect(ctl.dup.get().notice).toBeNull();
  });
});
