# Long lines inside the existing HUD slots

The headset feedback was that captions were readable at speaking speed, but
continually removing the beginning made it hard to recover which sentence or
word the row belonged to. The five slot roles, speaker ordering, bilingual
pairing and permanent live-source row remain the existing projection contract.

## Behavior

The renderer keeps the current page's left edge until it overflows. It then
starts another page in that same physical row. A punctuation boundary in the
latter half of the full row is preferred. Otherwise the new page carries
approximately the last quarter of the previous row, starting at a word boundary
when available. Unpunctuated CJK and words wider than a line use intact Unicode
clusters. This overlap provides a small amount of reading context while leaving
room for the next words to arrive without shifting the start again.

Only the full page and its first overflowing cluster determine the next start.
Consequently replaying one token at a time or receiving the same text in one
update yields the same page. The append cache avoids reshaping all historical
pages. Corrections, shrinking drafts and changed width/font/language reflow the
text; an added scalar that extends a grapheme also reflows. Cached pagination
follows text across slot moves and is bounded to the five current rows.

The reducer still retains the full text. There is no change to sentence ids,
source/translation ownership, live hold, fade, row size, font size or redraw
deadlines. No synthetic punctuation is added. An indivisible cluster wider than
the entire row cannot be displayed at the configured scale and is skipped whole.

## Comparison and remaining judgment

Run from the repository root on Windows:

```powershell
cargo run --manifest-path vr_overlay/Cargo.toml --example long_line_replay -- vr_overlay/target/long-line-preview.html
```

The standalone HTML compares the old fitter and the production page fitter on
Chinese with/without punctuation and English. It supports stepping/scrubbing,
playback, three schematic widths and choosing a fixed slot. Both fitters receive
the same input and width. This is an algorithm comparison; it does not reproduce
DirectWrite metrics or headset optics.

Headset acceptance should focus on whether the carryover is enough to recognize
the sentence, whether the larger but less frequent page changes feel comfortable,
and whether translation corrections cause distracting reflow. The half/quarter
line choices are starting values, not empirically validated comfort thresholds.

A complete long translation arriving all at once still shows only its last
page. This change does not add timed playback of unseen text. Bilingual rows
remain paired at the display-segment level; pagination is independent per row
because the two languages have different lengths, not semantically aligned
sub-sentence pages.

## Verification

Regression coverage checks stable starts, retained context, batching equivalence,
CJK/English punctuation, literal ellipses, Unicode cluster integrity, revisions,
width changes and the append measurement fast path. HUD tests check all five
physical origins, unchanged full projected text, recency moves, draft/final color
changes, stable signatures and clearing pagination state.

The existing Windows test suite and release/packaging checks are the platform
gate. Physical SteamVR comfort remains a headset check.
