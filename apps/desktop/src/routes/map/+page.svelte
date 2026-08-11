<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import maplibregl from "maplibre-gl";
  import "maplibre-gl/dist/maplibre-gl.css";

  type DemSource =
    | "aws_terrarium"
    | "mapbox_terrain_rgb"
    | "open_topography"
    | "usgs_3dep";

  type DemPredictResult = {
    cell_m: number;
    cell_m_eff: number;
    approx_cells: number;
    zoom: number | null;
    zoom_cap: number | null;
    notes: string;
  };

  /** Default: small Boulder CO box (matches dem_predict tests spirit). */
  let north = $state(40.05);
  let south = $state(39.95);
  let east = $state(-105.2);
  let west = $state(-105.35);
  let demSource = $state<DemSource>("aws_terrarium");
  let densityFactor = $state(1);

  let predict = $state<DemPredictResult | null>(null);
  let predictError = $state("");
  let predicting = $state(false);
  let drawMode = $state(false);
  let mapReady = $state(false);

  let mapEl: HTMLDivElement | undefined = $state();
  let map: maplibregl.Map | null = null;
  let dragStart: maplibregl.LngLat | null = null;
  let drawing = false;

  const BBOX_SRC = "bbox-src";
  const BBOX_FILL = "bbox-fill";
  const BBOX_LINE = "bbox-line";

  function clampBbox() {
    // Ensure north > south, east >= west after field edits
    if (north <= south) {
      const mid = (north + south) / 2;
      north = mid + 0.001;
      south = mid - 0.001;
    }
    if (east < west) {
      const t = east;
      east = west;
      west = t;
    }
    const d = Number(densityFactor);
    densityFactor = Number.isFinite(d) ? Math.min(8, Math.max(1, Math.round(d))) : 1;
  }

  function bboxFeature(
    n: number,
    s: number,
    e: number,
    w: number,
  ): {
    type: "Feature";
    properties: Record<string, never>;
    geometry: {
      type: "Polygon";
      coordinates: number[][][];
    };
  } {
    return {
      type: "Feature",
      properties: {},
      geometry: {
        type: "Polygon",
        coordinates: [
          [
            [w, s],
            [e, s],
            [e, n],
            [w, n],
            [w, s],
          ],
        ],
      },
    };
  }

  function setBboxOnMap(n: number, s: number, e: number, w: number) {
    if (!map || !map.getSource(BBOX_SRC)) return;
    const src = map.getSource(BBOX_SRC) as maplibregl.GeoJSONSource;
    src.setData({
      type: "FeatureCollection",
      features: [bboxFeature(n, s, e, w)],
    });
  }

  function applyBboxFromLngLats(a: maplibregl.LngLat, b: maplibregl.LngLat) {
    north = Math.max(a.lat, b.lat);
    south = Math.min(a.lat, b.lat);
    east = Math.max(a.lng, b.lng);
    west = Math.min(a.lng, b.lng);
    // Degenerate drag → tiny pad
    if (north - south < 1e-6) {
      north += 0.001;
      south -= 0.001;
    }
    if (east - west < 1e-6) {
      east += 0.001;
      west -= 0.001;
    }
    setBboxOnMap(north, south, east, west);
  }

  async function runPredict() {
    clampBbox();
    predicting = true;
    predictError = "";
    try {
      const result = await invoke<DemPredictResult>("dem_predict", {
        request: {
          north,
          south,
          east,
          west,
          dem_source: demSource,
          density_factor: densityFactor,
        },
      });
      predict = result;
    } catch (e) {
      predict = null;
      predictError = String(e);
    } finally {
      predicting = false;
    }
  }

  // Live predict on bbox / source / density change (debounced)
  $effect(() => {
    const _n = north;
    const _s = south;
    const _e = east;
    const _w = west;
    const _src = demSource;
    const _d = densityFactor;
    void _n;
    void _s;
    void _e;
    void _w;
    void _src;
    void _d;
    if (!mapReady) return;
    const t = setTimeout(() => {
      setBboxOnMap(north, south, east, west);
      void runPredict();
    }, 180);
    return () => clearTimeout(t);
  });

  onMount(() => {
    if (!mapEl) return;

    map = new maplibregl.Map({
      container: mapEl,
      style: {
        version: 8,
        sources: {
          carto: {
            type: "raster",
            tiles: [
              "https://basemaps.cartocdn.com/dark_all/{z}/{x}/{y}@2x.png",
            ],
            tileSize: 256,
            attribution:
              '&copy; <a href="https://www.openstreetmap.org/copyright">OSM</a> &copy; <a href="https://carto.com/">CARTO</a>',
          },
        },
        layers: [
          {
            id: "carto",
            type: "raster",
            source: "carto",
            minzoom: 0,
            maxzoom: 20,
          },
        ],
      },
      center: [(west + east) / 2, (north + south) / 2],
      zoom: 10,
      attributionControl: { compact: true },
    });

    map.addControl(new maplibregl.NavigationControl({ showCompass: false }), "top-left");
    map.addControl(new maplibregl.ScaleControl({ unit: "metric" }), "bottom-left");

    map.on("load", () => {
      if (!map) return;
      map.addSource(BBOX_SRC, {
        type: "geojson",
        data: {
          type: "FeatureCollection",
          features: [bboxFeature(north, south, east, west)],
        },
      });
      map.addLayer({
        id: BBOX_FILL,
        type: "fill",
        source: BBOX_SRC,
        paint: {
          "fill-color": "#3d7ea6",
          "fill-opacity": 0.22,
        },
      });
      map.addLayer({
        id: BBOX_LINE,
        type: "line",
        source: BBOX_SRC,
        paint: {
          "line-color": "#5eb0e0",
          "line-width": 2,
        },
      });
      map.fitBounds(
        [
          [west, south],
          [east, north],
        ],
        { padding: 48, maxZoom: 12 },
      );
      mapReady = true;
    });

    const canvas = () => map?.getCanvas();

    map.on("mousedown", (e) => {
      if (!drawMode || e.originalEvent.button !== 0) return;
      e.preventDefault();
      drawing = true;
      dragStart = e.lngLat;
      map!.dragPan.disable();
      const c = canvas();
      if (c) c.style.cursor = "crosshair";
    });

    map.on("mousemove", (e) => {
      if (!drawing || !dragStart) return;
      applyBboxFromLngLats(dragStart, e.lngLat);
    });

    const endDraw = (e: maplibregl.MapMouseEvent) => {
      if (!drawing || !dragStart) return;
      applyBboxFromLngLats(dragStart, e.lngLat);
      drawing = false;
      dragStart = null;
      map?.dragPan.enable();
      const c = canvas();
      if (c) c.style.cursor = drawMode ? "crosshair" : "";
    };

    map.on("mouseup", endDraw);
    map.on("mouseleave", () => {
      if (!drawing) return;
      drawing = false;
      dragStart = null;
      map?.dragPan.enable();
    });

    return () => {
      map?.remove();
      map = null;
    };
  });

  $effect(() => {
    const c = map?.getCanvas();
    if (c) c.style.cursor = drawMode ? "crosshair" : "";
  });

  function onFieldBlur() {
    clampBbox();
    setBboxOnMap(north, south, east, west);
    if (map) {
      map.fitBounds(
        [
          [west, south],
          [east, north],
        ],
        { padding: 48, maxZoom: 14 },
      );
    }
  }

  function fmt(n: number, digits = 2): string {
    if (!Number.isFinite(n)) return "—";
    return n.toLocaleString(undefined, {
      maximumFractionDigits: digits,
      minimumFractionDigits: 0,
    });
  }
</script>

<div class="map-page">
  <div class="map-pane">
    <div class="map-host" bind:this={mapEl}></div>
    <div class="map-hint">
      {#if drawMode}
        Drag on the map to set the bbox · click Draw again to exit
      {:else}
        Click <strong>Draw box</strong> then drag a rectangle
      {/if}
    </div>
  </div>

  <aside class="panel">
    <h2>DEM region</h2>
    <p class="hint">Single-box predict (no fetch yet). Phase 3 shell.</p>

    <div class="actions">
      <button
        type="button"
        class="sec"
        class:on={drawMode}
        onclick={() => (drawMode = !drawMode)}
      >
        {drawMode ? "Drawing…" : "Draw box"}
      </button>
    </div>

    <div class="grid2">
      <label>
        North
        <input
          type="number"
          step="any"
          bind:value={north}
          onblur={onFieldBlur}
        />
      </label>
      <label>
        South
        <input
          type="number"
          step="any"
          bind:value={south}
          onblur={onFieldBlur}
        />
      </label>
      <label>
        East
        <input type="number" step="any" bind:value={east} onblur={onFieldBlur} />
      </label>
      <label>
        West
        <input type="number" step="any" bind:value={west} onblur={onFieldBlur} />
      </label>
    </div>

    <label>
      DEM source
      <select bind:value={demSource}>
        <option value="aws_terrarium">AWS Terrarium</option>
        <option value="mapbox_terrain_rgb">Mapbox Terrain-RGB</option>
        <option value="open_topography">OpenTopography</option>
        <option value="usgs_3dep">USGS 3DEP</option>
      </select>
    </label>

    <label>
      Downsample density (1–8)
      <input type="number" min="1" max="8" step="1" bind:value={densityFactor} />
    </label>

    <section class="predict card-inner">
      <h3>
        Predict
        {#if predicting}<span class="muted"> · …</span>{/if}
      </h3>
      {#if predictError}
        <p class="err">{predictError}</p>
      {:else if predict}
        <dl>
          <div><dt>cell_m</dt><dd>{fmt(predict.cell_m, 3)} m</dd></div>
          <div><dt>cell_m_eff</dt><dd>{fmt(predict.cell_m_eff, 3)} m</dd></div>
          <div>
            <dt>approx_cells</dt>
            <dd>{predict.approx_cells.toLocaleString()}</dd>
          </div>
          <div>
            <dt>zoom</dt>
            <dd>
              {predict.zoom ?? "—"}
              {#if predict.zoom_cap != null}
                <span class="muted"> / cap {predict.zoom_cap}</span>
              {/if}
            </dd>
          </div>
          <div class="notes">
            <dt>notes</dt>
            <dd>{predict.notes}</dd>
          </div>
        </dl>
      {:else}
        <p class="muted">Set a bbox to predict.</p>
      {/if}
    </section>

    <button
      type="button"
      class="build"
      disabled
      title="Phase 3 dem_fetch_build — coming next"
    >
      Build world
    </button>
    <p class="build-tip">Phase 3 dem_fetch_build — coming next</p>
  </aside>
</div>

<style>
  .map-page {
    display: grid;
    grid-template-columns: 1fr min(20rem, 38vw);
    flex: 1;
    min-height: 0;
    height: 100%;
  }
  .map-pane {
    position: relative;
    min-width: 0;
    min-height: 0;
  }
  .map-host {
    position: absolute;
    inset: 0;
  }
  .map-hint {
    position: absolute;
    left: 50%;
    bottom: 0.75rem;
    transform: translateX(-50%);
    z-index: 2;
    padding: 0.35rem 0.7rem;
    border-radius: 6px;
    background: rgba(14, 16, 22, 0.88);
    border: 1px solid #2c3140;
    font-size: 0.75rem;
    color: #b8b4ae;
    pointer-events: none;
    white-space: nowrap;
  }
  .panel {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
    padding: 1rem 1rem 1.25rem;
    background: #1a1d26;
    border-left: 1px solid #2c3140;
    overflow: auto;
  }
  h2 {
    margin: 0;
    font-size: 1.1rem;
  }
  h3 {
    margin: 0 0 0.45rem;
    font-size: 0.9rem;
  }
  .hint {
    margin: 0;
    color: #9a9690;
    font-size: 0.8rem;
  }
  .actions {
    display: flex;
    gap: 0.45rem;
  }
  .grid2 {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.5rem;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.75rem;
    color: #b8b4ae;
  }
  input,
  select {
    padding: 0.45rem 0.55rem;
    border-radius: 6px;
    border: 1px solid #3a4050;
    background: #0e1016;
    color: #e8e6e3;
    font: inherit;
  }
  button {
    padding: 0.55rem 0.85rem;
    border: none;
    border-radius: 6px;
    background: #3d7ea6;
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  button.sec {
    background: #2c3140;
    font-weight: 500;
  }
  button.sec.on {
    background: #3d7ea6;
  }
  button.build {
    margin-top: 0.25rem;
  }
  button:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  button:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  .card-inner {
    padding: 0.65rem 0.7rem;
    border-radius: 6px;
    background: #0e1016;
    border: 1px solid #2c3140;
  }
  dl {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  dl > div {
    display: grid;
    grid-template-columns: 6.5rem 1fr;
    gap: 0.35rem;
    font-size: 0.8rem;
  }
  dt {
    color: #9a9690;
    margin: 0;
  }
  dd {
    margin: 0;
    font-variant-numeric: tabular-nums;
  }
  .notes {
    grid-template-columns: 1fr !important;
  }
  .notes dd {
    color: #b8b4ae;
    font-size: 0.78rem;
  }
  .muted {
    color: #6a6660;
  }
  .err {
    margin: 0;
    color: #e08080;
    font-size: 0.8rem;
  }
  .build-tip {
    margin: 0;
    font-size: 0.72rem;
    color: #6a6660;
  }
  /* MapLibre dark chrome tweaks */
  :global(.maplibregl-ctrl-group) {
    background: #1a1d26 !important;
    border: 1px solid #2c3140;
  }
  :global(.maplibregl-ctrl-group button) {
    background-color: transparent !important;
  }
  :global(.maplibregl-ctrl-group button + button) {
    border-top: 1px solid #2c3140;
  }
  :global(.maplibregl-ctrl-attrib) {
    background: rgba(14, 16, 22, 0.75) !important;
    color: #9a9690;
  }
  :global(.maplibregl-ctrl-attrib a) {
    color: #b8b4ae;
  }
  :global(.maplibregl-ctrl-scale) {
    background: rgba(14, 16, 22, 0.75);
    border-color: #3a4050;
    color: #b8b4ae;
  }
  @media (max-width: 720px) {
    .map-page {
      grid-template-columns: 1fr;
      grid-template-rows: minmax(14rem, 45vh) 1fr;
    }
    .panel {
      border-left: none;
      border-top: 1px solid #2c3140;
    }
    .map-hint {
      white-space: normal;
      max-width: 90%;
      text-align: center;
    }
  }
</style>
