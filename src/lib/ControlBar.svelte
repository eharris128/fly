<script lang="ts">
  // The top control bar (U16). Tabs moved into the workspace sidebar, so this is
  // now a slim strip: a sidebar toggle + a `workspace / tab` breadcrumb on the
  // left, and the pane controls (split / close-pane / hotkeys) on the right.
  import LayoutDashboard from "@lucide/svelte/icons/layout-dashboard";
  import Bell from "@lucide/svelte/icons/bell";
  import VolumeX from "@lucide/svelte/icons/volume-x";
  import Volume2 from "@lucide/svelte/icons/volume-2";
  import Columns2 from "@lucide/svelte/icons/columns-2";
  import Rows2 from "@lucide/svelte/icons/rows-2";
  import X from "@lucide/svelte/icons/x";
  import Settings from "@lucide/svelte/icons/settings";
  import CircleQuestionMark from "@lucide/svelte/icons/circle-question-mark";
  import PanelLeftOpen from "@lucide/svelte/icons/panel-left-open";
  import PanelLeftClose from "@lucide/svelte/icons/panel-left-close";
  import { useIconDefaults } from "./icons";

  useIconDefaults(16);
  interface Props {
    workspaceName: string;
    tabName: string;
    sidebarCollapsed: boolean;
    muted: boolean;
    unreadTotal: number;
    showNotificationsIcon: boolean;
    /** App flavor from the shell (`fly` packaged, `fly-el` dev loop). Shown as a tag
     * when it is not the packaged one: macOS has no title bar to carry it. */
    flavor?: string;
    /** False while an overlay is open: a drag region swallows clicks even under a
     * covering element, so the bar must not be draggable beneath a backdrop. */
    dragEnabled?: boolean;
    onToggleSidebar: () => void;
    onToggleHome: () => void;
    /** The dashboard is showing: highlight its button. */
    homeOpen?: boolean;
    onSplitH: () => void;
    onSplitV: () => void;
    onClosePane: () => void;
    onToggleMute: () => void;
    onOpenNotifications: () => void;
    onOpenSettings: () => void;
    onMenu: () => void;
  }
  let {
    workspaceName,
    tabName,
    sidebarCollapsed,
    muted,
    unreadTotal,
    showNotificationsIcon,
    flavor = "fly",
    dragEnabled = true,
    homeOpen = false,
    onToggleSidebar,
    onToggleHome,
    onSplitH,
    onSplitV,
    onClosePane,
    onToggleMute,
    onOpenNotifications,
    onOpenSettings,
    onMenu,
  }: Props = $props();
</script>

<div class="controlbar" class:drag={dragEnabled}>
  <div class="left">
    <button
      class="iconbtn"
      title={sidebarCollapsed ? "show sidebar" : "hide sidebar"}
      onclick={onToggleSidebar}>{#if sidebarCollapsed}<PanelLeftOpen />{:else}<PanelLeftClose />{/if}</button
    >
    <span class="crumb">
      <span class="ws">{workspaceName}</span>
      <span class="sep">/</span>
      <span class="tab">{tabName}</span>
    </span>
    {#if flavor !== "fly"}<span class="flavor" title="app flavor">{flavor}</span>{/if}
  </div>
  <div class="controls">
    <button
      class="iconbtn"
      class:current={homeOpen}
      title="dashboard"
      aria-pressed={homeOpen}
      onclick={onToggleHome}><LayoutDashboard /></button
    >
    {#if showNotificationsIcon}
      <button
        class="iconbtn notif"
        title="notifications"
        onclick={onOpenNotifications}
      >
        <Bell />{#if unreadTotal > 0}<span class="ubadge">{unreadTotal}</span>{/if}
      </button>
    {/if}
    <button
      class="iconbtn"
      class:on={muted}
      title={muted ? "alerts muted — click to unmute" : "mute all alerts"}
      onclick={onToggleMute}>{#if muted}<VolumeX />{:else}<Volume2 />{/if}</button
    >
    <span class="divider"></span>
    <button class="iconbtn" title="split right" onclick={onSplitH}><Columns2 /></button>
    <button class="iconbtn" title="split down" onclick={onSplitV}><Rows2 /></button>
    <button class="iconbtn" title="close pane" onclick={onClosePane}><X /></button>
    <button class="iconbtn" title="settings" onclick={onOpenSettings}><Settings /></button>
    <button class="iconbtn" title="hotkeys" onclick={onMenu}><CircleQuestionMark /></button>
  </div>
</div>

<style>
  .controlbar {
    display: flex;
    align-items: stretch;
    justify-content: space-between;
    height: calc(var(--size-control) + var(--space-3));
    background: var(--color-bg-subtle);
    border-bottom: var(--border-width) solid var(--color-border);
    user-select: none;
    font-family: var(--font-sans);
    font-size: var(--font-size-sm);
    line-height: 1;
    color: var(--color-text);
    padding-left: var(--size-traffic-lights);
  }
  /* With the native title bar hidden this strip is the window's drag handle. */
  .controlbar.drag {
    -webkit-app-region: drag;
  }
  .flavor {
    margin-left: var(--space-2);
    padding: var(--space-0) var(--space-2);
    font-size: var(--font-size-xs);
    color: var(--color-text-muted);
    background: var(--color-bg);
    border: var(--border-width) solid var(--color-border);
    border-radius: var(--radius-full);
  }
  button {
    -webkit-app-region: no-drag;
  }
  .left,
  .controls {
    display: flex;
    align-items: center;
  }
  .crumb {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-0) var(--space-2);
    overflow: hidden;
    white-space: nowrap;
  }
  .crumb .ws {
    opacity: 0.6;
  }
  .crumb .sep {
    opacity: 0.35;
  }
  .crumb .tab {
    color: var(--color-text);
    font-weight: var(--font-weight-medium);
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .iconbtn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    color: var(--color-text-muted);
    cursor: pointer;
    font-size: var(--font-size-lg);
    padding: 0 var(--space-2);
    height: 100%;
  }
  .iconbtn:hover {
    color: var(--color-text);
    background: var(--color-surface);
  }
  .iconbtn.current {
    color: var(--color-accent);
  }
  .iconbtn.on {
    color: var(--color-warning);
  }
  .notif {
    position: relative;
  }
  .ubadge {
    position: absolute;
    top: var(--space-1);
    right: var(--space-0);
    min-width: var(--space-5);
    height: var(--space-5);
    padding: var(--space-0) var(--space-1);
    border-radius: var(--radius-full);
    background: var(--color-warning);
    color: var(--color-on-accent);
    font-size: var(--font-size-xs);
    font-weight: 700;
    line-height: var(--space-5);
    text-align: center;
  }
  .divider {
    width: var(--border-width);
    height: var(--space-5);
    margin: var(--space-0) var(--space-1);
    background: var(--color-border);
    opacity: 0.6;
  }
</style>
