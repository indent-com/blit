/**
 * The hero: the blit workspace around a recorded session, drawn to the
 * app's real design — bare tiles split by hairlines, a left dock with the
 * root picker and FILES / COMMIT LOG sections, one status bar. No pane
 * titles, because the app has none.
 *
 * The terminal tile is a real render — the bytes in /demo/hero-tests.blitrec
 * were captured from a live `blit server` with `blit terminal record` and
 * flow through BlitConnection, the WASM diff engine, and whichever renderer
 * this browser earns (see ../lib/replay.ts). The dock and editor are chrome
 * keyed to the replay's clock: `proj.rs` gains its M when the recorded fix
 * lands (~9.6s), the editor's line flips with it, and the commit appears in
 * the log when it is made (~18s) — timestamps extracted from the recording.
 */

import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { BlitWorkspace, measureCell, PALETTES } from "@blit-sh/core";
import type { BlitWasmModule } from "@blit-sh/core";
import { BlitTerminal, BlitWorkspaceProvider } from "@blit-sh/solid";
import { initWasm } from "../lib/wasm";
import { parseBlitrec, ReplayTransport } from "../lib/replay";
import { monoLoadSpec, MONO_STACK } from "../lib/fonts";

const DARK = PALETTES.find((p) => p.id === "github-dark")!;
const FONT = MONO_STACK;
const FONT_SIZE = 12.5;
const PTY = 1;
const COLS = 80;
const ROWS = 14;

/** Story keyframes, in replay milliseconds (see scripts/validate-blitrec). */
const T_FIX = 9600;
const T_COMMIT = 18000;

const HISTORY = [
  { age: "9h", oid: "c323244", subject: "proj: southern rows" },
  { age: "1d", oid: "c5b3844", subject: "zoom for a viewport span" },
  { age: "2d", oid: "c2dd46d", subject: "proj: wrap tile_x at ±180" },
];

export default function HeroWorkspace() {
  const [wasm, setWasm] = createSignal<BlitWasmModule | null>(null);
  const [workspace, setWorkspace] = createSignal<BlitWorkspace | null>(null);
  // Sized so the grid renders at native resolution: a passive surface
  // CSS-scales its canvas to fit, and a bitmap of 1px-aligned cells scaled
  // by ~0.9 shimmers on every partial repaint. The font size is the knob
  // that changes the native size instead of the scale factor.
  const [fontSize, setFontSize] = createSignal(FONT_SIZE);
  const [cellH, setCellH] = createSignal(20);
  const [termH, setTermH] = createSignal(240);
  const [now, setNow] = createSignal(0);
  let transport: ReplayTransport | null = null;
  let frame!: HTMLDivElement;
  let main!: HTMLDivElement;

  const fixed = () => now() >= T_FIX;
  const committed = () => now() >= T_COMMIT;

  onMount(async () => {
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
    try {
      await document.fonts?.load(monoLoadSpec(FONT_SIZE));
    } catch {
      // Fallback metrics are fine; the tile just letterboxes a little.
    }
    const fit = () => {
      const avail = main.clientWidth - 10;
      if (avail <= 0) return;
      const base = measureCell(FONT, FONT_SIZE);
      const size = Math.max(
        8,
        Math.min(16, (FONT_SIZE * avail) / (COLS * base.w)),
      );
      const cell = measureCell(FONT, size);
      setFontSize(size);
      setCellH(cell.h);
      setTermH(Math.ceil(ROWS * cell.h) + 10);
    };
    fit();
    // Late font arrival changes cell metrics without resizing the frame,
    // so the observer alone would never refit.
    document.fonts?.ready.then(fit).catch(() => {});
    const ro = new ResizeObserver(fit);
    ro.observe(main);

    const [mod, tests] = await Promise.all([
      initWasm(),
      fetch("/demo/hero-tests.blitrec").then((r) => r.arrayBuffer()),
    ]);
    transport = new ReplayTransport(
      [{ ptyId: PTY, tag: "tests", frames: parseBlitrec(tests) }],
      { static: reduced.matches, holdMs: 5000 },
    );
    const ws = new BlitWorkspace({ wasm: mod });
    ws.addConnection({ id: "replay", transport });
    setWasm(mod);
    setWorkspace(ws);

    const clock = setInterval(() => setNow(transport?.position() ?? 0), 400);
    // Offscreen replays are paused, not just unpainted.
    const io = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) transport?.play();
        else transport?.pause();
      },
      { threshold: 0.15 },
    );
    io.observe(frame);
    onCleanup(() => {
      clearInterval(clock);
      io.disconnect();
      ro.disconnect();
      ws.dispose();
    });
  });

  const sessionId = () =>
    workspace()
      ?.getSnapshot()
      .sessions.find((s) => s.ptyId === PTY)?.id;

  const sectionHeader = (label: string) => (
    <div class="border-y border-[#21262d] px-2 py-1 text-[9px] font-bold uppercase tracking-[0.14em] text-[#8b949e]">
      <span class="mr-1 text-[8px]">▾</span>
      {label}
    </div>
  );

  const fileRow = (
    name: string,
    size: string,
    opts: { indent?: boolean; mark?: boolean } = {},
  ) => (
    <div
      class="flex items-center justify-between py-[1px] pr-2"
      classList={{ "pl-8": !!opts.indent, "pl-4": !opts.indent }}
    >
      <span class="flex items-center gap-1.5">
        <span class="text-[#8b949e]">▤</span>
        <span classList={{ "text-[#e3b341]": !!opts.mark }}>{name}</span>
      </span>
      <span
        class="text-[9px] text-[#8b949e]"
        classList={{ "text-[#e3b341]": !!opts.mark }}
      >
        {opts.mark ? "M" : size}
      </span>
    </div>
  );

  return (
    <div
      ref={frame}
      data-theme="dark"
      class="overflow-hidden rounded-xl border border-[#21262d] bg-[#0a0d12] shadow-2xl shadow-[color:var(--accent-glow)]"
      style={{ "line-height": "1" }}
    >
      {/* browser chrome — the tab this all lives in */}
      <div class="flex items-center gap-3 border-b border-[#21262d] bg-[#11161d] px-3 py-2">
        <div class="flex items-center gap-1.5">
          <span class="h-2.5 w-2.5 rounded-full bg-[#ff5f57]"></span>
          <span class="h-2.5 w-2.5 rounded-full bg-[#febc2e]"></span>
          <span class="h-2.5 w-2.5 rounded-full bg-[#28c840]"></span>
        </div>
        <div class="flex flex-1 items-center justify-center font-mono text-[11px] text-[#8b949e]">
          blit.sh/s#rf6e…
        </div>
        <span class="w-12"></span>
      </div>

      <div class="grid grid-cols-[11.5rem_minmax(0,1fr)] bg-black max-sm:grid-cols-1">
        {/* left dock */}
        <div class="border-r border-[#21262d] font-mono text-[11px] leading-[1.8] max-sm:hidden">
          {/* root picker */}
          <div class="flex items-center gap-1 px-2 py-1.5 text-[10px] text-[#c9d1d9]">
            <span class="text-[#8b949e]">⌖</span>
            <span class="truncate">box:/tmp/mercator</span>
            <span class="ml-auto text-[#8b949e]">▾</span>
          </div>
          {sectionHeader("Files")}
          <div class="bg-[#161b22] px-4 py-[2px] text-[#c9d1d9]">
            <span class="text-[#8b949e]">⌥</span> main
          </div>
          <div class="py-1 text-[#c9d1d9]">
            <div class="py-[1px] pl-4">
              <span class="text-[#8b949e]">▾ 🗀</span> src
            </div>
            {fileRow("lib.rs", "1.2 K", { indent: true })}
            {fileRow("proj.rs", "764 B", {
              indent: true,
              mark: fixed() && !committed(),
            })}
            {fileRow(".gitignore", "8 B")}
            {fileRow("Cargo.toml", "88 B")}
            {fileRow("README.md", "38 B")}
          </div>
          {sectionHeader("Commit log")}
          <div class="py-1 pr-1 text-[#8b949e]">
            <Show when={committed()}>
              <div class="flex items-center gap-1 py-[1px] pl-2 text-[10.5px]">
                <span class="w-6 shrink-0 text-right text-[9px]">now</span>
                <span class="text-[#e3b341]">3494f25</span>
                <span class="rounded-sm bg-[#1f6feb] px-1 text-[9px] font-bold text-white">
                  main
                </span>
                <span class="truncate text-[#c9d1d9]">clamp tile_y…</span>
              </div>
            </Show>
            <For each={HISTORY}>
              {(c, i) => (
                <div class="flex items-center gap-1 py-[1px] pl-2 text-[10.5px]">
                  <span class="w-6 shrink-0 text-right text-[9px]">
                    {c.age}
                  </span>
                  <span class="text-[#e3b341]">{c.oid}</span>
                  <Show when={i() === 0 && !committed()}>
                    <span class="rounded-sm bg-[#1f6feb] px-1 text-[9px] font-bold text-white">
                      main
                    </span>
                  </Show>
                  <span class="truncate">{c.subject}</span>
                </div>
              )}
            </For>
          </div>
          <div class="border-t border-[#21262d] px-2 py-1 text-[9px] font-bold uppercase tracking-[0.14em] text-[#8b949e]">
            <span class="mr-1 text-[8px]">▸</span>
            Problems
          </div>
        </div>

        {/* main column: editor tile over terminal tile, hairline apart */}
        <div ref={main} class="flex min-w-0 flex-col">
          {/* The same face and metrics as the terminal below — one surface,
              two tiles, as the app renders. */}
          <pre
            class="m-0 flex-1 overflow-hidden px-3 py-2.5 text-[#c9d1d9]"
            style={{
              "font-family": FONT,
              "font-size": `${fontSize()}px`,
              "line-height": `${cellH() * 1.15}px`,
            }}
          >
            <code>
              {
                "/// Tile row for `lat` at zoom `z`, clamped to the mercator square.\n"
              }
              <span class="text-[#ff7b72]">pub fn</span>{" "}
              <span class="text-[#d2a8ff]">tile_y</span>(lat:{" "}
              <span class="text-[#79c0ff]">f64</span>, z:{" "}
              <span class="text-[#79c0ff]">u8</span>) -&gt;{" "}
              <span class="text-[#79c0ff]">u32</span> {"{"}
              {"\n"} <span class="text-[#ff7b72]">let</span> n ={" "}
              <span class="text-[#79c0ff]">1u32</span> &lt;&lt; z;{"\n"}{" "}
              <span class="text-[#ff7b72]">let</span> rad = lat.
              <span class="text-[#d2a8ff]">to_radians</span>();{"\n"}{" "}
              <span class="text-[#ff7b72]">let</span> y = (
              <span class="text-[#79c0ff]">1.0</span> - (rad.
              <span class="text-[#d2a8ff]">tan</span>() +{" "}
              <span class="text-[#79c0ff]">1.0</span> / rad.
              <span class="text-[#d2a8ff]">cos</span>()).
              <span class="text-[#d2a8ff]">ln</span>() / PI) /{" "}
              <span class="text-[#79c0ff]">2.0</span>;{"\n"}{" "}
              <span
                classList={{
                  "bg-[#2ea04326] rounded-[2px]": fixed() && !committed(),
                }}
              >
                ((y * n <span class="text-[#ff7b72]">as</span>{" "}
                <span class="text-[#79c0ff]">f64</span>){" "}
                <span class="text-[#ff7b72]">as</span>{" "}
                <span class="text-[#79c0ff]">u32</span>).
                <span class="text-[#d2a8ff]">min</span>(n{fixed() ? " - 1" : ""}
                )
              </span>
              {"\n"}
              {"}"}
            </code>
          </pre>

          {/* the real thing: the recorded session, re-rendered live */}
          <div
            class="relative border-t border-[#21262d] p-1"
            style={{ height: `${termH()}px` }}
          >
            <Show
              when={wasm() && workspace() && sessionId()}
              fallback={
                <div class="flex h-full items-center justify-center font-mono text-[11px] text-[#8b949e]">
                  loading replay…
                </div>
              }
            >
              <BlitWorkspaceProvider
                workspace={workspace()!}
                palette={DARK}
                fontFamily={FONT}
                fontSize={fontSize()}
              >
                <BlitTerminal
                  sessionId={sessionId()!}
                  readOnly
                  resizable={false}
                  showCursor
                />
              </BlitWorkspaceProvider>
            </Show>
          </div>
        </div>
      </div>

      {/* the one status bar */}
      <div class="flex items-center justify-between border-t border-[#21262d] bg-[#0d1117] px-2 py-1 font-mono text-[10px] text-[#8b949e]">
        <span>
          <span class="text-[#c9d1d9]">1T</span> box:1 › 1 /tmp/mercator
        </span>
        <span class="tracking-[0.2em]">♪ ◆ ▣ ◨ ◑ Aa ●1</span>
      </div>
    </div>
  );
}
