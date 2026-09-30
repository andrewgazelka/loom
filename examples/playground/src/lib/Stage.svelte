<script lang="ts">
  import { onMount } from "svelte";

  // One drawing command from the handler: [kind, coordinates, colour]. Kinds: 0 line2d, 1 circle, 2 line3d, 3 triangle.
  type Cmd = [number, number[], number[]];
  let { scene = [], limit = null, drawMs = $bindable(0) }: { scene?: Cmd[]; limit?: number | null; drawMs?: number } = $props();

  let canvas: HTMLCanvasElement;
  let w = 0, h = 0, dpr = 1;
  // Orbit camera: yaw/pitch follow the pointer, with inertia; it spins by itself until touched.
  let yaw = 0.6, pitch = 0.45, dist = 3.6, vy = 0.004, vp = 0, dragging = false, touched = false;
  let last: [number, number] = [0, 0];

  const is3d = $derived(scene.some((c) => c[0] >= 2));
  const rgb = (c: number[], a = 1) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

  function resize() {
    dpr = window.devicePixelRatio || 1;
    w = canvas.clientWidth;
    h = canvas.clientHeight;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }

  function draw2d(ctx: CanvasRenderingContext2D, cmds: Cmd[]) {
    const s = Math.min(w, h) / 2.1;
    const P = (p: number[]): [number, number] => [w / 2 + p[0] * s, h / 2 - p[1] * s];
    ctx.lineWidth = 1.4;
    ctx.lineCap = "round";
    for (const [kind, c, color] of cmds) {
      ctx.strokeStyle = rgb(color);
      ctx.beginPath();
      if (kind === 0) {
        const [ax, ay] = P([c[0], c[1]]), [bx, by] = P([c[2], c[3]]);
        ctx.moveTo(ax, ay);
        ctx.lineTo(bx, by);
      } else if (kind === 1) {
        const [x, y] = P([c[0], c[1]]);
        ctx.arc(x, y, c[2] * s, 0, Math.PI * 2);
      }
      ctx.stroke();
    }
  }

  function draw3d(ctx: CanvasRenderingContext2D, cmds: Cmd[]) {
    const [cy, sy, cp, sp] = [Math.cos(yaw), Math.sin(yaw), Math.cos(pitch), Math.sin(pitch)];
    const f = Math.min(w, h) * 0.95;
    // world -> view (yaw about Y, then pitch about X), then perspective
    const V = (p: number[]) => {
      const x = p[0] * cy + p[2] * sy, z0 = -p[0] * sy + p[2] * cy;
      const y = p[1] * cp - z0 * sp, z = p[1] * sp + z0 * cp + dist;
      return { x: w / 2 + (x / z) * f, y: h / 2 - (y / z) * f, z, nx: x, ny: y };
    };
    const tris: { p: ReturnType<typeof V>[]; color: number[]; z: number; lit: number }[] = [];
    ctx.lineWidth = 1;
    const lines: { a: ReturnType<typeof V>; b: ReturnType<typeof V>; color: number[] }[] = [];
    for (const [kind, c, color] of cmds) {
      if (kind === 3) {
        const p = [V(c.slice(0, 3)), V(c.slice(3, 6)), V(c.slice(6, 9))];
        // flat shading from the view-space normal against a fixed light
        const [u, v] = [[p[1].nx - p[0].nx, p[1].ny - p[0].ny, p[1].z - p[0].z], [p[2].nx - p[0].nx, p[2].ny - p[0].ny, p[2].z - p[0].z]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        const len = Math.hypot(n[0], n[1], n[2]) || 1;
        n = n.map((c) => c / len);
        const lit = 0.38 + 0.62 * Math.abs(n[0] * -0.35 + n[1] * 0.75 + n[2] * -0.55);
        tris.push({ p, color, z: (p[0].z + p[1].z + p[2].z) / 3, lit });
      } else if (kind === 2) {
        lines.push({ a: V(c.slice(0, 3)), b: V(c.slice(3, 6)), color });
      }
    }
    tris.sort((a, b) => b.z - a.z); // painter's algorithm, far first
    for (const t of tris) {
      const c = t.color.map((x) => Math.round(x * t.lit));
      ctx.fillStyle = ctx.strokeStyle = rgb(c);
      ctx.beginPath();
      ctx.moveTo(t.p[0].x, t.p[0].y);
      ctx.lineTo(t.p[1].x, t.p[1].y);
      ctx.lineTo(t.p[2].x, t.p[2].y);
      ctx.closePath();
      ctx.fill();
      ctx.stroke(); // closes hairline seams between neighbours
    }
    for (const l of lines) {
      const k = Math.max(0, Math.min(1, 1 - (((l.a.z + l.b.z) / 2) - (dist - 1.6)) / 3.2));
      ctx.strokeStyle = rgb(l.color, 0.15 + 0.85 * k);
      ctx.beginPath();
      ctx.moveTo(l.a.x, l.a.y);
      ctx.lineTo(l.b.x, l.b.y);
      ctx.stroke();
    }
  }

  onMount(() => {
    const ctx = canvas.getContext("2d")!;
    resize();
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    let raf = 0;
    const tick = () => {
      if (!dragging) {
        yaw += vy; pitch = Math.max(-1.45, Math.min(1.45, pitch + vp));
        vp *= 0.92;
        if (touched) vy *= 0.94;
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const t0 = performance.now();
      const shown = limit === null ? (scene as Cmd[]) : (scene as Cmd[]).slice(0, limit);
      if (is3d) draw3d(ctx, shown); else draw2d(ctx, shown);
      drawMs = Math.round((drawMs * 7 + (performance.now() - t0)) / 8 * 10) / 10;
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => { cancelAnimationFrame(raf); ro.disconnect(); };
  });

  function down(e: PointerEvent) {
    if (!is3d) return;
    dragging = touched = true; vy = vp = 0;
    last = [e.clientX, e.clientY];
    canvas.setPointerCapture(e.pointerId);
  }
  function move(e: PointerEvent) {
    if (!dragging) return;
    const dx = e.clientX - last[0], dy = e.clientY - last[1];
    last = [e.clientX, e.clientY];
    yaw += dx * 0.008; pitch = Math.max(-1.45, Math.min(1.45, pitch + dy * 0.008));
    vy = dx * 0.008 * 0.6; vp = dy * 0.008 * 0.3;
  }
  const up = () => (dragging = false);
  function wheel(e: WheelEvent) {
    if (!is3d) return;
    e.preventDefault();
    dist = Math.max(1.8, Math.min(9, dist * Math.exp(e.deltaY * 0.001)));
  }
</script>

<canvas
  bind:this={canvas}
  class:grab={is3d}
  onpointerdown={down}
  onpointermove={move}
  onpointerup={up}
  onpointercancel={up}
  onwheel={wheel}
></canvas>

<style>
  canvas { display: block; width: 100%; height: 420px; touch-action: none; }
  .grab { cursor: grab; }
  .grab:active { cursor: grabbing; }
</style>
