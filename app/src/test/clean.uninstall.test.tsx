import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { createMockApi } from "../api/mock";
import type { SeekApi, UninstallState } from "../api/types";

function makeApi(over: Partial<SeekApi> = {}): SeekApi {
  return { ...createMockApi({ rowCount: 2000, latency: [0, 0], state: "connected" }), ...over };
}

async function openApps(api: SeekApi, advanceTimers?: (ms: number) => void) {
  const user = userEvent.setup(advanceTimers ? { advanceTimers } : undefined);
  render(<App api={api} />);
  await user.click(await screen.findByRole("radio", { name: "Decimator" }));
  await user.click(screen.getByRole("tab", { name: /^Apps/ }));
  return user;
}

async function openSheet(user: ReturnType<typeof userEvent.setup>, app: RegExp) {
  await user.click(await screen.findByRole("button", { name: app }));
  await user.click(screen.getByRole("button", { name: /^Uninstall…/ }));
}

afterEach(() => {
  vi.useRealTimers();
});

describe("the uninstall sheet", () => {
  it("the sheet offers its finished and cancel when unresolved", async () => {
    const base = makeApi();
    const uninstallLeftovers = vi.fn(base.uninstallLeftovers);
    const user = await openApps({ ...base, uninstallLeftovers });
    await openSheet(user, /^Legacy Backup Suite/);
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    const finished = await screen.findByRole("button", { name: "It's finished" });
    expect(screen.getByRole("button", { name: "Cancel" })).toBeInTheDocument();
    expect(uninstallLeftovers).not.toHaveBeenCalled();
    // "It's finished" goes on to what's left, whatever the registry said.
    await user.click(finished);
    expect(await screen.findByRole("list", { name: "Left behind" })).toBeInTheDocument();
    expect(uninstallLeftovers).toHaveBeenCalledWith("app-stubborn");
  });

  it("a running app shows the warning before it starts", async () => {
    const base = makeApi();
    const uninstallStart = vi.fn(base.uninstallStart);
    const user = await openApps({ ...base, uninstallStart });
    await openSheet(user, /^Spotify/);
    const dialog = await screen.findByRole("dialog", { name: "Uninstall Spotify?" });
    expect(dialog).toHaveTextContent(/Spotify is running/);
    expect(uninstallStart).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Uninstall" }));
    expect(uninstallStart).toHaveBeenCalledWith("app-spotify");
  });

  it("a failed start shows the message and goes back to the first step", async () => {
    const text = "Windows didn't get permission to run the uninstaller.";
    // the first question, when the sheet opens, finds nothing started; it is never asked again
    const uninstallState = vi.fn(async (): Promise<UninstallState> => {
      throw new Error("Seek hasn't started an uninstaller for that app.");
    });
    const user = await openApps({ ...makeApi(), uninstallStart: () => Promise.reject(new Error(text)), uninstallState });
    await openSheet(user, /^Audacity/);
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    const dialog = await screen.findByRole("dialog", { name: "Uninstall Audacity?" });
    await waitFor(() => expect(dialog).toHaveTextContent(text));
    expect(within(dialog).getByRole("button", { name: "Uninstall" })).toBeInTheDocument();
    expect(uninstallState).toHaveBeenCalledTimes(1);
  });

  it("asks once a second while it is open, and stops when it is closed", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let first = true;
    const uninstallState = vi.fn(async (): Promise<UninstallState> => {
      if (first) {
        first = false;
        throw new Error("Seek hasn't started an uninstaller for that app.");
      }
      return "running";
    });
    const user = await openApps({ ...makeApi(), uninstallState }, (ms) => vi.advanceTimersByTime(ms));
    await openSheet(user, /^Audacity/);
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    await waitFor(() => expect(uninstallState).toHaveBeenCalledTimes(2));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    const asked = uninstallState.mock.calls.length;
    expect(asked).toBeGreaterThanOrEqual(3);
    expect(asked).toBeLessThanOrEqual(5);
    expect(uninstallState).toHaveBeenCalledWith("app-audacity");
    // Closing the sheet stops the asking. The uninstaller itself is not stopped: there is nothing to call.
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const after = uninstallState.mock.calls.length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(uninstallState).toHaveBeenCalledTimes(after);
  });

  it("lists what is left with nothing ticked and sends the app with the review", async () => {
    const base = makeApi();
    const cleanPrepare = vi.fn(base.cleanPrepare);
    const user = await openApps({ ...base, cleanPrepare });
    await openSheet(user, /^Audacity/);
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    const left = await screen.findByRole("list", { name: "Left behind" });
    expect(within(left).queryAllByRole("checkbox", { checked: true })).toHaveLength(0);
    expect(screen.getByRole("button", { name: "Review…" })).toBeDisabled();
    await user.click(within(left).getAllByRole("checkbox")[0]!);
    await user.click(screen.getByRole("button", { name: "Review…" }));
    await waitFor(() => expect(cleanPrepare).toHaveBeenCalledTimes(1));
    const req = cleanPrepare.mock.calls[0]![0];
    expect(req).toMatchObject({ kind: "folders", why: "uninstall", appId: "app-audacity" });
  });
});

describe("the uninstall sheet, round 2", () => {
  it("a failed check is retried, and after three in a row shows the error with Try again", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    const uninstallState = vi.fn(async (): Promise<UninstallState> => {
      calls++;
      // Not started yet for the resume check, then three failures.
      if (calls === 1) throw new Error("Seek hasn't started an uninstaller for that app.");
      if (calls <= 4) throw new Error("The service is busy.");
      return "running";
    });
    const user = await openApps({ ...makeApi(), uninstallState }, (ms) => vi.advanceTimersByTime(ms));
    await openSheet(user, /^Audacity/);
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    expect(screen.queryByRole("button", { name: "It's finished" })).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4000);
    });
    const dialog = await screen.findByRole("dialog");
    await waitFor(() => expect(dialog).toHaveTextContent("The service is busy."));
    expect(screen.queryByRole("button", { name: "It's finished" })).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(calls).toBeGreaterThanOrEqual(5));
    await waitFor(() => expect(within(screen.getByRole("dialog")).queryByRole("button", { name: "Try again" })).toBeNull());
  });

  it("opens at the right step for an uninstall already in progress", async () => {
    const running = vi.fn(async (): Promise<UninstallState> => "running");
    const uninstallStart = vi.fn(async () => {});
    const user = await openApps({ ...makeApi(), uninstallState: running, uninstallStart });
    await openSheet(user, /^Audacity/);
    expect(await screen.findByText(/Windows is running the uninstaller/)).toBeInTheDocument();
    expect(uninstallStart).not.toHaveBeenCalled();
    await user.keyboard("{Escape}");
  });

  it("opens at what's left when the uninstall already finished", async () => {
    const user = await openApps({ ...makeApi(), uninstallState: async () => "finished" });
    await openSheet(user, /^Audacity/);
    expect(await screen.findByRole("dialog", { name: "Audacity: what's left" })).toBeInTheDocument();
  });

  it("shows why what's left can't be listed yet, with Look again", async () => {
    const base = makeApi();
    const text = "Windows still lists this app as installed. Seek lists what's left once it's gone.";
    let n = 0;
    const uninstallLeftovers = vi.fn(async (id: string) => {
      if (n++ === 0) throw new Error(text);
      return base.uninstallLeftovers(id);
    });
    const user = await openApps({ ...base, uninstallLeftovers });
    await openSheet(user, /^Legacy Backup Suite/);
    await user.click(await screen.findByRole("button", { name: "Uninstall" }));
    await user.click(await screen.findByRole("button", { name: "It's finished" }));
    const dialog = await screen.findByRole("dialog");
    await waitFor(() => expect(dialog).toHaveTextContent(text));
    await user.click(within(dialog).getByRole("button", { name: "Look again" }));
    expect(await screen.findByRole("list", { name: "Left behind" })).toBeInTheDocument();
    expect(uninstallLeftovers).toHaveBeenCalledTimes(2);
  });
});
