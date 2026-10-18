import { isTauri } from "../api";

type Action = "minimize" | "toggleMaximize" | "close";

/** Window chrome actions; no-ops in a plain browser. */
export async function windowAction(action: Action): Promise<void> {
  if (!isTauri()) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  const w = getCurrentWindow();
  if (action === "minimize") await w.minimize();
  else if (action === "toggleMaximize") await w.toggleMaximize();
  else await w.close();
}
