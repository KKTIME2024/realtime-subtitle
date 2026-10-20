//! End-to-end protocol parity replays.
//!
//! These tests feed backend-contract JSON frames (the exact shapes emitted by
//! `soniox_session.py`) through the production `DesktopProtocol` adapter and
//! the production `TranscriptState` reducer, then assert the transcript facts
//! and the projected HUD. They are the permanent replacement for the one-off
//! replay harness: every scenario here is a regression guard for a confirmed
//! desktop/VR semantic divergence.
//!
//! Contract facts verified against the backend source:
//! - `update.final_tokens` is an incremental delta (each final token is sent
//!   exactly once per stream; the desktop appends unconditionally).
//! - `update.non_final_tokens` is a replaceable snapshot and carries no
//!   `llm_sentence_id` (only final tokens are pairer-stamped).
//! - Separators are typed display line breaks; they never change sentence
//!   identity, and late translations keep the closed sentence's id.
//! - `refine_result` carries `sentence_id`, `source`,
//!   `original_translation`, `refined_translation` (null when `no_change`),
//!   `no_change`, and `target_lang`.
//! - `subtitle_retract` removes a sentence by id; the replacement is
//!   re-emitted by the backend as fresh final tokens.
//! - `spec_translation_pending` / `spec_translation` are keyed by exact
//!   source text.
//! - `clear {preserve_existing:true}` (stream rollover) keeps sentence ids
//!   alive; drafts are finalized, not dropped.

mod support;

use rinbridge_overlay::projection::project;
use rinbridge_overlay::protocol::{BridgeError, DesktopProtocol};
use rinbridge_overlay::transcript::{CaptionEvent, SpeakerKey, TrackPhase, TranscriptState};
use rinbridge_overlay::views::{DisplayMode, VrViewSettings};
use support::{spawn_scripted_server, test_manifest_with_url};

/// Run a scripted frame sequence through the production adapter and reducer.
async fn replay(frames: Vec<String>) -> Result<TranscriptState, BridgeError> {
    let (url, server) = spawn_scripted_server(frames);
    let manifest = test_manifest_with_url(&url);
    let mut protocol = DesktopProtocol::connect(&manifest).await?;
    let mut state = TranscriptState::default();
    let now = std::time::Instant::now();
    loop {
        match protocol.next_event().await {
            Ok(event) => state.apply(&event, now),
            Err(BridgeError::Disconnected) => break,
            Err(error) => {
                server.await.unwrap();
                return Err(error);
            }
        }
    }
    server.await.unwrap();
    Ok(state)
}

fn source_text(state: &TranscriptState, id: &str) -> Option<String> {
    state
        .sentence_by_upstream_id(id)
        .map(|record| record.source.text.clone())
}

fn target_of(state: &TranscriptState, id: &str) -> Option<(String, TrackPhase, Option<String>)> {
    state.sentence_by_upstream_id(id).map(|record| {
        (
            record.target.text.clone(),
            record.target.phase,
            record.target.language.clone(),
        )
    })
}

fn update(final_tokens: &str, non_final_tokens: &str) -> String {
    format!(
        "{{\"type\":\"update\",\"final_tokens\":[{final_tokens}],\"non_final_tokens\":[{non_final_tokens}]}}"
    )
}

fn source_token(text: &str, speaker: &str, id: Option<&str>) -> String {
    match id {
        Some(id) => format!(
            "{{\"text\":\"{text}\",\"speaker\":\"{speaker}\",\"translation_status\":\"original\",\"llm_sentence_id\":\"{id}\",\"is_final\":true}}"
        ),
        None => format!(
            "{{\"text\":\"{text}\",\"speaker\":\"{speaker}\",\"translation_status\":\"original\",\"is_final\":true}}"
        ),
    }
}

fn target_token(text: &str, speaker: &str, id: Option<&str>) -> String {
    match id {
        Some(id) => format!(
            "{{\"text\":\"{text}\",\"speaker\":\"{speaker}\",\"translation_status\":\"translation\",\"llm_sentence_id\":\"{id}\",\"is_final\":true}}"
        ),
        None => format!(
            "{{\"text\":\"{text}\",\"speaker\":\"{speaker}\",\"translation_status\":\"translation\",\"is_final\":false}}"
        ),
    }
}

fn separator(kind: &str) -> String {
    format!("{{\"is_separator\":true,\"is_final\":true,\"separator_type\":\"{kind}\"}}")
}

// ---------------------------------------------------------------- requirement 1

#[tokio::test]
async fn incremental_cjk_translation_appends_across_frames() {
    // 哈 then 哈哈 as incremental deltas must yield 哈哈哈, not 哈哈.
    let frames = vec![
        update(&target_token("哈", "1", Some("A")), ""),
        update(&target_token("哈哈", "1", Some("A")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "A").map(|(text, _, _)| text),
        Some("哈哈哈".to_string())
    );
}

// ---------------------------------------------------------------- requirement 2

#[tokio::test]
async fn late_translation_after_separator_still_appends() {
    // Source A closes with a display separator; its translation keeps
    // streaming under the same id afterwards and must not be truncated.
    let frames = vec![
        update(
            &format!(
                "{},{}",
                source_token("你好", "1", Some("A")),
                target_token("hello", "1", Some("A"))
            ),
            "",
        ),
        update(
            &format!(
                "{},{},{}",
                source_token("。", "1", Some("A")),
                separator("punctuation"),
                source_token("世界", "1", Some("B"))
            ),
            "",
        ),
        update(&target_token(" world", "1", Some("A")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(source_text(&state, "A").as_deref(), Some("你好。"));
    assert_eq!(source_text(&state, "B").as_deref(), Some("世界"));
    assert_eq!(
        target_of(&state, "A").map(|(text, _, _)| text),
        Some("hello world".to_string())
    );
}

// ---------------------------------------------------------------- requirement 3

#[tokio::test]
async fn subtitle_retract_removes_only_the_retracted_sentence() {
    let frames = vec![
        update(
            &format!(
                "{},{},{}",
                source_token("first", "1", Some("A")),
                target_token("one", "1", Some("A")),
                separator("punctuation")
            ),
            "",
        ),
        update(
            &format!(
                "{},{}",
                source_token("second", "1", Some("B")),
                target_token("two", "1", Some("B"))
            ),
            "",
        ),
        r#"{"type":"subtitle_retract","sentence_id":"A","replacement_sentence_id":"M","reason":"short_interrupt"}"#
            .to_string(),
        // The backend re-emits the merged replacement as fresh final tokens.
        update(
            &format!(
                "{},{},{}",
                separator("interrupt_repair"),
                source_token("first second", "1", Some("M")),
                target_token("one two", "1", Some("M"))
            ),
            "",
        ),
    ];
    let state = replay(frames).await.unwrap();
    assert!(
        state.sentence_by_upstream_id("A").is_none(),
        "A must be gone"
    );
    assert_eq!(source_text(&state, "B").as_deref(), Some("second"));
    assert_eq!(
        target_of(&state, "B").map(|(text, _, _)| text),
        Some("two".to_string())
    );
    assert_eq!(source_text(&state, "M").as_deref(), Some("first second"));
    assert_eq!(
        target_of(&state, "M").map(|(text, _, _)| text),
        Some("one two".to_string())
    );
}

#[tokio::test]
async fn late_refine_for_retracted_sentence_is_dropped() {
    let frames = vec![
        update(&source_token("frag", "1", Some("A")), ""),
        r#"{"type":"subtitle_retract","sentence_id":"A","replacement_sentence_id":"M","reason":"short_interrupt"}"#
            .to_string(),
        r#"{"type":"refine_result","sentence_id":"A","source":"frag","original_translation":"x","refined_translation":"late","no_change":false,"target_lang":"fr"}"#
            .to_string(),
    ];
    let state = replay(frames).await.unwrap();
    assert!(state.sentence_by_upstream_id("A").is_none());
    // The late refinement must not resurface on a later event.
    let mut state = state;
    state.apply(
        &CaptionEvent::SourceCommitted(rinbridge_overlay::transcript::CommittedSource {
            speaker: SpeakerKey::Diarized("1".into()),
            sentence_id: Some("A".into()),
            text: "new A".into(),
            language: None,
        }),
        std::time::Instant::now(),
    );
    assert!(
        state.sentence_by_upstream_id("A").is_none(),
        "retracted id must not be revived by any late event"
    );
}

#[tokio::test]
async fn late_source_and_target_for_retracted_id_are_dropped() {
    let frames = vec![
        update(&source_token("old", "1", Some("A")), ""),
        r#"{"type":"subtitle_retract","sentence_id":"A","replacement_sentence_id":"M","reason":"short_interrupt"}"#
            .to_string(),
        update(
            &format!(
                "{},{}",
                source_token(" late", "1", Some("A")),
                target_token("late target", "1", Some("A"))
            ),
            "",
        ),
    ];

    let state = replay(frames).await.unwrap();

    assert!(state.sentence_by_upstream_id("A").is_none());
}

// ---------------------------------------------------------------- requirement 4

#[tokio::test]
async fn idless_streaming_translation_binds_after_source_final() {
    // Non-final translation tokens carry no sentence id. When the source
    // sentence is already committed, the desktop still binds the late draft
    // to that sentence; VR must not drop it.
    let frames = vec![
        update(
            &format!(
                "{},{}",
                source_token("你好。", "1", Some("A")),
                separator("punctuation")
            ),
            "",
        ),
        update("", &target_token("hello", "1", None)),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "A").map(|(text, phase, _)| (text, phase)),
        Some(("hello".to_string(), TrackPhase::Draft))
    );
}

#[tokio::test]
async fn idless_translation_with_no_original_becomes_its_own_sentence() {
    let frames = vec![update("", &target_token("orphan", "2", None))];
    let state = replay(frames).await.unwrap();
    assert_eq!(state.sentences.len(), 1);
    assert_eq!(state.sentences[0].target.text, "orphan");
    assert_eq!(state.sentences[0].source.phase, TrackPhase::Empty);
}

// ---------------------------------------------------------------- requirement 5

#[tokio::test]
async fn spec_pending_and_result_change_the_sentence_state() {
    // The backend fires speculative frames keyed by exact source text while
    // the sentence is still non-final; the draft record must receive the
    // pending language and then the speculative translation.
    let frames = vec![
        update("", &format!("{{\"text\":\"你好。\",\"speaker\":\"1\",\"translation_status\":\"original\",\"is_final\":false}}")),
        r#"{"type":"spec_translation_pending","source":"你好。","target_lang":"fr"}"#
            .to_string(),
        r#"{"type":"spec_translation","source":"你好。","translation":"bonjour","target_lang":"fr"}"#
            .to_string(),
        // The sentence commits with the exact source text the spec was fired for.
        update(&source_token("你好。", "1", Some("A")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "A"),
        Some(("bonjour".to_string(), TrackPhase::Draft, Some("fr".into())))
    );
}

#[tokio::test]
async fn spec_result_arriving_before_the_sentence_is_replayed() {
    // Speculative frames can land before the sentence's final tokens.
    let frames = vec![
        r#"{"type":"spec_translation_pending","source":"你好。","target_lang":"fr"}"#
            .to_string(),
        r#"{"type":"spec_translation","source":"你好。","translation":"bonjour","target_lang":"fr"}"#
            .to_string(),
        update(&source_token("你好。", "1", Some("A")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "A"),
        Some(("bonjour".to_string(), TrackPhase::Draft, Some("fr".into())))
    );
}

// ---------------------------------------------------------------- requirement 6

#[tokio::test]
async fn refine_result_replaces_the_speculative_translation() {
    let frames = vec![
        update(&source_token("你好。", "1", Some("A")), ""),
        r#"{"type":"spec_translation","source":"你好。","translation":"bonjour","target_lang":"fr"}"#
            .to_string(),
        r#"{"type":"refine_result","sentence_id":"A","source":"你好。","original_translation":"bonjour","refined_translation":"bonsoir","no_change":false,"target_lang":"fr"}"#
            .to_string(),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "A"),
        Some((
            "bonsoir".to_string(),
            TrackPhase::Refined,
            Some("fr".into())
        ))
    );
}

#[tokio::test]
async fn refine_no_change_confirms_the_existing_translation() {
    // no_change:true is a confirmation, not a no-op: the draft target becomes
    // settled and the speculative pending marker is retired.
    let frames = vec![
        update(&source_token("你好。", "1", Some("A")), ""),
        r#"{"type":"spec_translation","source":"你好。","translation":"bonjour","target_lang":"fr"}"#
            .to_string(),
        r#"{"type":"refine_result","sentence_id":"A","source":"你好。","original_translation":"bonjour","refined_translation":null,"no_change":true,"target_lang":"fr"}"#
            .to_string(),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "A"),
        Some((
            "bonjour".to_string(),
            TrackPhase::Committed,
            Some("fr".into())
        ))
    );
    assert_eq!(state.spec_pending_for("你好。"), None);
}

// ---------------------------------------------------------------- requirement 7

#[tokio::test]
async fn target_lang_reaches_the_projected_display_row() {
    let frames = vec![
        update(&source_token("hello", "1", Some("A")), ""),
        r#"{"type":"refine_result","sentence_id":"A","source":"hello","original_translation":"","refined_translation":"salut","no_change":false,"target_lang":"fr"}"#
            .to_string(),
    ];
    let state = replay(frames).await.unwrap();
    let settings = VrViewSettings {
        display_mode: DisplayMode::Translation,
        ..Default::default()
    };
    let frame = project(&state, &settings, std::time::Instant::now()).frame;
    let row = frame
        .slots
        .iter()
        .flatten()
        .find(|row| row.text == "salut")
        .expect("refined row must be projected");
    assert_eq!(row.language.as_deref(), Some("fr"));
}

// ---------------------------------------------------------------- requirement 8

#[tokio::test]
async fn preserve_existing_keeps_committed_and_finalizes_drafts() {
    // A stream rollover: committed content survives, the in-progress draft is
    // finalized (not dropped), and the same sentence id keeps accumulating
    // across the clear.
    let frames = vec![
        update(
            &format!(
                "{},{},{}",
                source_token("done", "1", Some("A")),
                target_token("fait", "1", Some("A")),
                separator("punctuation")
            ),
            "",
        ),
        update(
            "",
            &format!(
                "{{\"text\":\"draft\",\"speaker\":\"1\",\"translation_status\":\"original\",\"is_final\":false}}"
            ),
        ),
        r#"{"type":"clear","message":"Recognition reconnecting...","preserve_existing":true}"#
            .to_string(),
        update(&source_token(" more", "1", Some("B")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(source_text(&state, "A").as_deref(), Some("done"));
    assert_eq!(
        target_of(&state, "A").map(|(text, _, _)| text),
        Some("fait".to_string())
    );
    // The draft was finalized into a committed record, not dropped.
    let draft_record = state
        .sentences
        .iter()
        .find(|record| record.source.text == "draft")
        .expect("draft content must survive the rollover");
    assert_eq!(draft_record.source.phase, TrackPhase::Committed);
    // The new sentence accumulated fresh on the other side of the clear.
    assert_eq!(source_text(&state, "B").as_deref(), Some(" more"));
}

#[tokio::test]
async fn full_clear_drops_everything() {
    let frames = vec![
        update(&source_token("done", "1", Some("A")), ""),
        r#"{"type":"clear","message":"Recognition restarting..."}"#.to_string(),
        update(&source_token("fresh", "1", Some("B")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert!(state.sentence_by_upstream_id("A").is_none());
    assert_eq!(source_text(&state, "B").as_deref(), Some("fresh"));
}

// ---------------------------------------------------------------- requirement 9

#[tokio::test]
async fn repeated_text_and_multi_speaker_interleave() {
    // Repeated identical deltas are content; two speakers interleaving must
    // keep per-sentence accumulators independent.
    let frames = vec![
        update(&source_token("哈", "1", Some("A")), ""),
        update(&source_token("哈", "1", Some("A")), ""),
        update(&source_token("yo", "2", Some("B")), ""),
        update(&source_token("哈", "1", Some("A")), ""),
        update(&source_token("yo", "2", Some("B")), ""),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(source_text(&state, "A").as_deref(), Some("哈哈哈"));
    assert_eq!(source_text(&state, "B").as_deref(), Some("yoyo"));
    assert_eq!(
        state
            .sentences
            .iter()
            .filter(|record| record.speaker == SpeakerKey::Diarized("1".into()))
            .count(),
        1
    );
}

#[tokio::test]
async fn multi_speaker_translation_binding_stays_within_speaker() {
    // Speaker 2's id-less draft must bind to speaker 2's sentence, never to
    // speaker 1's, even when speaker 1 is more recent.
    let frames = vec![
        update(&source_token("one", "1", Some("A")), ""),
        update(&source_token("two", "2", Some("B")), ""),
        update("", &target_token("deux", "2", None)),
    ];
    let state = replay(frames).await.unwrap();
    assert_eq!(
        target_of(&state, "B").map(|(text, _, _)| text),
        Some("deux".to_string())
    );
    assert_eq!(
        target_of(&state, "A").map(|(text, _, _)| text),
        Some(String::new())
    );
}
