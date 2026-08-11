<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { open, save } from "@tauri-apps/plugin-dialog";

  type SculptTool = "raise" | "lower" | "smooth" | "flatten" | "set";

  type SessionInfo = {
    session_id: number;
    width: number;
    height: number;
    min_m: number;
    max_m: number;
    cell_m: number;
    studs_per_meter: number;
    vertical_exaggeration: number;
    micro: boolean;
    source_name: string;
  };

  type Preview = {
    width: number;
    height: number;
    min_m: number;
    max_m: number;
    gray: number[];
  };

  type Progress = { phase: string; frac: number };

  type ExportResult = {
    path: string;
    installed_path?: string | null;
    install_warning?: string | null;
    brick_count: number;
    dem_width: number;
    dem_height: number;
    elevation_min_m: number;
    elevation_max_m: number;
  };

  let session = $state<SessionInfo | null>(null);
  let tool = $state<SculptTool>("raise");
  let radius = $state(12);
  let strength = $state(3);
  let targetM = $state(10);
  let blankW = $state(128);
  let blankH = $state(128);
  let cellM = $state(1);
  let studsPerMeter = $state(4);
  let install = $state(true);
  let overwrite = $state(false);
  let status = $state("Create blank field or load a PNG heightmap.");
  let error = $state("");
  let busy = $state(false);
  let exporting = $state(false);
  let progress = $state<Progress | null>(null);
  let exportResult = $state<ExportResult | null>(null);
  let buildsDirHint = $state("");

  let canvasEl: HTMLCanvasElement | undefined = $state();
  let painting = false;
  let previewTimer: ReturnType<typeof setTimeout> | null = null;

  onMount(() => {
    invoke<string>("builds_dir")
      .then((d) => (buildsDirHint = d))
      .catch(() => (buildsDirHint = ""));

    let unlisten: (() => void) | undefined;
    listen<Progress>("sculpt:progress", (e) => {
      progress = e.payload;
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      unlisten?.();
      if (session) {
        void invoke("sculpt_close", { sessionId: session.session_id }).catch(() => {});
      }
    };
  });

  function schedulePreview() {
    if (previewTimer) clearTimeout(previewTimer);
    previewTimer = setTimeout(() => {
      void refreshPreview();
    }, 40);
  }

  async function refreshPreview() {
    if (!session || !canvasEl) return;
    try {
      const prev = await invoke<Preview>("sculpt_preview", {
        sessionId: session.session_id,
      });
      drawPreview(prev);
      session = {
        ...session,
        min_m: prev.min_m,
        max_m: prev.max_m,
      };
    } catch (e) {
      error = String(e);
    }
  }

  function drawPreview(prev: Preview) {
    if (!canvasEl) return;
    const ctx = canvasEl.getContext("2d");
    if (!ctx) return;
    const { width, height, gray } = prev;
    canvasEl.width = width;
    canvasEl.height = height;
    const img = ctx.createImageData(width, height);
    for (let i = 0; i < gray.length; i++) {
      const g = gray[i] ?? 0;
      const o = i * 4;
      img.data[o] = g;
      img.data[o + 1] = g;
      img.data[o + 2] = g;
      img.data[o + 3] = 255;
    }
    ctx.putImageData(img, 0, 0);
  }

  async function createBlank() {
    error = "";
    busy = true;
    exportResult = null;
    try {
      if (session) {
        await invoke("sculpt_close", { sessionId: session.session_id }).catch(() => {});
      }
      const info = await invoke<SessionInfo>("sculpt_create_blank", {
        request: {
          width: blankW,
          height: blankH,
          cell_m: cellM,
          studs_per_meter: studsPerMeter,
          vertical_exaggeration: 1,
          micro: false,
          source_name: "sculpt",
        },
      });
      session = info;
      status = `Blank ${info.width}×${info.height} ready.`;
      await refreshPreview();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function loadPng() {
    error = "";
    const path = await open({
      multiple: false,
      filters: [{ name: "Heightmap", extensions: ["png", "jpg", "jpeg"] }],
    });
    if (typeof path !== "string") return;
    busy = true;
    exportResult = null;
    try {
      if (session) {
        await invoke("sculpt_close", { sessionId: session.session_id }).catch(() => {});
      }
      const info = await invoke<SessionInfo>("sculpt_load_png", {
        request: {
          path,
          cell_m: cellM,
          studs_per_meter: studsPerMeter,
          vertical_exaggeration: 1,
          micro: false,
          source_name: null,
        },
      });
      session = info;
      status = `Loaded ${info.source_name} (${info.width}×${info.height}).`;
      await refreshPreview();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function canvasToCell(ev: PointerEvent): { x: number; y: number } | null {
    if (!canvasEl || !session) return null;
    const rect = canvasEl.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    const nx = (ev.clientX - rect.left) / rect.width;
    const ny = (ev.clientY - rect.top) / rect.height;
    return {
      x: nx * session.width,
      y: ny * session.height,
    };
  }

  async function applyAt(ev: PointerEvent, begin: boolean) {
    if (!session || busy || exporting) return;
    const cell = canvasToCell(ev);
    if (!cell) return;
    // Strength: Raise/Lower use meters; Smooth/Flatten/Set use 0..1 blend.
    const str =
      tool === "raise" || tool === "lower"
        ? strength
        : Math.min(1, Math.max(0.05, strength / 10));
    try {
      const info = await invoke<SessionInfo>("sculpt_apply_stroke", {
        request: {
          session_id: session.session_id,
          tool,
          center_x: cell.x,
          center_y: cell.y,
          radius_cells: radius,
          strength: str,
          target_m: targetM,
          begin_stroke: begin,
        },
      });
      session = info;
      schedulePreview();
    } catch (e) {
      error = String(e);
    }
  }

  function onPointerDown(ev: PointerEvent) {
    if (!session || ev.button !== 0) return;
    painting = true;
    canvasEl?.setPointerCapture(ev.pointerId);
    void applyAt(ev, true);
  }

  function onPointerMove(ev: PointerEvent) {
    if (!painting || !session) return;
    void applyAt(ev, false);
  }

  function onPointerUp(ev: PointerEvent) {
    if (!painting) return;
    painting = false;
    try {
      canvasEl?.releasePointerCapture(ev.pointerId);
    } catch {
      /* ignore */
    }
    void refreshPreview();
  }

  async function undo() {
    if (!session) return;
    error = "";
    try {
      session = await invoke<SessionInfo>("sculpt_undo", {
        sessionId: session.session_id,
      });
      status = "Undid last stroke.";
      await refreshPreview();
    } catch (e) {
      error = String(e);
    }
  }

  async function runExport() {
    if (!session) {
      status = "No field loaded.";
      return;
    }
    error = "";
    exporting = true;
    exportResult = null;
    progress = { phase: "Starting…", frac: 0 };
    status = "Exporting…";
    try {
      const defaultPath = buildsDirHint
        ? `${buildsDirHint}/${session.source_name || "sculpt"}.brdb`
        : `${session.source_name || "sculpt"}.brdb`;
      const out = await save({
        filters: [{ name: "Brickadia world", extensions: ["brdb"] }],
        defaultPath,
      });
      if (typeof out !== "string") {
        status = "Export cancelled.";
        exporting = false;
        progress = null;
        return;
      }
      const result = await invoke<ExportResult>("sculpt_export", {
        request: {
          session_id: session.session_id,
          out_file: out,
          install,
          overwrite,
          micro: null,
          studs_per_meter: studsPerMeter,
          vertical_exaggeration: 1,
          cell_m: cellM,
        },
      });
      exportResult = result;
      status = `Wrote ${result.path} (${result.brick_count} bricks).`;
      if (result.install_warning) {
        status += ` Install: ${result.install_warning}`;
      } else if (result.installed_path) {
        status += ` Installed → ${result.installed_path}`;
      }
    } catch (e) {
      error = String(e);
      status = "Export failed.";
    } finally {
      exporting = false;
      progress = null;
    }
  }
</script>

<div class="page">
  <aside class="panel">
    <h1>Sculpt</h1>
    <p class="hint">MVP: Raise / Lower / Smooth (+ Flatten, Set). Greyscale height preview.</p>

    <section>
      <h2>Field</h2>
      <div class="row">
        <label>W <input type="number" min="8" max="1024" bind:value={blankW} /></label>
        <label>H <input type="number" min="8" max="1024" bind:value={blankH} /></label>
      </div>
      <div class="row">
        <label>m/cell <input type="number" min="0.1" step="0.1" bind:value={cellM} /></label>
        <label>studs/m <input type="number" min="0.5" step="0.5" bind:value={studsPerMeter} /></label>
      </div>
      <div class="actions">
        <button type="button" disabled={busy || exporting} onclick={createBlank}>Blank</button>
        <button type="button" disabled={busy || exporting} onclick={loadPng}>Load PNG</button>
        <button type="button" disabled={!session || busy || exporting} onclick={undo}>Undo</button>
      </div>
    </section>

    <section>
      <h2>Brush</h2>
      <div class="tools">
        {#each ["raise", "lower", "smooth", "flatten", "set"] as t}
          <button
            type="button"
            class:active={tool === t}
            onclick={() => (tool = t as SculptTool)}
          >
            {t}
          </button>
        {/each}
      </div>
      <label class="slider">
        Radius (cells)
        <input type="range" min="1" max="80" bind:value={radius} />
        <span>{radius}</span>
      </label>
      <label class="slider">
        Strength
        <input type="range" min="0.1" max="20" step="0.1" bind:value={strength} />
        <span>{strength}</span>
      </label>
      {#if tool === "flatten" || tool === "set"}
        <label class="slider">
          Target (m)
          <input type="range" min="0" max="200" step="0.5" bind:value={targetM} />
          <span>{targetM}</span>
        </label>
      {/if}
    </section>

    <section>
      <h2>Export</h2>
      <label class="check">
        <input type="checkbox" bind:checked={install} /> Install to Worlds
      </label>
      <label class="check">
        <input type="checkbox" bind:checked={overwrite} /> Overwrite install
      </label>
      <button
        type="button"
        class="primary"
        disabled={!session || busy || exporting}
        onclick={runExport}
      >
        {exporting ? "Exporting…" : "Export .brdb"}
      </button>
      {#if progress}
        <div class="progress">
          <div class="bar" style={`width: ${Math.round(progress.frac * 100)}%`}></div>
          <span>{progress.phase}</span>
        </div>
      {/if}
    </section>

    {#if session}
      <p class="meta">
        {session.width}×{session.height} · {session.min_m.toFixed(1)}–{session.max_m.toFixed(1)} m
        · id {session.session_id}
      </p>
    {/if}
    <p class="status">{status}</p>
    {#if error}
      <p class="err">{error}</p>
    {/if}
    {#if exportResult}
      <p class="ok">
        {exportResult.brick_count} bricks · {exportResult.path}
      </p>
    {/if}
  </aside>

  <main class="canvas-wrap">
    {#if session}
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <canvas
        bind:this={canvasEl}
        class="height-canvas"
        onpointerdown={onPointerDown}
        onpointermove={onPointerMove}
        onpointerup={onPointerUp}
        onpointercancel={onPointerUp}
        onpointerleave={onPointerUp}
      ></canvas>
    {:else}
      <div class="empty">No field — Blank or Load PNG.</div>
    {/if}
  </main>
</div>

<style>
  .page {
    display: grid;
    grid-template-columns: minmax(260px, 320px) 1fr;
    min-height: 0;
    flex: 1;
    height: 100%;
  }
  .panel {
    overflow: auto;
    padding: 0.85rem 1rem 1.25rem;
    border-right: 1px solid #2c3140;
    background: #0e1016;
  }
  h1 {
    margin: 0 0 0.25rem;
    font-size: 1.1rem;
  }
  h2 {
    margin: 0 0 0.45rem;
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #9a9690;
  }
  .hint {
    margin: 0 0 0.85rem;
    font-size: 0.8rem;
    color: #9a9690;
  }
  section {
    margin-bottom: 1rem;
    padding-bottom: 0.85rem;
    border-bottom: 1px solid #1e2230;
  }
  .row {
    display: flex;
    gap: 0.5rem;
    margin-bottom: 0.4rem;
  }
  .row label {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    font-size: 0.75rem;
    color: #9a9690;
  }
  input[type="number"] {
    width: 100%;
    box-sizing: border-box;
    background: #1a1d26;
    border: 1px solid #2c3140;
    color: #e8e6e3;
    border-radius: 4px;
    padding: 0.3rem 0.4rem;
  }
  .actions,
  .tools {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin-top: 0.45rem;
  }
  button {
    background: #1a1d26;
    border: 1px solid #2c3140;
    color: #e8e6e3;
    border-radius: 6px;
    padding: 0.35rem 0.6rem;
    font-size: 0.8rem;
    font-weight: 600;
    cursor: pointer;
  }
  button:hover:not(:disabled) {
    background: #242836;
  }
  button:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  button.active {
    background: #3d4a6b;
    border-color: #5a6a9a;
  }
  button.primary {
    width: 100%;
    margin-top: 0.5rem;
    background: #2a4a3a;
    border-color: #3d6b52;
  }
  button.primary:hover:not(:disabled) {
    background: #355c48;
  }
  .slider {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 0.25rem 0.5rem;
    align-items: center;
    font-size: 0.78rem;
    color: #9a9690;
    margin-top: 0.45rem;
  }
  .slider input[type="range"] {
    grid-column: 1 / -1;
    width: 100%;
  }
  .check {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.82rem;
    margin: 0.25rem 0;
  }
  .meta,
  .status,
  .err,
  .ok {
    font-size: 0.78rem;
    margin: 0.35rem 0 0;
    word-break: break-all;
  }
  .meta {
    color: #9a9690;
  }
  .err {
    color: #e88;
  }
  .ok {
    color: #8c8;
  }
  .progress {
    position: relative;
    margin-top: 0.5rem;
    height: 1.4rem;
    background: #1a1d26;
    border-radius: 4px;
    overflow: hidden;
    font-size: 0.7rem;
  }
  .progress .bar {
    position: absolute;
    inset: 0 auto 0 0;
    background: #3d6b52;
    max-width: 100%;
  }
  .progress span {
    position: relative;
    z-index: 1;
    padding: 0.2rem 0.4rem;
    display: block;
  }
  .canvas-wrap {
    min-width: 0;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: #0a0c10;
    padding: 1rem;
  }
  .height-canvas {
    max-width: 100%;
    max-height: 100%;
    width: auto;
    height: auto;
    image-rendering: pixelated;
    cursor: crosshair;
    border: 1px solid #2c3140;
    touch-action: none;
  }
  .empty {
    color: #9a9690;
    font-size: 0.9rem;
  }
</style>
