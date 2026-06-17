// Session serialization (U12). The backend stores this blob opaquely; the
// frontend owns the layout tree, so rehydration happens here.
import { invoke } from "@tauri-apps/api/core";
import type { Node } from "./layout";

export interface SavedPane {
  cwd: string | null;
  title: string | null;
}
export interface SavedTab {
  tree: Node;
  panes: Record<string, SavedPane>;
}
export interface SavedSession {
  tabs: SavedTab[];
  activeIndex: number;
}

export function saveSession(session: SavedSession): Promise<void> {
  return invoke("save_session", { layout: session });
}

export async function loadSession(): Promise<SavedSession | null> {
  const v = await invoke<SavedSession | null>("load_session");
  // Defensive: a malformed blob restores as a default workspace (R14).
  return v && Array.isArray(v.tabs) ? v : null;
}
