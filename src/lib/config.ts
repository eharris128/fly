// Typed access to the backend config substrate (U13).
import { invoke } from "@tauri-apps/api/core";

export type Renderer = "auto" | "webgl" | "dom";

export interface Config {
  leaderKey: string;
  attentionDebounceMs: number;
  notificationCoalesceThreshold: number;
  oscBelFallback: boolean;
  renderer: Renderer;
  scrollbackLines: number;
  fontSize: number;
  saveScrollback: boolean;
}

let cached: Config | null = null;

/** Load settings once and cache them for the session. */
export async function getConfig(): Promise<Config> {
  if (cached === null) {
    cached = await invoke<Config>("get_config");
  }
  return cached;
}
