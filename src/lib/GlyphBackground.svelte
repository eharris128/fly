<script lang="ts">
  import { onMount } from "svelte";
  import { figureIntensity, randomFigure, type Primitive } from "./sacredGeometry";

  const CELL = 9;
  const DECAY = 0.9;
  const BUILD_MS = 1400;
  const HOLD_MS = 1200;
  const GLYPHS = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789<>/{}[]=+*#%&";

  let canvas: HTMLCanvasElement;

  onMount(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let cols = 0;
    let rows = 0;
    let field = new Float32Array(0);
    let chars: string[] = [];
    let color = "79 70 229";
    let raf = 0;
    let frame = 0;

    const randomGlyph = () => GLYPHS.charAt(Math.floor(Math.random() * GLYPHS.length));

    function readColor() {
      // Resolve the accent token to "r g b" so alpha can be applied per cell.
      const probe = document.createElement("span");
      probe.style.color = getComputedStyle(canvas).getPropertyValue("--color-accent") || "#4f46e5";
      document.body.appendChild(probe);
      const m = getComputedStyle(probe).color.match(/[\d.]+/g);
      probe.remove();
      if (m && m.length >= 3) color = `${m[0]} ${m[1]} ${m[2]}`;
    }

    function resize() {
      const dpr = window.devicePixelRatio || 1;
      const { width, height } = canvas.getBoundingClientRect();
      canvas.width = Math.max(1, Math.floor(width * dpr));
      canvas.height = Math.max(1, Math.floor(height * dpr));
      ctx!.setTransform(dpr, 0, 0, dpr, 0, 0);
      cols = Math.ceil(width / CELL);
      rows = Math.ceil(height / CELL);
      field = new Float32Array(cols * rows);
      chars = Array.from({ length: cols * rows }, randomGlyph);
      ctx!.font = `${CELL - 1}px ui-monospace, SFMono-Regular, Menlo, monospace`;
      ctx!.textBaseline = "top";
    }

    type Stamp = { x: number; y: number; r: number; start: number; figure: Primitive[] };
    const stamps: Stamp[] = [];

    // Cheap soft dot for the pointer-move trail.
    function dot(x: number, y: number, radius: number, strength: number) {
      const cx = x / CELL;
      const cy = y / CELL;
      const r = radius / CELL;
      for (let j = Math.max(0, Math.floor(cy - r)); j <= Math.min(rows - 1, Math.ceil(cy + r)); j++) {
        for (let i = Math.max(0, Math.floor(cx - r)); i <= Math.min(cols - 1, Math.ceil(cx + r)); i++) {
          const v = Math.max(0, 1 - Math.hypot(i - cx, j - cy) / r) * strength;
          const k = j * cols + i;
          if (v > (field[k] ?? 0)) field[k] = v;
        }
      }
    }

    function writeStamp(s: Stamp, now: number) {
      const progress = Math.min(1, (now - s.start) / BUILD_MS);
      const reach = s.r * 1.1;
      const i0 = Math.max(0, Math.floor((s.x - reach) / CELL));
      const i1 = Math.min(cols - 1, Math.ceil((s.x + reach) / CELL));
      const j0 = Math.max(0, Math.floor((s.y - reach) / CELL));
      const j1 = Math.min(rows - 1, Math.ceil((s.y + reach) / CELL));
      for (let j = j0; j <= j1; j++) {
        for (let i = i0; i <= i1; i++) {
          const px = (i + 0.5) * CELL - s.x;
          const py = (j + 0.5) * CELL - s.y;
          const v = figureIntensity(s.figure, px, py, progress, CELL * 0.8);
          const k = j * cols + i;
          if (v > (field[k] ?? 0)) field[k] = v;
        }
      }
    }

    function onPointerDown(e: PointerEvent) {
      const rect = canvas.getBoundingClientRect();
      const r = 150 + Math.random() * 100;
      stamps.push({
        x: e.clientX - rect.left,
        y: e.clientY - rect.top,
        r,
        start: performance.now(),
        figure: randomFigure(r),
      });
      if (stamps.length > 4) stamps.shift();
    }

    let lastMove = 0;
    function onPointerMove(e: PointerEvent) {
      const now = performance.now();
      if (now - lastMove < 60) return;
      lastMove = now;
      const rect = canvas.getBoundingClientRect();
      dot(e.clientX - rect.left, e.clientY - rect.top, 24, 0.5);
    }

    function draw() {
      frame++;
      const now = performance.now();
      for (let n = stamps.length - 1; n >= 0; n--) {
        const st = stamps[n]!;
        writeStamp(st, now);
        if (now - st.start > BUILD_MS + HOLD_MS) stamps.splice(n, 1);
      }
      const width = cols * CELL;
      const height = rows * CELL;
      ctx!.clearRect(0, 0, width, height);
      const t = frame / 240;
      for (let j = 0; j < rows; j++) {
        for (let i = 0; i < cols; i++) {
          const k = j * cols + i;
          // Slow ambient drift so the grid is never fully dead.
          const ambient = Math.max(0, Math.sin(i * 0.21 + t) * Math.sin(j * 0.17 - t * 1.3) - 0.55) * 0.35;
          const level = field[k] ?? 0;
          const v = Math.max(level, ambient);
          field[k] = level * DECAY;
          if (v < 0.03) continue;
          if (Math.random() < v * 0.12) chars[k] = randomGlyph();
          ctx!.fillStyle = `rgb(${color} / ${(v * 0.55).toFixed(3)})`;
          ctx!.fillText(chars[k] ?? "", i * CELL, j * CELL);
        }
      }
      raf = requestAnimationFrame(draw);
    }

    const onVisibility = () => {
      cancelAnimationFrame(raf);
      if (!document.hidden && onScreen) raf = requestAnimationFrame(draw);
    };

    // Scoped to the host element (not window): fly's dashboard is an overlay
    // that keeps its panes mounted, so window-level listeners would stamp
    // figures from pointer activity elsewhere in the app.
    const host = canvas.parentElement ?? window;
    // The dashboard hides rather than unmounts (App's never-unmount rule), so
    // pause the loop whenever the canvas isn't actually on screen.
    let onScreen = true;
    const io = new IntersectionObserver((entries) => {
      onScreen = entries.some((e) => e.isIntersecting);
      cancelAnimationFrame(raf);
      if (onScreen && !document.hidden) raf = requestAnimationFrame(draw);
    });
    io.observe(canvas);

    readColor();
    resize();
    const observer = new ResizeObserver(resize);
    observer.observe(canvas);
    host.addEventListener("pointerdown", onPointerDown as EventListener);
    host.addEventListener("pointermove", onPointerMove as EventListener);
    document.addEventListener("visibilitychange", onVisibility);
    raf = requestAnimationFrame(draw);

    return () => {
      cancelAnimationFrame(raf);
      observer.disconnect();
      host.removeEventListener("pointerdown", onPointerDown as EventListener);
      host.removeEventListener("pointermove", onPointerMove as EventListener);
      io.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  });
</script>

<canvas bind:this={canvas} aria-hidden="true"></canvas>

<style>
  canvas {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    pointer-events: none;
  }
</style>
