# VR Protocol Parity Report

Date: 2026-10-11 (follow-up) · Branch: `codex/vr-subtitle-rust-only`
Scope: semantic parity between the Rust VR overlay (`vr_overlay/`) and the
desktop reference client for the desktop raw `/ws` caption protocol emitted by
`soniox_session.py`.

---

## 1. Executive Summary

The VR overlay's protocol layer had **six confirmed P0 divergences** and
**four P1 divergences** from the backend contract and the desktop reference.
All of them were in the two layers that own protocol semantics —
`protocol.rs` (adapter) and `transcript.rs` (state reducer) — and all of them
are now fixed with permanent regression tests.

The single most important contract fact, which the old VR code got wrong, is
that **`update.final_tokens` is an incremental delta, not a snapshot**: the
backend tracks `sent_count` per stream and sends each final token exactly
once, and the desktop appends unconditionally. The old VR adapter instead
guessed prefix relationships and replayed/deduplicated frames, which corrupted
repeated content (e.g. `哈` + `哈哈` → `哈哈` instead of `哈哈哈`) and the old
"replace on next separator" flag truncated translations that kept streaming
after their source sentence's display separator.

`subtitle_retract`, `spec_translation_pending`, `spec_translation`,
`refine_result.no_change`, `refine_result.target_lang`, and
`clear {preserve_existing: true}` were all either ignored or misinterpreted;
they are now implemented to match the desktop reference semantics.

The follow-up also implements the previously deferred display behavior:
consecutive backend sentences from one speaker are concatenated into one
projection segment. The transcript keeps sentence ids separate for repair and
translation attribution; only the display projection combines them.

**Current result:** the complete Windows crate passes 259 tests with 0 failures
and 1 ignored test; the release build succeeds. Physical HMD verification
remains outstanding (see §6).

---

## 2. Protocol Contract

Verified against `soniox_session.py` (the single source of truth) and the
desktop reference (`static/js/subtitle-frame-controller.js`,
`static/js/subtitle-session.js`, `static/js/refine-state.js`,
`static/js/token-stream.js`, `sentence_pairing.py`).

### 2.1 Event types on `/ws`

| Type | Fields | Semantics |
|---|---|---|
| `update` | `final_tokens[]`, `non_final_tokens[]` | The only streaming frame. |
| `refine_result` | `sentence_id`, `source`, `original_translation`, `refined_translation` (null when `no_change`), `no_change`, `target_lang` | Post-hoc translation refinement for a committed sentence. |
| `spec_translation_pending` | `source`, `target_lang` | Speculative translation in flight, keyed by **exact source text** (no sentence id). |
| `spec_translation` | `source`, `translation`, `target_lang` | Speculative translation result, keyed by exact source text. |
| `subtitle_retract` | `sentence_id`, `replacement_sentence_id`, `reason` | Interrupt repair: the retracted sentence is removed; the merged replacement is **re-emitted by the backend** as fresh final tokens under `replacement_sentence_id` (preceded by an `interrupt_repair` separator). |
| `clear` | `message`, `preserve_existing?` | `preserve_existing: true` = stream rollover (reconnect); sentence ids stay alive and in-progress sentences keep receiving tokens. Absent/false = full session reset. |
| `vr_view_settings` | view settings object | Broadcast by the desktop connection panel; the VR overlay is a pure consumer. |
| `shutdown` | — | Graceful exit. |

### 2.2 Token semantics (the core of the contract)

- **`final_tokens` is an incremental delta.** Each final token is sent exactly
  once per stream (`sent_count` bookkeeping in `soniox_session.py`); the
  desktop appends unconditionally. Repeated identical text across frames is
  valid content, never a replay.
- **`non_final_tokens` is a replaceable snapshot** of the current interim
  recognition. It carries **no `llm_sentence_id`** — only final tokens are
  stamped by the pairer.
- **Sentence identity is the `llm_sentence_id`** stamped on final source and
  final translation tokens (except IPC-injected speaker `"0"` text and
  separators, which have no id).
- **Separators are typed display line breaks** (`punctuation`, `translation`,
  `endpoint`, `speaker_change`, `interrupt_repair`, `reconnect`). They do
  **not** change sentence identity: a new sentence after a separator gets a
  **new** id, and late translations for the closed sentence keep the **old**
  id and keep appending.
- **`refine_result.no_change: true`** is a *confirmation* of the existing
  translation (desktop: `confirmedSentenceIds.add(id)`), not a no-op.
- **`refine_result.target_lang`** is the language of the (refined or
  confirmed) translation.
- **Speculative frames are keyed by exact source text**; a revised
  recognition simply does not match (desktop behavior).
- **Retracted ids are dead**: the backend drops late refine results for them
  (`refine_dropped_retracted`), and the desktop marks them retracted so no
  cached state resurfaces.
- **`clear {preserve_existing: true}`** finalizes the current non-final
  tokens into final content (desktop: `finalizeCurrentNonFinalTokens()` + a
  `reconnect` separator) and keeps all committed content; sentence ids
  survive the rollover.

### 2.3 What the desktop does (reference semantics)

- Appends every final token to the per-sentence accumulator
  (`token-stream.js`); no prefix guessing, no dedup.
- Binds id-less (non-final) translation tokens to the speaker's most recent
  sentence **that has original text**, even when that sentence is already
  closed; a translation with no original at all becomes its own sentence.
- `retract`: removes **all** final tokens with the retracted id, re-merges,
  invalidates the render cache, and marks the id retracted
  (`refine-state.js: markRetracted`).
- `refine_result`: stores the refined translation per id (and per-source
  language); `no_change` stores a per-sentence confirmation; retracted ids
  are ignored; a finalized result retires the spec-pending marker for that
  source.
- `clear {preserve_existing}`: finalizes pending non-final tokens as final
  content, keeps everything, and unblocks pending "hide after sequence"
  states.

---

## 3. Confirmed Divergences (before this change)

### P0 — transcript correctness

| # | Divergence | Old VR behavior | Contract behavior | Evidence |
|---|---|---|---|---|
| 1 | **Incremental deltas misread as snapshots** | Prefix heuristic: if the new frame's text started with the old accumulated text it was treated as a cumulative snapshot and *replaced*; otherwise appended. No replay dedup existed at the adapter level, but the heuristic silently dropped content. | Pure append; repeated text is content. | `soniox_session.py` `sent_count`; desktop `token-stream.js` append. Old tests encoded the buggy behavior. |
| 2 | **Separator truncation** | `replace_on_next` flag set on the accumulators of the tracks that saw a separator; the next token for the same track *replaced* the accumulated text. | Separators never reset id-keyed accumulators; late translations keep appending under the old id. | Backend re-opens a **new** id after a separator; late translations keep the old id. |
| 3 | **Fingerprint replay-dedup** | A frame whose token fingerprint matched a recent one was dropped as a "replay". | No dedup: identical frames are legitimate repeated content. | Contract §2.2. |
| 4 | **`subtitle_retract` ignored** | Mapped to a no-content `Activity` event; the retracted sentence stayed on screen forever. | Remove the sentence by id; the backend re-emits the replacement. | `soniox_session.py:1377`; desktop `session.retract()`. |
| 5 | **Id-less streaming translation dropped after source final** | Non-final translation tokens (no id) bound only to an *open* draft; once the source sentence was committed, late drafts were dropped. | Bind to the speaker's most recent sentence with original text, open or closed; else create its own sentence. | Desktop `associateLiveTranslation` / `findOriginalSentenceBySpeaker`. |
| 6 | **`clear {preserve_existing: true}` lost draft content** | Both clear kinds wiped the transcript. | Preserve committed content; finalize (not drop) drafts; keep accumulators so in-progress sentences continue across the rollover. | Desktop `finalizeCurrentNonFinalTokens()` + `revealPendingForRestart()`. |

### P1 — missing lifecycle / metadata

| # | Divergence | Old VR behavior | Contract behavior |
|---|---|---|---|
| 7 | **Speculative translation ignored** | `spec_translation_pending` / `spec_translation` frames were not parsed at all. | Pending marker (source → language) and spec result (source → translation) keyed by exact source text; applied to the matching sentence (draft target only); pending results replayed when the sentence's final tokens arrive. |
| 8 | **`refine_result.no_change` ignored** | Treated like any other refine (or dropped when `refined_translation` was null). | Confirmation: the sentence's draft target is promoted to committed; the spec-pending marker for that source is retired; retracted ids are ignored. |
| 9 | **`refine_result.target_lang` dropped** | `Refinement.language` was never populated (always `None`). | Carried into the target track's language and into the projected display row. |
| 10 | **Untyped separators** | Separator was an untyped event; `separator_type` was discarded. | Typed (`SeparatorKind`): punctuation/translation/endpoint/speaker_change/interrupt_repair/reconnect/unknown; only the id-less source track (IPC text) resets on a separator. |

### Explicitly not divergences (verified, no change needed)

- `display_mode` / `max_speakers` / `bilingual_pair_count` settings transport:
  already a validated consumer. Projection grouping itself did require the
  follow-up described in §4.3.
- `vr_view_settings` handling: already a validated consumer. No change.
- Backend event *formats*: unchanged. All fixes are VR-side.

---

## 4. Changes Made

All changes are under `vr_overlay/`. No backend or desktop files were changed.
The working tree contains unrelated pre-existing renderer/build changes;
running the required crate-wide formatter normalized their formatting but did
not add renderer behavior changes.

### 4.1 `src/transcript.rs` (state reducer)

- New `SeparatorKind` enum (7 variants) and new `CaptionEvent` variants:
  `TargetConfirmed`, `Retract`, `Separator`, `SpecPending`,
  `SpecTranslation`.
- New state: session-lifetime `retracted_ids`, bounded FIFO `spec_pending`
  (normalized exact source → language), persistent bounded
  `spec_translations` cache, plus the existing `pending_refinements`.
- `apply_target` (draft/committed): id-less targets now bind to the speaker's
  most recent record **with non-empty source text** (open or closed); a
  target with no original at all becomes its own sentence. Phase downgrades
  (Committed/Refined → Draft) are still rejected.
- `apply_retract`: marks the id retracted, drops pending refinements for it,
  removes the record and matching live row. Late source, target, source-end,
  refine, and confirm events for retracted ids are dropped.
- `apply_separator`: finalizes the live row's open draft and settles the live
  row; **never** resets id-keyed accumulators.
- `apply_spec_pending` / `apply_spec_translation`: trimmed exact-source-text
  binding; pending projects a draft placeholder; cached results only fill or
  refresh Empty/Draft targets (committed/refined are authoritative) and are
  reusable when the same final source text appears again.
- `apply_refinement`: retracted-id guard, spec-pending retirement by source,
  carries `language` + `source`.
- `apply_target_confirmed`: draft target promoted to committed; retracted-id
  guard; spec-pending retirement.
- `clear(true)`: keeps all records and caches, promotes Draft → Committed, and
  drops only fully-empty records. `clear(false)` also clears retract tombstones
  and speculative session state.

### 4.2 `src/protocol.rs` (adapter)

- **Removed** the prefix heuristic, the `replace_on_next` separator flag, and
  the fingerprint replay-dedup. `accumulate_final_tokens` now appends every
  final token to the per-`(sentence_id, speaker, track)` accumulator and
  emits the full accumulated text.
- Separators: typed emission (`emit_separator` maps `separator_type`); the
  separator flushes in-flight final groups and **only** resets the id-less
  source accumulator (IPC text), so each displayed IPC segment starts fresh
  while id-keyed accumulators survive.
- `process_retract`, `process_spec_pending`, `process_spec_translation`
  added (validate required fields, fall back to `Activity` on malformed
  frames).
- `process_refine`: emits `TargetConfirmed` on `no_change`; carries
  `target_lang` and `source` on `RefinedTarget`; falls back to
  `original_translation` when `refined_translation` is empty.
- `clear`: `preserve_existing` is now parsed and forwarded; only a full clear
  resets the accumulators (a rollover keeps sentence ids alive).

### 4.3 `src/projection.rs`

- Consecutive sentence records from the same speaker are reduced into one
  display segment. A speaker change starts a new segment.
- `original` and `translation` concatenate the current segment rather than
  selecting only its final sentence. `both` concatenates complete sentence
  pairs only, preserving source/target identity and never cross-pairing.
- Spec-pending state produces a visible draft placeholder, so the pending
  lifecycle is represented before translation text arrives.

### 4.4 `src/runtime.rs`

- `log_caption_event` / `event_speaker` / `event_sentence_id` /
  `event_text`: exhaustive handling of the new event variants (diagnosable
  logs for retract, confirm, spec, and typed separators).
- `apply_event`: `SpecPending` and `SpecTranslation` count as potentially
  visible updates for visibility timing.

### 4.5 `src/lib.rs`

- Re-exports the new public types (`SeparatorKind`, `SpecTranslation`,
  `LiveInputRow`, `LiveSourceSnapshot`, `SentenceKey`, `SentenceRecord`,
  `TextTrack`).

### 4.6 Tests

- `tests/protocol.rs`: rewrote three tests that had encoded the old buggy
  behavior (`final_source_true_delta_appends_and_replay_dedupes`,
  `separator_replaces_each_same_track_accumulator_independently`,
  `trailing_separator_applies_after_the_preceding_delta`) to encode the
  verified contract; added adapter tests for retract, spec frames,
  `no_change` confirmation, `target_lang`, and both clear kinds.
- `tests/protocol_replay.rs` (**new**, 16 tests): end-to-end replays feeding
  backend-contract JSON frames through the production
  `DesktopProtocol` → `TranscriptState` → `project()` pipeline, covering all
  nine required scenarios (incremental CJK append, late translation across a
  separator, retract isolation, id-less binding after source final, spec
  lifecycle, refine replacement, `no_change` confirmation, `target_lang`
  reaching the projected row, `preserve_existing` retention, repeated text
  and multi-speaker interleaving).
- `tests/projection.rs`: regression coverage for same-speaker source and
  bilingual segment concatenation, speaker-interruption boundaries, strict
  pair attribution, and the speculative-pending placeholder.
- `tests/coordinator.rs`: regression coverage that a newly visible
  speculative-pending placeholder wakes translation mode.
- `tests/support/mod.rs`: the `refine()` fixture helper now includes
  `source` and `target_lang` fields matching the real backend frame.

---

## 5. Test Results

### Windows toolchain note

The host process exposes both `Path` and `PATH`, which makes this MSBuild
version fail with `MSB6001` when CMake builds in parallel. Verification used a
one-off process wrapper that passed CMake a de-duplicated environment and
`cargo -j 1`; it did not modify the repository or system environment. The
temporary core harness and wrapper were removed after verification.

### Commands and results

```
cargo test --manifest-path vr_overlay/Cargo.toml -j 1 --quiet
cargo build --manifest-path vr_overlay/Cargo.toml --release -j 1
```

| Suite | Result |
|---|---|
| crate unit tests | 107 passed, 0 failed |
| `tests/coordinator.rs` | 20 passed, 0 failed |
| `tests/projection.rs` | 17 passed, 0 failed |
| `tests/protocol.rs` | 20 passed, 0 failed |
| `tests/protocol_replay.rs` (new) | 16 passed, 0 failed |
| renderer integration | 71 passed, 0 failed, 1 ignored |
| `tests/runtime.rs` | 8 passed, 0 failed |
| **Total** | **259 passed, 0 failed, 1 ignored** |

### Not run (and why)

- The repository-wide `cargo fmt --check` gate passes using the crate's Rust
  2021 edition.
- Desktop JS/Python test suites: no desktop or backend code was modified, so
  no re-run was required; the desktop files were read-only references.
- Physical SteamVR/HMD visibility: requires a headset; automated tests do
  not establish it (per `vr_overlay/AGENTS.md`).

---

## 6. Remaining Issues

1. **HMD verification outstanding.** Row behavior for retract (row
   disappears, replacement row appears), spec translation (draft row appears
   before source final), and `no_change` confirmation must be eyeballed on a
   headset with a real session, including an interrupt/repair event.
2. **`spec_translation` on an already-committed target is deliberately
   ignored** (committed/refined targets are authoritative). If the backend
   ever emits a spec result *after* a refine for the same sentence, the
   refined text wins — this matches the desktop (spec maps are display
   hints, refine results are stored per id), but it has not been observed in
   a live session and is worth confirming during the HMD test.
3. **Exact-text spec keying is brittle by design** (backend contract): a
   revised recognition that changes the source text will not match the
   pending spec. Same as desktop; noted so it is not "fixed" later by
   fuzzy-matching without a backend change.
4. **Pre-existing working-tree changes** (renderer, docs, Cargo.toml,
   LICENSE, etc.) remain mixed into the uncommitted tree. The combined tree now
   passes the Windows test/build gate, but patch ownership should still be split
   before review if a clean protocol-only diff is required.

---

## 7. Architecture Recommendation

Keep the three-layer split, and enforce it:

```
protocol.rs   adapter: JSON frames -> typed CaptionEvent (no state)
transcript.rs reducer: CaptionEvent -> TranscriptState (all protocol facts)
projection.rs projector: TranscriptState + settings -> HudFrame (display only)
```

- **Transcript owns all protocol facts**: sentence identity, source/target
  text, phase (Draft/Committed/Refined), language, retract, spec, confirm,
  and history. Projection must never patch transcript data errors — if a
  projected row looks wrong, the bug is in the reducer and the replay tests
  should catch it.
- **Projection owns display strategy only**: row selection, speaker ordering,
  strict pair attribution, the reserved live row, and utterance-segment
  grouping. Consecutive same-speaker sentence records are combined only in
  this pure `(TranscriptState, settings) -> HudFrame` layer and never fed back
  into the reducer.
- **Events are the contract boundary.** Any new backend frame type gets:
  (1) an adapter parser in `protocol.rs` with an `Activity` fallback for
  malformed frames, (2) a reducer method in `transcript.rs`, (3) a replay
  test in `tests/protocol_replay.rs` using the exact backend JSON shape.
  This keeps the parity surface small and testable on any OS.
- **Do not reintroduce heuristics in the adapter.** The prefix/dedup
  heuristics were the root cause of the P0 class of bugs. The adapter should
  stay a mechanical, stateless-per-frame mapping; all accumulation lives in
  the reducer where it is unit-testable.

---

## 8. Next Actions

1. **HMD session test**: run a live session covering (a) repeated CJK
   content, (b) a late translation after a punctuation separator, (c) an
   interrupt/repair retract, (d) a speculative translation visible before
   source final, (e) a `no_change` refine, (f) a reconnect rollover
   (`clear {preserve_existing: true}`).
2. **Consider (optional, no action taken)**: moving a core harness into
   the repo (e.g. `vr_overlay/tests/` with a `#[cfg]`-gated renderer stub)
   so the core protocol surface is CI-testable on Linux in the future. Not
   done here to avoid expanding the change set.
