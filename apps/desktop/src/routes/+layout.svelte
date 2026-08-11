<script lang="ts">
  import { page } from "$app/stores";

  let { children } = $props();

  const path = $derived($page.url.pathname);
</script>

<div class="shell">
  <nav class="topnav">
    <a href="/" class="brand">Brickadia World Tools</a>
    <div class="tabs">
      <a href="/" class:active={path === "/" || path === "/convert"}>Convert</a>
      <a href="/map" class:active={path.startsWith("/map")}>Map</a>
      <a href="/sculpt" class:active={path.startsWith("/sculpt")}>Sculpt</a>
    </div>
  </nav>
  <div class="body">
    {@render children()}
  </div>
</div>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    height: 100%;
    font-family: "IBM Plex Sans", system-ui, sans-serif;
    color: #e8e6e3;
    background: #12141a;
    line-height: 1.45;
  }
  :global(#svelte),
  :global(body > div) {
    height: 100%;
  }
  .shell {
    display: flex;
    flex-direction: column;
    min-height: 100%;
    height: 100%;
  }
  .topnav {
    display: flex;
    align-items: center;
    gap: 1.25rem;
    padding: 0.55rem 1rem;
    background: #0e1016;
    border-bottom: 1px solid #2c3140;
    flex-shrink: 0;
  }
  .brand {
    font-weight: 700;
    font-size: 0.95rem;
    letter-spacing: -0.02em;
    color: #e8e6e3;
    text-decoration: none;
  }
  .brand:hover {
    color: #fff;
  }
  .tabs {
    display: flex;
    gap: 0.25rem;
  }
  .tabs a {
    padding: 0.35rem 0.75rem;
    border-radius: 6px;
    color: #9a9690;
    text-decoration: none;
    font-size: 0.85rem;
    font-weight: 600;
  }
  .tabs a:hover {
    color: #e8e6e3;
    background: #1a1d26;
  }
  .tabs a.active {
    color: #fff;
    background: #2c3140;
  }
  .body {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  :global(code) {
    font-family: "IBM Plex Mono", ui-monospace, monospace;
    font-size: 0.9em;
  }
</style>
