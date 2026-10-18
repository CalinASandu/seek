import { createMockApi } from "./mock";
import { createTauriApi } from "./tauri";
import type { SeekApi } from "./types";

export const isTauri = (): boolean => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let shared: SeekApi | null = null;

/** Singleton so StrictMode double-mounts don't regenerate the 300k mock rows. */
export function createApi(): SeekApi {
  shared ??= isTauri() ? createTauriApi() : createMockApi({ simulate: true });
  return shared;
}

export * from "./types";
