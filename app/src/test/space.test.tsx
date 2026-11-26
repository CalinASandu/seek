import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { SeekApi } from "../api/types";

function makeApi(over: Partial<SeekApi> = {}): SeekApi {
  return { ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected" }), ...over };
}

async function openSpace(api: SeekApi = makeApi()) {
  const user = userEvent.setup();
  render(<App api={api} />);
  await user.click(await screen.findByRole("radio", { name: "Space" }));
  const list = await screen.findByRole("listbox", { name: "Contents" });
  await within(list).findByRole("option", { name: /^Users/ });
  return { user, list };
}

const path = () => screen.getByRole("navigation", { name: "Folder path" });

describe("Space view", () => {
  it("opens on the first drive and lists what fills it, largest first", async () => {
    const { list } = await openSpace();
    expect(within(path()).getByRole("button", { name: /System \(C:\)/ })).toBeInTheDocument();
    const names = within(list).getAllByRole("option").map((o) => o.getAttribute("aria-label") ?? "");
    expect(names[0]).toMatch(/^Program Files \(x86\)/);
    expect(screen.getByRole("img", { name: /System \(C:\)/ })).toBeInTheDocument();
  });

  it("opens a folder from the list and goes back up with Backspace", async () => {
    const { user, list } = await openSpace();
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    expect(await within(path()).findByRole("button", { name: "Users" })).toBeInTheDocument();
    expect(await within(list).findByRole("option", { name: /^Calin/ })).toBeInTheDocument();
    await user.keyboard("{Backspace}");
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Users" })).toBeNull());
  });

  it("opens a folder from the chart and goes up from the center", async () => {
    const { user } = await openSpace();
    const chart = screen.getByRole("img", { name: /System \(C:\)/ }).closest("svg")!;
    await user.click(chart.querySelector('[data-key="Windows"]')!);
    expect(await within(path()).findByRole("button", { name: "Windows" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Go up to System \(C:\)/ }));
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Windows" })).toBeNull());
  });

  it("highlights the slice of the row under the pointer", async () => {
    const { user, list } = await openSpace();
    await user.hover(within(list).getByRole("option", { name: /^Users/ }));
    const svg = screen.getByRole("img", { name: /System \(C:\)/ }).closest("svg")!;
    await waitFor(() => expect(svg.querySelector('[data-key="Users"]')).toHaveAttribute("data-active", "true"));
    expect(svg.querySelector('[data-key="Windows"]')).toHaveAttribute("data-dim", "true");
  });

  it("offers Show in Explorer only for this PC's drives", async () => {
    const { user, list } = await openSpace();
    fireEvent.contextMenu(within(list).getByRole("option", { name: /^Users/ }));
    expect(within(screen.getByRole("menu")).getByRole("menuitem", { name: /Show in Explorer/ })).toBeInTheDocument();
    await user.keyboard("{Escape}");

    const laptop = screen.getByRole("group", { name: "Laptop" });
    await user.click(within(laptop).getByRole("button", { name: /System \(C:\)/ }));
    // the list is replaced while the other drive loads
    const row = await waitFor(() =>
      within(screen.getByRole("listbox", { name: "Contents" })).getByRole("option", { name: /^Projects/ }),
    );
    fireEvent.contextMenu(row);
    const menu = screen.getByRole("menu");
    expect(within(menu).queryByRole("menuitem", { name: /Show in Explorer/ })).toBeNull();
    expect(within(menu).getByRole("menuitem", { name: /Copy path/ })).toBeInTheDocument();
  });

  it("searches inside a folder from its menu", async () => {
    const { user, list } = await openSpace();
    fireEvent.contextMenu(within(list).getByRole("option", { name: /^Users/ }));
    await user.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: /Search in this folder/ }));
    expect(await screen.findByRole("searchbox")).toHaveValue("");
    const pill = screen.getByRole("button", { name: "Remove filter in Users" });
    expect(pill).toHaveAttribute("title", "C:\\Users");
    // back returns to Space, in the same place
    fireEvent.mouseUp(window, { button: 3 });
    expect(await screen.findByRole("listbox", { name: "Contents" })).toBeInTheDocument();
    // once back in Space, the button walks history again; Search does nothing more
    await user.click(screen.getByRole("radio", { name: "Search" }));
    fireEvent.mouseUp(window, { button: 3 });
    expect(await screen.findByRole("searchbox")).toBeInTheDocument();
  });

  it("walks back and forward through folders with the mouse's side buttons", async () => {
    const { user, list } = await openSpace();
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    await user.dblClick(await within(list).findByRole("option", { name: /^Calin/ }));
    await within(path()).findByRole("button", { name: "Calin" });

    fireEvent.mouseUp(window, { button: 3 });
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Calin" })).toBeNull());
    expect(within(path()).getByRole("button", { name: "Users" })).toBeInTheDocument();
    fireEvent.mouseUp(window, { button: 3 });
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Users" })).toBeNull());
    fireEvent.mouseUp(window, { button: 3 }); // nothing further back
    fireEvent.mouseUp(window, { button: 4 });
    fireEvent.mouseUp(window, { button: 4 });
    expect(await within(path()).findByRole("button", { name: "Calin" })).toBeInTheDocument();
  });

  it("counts a drive change as a step and keeps the place when leaving Space", async () => {
    const { user, list } = await openSpace();
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    await within(path()).findByRole("button", { name: "Users" });
    await user.click(screen.getByRole("button", { name: /^Data \(D:\)/ }));
    await within(path()).findByRole("button", { name: /Data \(D:\)/ });
    await user.keyboard("{Alt>}{ArrowLeft}{/Alt}");
    expect(await within(path()).findByRole("button", { name: "Users" })).toBeInTheDocument();
    expect(within(path()).getByRole("button", { name: /System \(C:\)/ })).toBeInTheDocument();
    await user.keyboard("{Alt>}{ArrowRight}{/Alt}");
    await within(path()).findByRole("button", { name: /Data \(D:\)/ });
    await user.keyboard("{Alt>}{ArrowLeft}{/Alt}");

    await user.click(screen.getByRole("radio", { name: "Search" }));
    await user.click(await screen.findByRole("radio", { name: "Space" }));
    expect(await within(path()).findByRole("button", { name: "Users" })).toBeInTheDocument();
  });

  it("moves between tabs with the arrow keys without leaving the folder", async () => {
    const { user, list } = await openSpace();
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    await within(path()).findByRole("button", { name: "Users" });
    const tabs = screen.getByRole("tablist", { name: "Show" });
    await user.click(within(tabs).getByRole("tab", { name: "Largest" }));
    await user.keyboard("{ArrowRight}");
    expect(within(tabs).getByRole("tab", { name: "By type" })).toHaveAttribute("aria-selected", "true");
    await user.keyboard("{ArrowLeft}");
    expect(within(tabs).getByRole("tab", { name: "Largest" })).toHaveAttribute("aria-selected", "true");
    expect(within(path()).getByRole("button", { name: "Users" })).toBeInTheDocument();
    // Alt+← still walks history from a focused tab
    await user.keyboard("{Alt>}{ArrowLeft}{/Alt}");
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Users" })).toBeNull());
  });

  it("leaves a dialog alone: side buttons do nothing behind it", async () => {
    const { user, list } = await openSpace();
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    await within(path()).findByRole("button", { name: "Users" });
    await user.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("dialog", { name: "Settings" });
    fireEvent.mouseUp(window, { button: 3 });
    await new Promise((r) => setTimeout(r, 50));
    expect(within(path()).getByRole("button", { name: "Users" })).toBeInTheDocument();
  });

  it("drops a vanished folder from history instead of adding a step", async () => {
    const base = makeApi();
    let gone = false;
    const api = makeApi({
      spaceTree: (id, p, depth) =>
        gone && p.endsWith("Calin") ? Promise.reject(new Error("That folder no longer exists")) : base.spaceTree(id, p, depth),
    });
    const { user, list } = await openSpace(api);
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    await user.dblClick(await within(list).findByRole("option", { name: /^Calin/ }));
    await within(path()).findByRole("button", { name: "Calin" });
    gone = true;
    fireEvent.mouseUp(window, { button: 3 });
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Calin" })).toBeNull());
    fireEvent.mouseUp(window, { button: 4 }); // Calin again, which is gone: climbs back to Users
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Calin" })).toBeNull());
    expect(within(path()).getByRole("button", { name: "Users" })).toBeInTheDocument();
  });

  it("climbs to the parent when a folder no longer exists", async () => {
    const base = makeApi();
    const api = makeApi({
      spaceTree: (id, p, depth) =>
        p.endsWith("Calin") ? Promise.reject(new Error("That folder no longer exists")) : base.spaceTree(id, p, depth),
    });
    const { user, list } = await openSpace(api);
    await user.dblClick(within(list).getByRole("option", { name: /^Users/ }));
    await user.dblClick(await within(list).findByRole("option", { name: /^Calin/ }));
    await waitFor(() => expect(within(path()).queryByRole("button", { name: "Calin" })).toBeNull());
    expect(within(path()).getByRole("button", { name: "Users" })).toBeInTheDocument();
  });

  it("toggles between Search and Space with Ctrl+Shift+S", async () => {
    const user = userEvent.setup();
    render(<App api={makeApi()} />);
    await screen.findByRole("searchbox");
    await user.keyboard("{Control>}{Shift>}s{/Shift}{/Control}");
    expect(await screen.findByRole("listbox", { name: "Contents" })).toBeInTheDocument();
    await user.keyboard("{Control>}{Shift>}s{/Shift}{/Control}");
    expect(await screen.findByRole("searchbox")).toBeInTheDocument();
  });
});
