// Lucide icon defaults, shared with the Local Chat app's icon style (1.75
// stroke). Call once during component init: every <Icon /> in that component's
// subtree then takes this size and stroke, so call sites carry no props.

import { setLucideProps } from "@lucide/svelte/context";

export const ICON_STROKE = 1.75;

export function useIconDefaults(size: number): void {
  setLucideProps({ size, strokeWidth: ICON_STROKE });
}
