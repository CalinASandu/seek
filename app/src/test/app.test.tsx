import { render, screen, waitFor, within, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { SeekApi } from "../api/types";

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
    const rows = await waitFor(() => {
      const r = within(grid).getAllByRole("row").slice(1);
      expect(r.length).toBeGreaterThan(0);
      return r;
    });
    const first = await waitFor(() => {
      const row = rows.find((r) => within(r).queryByText("offline"));
      expect(row).toBeTruthy();
      return row as HTMLElement;
    });
    expect(within(first).getByText("offline")).toBeInTheDocument();
    fireEvent.contextMenu(first);
    const menu = await screen.findByRole("menu");
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
    expect(within(laptop).getByRole("button", { name: /Laptop SSD \(C:\), online/ })).toBeInTheDocument();
    expect(within(laptop).getByRole("button", { name: /Travel stick, offline/ })).toBeInTheDocument();
    const desktop = screen.getByRole("group", { name: "DESKTOP" });
    expect(within(desktop).getByText("This device")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Devices" })).toBeInTheDocument();
    expect(within(desktop).queryByRole("button", { name: /Laptop SSD/ })).toBeNull();
    expect(screen.getByRole("button", { name: "Sync now" })).toBeInTheDocument();
    expect(await screen.findByText(/2 devices/)).toBeInTheDocument();
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
    await user.type(await screen.findByRole("searchbox"), "pc:laptop online:");
    const grid = await screen.findByRole("grid");
    const first = await waitFor(() => {
      const row = within(grid).getAllByRole("row").slice(1).find((r) => within(r).queryByText(/^Laptop › /));
      expect(row).toBeTruthy();
      return row as HTMLElement;
    });
    fireEvent.contextMenu(first);
    const menu = await screen.findByRole("menu");
    const open = within(menu).getByRole("menuitem", { name: /^Open(?! containing)/ });
    expect(open).toBeDisabled();
    expect(open).toHaveAttribute("title", "On Laptop — not on this device");
    expect(within(menu).getByRole("menuitem", { name: /Copy path/ })).toBeEnabled();
  });
});
