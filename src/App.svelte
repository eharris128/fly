<script lang="ts">
  // U3: a single full-window terminal pane. Tabs + splits arrive in U5.
  import Terminal from "./lib/Terminal.svelte";
</script>

<main>
  <Terminal />
</main>

<style>
  main {
    width: 100vw;
    height: 100vh;
    overflow: hidden;
  }
</style>
