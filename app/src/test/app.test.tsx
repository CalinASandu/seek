import { render, screen, waitFor, within, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { HubStatus, SeekApi } from "../api/types";

function makeApi(over: Partial<SeekApi> = {}): SeekApi {
  return { ...createMockApi({ rowCount: 4000, latency: [0, 0], state: "connected" }), ...over };
}

describe("Seek app", () => {
  it("shows matching rows when typing a query", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    await user.type(await screen.findByRole("searchbox"), "report");
    const grid = await screen.findByRole("grid");
    await waitFor(() => expect(within(grid).getAllByRole("row").length).toBeGreaterThan(2));
    const marks = await within(grid).findAllByText(/report/i, { selector: "mark" });
    expect(marks.length).toBeGreaterThan(0);
  });

  it("renders a parse error from the API under the search field", async () => {
    const api = makeApi({ search: async () => ({ ok: false, message: "Unterminated quote", position: 3 }) });
    const user = userEvent.setup();
    render(<App api={api} />);
    await user.type(await screen.findByRole("searchbox"), 'ab"');
    expect(await screen.findByRole("alert")).toHaveTextContent("Unterminated quote");
  });

  it("sorts by Size ascending first, then descending on the second click", async () => {
    const base = makeApi();
    const search = vi.fn(base.search);
    const user = userEvent.setup();
    render(<App api={{ ...base, search }} />);
    await user.type(await screen.findByRole("searchbox"), "report");
    await screen.findByRole("grid");
    await user.click(screen.getByRole("button", { name: "Sort by Size" }));
    await waitFor(() => expect(search).toHaveBeenLastCalledWith(expect.any(String), "size", false));
    await user.click(screen.getByRole("button", { name: "Sort by Size" }));
    await waitFor(() => expect(search).toHaveBeenLastCalledWith(expect.any(String), "size", true));
    expect(screen.getByRole("columnheader", { name: /Size/ })).toHaveAttribute("aria-sort", "descending");
  });

  it("marks offline rows with a pill and disables Open in the context menu", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    await user.type(await screen.findByRole("searchbox"), "offline:");
    const grid = await screen.findByRole("grid");
    // the mock's scanning drive keeps refreshing pages: find the row and open the menu again on every try
    const menu = await waitFor(() => {
      const row = within(grid).getAllByRole("row").slice(1).find((r) => within(r).queryByText("offline"));
      expect(row).toBeTruthy();
      fireEvent.contextMenu(row as HTMLElement);
      const m = screen.getByRole("menu");
      expect(within(m).getByRole("menuitem", { name: /^Open(?! containing)/ })).toHaveAttribute("title", "The drive is offline");
      return m;
    });
    expect(within(menu).getByRole("menuitem", { name: /^Open(?! containing)/ })).toBeDisabled();
    expect(within(menu).getByRole("menuitem", { name: /Copy path/ })).toBeEnabled();
  });

  it("renames a device inline and calls renameDevice", async () => {
    const base = makeApi();
    const renameDevice = vi.fn(base.renameDevice);
    const user = userEvent.setup();
    render(<App api={{ ...base, renameDevice }} />);
    const item = await screen.findByRole("button", { name: /Backup 2TB, offline/ });
    fireEvent.contextMenu(item);
    await user.click(await screen.findByRole("menuitem", { name: /Rename/ }));
    const input = await screen.findByRole("textbox", { name: "Rename Backup 2TB" });
    await user.clear(input);
    await user.type(input, "Cold storage{Enter}");
    await waitFor(() => expect(renameDevice).toHaveBeenCalledWith("dev-backup", "Cold storage"));
  });

  it("shows the service-not-installed card with an install button", async () => {
    const api = makeApi({ serviceState: async () => "notInstalled" });
    render(<App api={api} />);
    expect(await screen.findByRole("button", { name: "Install Seek service" })).toBeInTheDocument();
  });

  it("validates the UNC path in the add-share dialog", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    await user.click(await screen.findByRole("button", { name: "Add network share" }));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByLabelText("Network path"), "nas\\media");
    await user.click(within(dialog).getByRole("button", { name: "Add share" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("\\\\server\\share");
  });
  it("does not re-search when the index changes; F5 / the hint refreshes", async () => {
    const base = makeApi();
    const listeners: Array<(e: Parameters<Parameters<SeekApi["onEvent"]>[0]>[0]) => void> = [];
    const search = vi.fn(base.search);
    const onEvent: SeekApi["onEvent"] = (fn) => {
      listeners.push(fn);
      const off = base.onEvent(fn);
      return () => {
        listeners.splice(listeners.indexOf(fn), 1);
        off();
      };
    };
    const user = userEvent.setup();
    render(<App api={{ ...base, search, onEvent }} />);
    await user.type(await screen.findByRole("searchbox"), "report");
    await screen.findByRole("grid");
    await waitFor(() => expect(search).toHaveBeenLastCalledWith("report", "name", false));
    await new Promise((r) => setTimeout(r, 100)); // let the typing debounce settle
    const calls = search.mock.calls.length;

    // a busy drive: many invalidations, no new searches
    for (let i = 0; i < 5; i++) listeners.forEach((l) => l({ type: "resultsInvalidated" }));
    const hint = await screen.findByRole("button", { name: /Index updated/ });
    await new Promise((r) => setTimeout(r, 100));
    expect(search.mock.calls.length).toBe(calls);

    await user.click(hint);
    await waitFor(() => expect(search.mock.calls.length).toBe(calls + 1));
    await waitFor(() => expect(screen.queryByRole("button", { name: /Index updated/ })).toBeNull());

    fireEvent.keyDown(window, { key: "F5" });
    await waitFor(() => expect(search.mock.calls.length).toBe(calls + 2));
  });

  it("groups the sidebar by device: this device first, then the linked Laptop with its drives", async () => {
    render(<App api={makeApi()} />);
    const laptop = await screen.findByRole("group", { name: "Laptop" });
    expect(within(laptop).getByRole("button", { name: /System \(C:\), online/ })).toBeInTheDocument();
    expect(within(laptop).getByRole("button", { name: /Travel stick, offline/ })).toBeInTheDocument();
    const desktop = screen.getByRole("group", { name: "DESKTOP" });
    expect(within(desktop).getByText("This device")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Devices" })).toBeInTheDocument();
    expect(within(desktop).queryByRole("button", { name: /Travel stick/ })).toBeNull();
    expect(screen.getByRole("button", { name: "Sync now" })).toBeInTheDocument();
    expect(await screen.findByText(/2 devices/)).toBeInTheDocument();
  });

  it("keeps drives of an unlinked former LAN PC out of this device's group", async () => {
    const base = makeApi();
    const ghost = { ...(await base.listDevices())[0]!, id: "dev-ghost", displayName: "Ghost disk", indexedByHost: "host-ghost" };
    render(<App api={{ ...base, listDevices: async () => [...(await base.listDevices()), ghost] }} />);
    const other = await screen.findByRole("group", { name: "Other devices" });
    expect(within(other).getByRole("button", { name: /Ghost disk/ })).toBeInTheDocument();
    expect(within(other).getByText("Not linked · offline")).toBeInTheDocument();
    expect(within(screen.getByRole("group", { name: "DESKTOP" })).queryByRole("button", { name: /Ghost disk/ })).toBeNull();
    // no PC menu for it, and "Forget drive" stays the way to clean up
    fireEvent.contextMenu(within(other).getByRole("button", { name: /Other devices/ }));
    expect(screen.queryByRole("menuitem", { name: /Sync now/ })).toBeNull();
  });

  it("offers neither Rename nor Unlink for the hub's PCs", async () => {
    const api = createMockApi({ rowCount: 4000, latency: [0, 0], state: "connected", hub: true });
    render(<App api={api} />);
    const laptop = await screen.findByRole("group", { name: "Laptop" });
    fireEvent.contextMenu(within(laptop).getByRole("button", { name: /Laptop/ }));
    expect(await screen.findByRole("menuitem", { name: /Sync now/ })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: /Rename/ })).toBeNull();
    expect(screen.queryByRole("menuitem", { name: /Unlink/ })).toBeNull();
  });

  it("selects one of two same-named drives on different devices, not both", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    const laptop = await screen.findByRole("group", { name: "Laptop" });
    const desktop = screen.getByRole("group", { name: "DESKTOP" });
    await user.click(within(laptop).getByRole("button", { name: /System \(C:\), online/ }));
    expect(screen.getByRole("searchbox")).toHaveValue('drive:"Laptop/System (C:)" ');
    await waitFor(() =>
      expect(within(laptop).getByRole("button", { name: /System \(C:\), online/ })).toHaveAttribute("aria-pressed", "true"),
    );
    expect(within(desktop).getByRole("button", { name: /System \(C:\), online/ })).toHaveAttribute("aria-pressed", "false");
    // a name only one drive has stays short
    await user.click(within(desktop).getByRole("button", { name: /^Data \(D:\)/ }));
    expect(screen.getByRole("searchbox")).toHaveValue('drive:"Laptop/System (C:)" drive:"Data (D:)" ');
  });

  it("shows a pairing code in the Link a device dialog (sidebar button)", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    await user.click(await screen.findByRole("button", { name: "Link a device" }));
    const dialog = await screen.findByRole("dialog", { name: "Link a device" });
    expect(await within(dialog).findByText("482 913")).toBeInTheDocument();
    expect(within(dialog).getByText(/Valid for [12]:\d\d · works once/)).toBeInTheDocument();
  });

  it("links with a discovered PC by entering its code", async () => {
    const base = makeApi();
    const joinPairing = vi.fn(base.joinPairing);
    const user = userEvent.setup();
    render(<App api={{ ...base, joinPairing }} />);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    const settings = await screen.findByRole("dialog", { name: "Settings" });
    await user.click(within(settings).getByRole("button", { name: /Link a device/ }));
    const dialog = await screen.findByRole("dialog", { name: "Link a device" });
    await user.click(within(dialog).getByRole("tab", { name: "Enter a code" }));
    await user.click(await within(dialog).findByRole("button", { name: /Office-PC/ }));
    const link = within(dialog).getByRole("button", { name: "Link" });
    await user.type(within(dialog).getByLabelText("6-digit code"), "111111");
    await user.click(link);
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("wrong code");
    await user.clear(within(dialog).getByLabelText("6-digit code"));
    await user.type(within(dialog).getByLabelText("6-digit code"), "482 913");
    await user.click(link);
    await waitFor(() => expect(joinPairing).toHaveBeenLastCalledWith("192.168.1.23:47321", "482913"));
    expect(await within(dialog).findByText("Linked with Office-PC")).toBeInTheDocument();
  });

  it("calls them drives and still offers Link a device when no other device is linked", async () => {
    const base = makeApi();
    render(<App api={{ ...base, listPeers: async () => ({ ...(await base.listPeers()), peers: [] }) }} />);
    expect(await screen.findByRole("heading", { name: "Drives" })).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "Laptop" })).toBeNull();
    expect(await screen.findByRole("button", { name: "Link a device" })).toBeEnabled();
  });

  it("names the PC in remote rows and disables Open for them", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    // linked devices load separately from the drives; rows are only known as remote once they have
    await screen.findByRole("group", { name: "Laptop" });
    await user.type(await screen.findByRole("searchbox"), "pc:laptop online:");
    const grid = await screen.findByRole("grid");
    // the mock's scanning drive keeps refreshing pages: find the row and open the menu again on every try
    const menu = await waitFor(() => {
      const row = within(grid).getAllByRole("row").slice(1).find((r) => within(r).queryByText(/^Laptop › /));
      expect(row).toBeTruthy();
      fireEvent.contextMenu(row as HTMLElement);
      const m = screen.getByRole("menu");
      expect(within(m).getByRole("menuitem", { name: /^Open(?! containing)/ })).toHaveAttribute(
        "title",
        "On Laptop — not on this device",
      );
      return m;
    });
    expect(within(menu).getByRole("menuitem", { name: /^Open(?! containing)/ })).toBeDisabled();
    expect(within(menu).getByRole("menuitem", { name: /Copy path/ })).toBeEnabled();
  });

  const HUB: HubStatus = {
    name: "Home Hub",
    addr: "tls://seek-hub.example.ts.net",
    online: true,
    lastSyncMs: Date.now() - 20_000,
    lastError: null,
    pcs: [
      { hostId: "host-laptop", name: "Laptop", online: true, lastSyncMs: Date.now() - 60_000, needsRelink: false },
      { hostId: "host-office", name: "Office-PC", online: false, lastSyncMs: Date.now() - 3 * 3_600_000, needsRelink: false },
    ],
  };

  it("link hub tab validates 8-digit code", async () => {
    const base = makeApi();
    const linkHub = vi.fn(base.linkHub);
    const user = userEvent.setup();
    render(<App api={{ ...base, linkHub }} />);
    await user.click(await screen.findByRole("button", { name: "Link a device" }));
    const dialog = await screen.findByRole("dialog", { name: "Link a device" });
    await user.click(within(dialog).getByRole("tab", { name: "Hub" }));
    expect(within(dialog).getByText(/Run `seek-hub link` on the hub to get a code\./)).toBeInTheDocument();
    const addr = within(dialog).getByLabelText("Hub address");
    expect(addr).toHaveAttribute("placeholder", "tls://seek-hub.your-tailnet.ts.net");
    const code = within(dialog).getByLabelText("Code");
    const link = within(dialog).getByRole("button", { name: "Link hub" });
    await user.type(addr, "tls://seek-hub.example.ts.net");
    await user.type(code, "8765432");
    expect(link).toBeDisabled();
    await user.type(code, "1");
    expect(code).toHaveValue("8765 4321");
    expect(link).toBeEnabled();

    // direct links exist (the Laptop): the user confirms first
    await user.click(link);
    expect(within(dialog).getByText(/Linking to a hub replaces your direct links with 1 PC\./)).toBeInTheDocument();
    expect(linkHub).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Link hub" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "That code didn't match. Run `seek-hub link` again for a new code.",
    );

    await user.clear(within(dialog).getByLabelText("Code"));
    await user.type(within(dialog).getByLabelText("Code"), "12345678");
    await user.click(within(dialog).getByRole("button", { name: "Link hub" }));
    await user.click(within(dialog).getByRole("button", { name: "Link hub" }));
    await waitFor(() => expect(linkHub).toHaveBeenLastCalledWith("tls://seek-hub.example.ts.net", "12345678"));
    expect(await within(dialog).findByText("Linked with Home Hub")).toBeInTheDocument();
  });

  it("explains a hub that is closed or untrusted", async () => {
    const user = userEvent.setup();
    const fail = (msg: string) => makeApi({ listPeers: async () => ({ ...(await makeApi().listPeers()), peers: [] }), linkHub: async () => Promise.reject(msg) });
    for (const [msg, shown] of [
      ["[hub:no-window] the hub is not showing a code", "The hub isn't accepting links right now. Run `seek-hub link` on the hub."],
      ["[hub:untrusted] Hub address not trusted (x)", "The hub's certificate isn't valid for that address."],
      ["could not reach tls://x: timed out", "could not reach tls://x: timed out"],
    ] as const) {
      const { unmount } = render(<App api={fail(msg)} />);
      await user.click(await screen.findByRole("button", { name: "Link a device" }));
      const dialog = await screen.findByRole("dialog", { name: "Link a device" });
      await user.click(within(dialog).getByRole("tab", { name: "Hub" }));
      await user.type(within(dialog).getByLabelText("Hub address"), "tls://h.example");
      await user.type(within(dialog).getByLabelText("Code"), "12345678");
      await user.click(within(dialog).getByRole("button", { name: "Link hub" }));
      expect(await within(dialog).findByRole("alert")).toHaveTextContent(shown);
      unmount();
    }
  });

  it("settings shows hub card and hides LAN link", async () => {
    const unlinkHub = vi.fn(async () => {});
    const user = userEvent.setup();
    render(<App api={makeApi({ hubStatus: async () => HUB, unlinkHub })} />);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    const settings = await screen.findByRole("dialog", { name: "Settings" });
    expect(await within(settings).findByText("Hub · Home Hub")).toBeInTheDocument();
    expect(within(settings).getByText("synced just now")).toBeInTheDocument();
    expect(within(settings).getByText("Office-PC")).toBeInTheDocument();
    expect(within(settings).queryByRole("button", { name: /Link a device/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Link a device" })).toBeNull();
    await user.click(within(settings).getByRole("button", { name: "Unlink hub" }));
    expect(unlinkHub).not.toHaveBeenCalled();
    const confirm = await screen.findByRole("dialog", { name: /Unlink Home Hub\?/ });
    await user.click(within(confirm).getByRole("button", { name: "Unlink hub" }));
    await waitFor(() => expect(unlinkHub).toHaveBeenCalledOnce());
  });

  it("shows the hub's unlink notice in Settings and dismisses it", async () => {
    const unlinkHub = vi.fn(async () => {});
    const gone: HubStatus = { ...HUB, online: false, lastSyncMs: 0, lastError: "The hub unlinked this PC", pcs: [], unlinked: true };
    const user = userEvent.setup();
    render(<App api={makeApi({ hubStatus: async () => gone, unlinkHub })} />);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    const settings = await screen.findByRole("dialog", { name: "Settings" });
    expect(await within(settings).findByText("The hub unlinked this PC")).toBeInTheDocument();
    await user.click(within(settings).getByRole("button", { name: "Dismiss" }));
    await waitFor(() => expect(unlinkHub).toHaveBeenCalledOnce());
  });

  it("status bar shows hub sync", async () => {
    const seg = (text: string) => (_: string, el: Element | null) =>
      !!el?.classList.contains("status-seg") && el.textContent?.trim() === text;
    const { unmount } = render(<App api={makeApi({ hubStatus: async () => HUB })} />);
    expect(await screen.findByText(seg("Hub · synced just now"))).toBeInTheDocument();
    unmount();
    const off: HubStatus = { ...HUB, online: false, lastSyncMs: Date.now() - 3 * 3_600_000 };
    render(<App api={makeApi({ hubStatus: async () => off })} />);
    expect(await screen.findByText(seg("Hub offline · synced 3 h ago"))).toBeInTheDocument();
  });
});
