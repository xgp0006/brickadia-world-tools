<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { open, save } from "@tauri-apps/plugin-dialog";

  type BrickMode = "default" | "tile" | "smooth_tile" | "stud" | "micro";

  type Progress = { phase: string; frac: number };

  let coreVersion = $state("…");
  let heightmapPath = $state("");
  let colormapPath = $state("");
  let outFile = $state("");
  let brickMode = $state<BrickMode>("tile");
  let horizontalSize = $state(1);
  let verticalScale = $state(1);
  let greedy = $state(true);
  let hdmap = $state(false);
  let status = $state("");
  let progress = $state<Progress | null>(null);
  let busy = $state(false);

  $effect(() => {
    invoke<string>("core_version")
      .then((v) => (coreVersion = v))
      .catch((e) => (coreVersion = `error: ${e}`));

    let unlisten: (() => void) | undefined;
    listen<Progress>("convert:progress", (e) => {
      progress = e.payload;
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  });

  async function pickHeightmap() {
    const path = await open({
      multiple: false,
      filters: [{ name: "Heightmap", extensions: ["png", "jpg", "jpeg"] }],
    });
    if (typeof path === "string") heightmapPath = path;
  }

  async function pickColormap() {
    const path = await open({
      multiple: false,
      filters: [{ name: "Colormap", extensions: ["png", "jpg", "jpeg"] }],
    });
    if (typeof path === "string") colormapPath = path;
  }

  async function pickOut() {
    const path = await save({
      filters: [{ name: "Brickadia save", extensions: ["brdb", "brz"] }],
      defaultPath: "world.brdb",
    });
    if (typeof path === "string") outFile = path;
  }

  async function runConvert(event: Event) {
    event.preventDefault();
    if (!heightmapPath.trim()) {
      status = "Heightmap path required.";
      return;
    }
    if (!outFile.trim()) {
      status = "Output path required — use Browse or type a path.";
      return;
    }
    busy = true;
    status = "Converting…";
    progress = { phase: "Reading", frac: 0 };
    try {
      const result = await invoke<{
        out_file: string;
        absolute_path: string;
      }>("convert_build", {
        request: {
          heightmaps: [heightmapPath.trim()],
          colormap: colormapPath.trim() || null,
          out_file: outFile.trim(),
          brick_mode: brickMode,
          horizontal_size: horizontalSize,
          vertical_scale: verticalScale,
          greedy,
          quadtree: false,
          cull: false,
          nocollide: false,
          glow: false,
          hdmap,
          lrgb: false,
          snap: false,
        },
      });
      status = `OK → ${result.absolute_path ?? result.out_file}`;
      progress = { phase: "Finished", frac: 1 };
    } catch (e) {
      status = `Error: ${e}`;
      progress = null;
    } finally {
      busy = false;
    }
  }
</script>

<main class="wrap">
  <header>
    <h1>Convert</h1>
    <p class="sub">
      Heightmap PNG → <code>.brdb</code> · core <code>heightmap {coreVersion}</code>
    </p>
  </header>

  <section class="card">
    <p class="hint">
      Same Rust path as the egui Convert tab. Install into Brickadia Worlds is Map-tab only for now.
    </p>

    <form onsubmit={runConvert}>
      <label>
        Heightmap
        <div class="path-row">
          <input
            type="text"
            bind:value={heightmapPath}
            placeholder="/path/to/height.png"
            disabled={busy}
          />
          <button type="button" class="sec" onclick={pickHeightmap} disabled={busy}>Browse</button>
        </div>
      </label>
      <label>
        Colormap (optional)
        <div class="path-row">
          <input
            type="text"
            bind:value={colormapPath}
            placeholder="defaults to heightmap"
            disabled={busy}
          />
          <button type="button" class="sec" onclick={pickColormap} disabled={busy}>Browse</button>
        </div>
      </label>
      <label>
        Output
        <div class="path-row">
          <input type="text" bind:value={outFile} placeholder="world.brdb" disabled={busy} />
          <button type="button" class="sec" onclick={pickOut} disabled={busy}>Browse</button>
        </div>
      </label>

      <div class="row">
        <label>
          Brick type
          <select bind:value={brickMode} disabled={busy}>
            <option value="default">Default</option>
            <option value="tile">Tile</option>
            <option value="smooth_tile">Smooth tile</option>
            <option value="stud">Studded</option>
            <option value="micro">Micro</option>
          </select>
        </label>
        <label>
          H size
          <input type="number" min="1" max="64" bind:value={horizontalSize} disabled={busy} />
        </label>
        <label>
          V scale
          <input type="number" min="1" max="100" bind:value={verticalScale} disabled={busy} />
        </label>
      </div>

      <div class="checks">
        <label class="check"
          ><input type="checkbox" bind:checked={greedy} disabled={busy} /> Greedy mesh</label
        >
        <label class="check"
          ><input type="checkbox" bind:checked={hdmap} disabled={busy} /> HD Map (RGB height)</label
        >
      </div>

      {#if progress && busy}
        <div class="prog">
          <div class="prog-label">{progress.phase} · {Math.round(progress.frac * 100)}%</div>
          <div class="bar">
            <div class="fill" style="width: {Math.min(100, progress.frac * 100)}%"></div>
          </div>
        </div>
      {/if}

      <button type="submit" disabled={busy}>
        {busy ? "Working…" : "Convert"}
      </button>
    </form>

    {#if status}
      <p class="status" class:err={status.startsWith("Error")}>{status}</p>
    {/if}
  </section>

  <footer>
    Run: <code>deno task tauri:dev</code> · egui: <code>brickadia-world-tools-gui</code>
  </footer>
</main>

<style>
  .wrap {
    max-width: 42rem;
    margin: 0 auto;
    padding: 1.5rem 1.25rem 3rem;
    width: 100%;
    box-sizing: border-box;
  }
  h1 {
    margin: 0;
    font-size: 1.5rem;
    letter-spacing: -0.02em;
  }
  .sub {
    margin: 0.35rem 0 0;
    color: #9a9690;
    font-size: 0.9rem;
  }
  .card {
    margin-top: 1.25rem;
    padding: 1.25rem 1.35rem 1.5rem;
    background: #1a1d26;
    border: 1px solid #2c3140;
    border-radius: 8px;
  }
  .hint {
    margin: 0 0 1rem;
    color: #9a9690;
    font-size: 0.85rem;
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: 0.8rem;
    color: #b8b4ae;
    text-align: left;
  }
  .path-row {
    display: flex;
    gap: 0.45rem;
  }
  .path-row input {
    flex: 1;
    min-width: 0;
  }
  input[type="text"],
  input[type="number"],
  select {
    padding: 0.55rem 0.65rem;
    border-radius: 6px;
    border: 1px solid #3a4050;
    background: #0e1016;
    color: #e8e6e3;
    font: inherit;
  }
  .row {
    display: grid;
    grid-template-columns: 1.4fr 0.7fr 0.7fr;
    gap: 0.65rem;
  }
  .checks {
    display: flex;
    gap: 1rem;
    flex-wrap: wrap;
  }
  .check {
    flex-direction: row;
    align-items: center;
    gap: 0.4rem;
  }
  button {
    margin-top: 0.15rem;
    padding: 0.65rem 1rem;
    border: none;
    border-radius: 6px;
    background: #3d7ea6;
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  button.sec {
    margin: 0;
    background: #2c3140;
    font-weight: 500;
    white-space: nowrap;
  }
  button:disabled {
    opacity: 0.55;
    cursor: wait;
  }
  button:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  .prog {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .prog-label {
    font-size: 0.8rem;
    color: #9a9690;
  }
  .bar {
    height: 6px;
    background: #0e1016;
    border-radius: 3px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: #3d7ea6;
    transition: width 0.12s linear;
  }
  .status {
    margin: 1rem 0 0;
    padding: 0.65rem 0.75rem;
    border-radius: 6px;
    background: #15261c;
    border: 1px solid #2a4a35;
    font-size: 0.85rem;
    word-break: break-all;
  }
  .status.err {
    background: #2a1515;
    border-color: #5a2a2a;
  }
  footer {
    margin-top: 2rem;
    color: #6a6660;
    font-size: 0.8rem;
  }
  @media (max-width: 560px) {
    .row {
      grid-template-columns: 1fr;
    }
  }
</style>
