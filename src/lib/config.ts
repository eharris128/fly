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

/** Shared defaults for automation agent runs (mirrors Rust AutomationDefaults,
 * automations-workspace-and-model U3, R12/R15). Resolution at dispatch is
 * automation → this default → Claude's own default. */
export interface AutomationDefaults {
  /** Shared default launch model (alias or full id); null ⇒ Claude default. */
  model: string | null;
  /** Shared default reasoning effort; null ⇒ Claude default. */
  effort: string | null;
  /** Model handed to --fallback-model for unattended over-quota runs (R15). */
  fallbackModel: string;
}

export interface Config {
  leaderKey: string;
  attentionDebounceMs: number;
  /** Idle delay (ms) before the attention-triage nudge appears (R16). */
  nudgeIdleMs: number;
  notificationCoalesceThreshold: number;
  oscBelFallback: boolean;
  renderer: Renderer;
  scrollbackLines: number;
  fontSize: number;
  saveScrollback: boolean;
  /** Show the 🔔 notifications icon in the control bar (settings-menu toggle). */
  showNotificationsIcon: boolean;
  /** Start with global do-not-disturb on (R17). */
  notificationsMutedDefault: boolean;
  /** Sound-theme name for surfaced notifications, or null for silent (R23). */
  notificationSound: string | null;
  /** Opt-in command run per surfaced notification (R23, KTD17); null = off. */
  notificationCommand: string | null;
  /** Per-reason, per-effect notification mask (R18). */
  reasonEffects: ReasonEffectsConfig;
  /** Flag floor replayed on resume when an agent's launch argv wasn't captured
   * (R8/KTD-C); default ["--dangerously-skip-permissions"]. */
  resumeDefaultArgs: string[];
  /** Shared default model/effort + fallback for automation agent runs (U3). */
  automationDefaults: AutomationDefaults;
}

let cached: Config | null = null;

/** Load settings once and cache them for the session. */
export async function getConfig(): Promise<Config> {
  if (cached === null) {
    cached = await invoke<Config>("get_config");
  }
  return cached;
}

/**
 * Persist a settings change: merge `patch` onto the cached config, write the
 * full object through the backend (atomic), and refresh the cache with the
 * canonical stored value so {@link getConfig} stays current for the session.
 * Merging onto the cached object (the raw parsed config) rather than a
 * hand-built one preserves any backend field this frontend doesn't yet model,
 * so a save never silently resets it. Resolves to the new full config.
 */
export async function setConfig(patch: Partial<Config>): Promise<Config> {
  const base = await getConfig();
  const next = { ...base, ...patch };
  cached = await invoke<Config>("set_config", { config: next });
  return cached;
}
