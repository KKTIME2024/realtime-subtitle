# vr_overlay — SteamVR subtitle overlay client (Rust)

Vendored from [RinBridge](https://github.com/KKTIME2024/RinBridge) (AGPL-3.0, see
[ACKNOWLEDGMENTS](../ACKNOWLEDGMENTS.md)). Renders subtitles received from the
desktop's unauthenticated `/ws` WebSocket into a shared five-slot HUD frame.
The Rust overlay owns reduction, speaker-aware projection, fixed slot geometry,
and compositor visibility; there is no Python-side subtitle mirror.

## Build (Windows)

```bat
cd vr_overlay
cargo build --release
```

`openvr_sys` 2.1.3 statically links OpenVR's client binding (`openvr_api64`);
that static loader reads the registered `openvrpaths.vrpath` and loads the
installed SteamVR `vrclient_x64.dll`. Do **not** copy `openvr_api.dll` next to
`RinBridgeOverlay.exe`; no sidecar DLL is needed or supported.

CJK rendering uses system fonts (Microsoft YaHei etc.); the bundled Noto CJK asset lives in the
upstream RinBridge repo and is not vendored.

## Contract

- Reads `--config <manifest.json>` (see `src/manifest.rs` for the schema;
  `contract_version` must be 7). The envelope has no `session_token`.
- `bridge_url` must be `ws://127.0.0.1:<port>/ws`; `/vr_ws` and every other
  path are rejected at startup. The adapter consumes `update`,
  `refine_result`, `subtitle_retract`, `spec_translation_pending`,
  `spec_translation`, `clear`, and validated `vr_view_settings` events.
- The shared HUD has five fixed slots. Upper rows follow the selected
  `original`, `translation`, or `both` projection; slot 4 is always the live
  source row. Labels are composed by the renderer and are not part of the
  protocol body text.
- Consecutive sentences from the same speaker are concatenated into one display
  segment. A speaker change starts a new segment; visual line length is handled
  by the HUD layout/truncation layer rather than by discarding earlier text.
- Every row projects one page from its complete segment text. The page's left
  edge stays fixed as text grows; only overflow advances it. The next page
  prefers a clause beginning in the latter half of the full line, otherwise
  carries roughly a quarter line at a word/Unicode-cluster boundary. The newest
  text stays visible without a timer or backlog. Revisions reflow, and identical
  text has the same pages regardless of token batching. It never inserts an
  ellipsis, shrinks the font, scrolls horizontally, or consumes another slot;
  literal ellipses in subtitle content are preserved. Geometry reuse is separate
  from draw-time draft/final color.
- Silence uses compositor alpha (4 s idle, 1.2 s fade) and does not redraw on
  a timer. Unchanged projected frames are not submitted again.
- Emits `EVENT <json>` lines on stderr: `overlay_ready`, `connect_failed`,
  `no_hmd`, and `startup_error`.

## Verification

For an offline old/new long-line comparison (no providers or SteamVR):

```powershell
cargo run --manifest-path vr_overlay/Cargo.toml --example long_line_replay -- vr_overlay/target/long-line-preview.html
```

Open the generated HTML in a browser and play/scrub the same input through both
fitters. Its widths are schematic; the production HUD uses DirectWrite. This
checks page transitions, not HMD comfort. A long translation arriving in a
single update displays its final page immediately; earlier pages are not queued
for playback. See [the long-line change notes](docs/2026-10-19-long-line-pages.md).

```powershell
powershell -ExecutionPolicy Bypass -File .\vr_overlay\scripts\verify.ps1
```

The script distinguishes missing Windows build prerequisites from Rust test or
compilation failures. Automated tests stop at the frame submission interface.
The `both` projection completed an initial physical HMD smoke test on
2026-09-13 with no observed severe correctness issue. Physical acceptance is
still pending for `original`, `translation`, runtime mode switching, and the
remaining lifecycle/performance checklist. See the implementation plan's
2026-09-14 status update for the field-test details and provenance caveats.
