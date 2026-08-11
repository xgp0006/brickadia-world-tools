<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  type BrickMode = "default" | "tile" | "smooth_tile" | "stud" | "micro";

  let coreVersion = $state("…");
  let heightmapPath = $state("");
  let colormapPath = $state("");
  let outFile = $state("/tmp/bwt-convert-out.brdb");
  let brickMode = $state<BrickMode>("tile");
  let horizontalSize = $state(1);
  let verticalScale = $state(1);
  let greedy = $state(true);
  let hdmap = $state(false);
  let status = $state("");
  let busy = $state(false);

  $effect(() => {
    invoke<string>("core_version")
      .then((v) => (coreVersion = v))
      .catch((e) => (coreVersion = `error: ${e}`));
  });

  async function runConvert(event: Event) {
    event.preventDefault();
    if (!heightmapPath.trim()) {
      status = "Heightmap path required.";
      return;
    }
    busy = true;
    status = "Converting…";
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
    } catch (e) {
      status = `Error: ${e}`;
    } finally {
      busy = false;
    }
  }
</script>

<main class="wrap">
  <header>
    <h1>Brickadia World Tools</h1>
    <p class="sub">
      Tauri shell · Convert (Phase 2) · core <code>heightmap {coreVersion}</code>
    </p>
  </header>

  <section class="card">
    <h2>Convert</h2>
    <p class="hint">
      Heightmap PNG → <code>.brdb</code> / <code>.brz</code>. Same Rust path as the egui Convert
      tab. Paste absolute paths (file dialog lands next).
    </p>

    <form onsubmit={runConvert}>
      <label>
        Heightmap path
        <input
          type="text"
          bind:value={heightmapPath}
          placeholder="/path/to/height.png"
          disabled={busy}
        />
      </label>
      <label>
        Colormap path (optional)
        <input
          type="text"
          bind:value={colormapPath}
          placeholder="defaults to heightmap"
          disabled={busy}
        />
      </label>
      <label>
        Output path
        <input type="text" bind:value={outFile} disabled={busy} />
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

      <button type="submit" disabled={busy}>
        {busy ? "Working…" : "Convert"}
      </button>
    </form>

    {#if status}
      <p class="status" class:err={status.startsWith("Error")}>{status}</p>
    {/if}
  </section>

  <footer>
    egui GUI still ships via <code>brickadia-world-tools-gui</code> until Map/Sculpt parity.
  </footer>
</main>

<style>
  :root {
    font-family: "IBM Plex Sans", system-ui, sans-serif;
    color: #e8e6e3;
    background: #12141a;
    line-height: 1.45;
  }
  .wrap {
    max-width: 42rem;
    margin: 0 auto;
    padding: 2rem 1.25rem 3rem;
  }
  h1 {
    margin: 0;
    font-size: 1.75rem;
    letter-spacing: -0.02em;
  }
  .sub {
    margin: 0.35rem 0 0;
    color: #9a9690;
    font-size: 0.9rem;
  }
  .card {
    margin-top: 1.75rem;
    padding: 1.25rem 1.35rem 1.5rem;
    background: #1a1d26;
    border: 1px solid #2c3140;
    border-radius: 8px;
  }
  h2 {
    margin: 0 0 0.35rem;
    font-size: 1.15rem;
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
    margin-top: 0.35rem;
    padding: 0.65rem 1rem;
    border: none;
    border-radius: 6px;
    background: #3d7ea6;
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.55;
    cursor: wait;
  }
  button:hover:not(:disabled) {
    background: #4a93c0;
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
  code {
    font-family: "IBM Plex Mono", ui-monospace, monospace;
    font-size: 0.9em;
  }
  @media (max-width: 560px) {
    .row {
      grid-template-columns: 1fr;
    }
  }
</style>
