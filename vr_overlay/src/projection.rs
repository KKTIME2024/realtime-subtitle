use std::time::{Duration, Instant};

use crate::hud::{HudFrame, HudRow, HudRowKind, HudRowRole};
use crate::transcript::{
    LiveInputRow, LiveSourceSnapshot, SentenceKey, SentenceRecord, SpeakerKey, TrackPhase,
    TranscriptState,
};
use crate::views::{DisplayMode, VrViewSettings};

pub const LIVE_SOURCE_HOLD: Duration = Duration::from_millis(1200);

/// Tells the coordinator whether the settled live row must be shown or consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveRowDirective {
    None,
    Show,
    Dismiss {
        speaker: SpeakerKey,
        sentence_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub frame: HudFrame,
    pub live_row: LiveRowDirective,
}

pub fn project(state: &TranscriptState, settings: &VrViewSettings, now: Instant) -> Projection {
    let mut frame = HudFrame::default();
    match settings.display_mode {
        DisplayMode::Original => project_original(state, settings, &mut frame),
        DisplayMode::Translation => project_translation(state, settings, &mut frame),
        DisplayMode::Both => project_both(state, settings, &mut frame),
    }
    let live_row = place_live_row(state, settings, now, &mut frame);
    Projection { frame, live_row }
}

fn project_original(state: &TranscriptState, settings: &VrViewSettings, frame: &mut HudFrame) {
    let segments = display_segments(state);
    let mut rows = Vec::new();
    for speaker in state.speaker_recency() {
        let Some(segment) = segments
            .iter()
            .rev()
            .find(|segment| &segment.speaker == speaker && !segment.settled_source.is_empty())
        else {
            continue;
        };
        rows.push(HudRow {
            role: HudRowRole::UpperPrimary,
            kind: HudRowKind::Settled,
            text: segment.settled_source.clone(),
            speaker_label: None,
            language: segment.settled_source_language.clone(),
            sentence: Some(segment.latest_key.clone()),
        });
    }
    truncate_to_capacity(&mut rows, settings.max_speakers as usize);
    place_bottom_aligned(frame, rows);
}

fn project_translation(state: &TranscriptState, settings: &VrViewSettings, frame: &mut HudFrame) {
    let segments = display_segments(state);
    let mut rows = Vec::new();
    for speaker in state.speaker_recency() {
        let Some(segment) = segments
            .iter()
            .rev()
            .find(|segment| &segment.speaker == speaker && !segment.target.is_empty())
        else {
            continue;
        };
        rows.push(HudRow {
            role: HudRowRole::UpperPrimary,
            kind: target_kind(segment),
            text: segment.target.clone(),
            speaker_label: None,
            language: segment.target_language.clone(),
            sentence: Some(segment.latest_key.clone()),
        });
    }
    truncate_to_capacity(&mut rows, settings.max_speakers as usize);
    place_bottom_aligned(frame, rows);
}

fn project_both(state: &TranscriptState, settings: &VrViewSettings, frame: &mut HudFrame) {
    let mut selected: Vec<DisplaySegment> = display_segments(state)
        .into_iter()
        .filter(|segment| !segment.paired_source.is_empty() && !segment.paired_target.is_empty())
        .collect();
    let capacity = settings.bilingual_pair_count as usize;
    if selected.len() > capacity {
        selected.drain(0..selected.len() - capacity);
    }
    match selected.as_slice() {
        [] => {}
        [record] => {
            frame.slots[2] = Some(pair_target_row(record));
            frame.slots[3] = Some(pair_source_row(record));
        }
        [older, newer, ..] => {
            frame.slots[0] = Some(pair_target_row(older));
            frame.slots[1] = Some(pair_source_row(older));
            frame.slots[2] = Some(pair_target_row(newer));
            frame.slots[3] = Some(pair_source_row(newer));
        }
    }
}

#[derive(Debug, Clone)]
struct DisplaySegment {
    speaker: SpeakerKey,
    latest_key: SentenceKey,
    settled_source: String,
    settled_source_language: Option<String>,
    target: String,
    target_language: Option<String>,
    target_phase: TrackPhase,
    paired_source: String,
    paired_source_language: Option<String>,
    paired_target: String,
    paired_target_language: Option<String>,
    paired_target_phase: TrackPhase,
}

fn display_segments(state: &TranscriptState) -> Vec<DisplaySegment> {
    let mut segments: Vec<DisplaySegment> = Vec::new();
    for record in &state.sentences {
        let starts_new_segment = segments
            .last()
            .is_none_or(|segment| segment.speaker != record.speaker);
        if starts_new_segment {
            segments.push(DisplaySegment {
                speaker: record.speaker.clone(),
                latest_key: record.key.clone(),
                settled_source: String::new(),
                settled_source_language: None,
                target: String::new(),
                target_language: None,
                target_phase: TrackPhase::Empty,
                paired_source: String::new(),
                paired_source_language: None,
                paired_target: String::new(),
                paired_target_language: None,
                paired_target_phase: TrackPhase::Empty,
            });
        }

        let segment = segments.last_mut().expect("segment was just created");
        segment.latest_key = record.key.clone();
        if matches!(
            record.source.phase,
            TrackPhase::Committed | TrackPhase::Refined
        ) && !record.source.text.is_empty()
        {
            segment.settled_source.push_str(&record.source.text);
            if record.source.language.is_some() {
                segment
                    .settled_source_language
                    .clone_from(&record.source.language);
            }
        }
        let pending_language = state.spec_pending_for(&record.source.text);
        let target_text = if record.target.text.is_empty() {
            pending_language.as_ref().map(|_| "\u{00a0}")
        } else {
            Some(record.target.text.as_str())
        };
        let target_language = record
            .target
            .language
            .clone()
            .or_else(|| pending_language.clone().flatten());
        let target_phase = if record.target.text.is_empty() && pending_language.is_some() {
            TrackPhase::Draft
        } else {
            record.target.phase
        };
        if let Some(target_text) = target_text {
            segment.target.push_str(target_text);
            if target_language.is_some() {
                segment.target_language.clone_from(&target_language);
            }
            segment.target_phase = target_phase;
        }
        if !record.source.text.is_empty() && target_text.is_some() {
            segment.paired_source.push_str(&record.source.text);
            segment
                .paired_target
                .push_str(target_text.expect("paired target was checked"));
            if record.source.language.is_some() {
                segment
                    .paired_source_language
                    .clone_from(&record.source.language);
            }
            if target_language.is_some() {
                segment.paired_target_language.clone_from(&target_language);
            }
            segment.paired_target_phase = target_phase;
        }
    }
    segments
}

fn place_live_row(
    state: &TranscriptState,
    settings: &VrViewSettings,
    now: Instant,
    frame: &mut HudFrame,
) -> LiveRowDirective {
    match &state.live_input {
        LiveInputRow::Hidden => LiveRowDirective::None,
        LiveInputRow::Streaming(snapshot) => {
            frame.slots[4] = Some(live_source_row(snapshot));
            LiveRowDirective::Show
        }
        LiveInputRow::Settled {
            snapshot,
            closed_at,
        } => {
            let hold_elapsed = now.saturating_duration_since(*closed_at) >= LIVE_SOURCE_HOLD;
            if hold_elapsed && handoff_visible(state, settings.display_mode, snapshot) {
                LiveRowDirective::Dismiss {
                    speaker: snapshot.speaker.clone(),
                    sentence_id: snapshot.sentence_id.clone(),
                }
            } else {
                frame.slots[4] = Some(live_source_row(snapshot));
                LiveRowDirective::Show
            }
        }
    }
}

fn settled_record<'a>(
    state: &'a TranscriptState,
    snapshot: &LiveSourceSnapshot,
) -> Option<&'a SentenceRecord> {
    if let Some(id) = snapshot.sentence_id.as_deref() {
        if let Some(record) = state.sentence_by_upstream_id(id) {
            return (record.speaker == snapshot.speaker).then_some(record);
        }
    }
    state
        .sentences
        .iter()
        .rev()
        .find(|record| {
            record.speaker == snapshot.speaker
                && matches!(
                    record.source.phase,
                    TrackPhase::Committed | TrackPhase::Refined
                )
                && record.source.text == snapshot.text
        })
        .or_else(|| {
            state
                .sentences
                .iter()
                .rev()
                .find(|record| record.speaker == snapshot.speaker)
        })
}

fn handoff_visible(
    state: &TranscriptState,
    mode: DisplayMode,
    snapshot: &LiveSourceSnapshot,
) -> bool {
    let Some(record) = settled_record(state, snapshot) else {
        return false;
    };
    match mode {
        DisplayMode::Original => {
            matches!(
                record.source.phase,
                TrackPhase::Committed | TrackPhase::Refined
            ) && !record.source.text.is_empty()
        }
        DisplayMode::Translation => {
            !record.target.text.is_empty() || state.spec_pending_for(&record.source.text).is_some()
        }
        DisplayMode::Both => {
            !record.source.text.is_empty()
                && (!record.target.text.is_empty()
                    || state.spec_pending_for(&record.source.text).is_some())
        }
    }
}

fn truncate_to_capacity(rows: &mut Vec<HudRow>, capacity: usize) {
    let capacity = capacity.max(1);
    if rows.len() > capacity {
        rows.drain(0..rows.len() - capacity);
    }
}

fn place_bottom_aligned(frame: &mut HudFrame, rows: Vec<HudRow>) {
    let count = rows.len().min(3);
    let start = 4 - count;
    for (offset, row) in rows.into_iter().enumerate().take(count) {
        frame.slots[start + offset] = Some(row);
    }
}

fn target_kind(segment: &DisplaySegment) -> HudRowKind {
    if segment.target_phase == TrackPhase::Draft {
        HudRowKind::Draft
    } else {
        HudRowKind::Settled
    }
}

fn pair_target_row(segment: &DisplaySegment) -> HudRow {
    HudRow {
        role: HudRowRole::UpperPrimary,
        kind: if segment.paired_target_phase == TrackPhase::Draft {
            HudRowKind::Draft
        } else {
            HudRowKind::Settled
        },
        text: segment.paired_target.clone(),
        speaker_label: None,
        language: segment.paired_target_language.clone(),
        sentence: Some(segment.latest_key.clone()),
    }
}

fn pair_source_row(segment: &DisplaySegment) -> HudRow {
    HudRow {
        role: HudRowRole::UpperSecondary,
        kind: HudRowKind::Settled,
        text: segment.paired_source.clone(),
        speaker_label: None,
        language: segment.paired_source_language.clone(),
        sentence: Some(segment.latest_key.clone()),
    }
}

fn live_source_row(snapshot: &LiveSourceSnapshot) -> HudRow {
    HudRow {
        role: HudRowRole::LiveSource,
        kind: HudRowKind::Settled,
        text: snapshot.text.clone(),
        speaker_label: None,
        language: snapshot.language.clone(),
        sentence: snapshot.sentence_id.as_ref().map(|id| SentenceKey {
            local_ordinal: 0,
            upstream_id: Some(id.clone()),
        }),
    }
}
