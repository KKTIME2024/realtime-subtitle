//! Offline comparison of the old sliding suffix and the current HUD fitter.
//! cargo run --manifest-path vr_overlay/Cargo.toml --example long_line_replay -- <output.html>
//! No provider, microphone, SteamVR, or network connection is used.

#[allow(dead_code)]
#[path = "../src/renderer/text_fit.rs"]
mod text_fit;

use serde_json::json;
use text_fit::{fit_newest_text_with_measure, PagedRowText};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "long-line-preview.html".into());
    let samples = [
        ("中文长句", "我昨天去了一家新开的店，那里的拉面特别好吃，而且老板还给我们推荐了一道平时菜单上没有的菜。下次我们可以一起去，不过最好提前问一下，因为周末人可能会比较多。"),
        ("暂时没有标点的原文", "我刚才其实是想说这个地方虽然看起来有点远但是坐地铁过去并不需要很久而且我们可以在回来的路上顺便去你上次提到的那家书店看看有没有新到的书然后再决定晚上去哪里吃饭"),
        ("English", "I was trying to say that the new place is a little farther away, but the train ride is actually quite short. We could stop at that bookshop you mentioned on the way back, and then decide where to have dinner."),
    ];
    // A declared schematic width, not a substitute for Windows DirectWrite
    // measurement. Both algorithms receive exactly the same input and budget.
    let measure = |text: &str| {
        text.chars()
            .map(|ch| if ch.is_ascii() { 1.0 } else { 2.0 })
            .sum()
    };
    let mut cases = Vec::new();
    for (name, source) in samples {
        for budget in [36.0, 48.0, 64.0] {
            let mut page = PagedRowText::default();
            let mut text = String::new();
            let mut frames = Vec::new();
            let mut old_start = 0;
            let mut new_start = 0;
            let mut old_moves = 0;
            let mut new_moves = 0;
            for ch in source.chars() {
                text.push(ch);
                let old = fit_newest_text_with_measure(&text, budget, &measure);
                let new = page.fit(&text, budget, &measure);
                let next_old = text.len() - old.len();
                let next_new = text.len() - new.len();
                old_moves += usize::from(next_old != old_start);
                new_moves += usize::from(next_new != new_start);
                frames.push(json!({
                    "source": text, "old": old, "new": new,
                    "oldMoves": old_moves, "newMoves": new_moves,
                    "oldTurn": next_old != old_start, "newTurn": next_new != new_start,
                }));
                old_start = next_old;
                new_start = next_new;
            }
            cases.push(json!({"name": name, "budget": budget, "frames": frames}));
        }
    }
    let data = serde_json::to_string(&cases)?.replace('<', "\\u003c");
    let html = include_str!("long_line_replay.html").replace("__REPLAY_DATA__", &data);
    std::fs::write(&output, html)?;
    println!("Wrote {output}");
    Ok(())
}
