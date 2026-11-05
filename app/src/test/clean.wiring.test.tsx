import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { CleanEvent, DupScanState, SeekApi } from "../api/types";
import { createCleanController } from "../hooks/useCleanup";

type UE = ReturnType<typeof userEvent.setup>;

function makeApi(over: Partial<SeekApi> = {}): SeekApi {
  return { ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected" }), ...over };
}
/** An API whose clean events can also be sent by the test, as the backend does. */
function withEvents(over: Partial<SeekApi> = {}) {
  const base = makeApi(over);
  const sinks = new Set<(e: CleanEvent) => void>();
  const api: SeekApi = {
    ...base,
    onCleanEvent: (cb) => {
      sinks.add(cb);
      const off = base.onCleanEvent(cb);
      return () => {
        sinks.delete(cb);
        off();
      };
    },
  };
  return { base, api, send: (e: CleanEvent) => act(() => sinks.forEach((s) => s(e))) };
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

describe("Results dropped by the backend", () => {
  it("releases the groups and offers Scan at once when told, with no view change", async () => {
    const h = withEvents();
    const user = await scanned(h.api);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    expect(screen.getByRole("region", { name: "Selection" })).toBeInTheDocument();

    await h.send({ type: "dropped" });

    await waitFor(() => expect(screen.getByRole("button", { name: "Scan" })).toBeEnabled());
    expect(screen.queryByRole("button", { name: "Rescan" })).toBeNull();
    expect(screen.getByText("Find files you have twice")).toBeInTheDocument();
    expect(groupButtons()).toHaveLength(0);
    expect(document.querySelectorAll(".cl-row")).toHaveLength(0);
    expect(screen.queryByRole("region", { name: "Selection" })).toBeNull();
  });

  it("keeps an open review sheet, whose Move then reports the expired review plainly", async () => {
    const expired = "This review is no longer valid. Review the items again.";
    const h = withEvents({
      cleanCommit: async () => {
        throw expired;
      },
    });
    const user = await scanned(h.api);
    await user.click(screen.getByRole("button", { name: "Use suggestions" }));
    await user.click(screen.getByRole("button", { name: "Review…" }));
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findAllByRole("row");

    await h.send({ type: "dropped" });
    expect(screen.getByRole("dialog")).toBe(dialog);
    expect(groupButtons()).toHaveLength(0);

    await user.click(within(dialog).getByRole("button", { name: /^Move \d/ }));
    expect(await within(dialog).findByText(expired)).toBeInTheDocument();
  });

  it("ignores a drop that arrives while a newer scan runs", async () => {
    const h = withEvents({ cleanScanState: async () => ({ ...EMPTY, running: true }) });
    const ctl = createCleanController(h.api);
    ctl.attach();
    ctl.scanStart([]);
    expect(ctl.dup.get().status).toBe("scanning");
    await h.send({ type: "dropped" });
    expect(ctl.dup.get().status).toBe("scanning");
  });

  it("shows the Scan offer on return when it was not listening", async () => {
    const base = makeApi();
    let dropped = false;
    const api: SeekApi = { ...base, cleanScanState: async () => (dropped ? EMPTY : base.cleanScanState()) };
    const user = await scanned(api);
    await toggleView(user); // away from the Decimator
    await screen.findByRole("searchbox");
    dropped = true;
    await toggleView(user); // back
    await waitFor(() => expect(screen.getByRole("button", { name: "Scan" })).toBeEnabled());
    expect(screen.getByText("Find files you have twice")).toBeInTheDocument();
    expect(groupButtons()).toHaveLength(0);
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

  it("shows the notice above the list and keeps it after the scan ends", async () => {
    const base = makeApi();
    const api: SeekApi = { ...base, cleanScanState: async () => ({ ...(await base.cleanScanState()), notice: NOTICE }) };
    await scanned(api);
    expect(screen.getByText(NOTICE)).toBeInTheDocument();
  });

  it("shows a notice as soon as the backend raises it", async () => {
    const h = withEvents();
    const ctl = createCleanController(h.api);
    ctl.attach();
    ctl.scanStart([]);
    await h.send({ type: "notice", text: NOTICE });
    expect(ctl.dup.get().notice).toBe(NOTICE);
    expect(ctl.dup.get().status).toBe("scanning"); // a notice is not the end of anything
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

  it("is cleared when a new scan starts and then read again from the backend", async () => {
    const base = makeApi();
    let notice: string | null = NOTICE;
    const reread = vi.fn();
    const ctl = createCleanController({
      ...base,
      cleanScanStart: async () => {
        notice = "Backup could not be read right now.";
      },
      cleanScanState: async () => {
        reread();
        return { ...EMPTY, running: true, notice };
      },
      onCleanEvent: () => () => {},
    });
    ctl.attach();
    await waitFor(() => expect(ctl.dup.get().notice).toBe(NOTICE));
    ctl.dup.set((s) => ({ ...s, status: "done" }));
    reread.mockClear();
    ctl.scanStart([]);
    expect(ctl.dup.get().notice).toBeNull(); // the old text is gone before anything is read
    await waitFor(() => expect(ctl.dup.get().notice).toBe("Backup could not be read right now."));
    expect(reread).toHaveBeenCalled();
  });
});

describe("Settings that must not be lost", () => {
  async function openSettings(api: SeekApi) {
    const user = userEvent.setup();
    render(<App api={api} />);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    return user;
  }

  it("never sends a save when the first load failed, and says nothing was saved", async () => {
    const setSettings = vi.fn(async () => {});
    const api = makeApi({
      getSettings: async () => {
        throw new Error("the service is not there");
      },
      setSettings,
    });
    const user = await openSettings(api);
    await user.click(await screen.findByRole("radio", { name: "Dark" }));
    expect(setSettings).not.toHaveBeenCalled();
    expect(await screen.findByText(/haven't loaded/)).toBeInTheDocument();
  });

  it("puts a setting back and says why when it could not be saved", async () => {
    const why = "Your settings file (X:\\Seek\\settings.json) couldn't be read. It wasn't overwritten; fix or delete it first.";
    const api = makeApi({
      setSettings: async () => {
        throw why;
      },
    });
    const user = await openSettings(api);
    const light = await screen.findByRole("radio", { name: "Light" });
    await user.click(light);
    expect(await screen.findByText(why)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("radio", { name: "System" })).toBeChecked());
  });

  it("tells the user in Protected from Cleanup when the list could not be read", async () => {
    const problem = "Your settings file (X:\\Seek\\settings.json) couldn't be read.";
    const user = await openSettings(makeApi({ settingsProblem: async () => problem }));
    const section = (await screen.findByRole("heading", { name: "Protected from Cleanup" })).closest("section")!;
    const note = await within(section).findByRole("alert");
    expect(note).toHaveTextContent(problem);
    expect(note).toHaveTextContent(/removals are off/i);
    void user;
  });

  it("shows no such note when the settings are fine", async () => {
    await openSettings(makeApi());
    const section = (await screen.findByRole("heading", { name: "Protected from Cleanup" })).closest("section")!;
    expect(within(section).queryByRole("alert")).toBeNull();
  });
});
