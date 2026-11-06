import { act, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import { createMockApi } from "../api/mock";
import { ApiContext } from "../api/context";
import type { Settings, SeekApi } from "../api/types";
import { useSettings } from "../hooks/useSettings";
import { SettingsPanel } from "../components/SettingsPanel";

/** An API whose saves wait until the test lets them finish (or fail). */
function gatedApi() {
  const base = createMockApi({ rowCount: 10, latency: [0, 0], state: "connected" });
  const gates: Array<{ ok: () => void; fail: (e: Error) => void }> = [];
  const setSettings = vi.fn(
    (s: Settings) =>
      new Promise<void>((resolve, reject) => gates.push({ ok: resolve, fail: reject })).then(() => base.setSettings(s)),
  );
  const api: SeekApi = { ...base, setSettings };
  return { api, base, gates, setSettings };
}

async function loaded(api: SeekApi) {
  const wrapper = ({ children }: { children: ReactNode }) => <ApiContext.Provider value={api}>{children}</ApiContext.Provider>;
  const hook = renderHook(() => useSettings(), { wrapper });
  // loaded once the saved settings arrived: a save before that would throw
  await waitFor(() => expect(hook.result.current.settings.protectedPaths).toEqual([]));
  await act(async () => undefined);
  return hook;
}

const add = (path: string) => (s: Settings) => ({ protectedPaths: [...s.protectedPaths, path] });
const remove = (path: string) => (s: Settings) => ({ protectedPaths: s.protectedPaths.filter((p) => p !== path) });

describe("useSettings save queue", () => {
  it("two adds started before the first save resolves both survive", async () => {
    const { api, gates, setSettings } = gatedApi();
    const { result } = await loaded(api);
    let a!: Promise<void>, b!: Promise<void>;
    act(() => {
      a = result.current.update(add("A"), { saveFirst: true });
      b = result.current.update(add("B"), { saveFirst: true });
    });
    await waitFor(() => expect(gates).toHaveLength(1));
    await act(async () => gates[0]!.ok());
    await waitFor(() => expect(gates).toHaveLength(2));
    await act(async () => gates[1]!.ok());
    await Promise.all([a, b]);
    expect(setSettings.mock.calls.at(-1)![0].protectedPaths).toEqual(["A", "B"]);
    expect(result.current.settings.protectedPaths).toEqual(["A", "B"]);
  });

  it("protect A then Stop protecting A while the first save is pending ends unprotected", async () => {
    const { api, gates, setSettings } = gatedApi();
    const { result } = await loaded(api);
    let a!: Promise<void>, b!: Promise<void>;
    act(() => {
      a = result.current.update(add("A"), { saveFirst: true });
      b = result.current.update(remove("A"), { saveFirst: true });
    });
    await waitFor(() => expect(gates).toHaveLength(1));
    await act(async () => gates[0]!.ok());
    await waitFor(() => expect(gates).toHaveLength(2));
    await act(async () => gates[1]!.ok());
    await Promise.all([a, b]);
    expect(setSettings.mock.calls.at(-1)![0].protectedPaths).toEqual([]);
    expect(result.current.settings.protectedPaths).toEqual([]);
  });

  it("a Settings-panel removal made while a save is pending is not lost", async () => {
    const { api, base, gates } = gatedApi();
    await base.setSettings({ ...(await base.getSettings()), protectedPaths: ["X"] });
    const { result } = await (async () => {
      const wrapper = ({ children }: { children: ReactNode }) => <ApiContext.Provider value={api}>{children}</ApiContext.Provider>;
      const h = renderHook(() => useSettings(), { wrapper });
      await waitFor(() => expect(h.result.current.settings.protectedPaths).toEqual(["X"]));
      await act(async () => undefined);
      return h;
    })();
    let a!: Promise<void>, b!: Promise<void>;
    act(() => {
      a = result.current.update(add("A"), { saveFirst: true });
      b = result.current.update(remove("X")); // optimistic, as the panel does it
    });
    await waitFor(() => expect(gates).toHaveLength(1));
    await act(async () => gates[0]!.ok());
    await waitFor(() => expect(gates).toHaveLength(2));
    await act(async () => gates[1]!.ok());
    await Promise.all([a, b]);
    expect(result.current.settings.protectedPaths).toEqual(["A"]);
    expect((await base.getSettings()).protectedPaths).toEqual(["A"]);
  });

  it("a failed save does not block the next, and the next starts from what was really saved", async () => {
    const { api, gates, setSettings } = gatedApi();
    const { result } = await loaded(api);
    let a!: Promise<void>, b!: Promise<void>;
    act(() => {
      a = result.current.update(add("A"), { saveFirst: true });
      b = result.current.update(add("B"), { saveFirst: true });
    });
    const aFailed = expect(a).rejects.toThrow("disk full");
    await waitFor(() => expect(gates).toHaveLength(1));
    await act(async () => gates[0]!.fail(new Error("disk full")));
    await aFailed;
    await waitFor(() => expect(gates).toHaveLength(2));
    await act(async () => gates[1]!.ok());
    await b;
    expect(setSettings.mock.calls.at(-1)![0].protectedPaths).toEqual(["B"]);
    expect(result.current.settings.protectedPaths).toEqual(["B"]);
  });
});

describe("Settings panel removal", () => {
  it("a Stop protecting click made while a row protect is still saving keeps both results", async () => {
    const { api, base, gates } = gatedApi();
    await base.setSettings({ ...(await base.getSettings()), protectedPaths: ["Y"] });
    const hookRef: { current?: ReturnType<typeof useSettings> } = {};
    function Harness() {
      const h = useSettings();
      hookRef.current = h;
      return (
        <SettingsPanel
          settings={h.settings}
          version={undefined}
          peers={null}
          hub={null}
          problem={null}
          onChange={(patch) => void h.update(patch).catch(() => undefined)}
          onSharing={() => undefined}
          onLinkPc={() => undefined}
          onUnlinkHub={() => undefined}
          onClose={() => undefined}
        />
      );
    }
    render(
      <ApiContext.Provider value={api}>
        <Harness />
      </ApiContext.Provider>,
    );
    await screen.findByLabelText("Stop protecting Y");
    await act(async () => undefined);
    let a!: Promise<void>;
    // the row menu protects X (saveFirst: the panel doesn't know about X until the save lands)
    act(() => {
      a = hookRef.current!.update(add("X"), { saveFirst: true });
    });
    await waitFor(() => expect(gates).toHaveLength(1));
    // meanwhile the panel's own button removes Y, from the props it has drawn so far
    fireEvent.click(screen.getByLabelText("Stop protecting Y"));
    await act(async () => gates[0]!.ok());
    await waitFor(() => expect(gates).toHaveLength(2));
    await act(async () => gates[1]!.ok());
    await a;
    expect((await base.getSettings()).protectedPaths).toEqual(["X"]);
    expect(hookRef.current!.settings.protectedPaths).toEqual(["X"]);
  });
});
