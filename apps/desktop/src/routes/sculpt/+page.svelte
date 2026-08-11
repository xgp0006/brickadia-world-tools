<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { open, save } from "@tauri-apps/plugin-dialog";

  type SculptTool =
    | "raise"
    | "lower"
    | "smooth"
    | "flatten"
    | "set"
    | "stamp"
    | "paint";

  type StampKind = "cone" | "mesa" | "crater" | "ramp";
  type ZoneMode = "omit" | "include";

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
    paint?: number[];
    palette?: number[][];
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

  type LayersInfo = {
    session_id: number;
    active: number;
    grid_cols: number;
    grid_rows: number;
    layers: {
      id: number;
      name: string;
      color: number[];
      visible: boolean;
      selected_cells: number;
    }[];
  };

  type LayerPartResult = {
    layer_name: string;
    path: string;
    brick_count: number;
    installed_path?: string | null;
    install_warning?: string | null;
  };

  let session = $state<SessionInfo | null>(null);
  let tool = $state<SculptTool>("raise");
  let radius = $state(12);
  let strength = $state(3);
  let targetM = $state(10);
  let stampKind = $state<StampKind>("cone");
  let peakM = $state(40);
  let innerRatio = $state(0.4);
  let angleDeg = $state(0);
  let paintIndex = $state(1);
  let paintRes = $state(1);
  let palette = $state<number[][]>([
    [154, 163, 126, 255],
    [200, 90, 60, 255],
    [79, 138, 91, 255],
    [60, 110, 168, 255],
    [217, 194, 122, 255],
  ]);
  let zoneMode = $state<ZoneMode>("omit");
  let zoneCount = $state(0);
  let zoneDragStart: { x: number; y: number } | null = null;
  let layers = $state<LayersInfo | null>(null);
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
  let layerExportParts = $state<LayerPartResult[] | null>(null);
  let buildsDirHint = $state("");

  let canvasEl: HTMLCanvasElement | undefined = $state();
  let painting = false;
  let previewTimer: ReturnType<typeof setTimeout> | null = null;

  const heightTools: SculptTool[] = ["raise", "lower", "smooth", "flatten", "set"];
  const stampKinds: StampKind[] = ["cone", "mesa", "crater", "ramp"];

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
      if (prev.palette && prev.palette.length > 0) {
        palette = prev.palette;
      }
    } catch (e) {
      error = String(e);
    }
  }

  async function refreshLayers() {
    if (!session) return;
    try {
      layers = await invoke<LayersInfo>("sculpt_layers_info", {
        sessionId: session.session_id,
      });
      const z = await invoke<{ count: number }>("sculpt_zones_info", {
        sessionId: session.session_id,
      });
      zoneCount = z.count;
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
    const paint = prev.paint ?? [];
    const pal = prev.palette ?? palette;
    for (let i = 0; i < gray.length; i++) {
      const g = gray[i] ?? 0;
      const o = i * 4;
      const idx = paint[i] ?? 0;
      if (idx > 0 && pal[idx]) {
        const c = pal[idx];
        const t = 0.35 + 0.65 * (g / 255);
        img.data[o] = Math.round((c[0] ?? 128) * t);
        img.data[o + 1] = Math.round((c[1] ?? 128) * t);
        img.data[o + 2] = Math.round((c[2] ?? 128) * t);
        img.data[o + 3] = 255;
      } else {
        img.data[o] = g;
        img.data[o + 1] = g;
        img.data[o + 2] = g;
        img.data[o + 3] = 255;
      }
    }
    ctx.putImageData(img, 0, 0);
  }

  async function createBlank() {
    error = "";
    busy = true;
    exportResult = null;
    layerExportParts = null;
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
      const pal = await invoke<{ palette: number[][] }>("sculpt_palette", {
        sessionId: info.session_id,
      });
      palette = pal.palette;
      await refreshPreview();
      await refreshLayers();
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
    layerExportParts = null;
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
      const pal = await invoke<{ palette: number[][] }>("sculpt_palette", {
        sessionId: info.session_id,
      });
      palette = pal.palette;
      await refreshPreview();
      await refreshLayers();
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
    if (mode === "zone" || mode === "layers") return;
    const cell = canvasToCell(ev);
    if (!cell) return;
    // Strength: Raise/Lower use meters; Smooth/Flatten/Set use 0..1 blend.
    const str =
      tool === "raise" || tool === "lower"
        ? strength
        : tool === "stamp" || tool === "paint"
          ? 0
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
          stamp_kind: stampKind,
          peak_m: peakM,
          inner_ratio: innerRatio,
          angle_deg: angleDeg,
          paint_index: paintIndex,
          paint_res: paintRes,
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
    if (mode === "zone") {
      const cell = canvasToCell(ev);
      if (!cell) return;
      zoneDragStart = cell;
      painting = true;
      canvasEl?.setPointerCapture(ev.pointerId);
      return;
    }
    if (mode === "layers") {
      // Layer box pick on canvas
      void pickLayerBox(ev);
      return;
    }
    painting = true;
    canvasEl?.setPointerCapture(ev.pointerId);
    void applyAt(ev, true);
  }

  function onPointerMove(ev: PointerEvent) {
    if (!painting || !session) return;
    if (mode === "zone") return; // commit on up
    if (tool === "stamp") return; // one dab per press
    void applyAt(ev, false);
  }

  async function onPointerUp(ev: PointerEvent) {
    if (!painting) return;
    painting = false;
    try {
      canvasEl?.releasePointerCapture(ev.pointerId);
    } catch {
      /* ignore */
    }
    if (mode === "zone" && zoneDragStart && session) {
      const end = canvasToCell(ev);
      if (end) {
        try {
          const z = await invoke<{ count: number }>("sculpt_zone_add_rect", {
            request: {
              session_id: session.session_id,
              mode: zoneMode,
              x0: zoneDragStart.x,
              y0: zoneDragStart.y,
              x1: end.x,
              y1: end.y,
            },
          });
          zoneCount = z.count;
          status = `Zone ${zoneMode} added (${zoneCount} total). Applied at export.`;
        } catch (e) {
          error = String(e);
        }
      }
      zoneDragStart = null;
    }
    void refreshPreview();
  }

  async function pickLayerBox(ev: PointerEvent) {
    if (!session || !layers) return;
    const cell = canvasToCell(ev);
    if (!cell) return;
    const bi = Math.min(
      layers.grid_cols - 1,
      Math.max(0, Math.floor((cell.x / session.width) * layers.grid_cols)),
    );
    const bj = Math.min(
      layers.grid_rows - 1,
      Math.max(0, Math.floor((cell.y / session.height) * layers.grid_rows)),
    );
    try {
      layers = await invoke<LayersInfo>("sculpt_layer_paint_box", {
        request: {
          session_id: session.session_id,
          bi,
          bj,
          on: true,
          layer_index: layers.active,
        },
      });
      status = `Layer box (${bi},${bj}) selected on active layer.`;
    } catch (e) {
      error = String(e);
    }
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
      await refreshLayers();
    } catch (e) {
      error = String(e);
    }
  }

  async function clearZones() {
    if (!session) return;
    try {
      const z = await invoke<{ count: number }>("sculpt_zone_clear", {
        sessionId: session.session_id,
      });
      zoneCount = z.count;
      status = "Zones cleared.";
    } catch (e) {
      error = String(e);
    }
  }

  async function addLayer() {
    if (!session) return;
    try {
      layers = await invoke<LayersInfo>("sculpt_layer_add", {
        sessionId: session.session_id,
      });
      status = `Added ${layers.layers[layers.active]?.name ?? "layer"}.`;
    } catch (e) {
      error = String(e);
    }
  }

  async function setActiveLayer(index: number) {
    if (!session) return;
    try {
      layers = await invoke<LayersInfo>("sculpt_layer_set_active", {
        sessionId: session.session_id,
        index,
      });
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
    layerExportParts = null;
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

  async function runLayerExport() {
    if (!session) return;
    error = "";
    exporting = true;
    layerExportParts = null;
    progress = { phase: "Starting…", frac: 0 };
    status = "Exporting layers…";
    try {
      const outDir = buildsDirHint || undefined;
      const result = await invoke<{ parts: LayerPartResult[] }>("sculpt_export_layers", {
        request: {
          session_id: session.session_id,
          out_file: outDir ?? null,
          install,
          overwrite,
          micro: null,
          studs_per_meter: studsPerMeter,
          vertical_exaggeration: 1,
          cell_m: cellM,
        },
      });
      layerExportParts = result.parts;
      status = `Exported ${result.parts.length} layer part(s).`;
    } catch (e) {
      error = String(e);
      status = "Layer export failed.";
    } finally {
      exporting = false;
      progress = null;
    }
  }

  // UI mode tabs (shape tools vs stamp/paint/zone/layers)
  type Mode = "shape" | "stamp" | "paint" | "zone" | "layers";
  let mode = $state<Mode>("shape");

  function setMode(m: Mode) {
    mode = m;
    if (m === "shape" && !heightTools.includes(tool)) tool = "raise";
    if (m === "stamp") tool = "stamp";
    if (m === "paint") tool = "paint";
    // zone/layers are not stroke tools
  }

  function swatchCss(c: number[]): string {
    return `rgb(${c[0] ?? 0},${c[1] ?? 0},${c[2] ?? 0})`;
  }
</script>

<div class="page">
  <aside class="panel">
    <h1>Sculpt</h1>
    <p class="hint">
      Stamp · Paint · Zones · Layers (BWT-4.5). Height tools + greyscale/paint preview.
    </p>

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
      <h2>Mode</h2>
      <div class="tools">
        {#each ["shape", "stamp", "paint", "zone", "layers"] as m}
          <button
            type="button"
            class:active={mode === m}
            onclick={() => setMode(m as Mode)}
          >
            {m}
          </button>
        {/each}
      </div>
    </section>

    {#if mode === "shape"}
      <section>
        <h2>Brush</h2>
        <div class="tools">
          {#each heightTools as t}
            <button
              type="button"
              class:active={tool === t}
              onclick={() => (tool = t)}
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
    {:else if mode === "stamp"}
      <section>
        <h2>Stamp</h2>
        <p class="subhint">Click once — cone/mesa/crater/ramp. Drag does not smear.</p>
        <div class="tools">
          {#each stampKinds as k}
            <button
              type="button"
              class:active={stampKind === k}
              onclick={() => (stampKind = k)}
            >
              {k}
            </button>
          {/each}
        </div>
        <label class="slider">
          Radius (cells)
          <input type="range" min="1" max="80" bind:value={radius} />
          <span>{radius}</span>
        </label>
        <label class="slider">
          Peak (m)
          <input type="range" min="-80" max="120" step="0.5" bind:value={peakM} />
          <span>{peakM}</span>
        </label>
        {#if stampKind === "mesa" || stampKind === "crater"}
          <label class="slider">
            Inner ratio
            <input type="range" min="0.05" max="0.95" step="0.01" bind:value={innerRatio} />
            <span>{innerRatio.toFixed(2)}</span>
          </label>
        {/if}
        {#if stampKind === "ramp"}
          <label class="slider">
            Angle (°)
            <input type="range" min="0" max="360" step="1" bind:value={angleDeg} />
            <span>{angleDeg}</span>
          </label>
        {/if}
      </section>
    {:else if mode === "paint"}
      <section>
        <h2>Paint</h2>
        <p class="subhint">Palette splat → brick colors on export. Index 0 erases.</p>
        <div class="swatches">
          {#each palette as c, i}
            <button
              type="button"
              class="swatch"
              class:active={paintIndex === i}
              style={`background:${swatchCss(c)}`}
              title={`index ${i}`}
              onclick={() => (paintIndex = i)}
            >
              {i}
            </button>
          {/each}
        </div>
        <label class="slider">
          Radius (cells)
          <input type="range" min="1" max="80" bind:value={radius} />
          <span>{radius}</span>
        </label>
        <label class="slider">
          Block res
          <input type="range" min="1" max="16" bind:value={paintRes} />
          <span>{paintRes}</span>
        </label>
      </section>
    {:else if mode === "zone"}
      <section>
        <h2>Zones</h2>
        <p class="subhint">
          Drag a rectangle on the canvas. Omit = hole; Include = keep only inside.
          Applied at single export (keep-mask). Freehand lasso still egui-only.
        </p>
        <div class="tools">
          <button
            type="button"
            class:active={zoneMode === "omit"}
            onclick={() => (zoneMode = "omit")}>omit</button
          >
          <button
            type="button"
            class:active={zoneMode === "include"}
            onclick={() => (zoneMode = "include")}>include</button
          >
          <button type="button" disabled={!session || zoneCount === 0} onclick={clearZones}
            >clear</button
          >
        </div>
        <p class="meta">{zoneCount} zone(s)</p>
      </section>
    {:else if mode === "layers"}
      <section>
        <h2>Layers</h2>
        <p class="subhint">
          Click canvas to select grid boxes on the active layer. Export Parts writes
          one `.brdb` per non-empty layer (geometry-only; top wins claim).
        </p>
        <div class="actions">
          <button type="button" disabled={!session} onclick={addLayer}>+ Layer</button>
          <button
            type="button"
            class="primary"
            disabled={!session || busy || exporting}
            onclick={runLayerExport}
          >
            Export parts
          </button>
        </div>
        {#if layers}
          <ul class="layer-list">
            {#each layers.layers as L, i}
              <li>
                <button
                  type="button"
                  class:active={layers.active === i}
                  onclick={() => setActiveLayer(i)}
                >
                  <span class="dot" style={`background:${swatchCss(L.color)}`}></span>
                  {L.name}
                  <span class="meta">({L.selected_cells})</span>
                </button>
              </li>
            {/each}
          </ul>
          <p class="meta">Grid {layers.grid_cols}×{layers.grid_rows}</p>
        {/if}
      </section>
    {/if}

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
    {#if layerExportParts}
      <ul class="ok-list">
        {#each layerExportParts as p}
          <li class="ok">{p.layer_name}: {p.brick_count} → {p.path}</li>
        {/each}
      </ul>
    {/if}
  </aside>

  <main class="canvas-wrap">
    {#if session}
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <canvas
        bind:this={canvasEl}
        class="height-canvas"
        class:zone={mode === "zone"}
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
  .hint,
  .subhint {
    margin: 0 0 0.85rem;
    font-size: 0.8rem;
    color: #9a9690;
  }
  .subhint {
    margin-bottom: 0.5rem;
    font-size: 0.75rem;
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
  .swatches {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin: 0.4rem 0;
  }
  .swatch {
    width: 2rem;
    height: 2rem;
    padding: 0;
    font-size: 0.65rem;
    text-shadow: 0 0 2px #000;
  }
  .layer-list {
    list-style: none;
    margin: 0.5rem 0 0;
    padding: 0;
  }
  .layer-list button {
    width: 100%;
    text-align: left;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-bottom: 0.25rem;
  }
  .dot {
    width: 0.65rem;
    height: 0.65rem;
    border-radius: 50%;
    display: inline-block;
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
  .ok-list {
    list-style: none;
    padding: 0;
    margin: 0.35rem 0 0;
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
  .height-canvas.zone {
    cursor: crosshair;
  }
  .empty {
    color: #9a9690;
    font-size: 0.9rem;
  }
</style>
