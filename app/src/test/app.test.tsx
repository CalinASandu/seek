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
});
