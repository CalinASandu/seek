import { createContext, useContext } from "react";
import type { SeekApi } from "./types";

export const ApiContext = createContext<SeekApi | null>(null);

export function useApi(): SeekApi {
  const api = useContext(ApiContext);
  if (!api) throw new Error("ApiContext missing");
  return api;
}
