mod support;

use rinbridge_overlay::protocol::{BridgeError, DesktopProtocol};
use rinbridge_overlay::transcript::{CaptionEvent, SpeakerKey};
use rinbridge_overlay::views::DisplayMode;
use support::{spawn_scripted_server, test_manifest_with_url};

fn ts_delta(text: &str, id: &str) -> String {
    format!(
        "{{\"type\":\"update\",\"final_tokens\":[{{\"text\":\"{text}\",\"speaker\":\"1\",\"translation_status\":\"original\",\"llm_sentence_id\":\"{id}\",\"is_final\":true}}],\"non_final_tokens\":[]}}"
    )
}

async fn adapter_events(frames: Vec<String>) -> Result<Vec<CaptionEvent>, BridgeError> {
    let (url, server) = spawn_scripted_server(frames);
    let manifest = test_manifest_with_url(&url);
    let mut protocol = DesktopProtocol::connect(&manifest).await?;
    let mut out = Vec::new();
    loop {
        match protocol.next_event().await {
            Ok(event) => out.push(event),
            Err(BridgeError::Disconnected) => break,
            Err(error) => return Err(error),
        }
    }
    server.await.unwrap();
    Ok(out)
}

fn committed_texts(events: &[CaptionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            CaptionEvent::SourceCommitted(committed) => Some(committed.text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn one_frame_can_yield_multiple_events_through_the_pending_queue() {
    let frames = vec![r#"{"type":"update","final_tokens":[
        {"text":"hi","speaker":"1","translation_status":"original","llm_sentence_id":"A","is_final":true},
        {"text":"salut","speaker":"1","translation_status":"translation","llm_sentence_id":"A","is_final":true}],
        "non_final_tokens":[{"text":"live","speaker":"2","translation_status":"original","is_final":false}]}"#
        .to_string()];
    let (url, server) = spawn_scripted_server(frames);
    let manifest = test_manifest_with_url(&url);
    let mut protocol = DesktopProtocol::connect(&manifest).await.unwrap();
    let first = protocol.next_event().await.unwrap();
    let mut seen = vec![first];
    for _ in 0..2 {
        seen.push(
            protocol
                .try_next_event()
                .await
                .expect("queued event")
                .unwrap(),
        );
    }
    assert!(seen
        .iter()
        .any(|event| matches!(event, CaptionEvent::SourceCommitted(_))));
    assert!(seen
        .iter()
        .any(|event| matches!(event, CaptionEvent::TargetCommitted(_))));
    assert!(seen
        .iter()
        .any(|event| matches!(event, CaptionEvent::SourceLive(_))));
    let extra = protocol.try_next_event().await;
    assert!(
        extra.is_none() || matches!(extra, Some(Err(_))),
        "no fourth caption event"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn final_tokens_are_pure_deltas_and_repeated_text_appends() {
    // The backend sends each final token exactly once (sent_count per
    // stream) and the desktop appends unconditionally. Identical text in
    // consecutive frames is repeated content, not a replay.
    let frames = vec![
        ts_delta("very", "A"),
        ts_delta("very", "A"),
        r#"{"type":"update","final_tokens":[
            {"text":"very","speaker":"1","translation_status":"original","llm_sentence_id":"A","is_final":true},
            {"text":"very","speaker":"1","translation_status":"original","llm_sentence_id":"A","is_final":true}],
            "non_final_tokens":[]}"#
            .to_string(),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert_eq!(
        committed_texts(&events),
        vec![
            "very".to_string(),
            "veryvery".to_string(),
            "veryveryveryvery".to_string(),
        ]
    );
}

#[tokio::test]
async fn repeated_cjk_deltas_append_instead_of_replacing() {
    // Regression: the old prefix heuristic treated "哈哈" as a cumulative
    // replay of "哈" and dropped the first character. Incremental deltas
    // must always append: 哈 + 哈哈 = 哈哈哈.
    let frames = vec![
        r#"{"type":"update","final_tokens":[{"text":"哈","speaker":"1","translation_status":"translation","llm_sentence_id":"A","is_final":true}],"non_final_tokens":[]}"#
            .to_string(),
        r#"{"type":"update","final_tokens":[{"text":"哈哈","speaker":"1","translation_status":"translation","llm_sentence_id":"A","is_final":true}],"non_final_tokens":[]}"#
            .to_string(),
    ];
    let events = adapter_events(frames).await.unwrap();
    let targets: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            CaptionEvent::TargetCommitted(update) => Some(update.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(targets, vec!["哈".to_string(), "哈哈哈".to_string()]);
}

#[tokio::test]
async fn final_tokens_segment_by_sentence_speaker_and_language() {
    let frames = vec![r#"{"type":"update","final_tokens":[
        {"text":"hello ","speaker":"1","translation_status":"original","llm_sentence_id":"A","language":"en","is_final":true},
        {"text":"bonjour ","speaker":"2","translation_status":"original","llm_sentence_id":"B","language":"en","is_final":true}],
        "non_final_tokens":[]}"#
        .to_string()];
    let events = adapter_events(frames).await.unwrap();
    let mut by_id = std::collections::BTreeMap::new();
    for event in &events {
        if let CaptionEvent::SourceCommitted(committed) = event {
            by_id.insert(
                committed.sentence_id.clone().unwrap(),
                committed.text.clone(),
            );
        }
    }
    assert_eq!(by_id.get("A").map(String::as_str), Some("hello "));
    assert_eq!(by_id.get("B").map(String::as_str), Some("bonjour "));
}

#[tokio::test]
async fn separator_does_not_reset_id_keyed_accumulators() {
    // A separator is a display line break. The backend re-opens a NEW
    // sentence id after it, and late translations for the closed sentence
    // keep the old id. Neither case may truncate accumulated text.
    let frames = vec![
        r#"{"type":"update","final_tokens":[
            {"text":"old-a","speaker":"1","translation_status":"original","llm_sentence_id":"A","is_final":true},
            {"text":"old-b","speaker":"2","translation_status":"original","llm_sentence_id":"B","is_final":true}],"non_final_tokens":[]}"#.to_string(),
        r#"{"type":"update","final_tokens":[
            {"is_separator":true,"is_final":true,"separator_type":"punctuation"},
            {"text":"trans-a","speaker":"1","translation_status":"translation","llm_sentence_id":"A","is_final":true},
            {"text":"trans-b","speaker":"2","translation_status":"translation","llm_sentence_id":"B","is_final":true}],"non_final_tokens":[]}"#.to_string(),
        r#"{"type":"update","final_tokens":[
            {"text":"new-c","speaker":"1","translation_status":"original","llm_sentence_id":"C","is_final":true},
            {"text":"new-d","speaker":"2","translation_status":"original","llm_sentence_id":"D","is_final":true}],"non_final_tokens":[]}"#.to_string(),
    ];
    let events = adapter_events(frames).await.unwrap();
    let mut source_by_id = std::collections::BTreeMap::new();
    let mut target_by_id = std::collections::BTreeMap::new();
    for event in &events {
        match event {
            CaptionEvent::SourceCommitted(committed) => {
                source_by_id.insert(
                    committed.sentence_id.clone().unwrap(),
                    committed.text.clone(),
                );
            }
            CaptionEvent::TargetCommitted(update) => {
                target_by_id.insert(update.sentence_id.clone().unwrap(), update.text.clone());
            }
            _ => {}
        }
    }
    assert_eq!(source_by_id.get("A").map(String::as_str), Some("old-a"));
    assert_eq!(source_by_id.get("B").map(String::as_str), Some("old-b"));
    assert_eq!(source_by_id.get("C").map(String::as_str), Some("new-c"));
    assert_eq!(source_by_id.get("D").map(String::as_str), Some("new-d"));
    assert_eq!(target_by_id.get("A").map(String::as_str), Some("trans-a"));
    assert_eq!(target_by_id.get("B").map(String::as_str), Some("trans-b"));
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::Separator {
            kind: rinbridge_overlay::SeparatorKind::Punctuation
        }
    )));
}

#[tokio::test]
async fn late_translation_after_separator_keeps_appending() {
    // Regression: the old replace-on-next flag truncated a translation that
    // kept streaming after the display separator for its source sentence.
    let frames = vec![
        r#"{"type":"update","final_tokens":[
            {"text":"hello ","speaker":"1","translation_status":"original","llm_sentence_id":"A","is_final":true},
            {"text":"hello","speaker":"1","translation_status":"translation","llm_sentence_id":"A","is_final":true}],"non_final_tokens":[]}"#.to_string(),
        r#"{"type":"update","final_tokens":[
            {"text":"world","speaker":"1","translation_status":"original","llm_sentence_id":"A","is_final":true},
            {"is_separator":true,"is_final":true,"separator_type":"punctuation"}],"non_final_tokens":[]}"#.to_string(),
        r#"{"type":"update","final_tokens":[
            {"text":" world","speaker":"1","translation_status":"translation","llm_sentence_id":"A","is_final":true}],"non_final_tokens":[]}"#.to_string(),
        ts_delta("next", "B"),
    ];
    let events = adapter_events(frames).await.unwrap();
    let mut target_by_id = std::collections::BTreeMap::new();
    for event in &events {
        if let CaptionEvent::TargetCommitted(update) = event {
            target_by_id.insert(update.sentence_id.clone().unwrap(), update.text.clone());
        }
    }
    assert_eq!(
        target_by_id.get("A").map(String::as_str),
        Some("hello world")
    );
    assert_eq!(
        committed_texts(&events),
        vec![
            "hello ".to_string(),
            "hello world".to_string(),
            "next".to_string(),
        ]
    );
}

#[tokio::test]
async fn separator_resets_only_the_idless_source_track() {
    // IPC-injected text has no sentence id; a separator is its line boundary,
    // so each segment starts a fresh accumulator instead of growing forever.
    let frames = vec![
        r#"{"type":"update","final_tokens":[
            {"text":"ipc one","speaker":"0","translation_status":"original","is_final":true}],"non_final_tokens":[]}"#.to_string(),
        r#"{"type":"update","final_tokens":[
            {"is_separator":true,"is_final":true,"separator_type":"punctuation"},
            {"text":"ipc two","speaker":"0","translation_status":"original","is_final":true}],"non_final_tokens":[]}"#.to_string(),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert_eq!(
        committed_texts(&events),
        vec!["ipc one".to_string(), "ipc two".to_string()]
    );
}

#[tokio::test]
async fn multi_speaker_non_final_emits_one_live_event_for_the_last_speaker() {
    let frames = vec![r#"{"type":"update","final_tokens":[],"non_final_tokens":[
        {"text":"from one","speaker":"1","translation_status":"original","is_final":false},
        {"text":"from two","speaker":"2","translation_status":"original","is_final":false}]}"#
        .to_string()];
    let events = adapter_events(frames).await.unwrap();
    let live: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            CaptionEvent::SourceLive(snapshot) => Some(snapshot.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].speaker, SpeakerKey::Diarized("2".into()));
    assert_eq!(live[0].text, "from two");
}

#[tokio::test]
async fn refinement_maps_to_a_sentence_keyed_event_with_target_lang() {
    let frames = vec![r#"{"type":"refine_result","sentence_id":"A","source":"src","original_translation":"draft","refined_translation":"refined","no_change":false,"target_lang":"fr"}"#
        .to_string()];
    let events = adapter_events(frames).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::RefinedTarget(refinement)
            if refinement.text == "refined"
                && refinement.sentence_id == "A"
                && refinement.language.as_deref() == Some("fr")
                && refinement.source.as_deref() == Some("src")
    )));
}

#[tokio::test]
async fn refine_no_change_maps_to_a_confirmation_not_activity() {
    let frames = vec![r#"{"type":"refine_result","sentence_id":"A","source":"src","original_translation":"draft","refined_translation":null,"no_change":true,"target_lang":"fr"}"#
        .to_string()];
    let events = adapter_events(frames).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::TargetConfirmed { sentence_id, .. } if sentence_id == "A"
    )));
    assert!(!events
        .iter()
        .any(|event| matches!(event, CaptionEvent::RefinedTarget(_))));
}

#[tokio::test]
async fn subtitle_retract_maps_to_a_retract_event() {
    let frames = vec![r#"{"type":"subtitle_retract","sentence_id":"A","replacement_sentence_id":"M","reason":"short_interrupt"}"#
        .to_string()];
    let events = adapter_events(frames).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::Retract { sentence_id, replacement_sentence_id, reason }
            if sentence_id == "A"
                && replacement_sentence_id.as_deref() == Some("M")
                && reason.as_deref() == Some("short_interrupt")
    )));
}

#[tokio::test]
async fn spec_translation_frames_map_to_source_keyed_events() {
    let frames = vec![
        r#"{"type":"spec_translation_pending","source":" 你好。 ","target_lang":"fr"}"#.to_string(),
        r#"{"type":"spec_translation","source":" 你好。 ","translation":"bonjour","target_lang":"fr"}"#.to_string(),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::SpecPending { source, target_lang }
            if source == "你好。" && target_lang.as_deref() == Some("fr")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::SpecTranslation(spec)
            if spec.source == "你好。"
                && spec.translation == "bonjour"
                && spec.target_lang.as_deref() == Some("fr")
    )));
}

#[tokio::test]
async fn clear_preserve_existing_keeps_the_accumulators() {
    // A stream rollover keeps sentence ids alive; the adapter must not
    // truncate in-progress accumulators on a preserve_existing clear.
    let frames = vec![
        ts_delta("hello", "A"),
        r#"{"type":"clear","message":"Recognition reconnecting...","preserve_existing":true}"#
            .to_string(),
        ts_delta(" world", "A"),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert_eq!(
        committed_texts(&events),
        vec!["hello".to_string(), "hello world".to_string()]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::Clear {
            preserve_existing: true
        }
    )));
}

#[tokio::test]
async fn clear_full_resets_the_accumulators() {
    let frames = vec![
        ts_delta("hello", "A"),
        r#"{"type":"clear","message":"Recognition restarting..."}"#.to_string(),
        ts_delta("hello", "A"),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert_eq!(
        committed_texts(&events),
        vec!["hello".to_string(), "hello".to_string()]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::Clear {
            preserve_existing: false
        }
    )));
}

#[tokio::test]
async fn explicit_end_token_becomes_source_end() {
    let frames = vec![r#"{"type":"update","final_tokens":[{"text":"<end>","speaker":"1","translation_status":"original","is_final":true}],"non_final_tokens":[]}"#
        .to_string()];
    let events = adapter_events(frames).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::SourceEnd { speaker, sentence_id: None }
            if *speaker == SpeakerKey::Diarized("1".into())
    )));
}

#[tokio::test]
async fn malformed_frames_are_ignored_without_breaking_state() {
    let frames = vec![
        ts_delta("ok", "A"),
        r#"{"type":"update","final_tokens":"not-an-array"}"#.to_string(),
        "this is not json".to_string(),
        ts_delta(" more", "A"),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert_eq!(
        committed_texts(&events),
        vec!["ok".to_string(), "ok more".to_string()]
    );
}

#[tokio::test]
async fn view_settings_and_clear_map_to_events() {
    let frames = vec![
        r#"{"type":"clear","preserve_existing":true}"#.to_string(),
        r#"{"type":"vr_view_settings","display_mode":"original","max_speakers":2,"bilingual_pair_count":1,"show_speaker_labels":false}"#
            .to_string(),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::Clear {
            preserve_existing: true
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CaptionEvent::ViewSettingsChanged(settings)
            if settings.display_mode == DisplayMode::Original && settings.max_speakers == 2
    )));
}

#[tokio::test]
async fn wrong_typed_clear_is_ignored_without_erasing_accumulators() {
    let frames = vec![
        ts_delta("hello", "A"),
        r#"{"type":"clear","preserve_existing":"false"}"#.to_string(),
        ts_delta(" world", "A"),
    ];
    let events = adapter_events(frames).await.unwrap();
    assert_eq!(
        committed_texts(&events),
        vec!["hello".to_string(), "hello world".to_string()]
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        CaptionEvent::Clear {
            preserve_existing: false
        }
    )));
}

#[tokio::test]
async fn shutdown_maps_to_a_control_event_without_touching_transcript_state() {
    let events = adapter_events(vec![r#"{"type":"shutdown"}"#.to_string()])
        .await
        .unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(event, CaptionEvent::Shutdown)));
}

#[tokio::test]
async fn non_ws_manifest_path_is_rejected() {
    let manifest = test_manifest_with_url("ws://127.0.0.1:1/vr_ws");
    assert!(matches!(
        DesktopProtocol::connect(&manifest).await,
        Err(BridgeError::UnsupportedPath(_))
    ));
}
