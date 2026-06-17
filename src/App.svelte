<script lang="ts">
  // U1 scaffold: a blank shell proving the build + launch path under
  // Wayland/X11. The terminal UI lands in U3.
  const version = "0.1.0";
</script>

<main>
  <h1>fly</h1>
  <p>a terminal for AI coding agents · v{version}</p>
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100vh;
    gap: 0.25rem;
    color: #c9d1d9;
    font-family:
      ui-monospace, "SFMono-Regular", "JetBrains Mono", Menlo, monospace;
  }
  h1 {
    margin: 0;
    font-size: 2.5rem;
    color: #4da3ff;
  }
  p {
    margin: 0;
    opacity: 0.6;
    font-size: 0.85rem;
  }
</style>
