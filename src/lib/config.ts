// Typed access to the backend config substrate (U13).
import { invoke } from "@tauri-apps/api/core";

export type Renderer = "auto" | "webgl" | "dom";

/** Which effects one attention reason may produce (mirrors Rust ReasonEffects). */
export interface ReasonEffects {
  desktop: boolean;
  sound: boolean;
  record: boolean;
}

/** Per-reason effect masks (mirrors Rust ReasonEffectsConfig). */
export interface ReasonEffectsConfig {
  question: ReasonEffects;
  permission: ReasonEffects;
  finished: ReasonEffects;
  error: ReasonEffects;
}

export interface Config {
  leaderKey: string;
  attentionDebounceMs: number;
  notificationCoalesceThreshold: number;
  oscBelFallback: boolean;
  renderer: Renderer;
  scrollbackLines: number;
  fontSize: number;
  saveScrollback: boolean;
  /** Start with global do-not-disturb on (R17). */
  notificationsMutedDefault: boolean;
  /** Sound-theme name for surfaced notifications, or null for silent (R23). */
  notificationSound: string | null;
  /** Opt-in command run per surfaced notification (R23, KTD17); null = off. */
  notificationCommand: string | null;
  /** Per-reason, per-effect notification mask (R18). */
  reasonEffects: ReasonEffectsConfig;
}

let cached: Config | null = null;

/** Load settings once and cache them for the session. */
export async function getConfig(): Promise<Config> {
  if (cached === null) {
    cached = await invoke<Config>("get_config");
  }
  return cached;
}
