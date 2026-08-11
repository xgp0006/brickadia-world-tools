<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import maplibregl from "maplibre-gl";
  import "maplibre-gl/dist/maplibre-gl.css";

  type DemSource =
    | "aws_terrarium"
    | "mapbox_terrain_rgb"
    | "open_topography"
    | "open_topography_cop30"
    | "usgs_3dep";

  type DemPredictResult = {
    cell_m: number;
    cell_m_eff: number;
    approx_cells: number;
    zoom: number | null;
    zoom_cap: number | null;
    notes: string;
  };

  type BuildProgress = { phase: string; frac: number };

  type DemBuildResult = {
    path: string;
    installed_path?: string | null;
    install_warning?: string | null;
    brick_count: number;
    dem_width: number;
    dem_height: number;
    elevation_min_m: number;
    elevation_max_m: number;
  };

  /** Default: small Boulder CO box (matches dem_predict tests spirit). */
  let north = $state(40.05);
  let south = $state(39.95);
  let east = $state(-105.2);
  let west = $state(-105.35);
  let demSource = $state<DemSource>("aws_terrarium");
  let densityFactor = $state(1);
  let studsPerMeter = $state(4);
  let verticalExaggeration = $state(1);
  let outputName = $state("map-build");
  let installWorld = $state(true);
  let overwrite = $state(false);
  let mapboxToken = $state("");
  let opentopoKey = $state("");

  let predict = $state<DemPredictResult | null>(null);
  let predictError = $state("");
  let predicting = $state(false);
  let drawMode = $state(false);
  /** Basemap: day = CARTO Voyager (light), night = CARTO dark_all. */
  let basemapMode = $state<"day" | "night">(
    (typeof localStorage !== "undefined" &&
      (localStorage.getItem("bwt-basemap") as "day" | "night" | null)) ||
      "day",
  );
  /**
   * Relief: free AWS Terrarium DEM tiles (same source as Map DEM default).
   * hillshade = 2D shaded relief; terrain3d = MapLibre mesh + pitch.
   */
  let reliefMode = $state<"off" | "hillshade" | "terrain3d">(
    (typeof localStorage !== "undefined" &&
      (localStorage.getItem("bwt-relief") as "off" | "hillshade" | "terrain3d" | null)) ||
      "hillshade",
  );
  let mapReady = $state(false);
  let building = $state(false);
  let buildProgress = $state<BuildProgress | null>(null);
  let buildError = $state("");
  let buildResult = $state<DemBuildResult | null>(null);
  let sendingSculpt = $state(false);

  // Grid (BWT-3.8)
  let gridMode = $state(false);
  let tileM = $state(750);
  let gridEst = $state<{
    tile_count: number;
    total_cells: number;
    est_bricks: number;
    est_seconds: number;
    fits_ram: boolean;
    over_cell_budget: boolean;
    zoom: number;
    cell_m: number;
    cols: number;
    rows: number;
  } | null>(null);
  let gridProgress = $state<{
    phase: string;
    frac: number;
    tiles_done: number;
    tiles_total: number;
  } | null>(null);
  let gridResult = $state<{
    paths: string[];
    installed_paths: string[];
    brick_count: number;
    tile_count: number;
    warnings: string[];
  } | null>(null);

  let mapEl: HTMLDivElement | undefined = $state();
  let map: maplibregl.Map | null = null;
  let dragStart: maplibregl.LngLat | null = null;
  let drawing = false;

  const BBOX_SRC = "bbox-src";
  const BBOX_FILL = "bbox-fill";
  const BBOX_LINE = "bbox-line";
  const DEM_SRC = "terrarium-dem";
  const HILLSHADE_LAYER = "terrarium-hillshade";
  // Free public Terrarium pyramid (Mapzen/AWS) — max z15 real data.
  const TERRARIUM_TILES =
    "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";

  function basemapStyle(mode: "day" | "night"): maplibregl.StyleSpecification {
    // Free CARTO basemaps (no token). Voyager = readable day; dark_all = night.
    const tiles =
      mode === "night"
        ? "https://basemaps.cartocdn.com/dark_all/{z}/{x}/{y}@2x.png"
        : "https://basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}@2x.png";
    return {
      version: 8,
      sources: {
        carto: {
          type: "raster",
          tiles: [tiles],
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
    };
  }

  function ensureDemSource() {
    if (!map || map.getSource(DEM_SRC)) return;
    map.addSource(DEM_SRC, {
      type: "raster-dem",
      tiles: [TERRARIUM_TILES],
      encoding: "terrarium",
      tileSize: 256,
      maxzoom: 15,
      attribution:
        'Elevation <a href="https://github.com/tilezen/joerd">AWS Terrarium / Mapzen</a>',
    });
  }

  function ensureBboxLayers() {
    if (!map) return;
    if (!map.getSource(BBOX_SRC)) {
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
    } else {
      setBboxOnMap(north, south, east, west);
    }
  }

  /** Hillshade + optional 3D terrain mesh (MapLibre). Bbox stays on top. */
  function applyReliefMode(mode: "off" | "hillshade" | "terrain3d") {
    if (!map || !map.isStyleLoaded()) return;

    // Clear previous relief layers / terrain.
    if (map.getLayer(HILLSHADE_LAYER)) {
      map.removeLayer(HILLSHADE_LAYER);
    }
    try {
      map.setTerrain(null);
    } catch {
      /* no terrain */
    }

    if (mode === "off") {
      map.easeTo({ pitch: 0, bearing: 0, duration: 400 });
      return;
    }

    ensureDemSource();

    // Insert hillshade under bbox so selection stays readable.
    const beforeId = map.getLayer(BBOX_FILL) ? BBOX_FILL : undefined;
    if (!map.getLayer(HILLSHADE_LAYER)) {
      map.addLayer(
        {
          id: HILLSHADE_LAYER,
          type: "hillshade",
          source: DEM_SRC,
          maxzoom: 18,
          paint: {
            "hillshade-exaggeration": mode === "terrain3d" ? 0.45 : 0.65,
            "hillshade-shadow-color": "#0a0a12",
            "hillshade-highlight-color": "#ffffff",
            "hillshade-accent-color": "#4a5568",
            "hillshade-illumination-direction": 315,
            "hillshade-illumination-anchor": "viewport",
          },
        },
        beforeId,
      );
    }

    if (mode === "terrain3d") {
      map.setTerrain({ source: DEM_SRC, exaggeration: 1.6 });
      map.easeTo({ pitch: 55, bearing: map.getBearing() || -18, duration: 500 });
    } else {
      map.easeTo({ pitch: 0, bearing: 0, duration: 400 });
    }
  }

  function setReliefMode(mode: "off" | "hillshade" | "terrain3d") {
    reliefMode = mode;
    try {
      localStorage.setItem("bwt-relief", mode);
    } catch {
      /* private mode */
    }
    applyReliefMode(mode);
  }

  function setBasemapMode(mode: "day" | "night") {
    basemapMode = mode;
    try {
      localStorage.setItem("bwt-basemap", mode);
    } catch {
      /* private mode */
    }
    if (!map) return;
    const center = map.getCenter();
    const zoom = map.getZoom();
    const pitch = map.getPitch();
    const bearing = map.getBearing();
    mapReady = false;
    map.setStyle(basemapStyle(mode));
    map.once("style.load", () => {
      if (!map) return;
      map.setCenter(center);
      map.setZoom(zoom);
      ensureBboxLayers();
      applyReliefMode(reliefMode);
      if (reliefMode === "terrain3d") {
        map.setPitch(pitch || 55);
        map.setBearing(bearing);
      }
      mapReady = true;
    });
  }

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
      style: basemapStyle(basemapMode),
      center: [(west + east) / 2, (north + south) / 2],
      zoom: 10,
      attributionControl: { compact: true },
    });

    map.addControl(
      new maplibregl.NavigationControl({ showCompass: true, visualizePitch: true }),
      "top-left",
    );
    map.addControl(new maplibregl.ScaleControl({ unit: "metric" }), "bottom-left");

    map.on("load", () => {
      if (!map) return;
      ensureBboxLayers();
      applyReliefMode(reliefMode);
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

  function gridRequest() {
    return {
      north,
      south,
      east,
      west,
      dem_source: demSource,
      tile_m: tileM,
      mode: "auto",
      cols: 2,
      rows: 2,
      studs_per_meter: studsPerMeter,
      vertical_exaggeration: verticalExaggeration,
      output_name: outputName.trim() || "grid-build",
      install: installWorld,
      overwrite,
      stitched: true,
      individual: false,
      mapbox_token: mapboxToken.trim() || null,
      opentopo_key: opentopoKey.trim() || null,
      brick_mode: "tile",
      glow: false,
      no_collision: false,
    };
  }

  async function refreshGridEstimate() {
    if (!gridMode) {
      gridEst = null;
      return;
    }
    clampBbox();
    try {
      gridEst = await invoke("grid_estimate", { request: gridRequest() });
    } catch (e) {
      gridEst = null;
      buildError = String(e);
    }
  }

  async function runBuild() {
    clampBbox();
    building = true;
    buildError = "";
    buildResult = null;
    gridResult = null;
    buildProgress = { phase: "Starting…", frac: 0 };
    gridProgress = null;
    try {
      if (gridMode) {
        const result = await invoke<NonNullable<typeof gridResult>>("grid_fetch_build", {
          request: gridRequest(),
        });
        gridResult = result;
        buildProgress = { phase: "Finished", frac: 1 };
        return;
      }
      const result = await invoke<DemBuildResult>("dem_fetch_build", {
        request: {
          north,
          south,
          east,
          west,
          dem_source: demSource,
          density_factor: densityFactor,
          studs_per_meter: studsPerMeter,
          vertical_exaggeration: verticalExaggeration,
          output_name: outputName.trim() || "map-build",
          install: installWorld,
          overwrite,
          mapbox_token: mapboxToken.trim() || null,
          opentopo_key: opentopoKey.trim() || null,
          brick_mode: "tile",
        },
      });
      buildResult = result;
      buildProgress = { phase: "Done", frac: 1 };
    } catch (e) {
      buildError = String(e);
      buildProgress = null;
    } finally {
      building = false;
    }
  }

  /** Fetch bbox DEM into a sculpt session and open the Sculpt tab. */
  async function sendToSculpt() {
    clampBbox();
    sendingSculpt = true;
    buildError = "";
    buildProgress = { phase: "Fetching for Sculpt…", frac: 0 };
    try {
      const info = await invoke<{
        session_id: number;
        width: number;
        height: number;
        source_name: string;
      }>("sculpt_from_dem", {
        request: {
          north,
          south,
          east,
          west,
          dem_source: demSource,
          density_factor: densityFactor,
          studs_per_meter: studsPerMeter,
          vertical_exaggeration: verticalExaggeration,
          micro: false,
          source_name: outputName.trim() || "sculpt",
          mapbox_token: mapboxToken.trim() || null,
          opentopo_key: opentopoKey.trim() || null,
        },
      });
      try {
        sessionStorage.setItem("bwt-sculpt-session", String(info.session_id));
      } catch {
        /* private mode */
      }
      buildProgress = { phase: "Opening Sculpt…", frac: 1 };
      await goto("/sculpt");
    } catch (e) {
      buildError = String(e);
      buildProgress = null;
    } finally {
      sendingSculpt = false;
    }
  }

  $effect(() => {
    let unlisten: (() => void) | undefined;
    listen<BuildProgress>("build:progress", (e) => {
      buildProgress = e.payload;
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  });

  $effect(() => {
    let unlisten: (() => void) | undefined;
    listen<{ phase: string; frac: number; tiles_done: number; tiles_total: number }>(
      "grid:progress",
      (e) => {
        gridProgress = e.payload;
        buildProgress = { phase: e.payload.phase, frac: e.payload.frac };
      },
    ).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  });

  $effect(() => {
    const _g = gridMode;
    const _t = tileM;
    const _n = north;
    const _s = south;
    void _g;
    void _t;
    void _n;
    void _s;
    if (!gridMode) return;
    const t = setTimeout(() => {
      void refreshGridEstimate();
    }, 200);
    return () => clearTimeout(t);
  });
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
    <p class="hint">
      Draw a box → predict → Build (Terrarium free). Enable <strong>Grid</strong> for large areas
      (tiles). COP30 needs OpenTopo key.
    </p>

    <div class="actions">
      <button
        type="button"
        class="sec"
        class:on={drawMode}
        onclick={() => (drawMode = !drawMode)}
      >
        {drawMode ? "Drawing…" : "Draw box"}
      </button>
      <div class="basemap-toggle" role="group" aria-label="Basemap day or night">
        <button
          type="button"
          class="sec"
          class:on={basemapMode === "day"}
          onclick={() => setBasemapMode("day")}
          title="Light basemap (CARTO Voyager)"
        >
          Day
        </button>
        <button
          type="button"
          class="sec"
          class:on={basemapMode === "night"}
          onclick={() => setBasemapMode("night")}
          title="Dark basemap (CARTO dark)"
        >
          Night
        </button>
      </div>
    </div>

    <div class="basemap-toggle relief-toggle" role="group" aria-label="Terrain relief">
      <button
        type="button"
        class="sec"
        class:on={reliefMode === "off"}
        onclick={() => setReliefMode("off")}
        title="Flat basemap only"
      >
        Flat
      </button>
      <button
        type="button"
        class="sec"
        class:on={reliefMode === "hillshade"}
        onclick={() => setReliefMode("hillshade")}
        title="2D hillshade from free AWS Terrarium DEM"
      >
        Relief
      </button>
      <button
        type="button"
        class="sec"
        class:on={reliefMode === "terrain3d"}
        onclick={() => setReliefMode("terrain3d")}
        title="3D terrain mesh (drag with right mouse / touch to orbit)"
      >
        3D
      </button>
    </div>
    <p class="hint small">
      Relief/3D uses free Terrarium elevation tiles (preview only — build still uses DEM source
      below).
    </p>

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
        <option value="open_topography">OpenTopography SRTM</option>
        <option value="open_topography_cop30">OpenTopography COP30</option>
        <option value="usgs_3dep">USGS 3DEP</option>
      </select>
    </label>

    <label class="check">
      <input type="checkbox" bind:checked={gridMode} />
      Grid build (tiled large worlds)
    </label>
    {#if gridMode}
      <label>
        Tile size (m)
        <input type="number" min="100" max="50000" step="50" bind:value={tileM} />
      </label>
      {#if gridEst}
        <div class="predict">
          <div class="row">
            <span>Tiles</span>
            <strong>{gridEst.cols}×{gridEst.rows} = {gridEst.tile_count}</strong>
          </div>
          <div class="row">
            <span>Cells</span>
            <strong>{fmt(gridEst.total_cells, 0)}</strong>
          </div>
          <div class="row">
            <span>Zoom / m·cell</span>
            <strong>z{gridEst.zoom} · ~{fmt(gridEst.cell_m, 1)} m</strong>
          </div>
          <div class="row">
            <span>~Time / RAM</span>
            <strong
              >~{fmt(gridEst.est_seconds, 0)}s · {gridEst.fits_ram ? "fits" : "OVER RAM"}</strong
            >
          </div>
          {#if gridEst.over_cell_budget}
            <p class="err">Per-tile over cell budget — raise tile size or shrink box.</p>
          {/if}
        </div>
      {/if}
    {/if}

    <label>
      Downsample density (1–8)
      <input
        type="number"
        min="1"
        max="8"
        step="1"
        bind:value={densityFactor}
        disabled={gridMode}
      />
    </label>

    <div class="grid2">
      <label>
        Studs / m
        <input type="number" min="0.5" max="32" step="0.5" bind:value={studsPerMeter} />
      </label>
      <label>
        Vert. exaggeration
        <input type="number" min="0.25" max="8" step="0.25" bind:value={verticalExaggeration} />
      </label>
    </div>

    <label>
      Output name
      <input type="text" bind:value={outputName} placeholder="map-build" />
    </label>

    <label class="check">
      <input type="checkbox" bind:checked={installWorld} />
      Install to Brickadia Worlds
    </label>
    <label class="check">
      <input type="checkbox" bind:checked={overwrite} />
      Overwrite same name
    </label>

    {#if demSource === "mapbox_terrain_rgb"}
      <label>
        Mapbox token
        <input type="password" autocomplete="off" bind:value={mapboxToken} placeholder="or config.toml" />
      </label>
    {/if}
    {#if demSource === "open_topography"}
      <label>
        OpenTopo API key
        <input type="password" autocomplete="off" bind:value={opentopoKey} placeholder="or config.toml" />
      </label>
    {/if}

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
      disabled={building || sendingSculpt || !mapReady}
      onclick={() => void runBuild()}
    >
      {building ? "Building…" : "Build world"}
    </button>
    <button
      type="button"
      class="sec build-secondary"
      disabled={building || sendingSculpt || !mapReady || gridMode}
      title={gridMode
        ? "Send to Sculpt works on single-box selection (turn off Grid)"
        : "Fetch this DEM area and open it in Sculpt to brush by hand"}
      onclick={() => void sendToSculpt()}
    >
      {sendingSculpt ? "Fetching for Sculpt…" : "✎ Send to Sculpt"}
    </button>

    {#if buildProgress && (building || sendingSculpt || buildResult)}
      <div class="progress card-inner">
        <div class="progress-label">{buildProgress.phase}</div>
        <div class="bar">
          <div class="bar-fill" style="width: {Math.round(Math.min(1, Math.max(0, buildProgress.frac)) * 100)}%"></div>
        </div>
      </div>
    {/if}
    {#if buildError}
      <p class="err">{buildError}</p>
    {/if}
    {#if gridResult}
      <section class="result card-inner">
        <p class="ok">
          Grid {gridResult.tile_count} tiles · {gridResult.brick_count.toLocaleString()} bricks
        </p>
        <dl>
          {#each gridResult.paths as p}
            <div class="notes"><dt>path</dt><dd>{p}</dd></div>
          {/each}
          {#each gridResult.installed_paths as p}
            <div class="notes"><dt>installed</dt><dd>{p}</dd></div>
          {/each}
        </dl>
        {#each gridResult.warnings as w}
          <p class="err">{w}</p>
        {/each}
      </section>
    {/if}
    {#if buildResult}
      <section class="result card-inner">
        <p class="ok">Wrote {buildResult.path}</p>
        <dl>
          <div><dt>bricks</dt><dd>{buildResult.brick_count.toLocaleString()}</dd></div>
          <div><dt>DEM</dt><dd>{buildResult.dem_width}×{buildResult.dem_height}</dd></div>
          <div>
            <dt>elev</dt>
            <dd>{fmt(buildResult.elevation_min_m, 1)}–{fmt(buildResult.elevation_max_m, 1)} m</dd>
          </div>
          {#if buildResult.installed_path}
            <div class="notes"><dt>installed</dt><dd>{buildResult.installed_path}</dd></div>
          {/if}
          {#if buildResult.install_warning}
            <div class="notes"><dt>install</dt><dd class="warn">{buildResult.install_warning}</dd></div>
          {/if}
        </dl>
      </section>
    {/if}
    <p class="build-tip">
      Grid mode tiles large boxes at full zoom. Prefer stitched install for one world.
    </p>
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
    flex-wrap: wrap;
    gap: 0.45rem;
    align-items: center;
  }
  .basemap-toggle {
    display: flex;
    gap: 0.25rem;
    margin-left: auto;
  }
  .basemap-toggle .sec {
    padding: 0.4rem 0.65rem;
    min-width: 3.2rem;
  }
  .relief-toggle {
    margin-left: 0;
    width: 100%;
  }
  .relief-toggle .sec {
    flex: 1;
  }
  .hint.small {
    font-size: 0.72rem;
    margin: -0.25rem 0 0;
    opacity: 0.85;
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
  button.build-secondary {
    width: 100%;
    margin-top: 0.35rem;
    background: #2c3140;
    color: #e8e6e3;
    font-weight: 600;
  }
  button.build-secondary:hover:not(:disabled) {
    background: #3a4050;
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
  label.check {
    flex-direction: row;
    align-items: center;
    gap: 0.45rem;
  }
  label.check input {
    width: auto;
  }
  .progress-label {
    font-size: 0.78rem;
    color: #b8b4ae;
    margin-bottom: 0.35rem;
  }
  .bar {
    height: 0.4rem;
    border-radius: 3px;
    background: #2c3140;
    overflow: hidden;
  }
  .bar-fill {
    height: 100%;
    background: #3d7ea6;
    transition: width 0.15s ease-out;
  }
  .ok {
    margin: 0 0 0.4rem;
    color: #8bc49a;
    font-size: 0.78rem;
    word-break: break-all;
  }
  .warn {
    color: #e0b080 !important;
  }
  .result dl {
    margin-top: 0.25rem;
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
