use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;

use crate::views::VrViewSettings;

const LEDGER_CAP: usize = 64;
const PENDING_REFINEMENT_CAP: usize = 64;
const SPEC_CACHE_CAP: usize = 80;

/// A session-scoped Soniox diarization value. It is intentionally not a
/// durable human identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SpeakerKey {
    Anonymous,
    Diarized(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TrackPhase {
    #[default]
    Empty,
    Draft,
    Committed,
    Refined,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TextTrack {
    pub text: String,
    pub phase: TrackPhase,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SentenceKey {
    pub local_ordinal: u64,
    pub upstream_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentenceRecord {
    pub key: SentenceKey,
    pub speaker: SpeakerKey,
    pub source: TextTrack,
    pub target: TextTrack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSourceSnapshot {
    pub speaker: SpeakerKey,
    pub sentence_id: Option<String>,
    pub text: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedSource {
    pub speaker: SpeakerKey,
    pub sentence_id: Option<String>,
    pub text: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetUpdate {
    pub speaker: SpeakerKey,
    pub sentence_id: Option<String>,
    pub text: String,
    pub language: Option<String>,
}

/// A refinement carries no speaker; it is keyed strictly by `sentence_id`.
/// `source` is the refine frame's source text, used to retire the matching
/// speculative-translation marker (the desktop clears its pending map by
/// source text on every refine result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refinement {
    pub sentence_id: String,
    pub text: String,
    pub language: Option<String>,
    pub source: Option<String>,
}

/// A typed display separator from the backend. It marks a display line break
/// and never changes sentence identity: source sentence IDs already delimit
/// sentences, and late translations for a closed sentence keep the old ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeparatorKind {
    Punctuation,
    Translation,
    Endpoint,
    SpeakerChange,
    InterruptRepair,
    Reconnect,
    Unknown,
}

/// A speculative translation is keyed by the exact source text the backend
/// fired it for (the backend carries no sentence id on these frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecTranslation {
    pub source: String,
    pub translation: String,
    pub target_lang: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LiveInputRow {
    #[default]
    Hidden,
    Streaming(LiveSourceSnapshot),
    Settled {
        snapshot: LiveSourceSnapshot,
        closed_at: Instant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptionEvent {
    SourceLive(LiveSourceSnapshot),
    SourceCommitted(CommittedSource),
    SourceEnd {
        speaker: SpeakerKey,
        sentence_id: Option<String>,
    },
    TargetDraft(TargetUpdate),
    TargetCommitted(TargetUpdate),
    RefinedTarget(Refinement),
    /// A `refine_result` with `no_change: true`: the sentence's current
    /// translation is confirmed final. The desktop records a per-sentence
    /// confirmation; here a draft target is promoted to committed.
    TargetConfirmed {
        sentence_id: String,
        source: Option<String>,
    },
    /// The backend retracted a sentence (interrupt repair). All tokens with
    /// the retracted id are removed; the replacement sentence is re-emitted
    /// by the backend as fresh final tokens under `replacement_sentence_id`.
    Retract {
        sentence_id: String,
        replacement_sentence_id: Option<String>,
        reason: Option<String>,
    },
    /// A typed display separator. It finalizes the live row's open draft and
    /// settles the live row; it never resets id-keyed accumulators.
    Separator {
        kind: SeparatorKind,
    },
    /// Speculative translation in flight for the exact source text.
    SpecPending {
        source: String,
        target_lang: Option<String>,
    },
    /// Speculative translation result for the exact source text.
    SpecTranslation(SpecTranslation),
    Clear {
        preserve_existing: bool,
    },
    ViewSettingsChanged(VrViewSettings),
    Shutdown,
    Activity,
}

#[derive(Debug, Default)]
pub struct TranscriptState {
    pub sentences: VecDeque<SentenceRecord>,
    pub live_input: LiveInputRow,
    speaker_recency: VecDeque<SpeakerKey>,
    next_local_ordinal: u64,
    pending_refinements: VecDeque<Refinement>,
    /// Retracted sentence ids. Late refinements for a retracted sentence are
    /// dropped (the desktop ignores them via its retracted-id set).
    retracted_ids: BTreeSet<String>,
    /// Normalized source text -> pending target language.
    spec_pending: VecDeque<(String, Option<String>)>,
    /// Completed speculative results remain reusable by repeated exact source.
    spec_translations: VecDeque<SpecTranslation>,
}

impl TranscriptState {
    pub fn spec_pending_for(&self, source: &str) -> Option<Option<String>> {
        let source = normalize_source(source);
        self.spec_pending
            .iter()
            .rev()
            .find(|(known, _)| known == source)
            .map(|(_, language)| language.clone())
    }

    pub fn is_retracted(&self, sentence_id: &str) -> bool {
        self.retracted_ids.contains(sentence_id)
    }

    pub fn apply(&mut self, event: &CaptionEvent, now: Instant) {
        match event {
            CaptionEvent::SourceLive(snapshot) => self.apply_source_live(snapshot, now),
            CaptionEvent::SourceCommitted(committed) => self.apply_source_committed(committed, now),
            CaptionEvent::SourceEnd {
                speaker,
                sentence_id,
            } => self.apply_source_end(speaker, sentence_id.as_deref(), now),
            CaptionEvent::TargetDraft(update) => self.apply_target(update, TrackPhase::Draft),
            CaptionEvent::TargetCommitted(update) => {
                self.apply_target(update, TrackPhase::Committed)
            }
            CaptionEvent::RefinedTarget(refinement) => self.apply_refinement(refinement),
            CaptionEvent::TargetConfirmed {
                sentence_id,
                source,
            } => self.apply_target_confirmed(sentence_id, source.as_deref()),
            CaptionEvent::Retract { sentence_id, .. } => self.apply_retract(sentence_id),
            CaptionEvent::Separator { kind } => self.apply_separator(*kind),
            CaptionEvent::SpecPending {
                source,
                target_lang,
            } => self.apply_spec_pending(source, target_lang.as_deref()),
            CaptionEvent::SpecTranslation(spec) => self.apply_spec_translation(spec),
            CaptionEvent::Clear { preserve_existing } => {
                self.clear(*preserve_existing);
                return;
            }
            CaptionEvent::ViewSettingsChanged(_)
            | CaptionEvent::Activity
            | CaptionEvent::Shutdown => return,
        }
        self.replay_pending_refinements();
        self.replay_cached_specs();
    }

    pub fn clear(&mut self, preserve_existing: bool) {
        if preserve_existing {
            // The desktop finalizes the current non-final tokens before a
            // rollover restart: draft content is preserved as committed text
            // instead of being dropped. Sentence ids survive a stream
            // rollover, so in-progress sentences may keep receiving tokens.
            let mut kept = VecDeque::new();
            for mut record in std::mem::take(&mut self.sentences) {
                if record.source.phase == TrackPhase::Draft {
                    record.source.phase = TrackPhase::Committed;
                }
                if record.target.phase == TrackPhase::Draft {
                    record.target.phase = TrackPhase::Committed;
                }
                if record.source.phase != TrackPhase::Empty
                    || record.target.phase != TrackPhase::Empty
                {
                    kept.push_back(record);
                }
            }
            self.sentences = kept;
        } else {
            self.sentences.clear();
            self.next_local_ordinal = 0;
            self.retracted_ids.clear();
            self.spec_pending.clear();
            self.spec_translations.clear();
        }
        self.live_input = LiveInputRow::Hidden;
        if preserve_existing {
            let kept_speakers: Vec<_> = self
                .sentences
                .iter()
                .map(|record| record.speaker.clone())
                .collect();
            self.speaker_recency
                .retain(|speaker| kept_speakers.iter().any(|kept| kept == speaker));
        } else {
            self.speaker_recency.clear();
        }
        self.pending_refinements.clear();
    }

    pub fn speaker_recency(&self) -> &VecDeque<SpeakerKey> {
        &self.speaker_recency
    }

    pub fn sentence_by_upstream_id(&self, id: &str) -> Option<&SentenceRecord> {
        self.find_by_upstream(id)
            .map(|index| &self.sentences[index])
    }

    /// The speaker's most recent record whose source is still a draft.
    pub fn open_record_index(&self, speaker: &SpeakerKey) -> Option<usize> {
        self.sentences.iter().rposition(|record| {
            &record.speaker == speaker && record.source.phase == TrackPhase::Draft
        })
    }

    /// One-shot consumption of a settled live row after a visible handoff.
    pub fn dismiss_settled_live(
        &mut self,
        speaker: &SpeakerKey,
        sentence_id: Option<&str>,
    ) -> bool {
        if let LiveInputRow::Settled { snapshot, .. } = &self.live_input {
            if &snapshot.speaker != speaker {
                return false;
            }
            let matches = match (sentence_id, snapshot.sentence_id.as_deref()) {
                (Some(requested), Some(current)) => requested == current,
                _ => true,
            };
            if matches {
                self.live_input = LiveInputRow::Hidden;
                return true;
            }
        }
        false
    }

    fn apply_source_live(&mut self, snapshot: &LiveSourceSnapshot, _now: Instant) {
        if snapshot.text.is_empty() {
            return;
        }
        if snapshot
            .sentence_id
            .as_deref()
            .is_some_and(|id| self.retracted_ids.contains(id))
        {
            return;
        }
        let index = match self.open_record_index(&snapshot.speaker) {
            Some(index) => index,
            None => self.push_record(snapshot.speaker.clone(), None),
        };
        self.sentences[index].source = TextTrack {
            text: snapshot.text.clone(),
            phase: TrackPhase::Draft,
            language: snapshot.language.clone(),
        };
        self.touch_recency(&snapshot.speaker);
        self.live_input = LiveInputRow::Streaming(snapshot.clone());
    }

    fn apply_source_committed(&mut self, committed: &CommittedSource, now: Instant) {
        if committed
            .sentence_id
            .as_deref()
            .is_some_and(|id| self.retracted_ids.contains(id))
        {
            return;
        }
        let index = self.resolve_or_create(&committed.speaker, committed.sentence_id.as_deref());
        self.sentences[index].source = TextTrack {
            text: committed.text.clone(),
            phase: TrackPhase::Committed,
            language: committed.language.clone(),
        };
        self.touch_recency(&committed.speaker);
        if let LiveInputRow::Streaming(snapshot) = &self.live_input {
            if snapshot.speaker == committed.speaker
                && !sentence_id_conflicts(
                    snapshot.sentence_id.as_deref(),
                    committed.sentence_id.as_deref(),
                )
            {
                self.live_input = LiveInputRow::Settled {
                    snapshot: LiveSourceSnapshot {
                        speaker: committed.speaker.clone(),
                        sentence_id: committed.sentence_id.clone(),
                        text: committed.text.clone(),
                        language: committed.language.clone(),
                    },
                    closed_at: now,
                };
            }
        }
    }

    fn apply_source_end(&mut self, speaker: &SpeakerKey, sentence_id: Option<&str>, now: Instant) {
        if sentence_id.is_some_and(|id| self.retracted_ids.contains(id)) {
            return;
        }
        let index = match sentence_id {
            Some(id) => self
                .find_by_upstream_for_speaker(id, speaker)
                .or_else(|| self.unbound_open_index(speaker)),
            None => self.open_record_index(speaker),
        };
        if let Some(index) = index {
            let record = &mut self.sentences[index];
            if &record.speaker != speaker {
                return;
            }
            if let Some(id) = sentence_id {
                match record.key.upstream_id.as_deref() {
                    Some(current) if current != id => return,
                    None => record.key.upstream_id = Some(id.to_owned()),
                    Some(_) => {}
                }
            }
            record.source.phase = TrackPhase::Committed;
            if let LiveInputRow::Streaming(live) = &self.live_input {
                if &live.speaker == speaker
                    && !sentence_id_conflicts(live.sentence_id.as_deref(), sentence_id)
                {
                    let snapshot = LiveSourceSnapshot {
                        speaker: speaker.clone(),
                        sentence_id: record.key.upstream_id.clone(),
                        text: record.source.text.clone(),
                        language: record.source.language.clone(),
                    };
                    self.live_input = LiveInputRow::Settled {
                        snapshot,
                        closed_at: now,
                    };
                }
            }
            return;
        }
        if let LiveInputRow::Streaming(snapshot) = &self.live_input {
            if &snapshot.speaker == speaker
                && !sentence_id_conflicts(snapshot.sentence_id.as_deref(), sentence_id)
            {
                self.live_input = LiveInputRow::Settled {
                    snapshot: snapshot.clone(),
                    closed_at: now,
                };
            }
        }
    }

    fn apply_target(&mut self, update: &TargetUpdate, phase: TrackPhase) {
        if update
            .sentence_id
            .as_deref()
            .is_some_and(|id| self.retracted_ids.contains(id))
        {
            return;
        }
        let index = match update.sentence_id.as_deref() {
            Some(id) => Some(self.resolve_or_create(&update.speaker, Some(id))),
            // Non-final translation tokens carry no sentence id. The desktop
            // binds them to the speaker's most recent sentence that has
            // original text, even when that sentence is already closed; a
            // translation with no original at all becomes its own sentence.
            None => match self.latest_source_record_index(&update.speaker) {
                Some(index) => Some(index),
                None => Some(self.push_record(update.speaker.clone(), None)),
            },
        };
        if let Some(index) = index {
            let current_phase = self.sentences[index].target.phase;
            let is_downgrade = matches!(
                (current_phase, phase),
                (
                    TrackPhase::Committed | TrackPhase::Refined,
                    TrackPhase::Draft
                ) | (TrackPhase::Refined, TrackPhase::Committed)
            );
            if is_downgrade {
                return;
            }
            self.sentences[index].target = TextTrack {
                text: update.text.clone(),
                phase,
                language: update.language.clone(),
            };
        }
    }

    fn apply_refinement(&mut self, refinement: &Refinement) {
        if self.retracted_ids.contains(&refinement.sentence_id) {
            return;
        }
        self.retire_spec_pending_for_source(refinement.source.as_deref());
        if let Some(index) = self.find_by_upstream(&refinement.sentence_id) {
            self.sentences[index].target = TextTrack {
                text: refinement.text.clone(),
                phase: TrackPhase::Refined,
                language: refinement.language.clone(),
            };
        } else {
            self.pending_refinements.push_back(refinement.clone());
            while self.pending_refinements.len() > PENDING_REFINEMENT_CAP {
                self.pending_refinements.pop_front();
            }
        }
    }

    fn apply_target_confirmed(&mut self, sentence_id: &str, source: Option<&str>) {
        if self.retracted_ids.contains(sentence_id) {
            return;
        }
        self.retire_spec_pending_for_source(source);
        if let Some(index) = self.find_by_upstream(sentence_id) {
            // The confirmed translation is the text already on the track; a
            // draft target becomes settled, matching the desktop's confirmed
            // sentence state (no replacement text is invented).
            if self.sentences[index].target.phase == TrackPhase::Draft {
                self.sentences[index].target.phase = TrackPhase::Committed;
            }
        }
    }

    fn apply_retract(&mut self, sentence_id: &str) {
        self.retracted_ids.insert(sentence_id.to_owned());
        self.pending_refinements
            .retain(|pending| pending.sentence_id != sentence_id);
        self.sentences
            .retain(|record| record.key.upstream_id.as_deref() != Some(sentence_id));
        let retracts_live = match &self.live_input {
            LiveInputRow::Streaming(snapshot) | LiveInputRow::Settled { snapshot, .. } => {
                snapshot.sentence_id.as_deref() == Some(sentence_id)
            }
            LiveInputRow::Hidden => false,
        };
        if retracts_live {
            self.live_input = LiveInputRow::Hidden;
        }
    }

    fn apply_separator(&mut self, _kind: SeparatorKind) {
        // The separator is a display line break. In the id-keyed model it has
        // exactly one transcript effect: the utterance behind the live row
        // has reached its boundary, so finalize any still-open draft for that
        // speaker and settle the live row.
        if let LiveInputRow::Streaming(snapshot) = &self.live_input {
            if let Some(index) = self.unbound_open_index(&snapshot.speaker) {
                if self.sentences[index].source.phase == TrackPhase::Draft {
                    self.sentences[index].source.phase = TrackPhase::Committed;
                }
            }
            self.live_input = LiveInputRow::Settled {
                snapshot: snapshot.clone(),
                closed_at: std::time::Instant::now(),
            };
        }
    }

    fn apply_spec_pending(&mut self, source: &str, target_lang: Option<&str>) {
        let source = normalize_source(source);
        if source.is_empty() {
            return;
        }
        self.spec_pending.retain(|(known, _)| known != source);
        self.spec_pending
            .push_back((source.to_owned(), target_lang.map(str::to_owned)));
        while self.spec_pending.len() > SPEC_CACHE_CAP {
            self.spec_pending.pop_front();
        }
        if let Some(index) = self.find_record_by_source_text(source) {
            let track = &mut self.sentences[index].target;
            if track.text.is_empty() {
                track.phase = TrackPhase::Draft;
                track.language = target_lang.map(str::to_owned);
            }
        }
    }

    fn apply_spec_translation(&mut self, spec: &SpecTranslation) {
        let source = normalize_source(&spec.source);
        let translation = spec.translation.trim();
        if source.is_empty() || translation.is_empty() {
            return;
        }
        self.spec_pending.retain(|(known, _)| known != source);
        self.spec_translations
            .retain(|known| normalize_source(&known.source) != source);
        self.spec_translations.push_back(SpecTranslation {
            source: source.to_owned(),
            translation: translation.to_owned(),
            target_lang: spec.target_lang.clone(),
        });
        while self.spec_translations.len() > SPEC_CACHE_CAP {
            self.spec_translations.pop_front();
        }
        self.replay_cached_specs();
    }

    fn replay_cached_specs(&mut self) {
        for record in &mut self.sentences {
            let source = normalize_source(&record.source.text);
            let Some(spec) = self
                .spec_translations
                .iter()
                .rev()
                .find(|spec| normalize_source(&spec.source) == source)
            else {
                continue;
            };
            if matches!(record.target.phase, TrackPhase::Empty | TrackPhase::Draft) {
                record.target = TextTrack {
                    text: spec.translation.clone(),
                    phase: TrackPhase::Draft,
                    language: spec.target_lang.clone(),
                };
            }
        }
    }

    fn retire_spec_pending_for_source(&mut self, source: Option<&str>) {
        if let Some(source) = source {
            let source = normalize_source(source);
            if !source.is_empty() {
                self.spec_pending.retain(|(known, _)| known != source);
            }
        }
    }

    /// The speaker's most recent record that carries original text, whether
    /// or not the sentence is still open (mirrors the desktop's nearest
    /// original-sentence binding for id-less translation tokens).
    fn latest_source_record_index(&self, speaker: &SpeakerKey) -> Option<usize> {
        self.sentences
            .iter()
            .rposition(|record| &record.speaker == speaker && !record.source.text.is_empty())
    }

    /// The most recent record whose source text equals `source` exactly. The
    /// backend keys speculative frames by exact source text, so the match is
    /// exact (a revised recognition simply does not match, same as desktop).
    fn find_record_by_source_text(&self, source: &str) -> Option<usize> {
        let source = normalize_source(source);
        self.sentences
            .iter()
            .rposition(|record| normalize_source(&record.source.text) == source)
    }

    fn replay_pending_refinements(&mut self) {
        if self.pending_refinements.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_refinements);
        for refinement in pending {
            if let Some(index) = self.find_by_upstream(&refinement.sentence_id) {
                self.sentences[index].target = TextTrack {
                    text: refinement.text,
                    phase: TrackPhase::Refined,
                    language: refinement.language,
                };
            } else {
                self.pending_refinements.push_back(refinement);
            }
        }
    }

    fn resolve_or_create(&mut self, speaker: &SpeakerKey, sentence_id: Option<&str>) -> usize {
        if let Some(id) = sentence_id {
            if let Some(index) = self.find_by_upstream_for_speaker(id, speaker) {
                return index;
            }
            if let Some(index) = self.unbound_open_index(speaker) {
                self.sentences[index].key.upstream_id = Some(id.to_string());
                return index;
            }
            return self.push_record(speaker.clone(), Some(id.to_string()));
        }
        match self.open_record_index(speaker) {
            Some(index) => index,
            None => self.push_record(speaker.clone(), None),
        }
    }

    fn push_record(&mut self, speaker: SpeakerKey, upstream_id: Option<String>) -> usize {
        self.next_local_ordinal += 1;
        self.sentences.push_back(SentenceRecord {
            key: SentenceKey {
                local_ordinal: self.next_local_ordinal,
                upstream_id,
            },
            speaker,
            source: TextTrack::default(),
            target: TextTrack::default(),
        });
        while self.sentences.len() > LEDGER_CAP {
            self.sentences.pop_front();
        }
        self.sentences.len() - 1
    }

    fn find_by_upstream(&self, id: &str) -> Option<usize> {
        self.sentences
            .iter()
            .position(|record| record.key.upstream_id.as_deref() == Some(id))
    }

    fn find_by_upstream_for_speaker(&self, id: &str, speaker: &SpeakerKey) -> Option<usize> {
        self.sentences.iter().position(|record| {
            record.key.upstream_id.as_deref() == Some(id) && &record.speaker == speaker
        })
    }

    fn unbound_open_index(&self, speaker: &SpeakerKey) -> Option<usize> {
        self.sentences.iter().rposition(|record| {
            &record.speaker == speaker
                && record.source.phase == TrackPhase::Draft
                && record.key.upstream_id.is_none()
        })
    }

    fn touch_recency(&mut self, speaker: &SpeakerKey) {
        self.speaker_recency.retain(|known| known != speaker);
        self.speaker_recency.push_back(speaker.clone());
    }
}

fn normalize_source(source: &str) -> &str {
    source.trim()
}

fn sentence_id_conflicts(streaming: Option<&str>, incoming: Option<&str>) -> bool {
    match (streaming, incoming) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn s_live(speaker: &str, text: &str) -> CaptionEvent {
        CaptionEvent::SourceLive(LiveSourceSnapshot {
            speaker: SpeakerKey::Diarized(speaker.into()),
            sentence_id: None,
            text: text.into(),
            language: Some("en".into()),
        })
    }

    fn s_commit(speaker: &str, id: &str, text: &str) -> CaptionEvent {
        CaptionEvent::SourceCommitted(CommittedSource {
            speaker: SpeakerKey::Diarized(speaker.into()),
            sentence_id: Some(id.into()),
            text: text.into(),
            language: Some("en".into()),
        })
    }

    fn s_end(speaker: &str, id: Option<&str>) -> CaptionEvent {
        CaptionEvent::SourceEnd {
            speaker: SpeakerKey::Diarized(speaker.into()),
            sentence_id: id.map(str::to_owned),
        }
    }

    fn t_draft(speaker: &str, id: Option<&str>, text: &str) -> CaptionEvent {
        CaptionEvent::TargetDraft(TargetUpdate {
            speaker: SpeakerKey::Diarized(speaker.into()),
            sentence_id: id.map(str::to_owned),
            text: text.into(),
            language: Some("fr".into()),
        })
    }

    fn t_commit(speaker: &str, id: &str, text: &str) -> CaptionEvent {
        CaptionEvent::TargetCommitted(TargetUpdate {
            speaker: SpeakerKey::Diarized(speaker.into()),
            sentence_id: Some(id.into()),
            text: text.into(),
            language: Some("fr".into()),
        })
    }

    fn refine(id: &str, text: &str) -> CaptionEvent {
        CaptionEvent::RefinedTarget(Refinement {
            sentence_id: id.into(),
            text: text.into(),
            language: Some("fr".into()),
            source: None,
        })
    }

    #[test]
    fn source_live_creates_open_record_and_streams() {
        let mut s = TranscriptState::default();
        s.apply(&s_live("1", "hel"), Instant::now());
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].source.phase, TrackPhase::Draft);
        assert!(matches!(&s.live_input, LiveInputRow::Streaming(snap) if snap.text == "hel"));
    }

    #[test]
    fn source_commit_refreshes_recency_and_settles_full_text() {
        let mut s = TranscriptState::default();
        let t0 = Instant::now();
        s.apply(&s_live("1", "hello wor"), t0);
        s.apply(&s_commit("1", "A", "hello world"), t0);
        match &s.live_input {
            LiveInputRow::Settled { snapshot, .. } => assert_eq!(snapshot.text, "hello world"),
            other => panic!("expected Settled, got {other:?}"),
        }
        assert_eq!(
            s.speaker_recency().back(),
            Some(&SpeakerKey::Diarized("1".into()))
        );
        // A later source commit for another speaker refreshes recency to it and
        // must not disturb the settled row owned by speaker 1.
        s.apply(&s_commit("2", "B", "other"), t0);
        assert_eq!(
            s.speaker_recency().back(),
            Some(&SpeakerKey::Diarized("2".into()))
        );
        assert!(
            matches!(&s.live_input, LiveInputRow::Settled { snapshot, .. } if snapshot.speaker == SpeakerKey::Diarized("1".into()))
        );
    }

    #[test]
    fn empty_non_final_does_not_settle_the_live_row() {
        let mut s = TranscriptState::default();
        s.apply(&s_live("1", "hello"), Instant::now());
        s.apply(&s_live("1", ""), Instant::now());
        assert!(matches!(&s.live_input, LiveInputRow::Streaming(snap) if snap.text == "hello"));
    }

    #[test]
    fn source_end_settles_only_the_matching_speaker() {
        let mut s = TranscriptState::default();
        s.apply(&s_live("1", "hello"), Instant::now());
        s.apply(&s_end("2", None), Instant::now());
        assert!(
            matches!(&s.live_input, LiveInputRow::Streaming(_)),
            "wrong speaker must not settle"
        );
        s.apply(&s_end("1", None), Instant::now());
        assert!(
            matches!(&s.live_input, LiveInputRow::Settled { snapshot, .. } if snapshot.text == "hello")
        );
    }

    #[test]
    fn source_end_with_conflicting_id_is_a_noop() {
        let mut s = TranscriptState::default();
        s.apply(&s_commit("1", "A", "a"), Instant::now());
        s.apply(&s_end("1", Some("B")), Instant::now());
        assert!(matches!(&s.live_input, LiveInputRow::Hidden));
    }

    #[test]
    fn source_end_binds_an_unbound_open_sentence_id() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_live("1", "hello"), now);
        s.apply(&s_end("1", Some("A")), now);

        let record = s.sentence_by_upstream_id("A").expect("bound sentence");
        assert_eq!(record.source.phase, TrackPhase::Committed);
        assert!(matches!(
            &s.live_input,
            LiveInputRow::Settled { snapshot, .. }
                if snapshot.speaker == SpeakerKey::Diarized("1".into())
                    && snapshot.sentence_id.as_deref() == Some("A")
                    && snapshot.text == "hello"
        ));
    }

    #[test]
    fn source_end_does_not_replace_another_speakers_active_live_row() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_live("1", "from one"), now);
        s.apply(&s_live("2", "from two"), now);
        s.apply(&s_end("1", Some("A")), now);

        assert!(matches!(
            &s.live_input,
            LiveInputRow::Streaming(snapshot)
                if snapshot.speaker == SpeakerKey::Diarized("2".into())
                    && snapshot.text == "from two"
        ));
    }

    #[test]
    fn source_end_id_is_resolved_within_the_owning_speaker() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_commit("1", "A", "first"), now);
        s.apply(&s_live("2", "second"), now);
        s.apply(&s_end("2", Some("A")), now);

        assert!(s.sentences.iter().any(|record| {
            record.speaker == SpeakerKey::Diarized("2".into())
                && record.key.upstream_id.as_deref() == Some("A")
                && record.source.phase == TrackPhase::Committed
        }));
    }

    #[test]
    fn target_without_id_binds_to_open_sentence() {
        let mut s = TranscriptState::default();
        s.apply(&s_live("1", "bonjour"), Instant::now());
        s.apply(&t_draft("1", None, "hello"), Instant::now());
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].target.text, "hello");
    }

    #[test]
    fn target_without_id_binds_to_the_most_recent_original_sentence() {
        // The source is already committed: the desktop still binds the late
        // id-less streaming translation to that sentence, not to nothing.
        let mut s = TranscriptState::default();
        s.apply(&s_commit("1", "A", "done"), Instant::now());
        s.apply(&t_draft("1", None, "late draft"), Instant::now());
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].target.text, "late draft");
        assert_eq!(s.sentences[0].target.phase, TrackPhase::Draft);
    }

    #[test]
    fn target_without_id_and_no_original_becomes_its_own_sentence() {
        let mut s = TranscriptState::default();
        s.apply(&t_draft("1", None, "translation only"), Instant::now());
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].source.phase, TrackPhase::Empty);
        assert_eq!(s.sentences[0].target.text, "translation only");
    }

    #[test]
    fn late_target_for_older_sentence_is_kept_in_the_reducer() {
        let mut s = TranscriptState::default();
        s.apply(&s_commit("1", "A", "a"), Instant::now());
        s.apply(&s_commit("1", "B", "b"), Instant::now());
        s.apply(&t_commit("1", "B", "bee"), Instant::now());
        s.apply(&t_commit("1", "A", "ay"), Instant::now());
        assert_eq!(s.sentence_by_upstream_id("A").unwrap().target.text, "ay");
        assert_eq!(s.sentence_by_upstream_id("B").unwrap().target.text, "bee");
    }

    #[test]
    fn refinement_resolves_by_id_and_replays_when_late() {
        let mut s = TranscriptState::default();
        s.apply(&refine("A", "early"), Instant::now());
        s.apply(&s_commit("1", "A", "a"), Instant::now());
        let record = s.sentence_by_upstream_id("A").unwrap();
        assert_eq!(record.target.phase, TrackPhase::Refined);
        assert_eq!(record.target.text, "early");
    }

    #[test]
    fn language_is_recorded_on_both_tracks() {
        let mut s = TranscriptState::default();
        s.apply(&s_commit("1", "A", "hello"), Instant::now());
        s.apply(&t_commit("1", "A", "bonjour"), Instant::now());
        assert_eq!(s.sentences[0].source.language.as_deref(), Some("en"));
        assert_eq!(s.sentences[0].target.language.as_deref(), Some("fr"));
    }

    #[test]
    fn final_target_wins_over_a_later_same_frame_draft() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_commit("1", "A", "source"), now);
        s.apply(&t_commit("1", "A", "final"), now);
        s.apply(&t_draft("1", Some("A"), "stale draft"), now);
        assert_eq!(s.sentence_by_upstream_id("A").unwrap().target.text, "final");
        assert_eq!(
            s.sentence_by_upstream_id("A").unwrap().target.phase,
            TrackPhase::Committed
        );
    }

    #[test]
    fn preserve_clear_keeps_recency_for_retained_tracks() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_commit("1", "A", "source"), now);
        s.apply(&t_draft("1", Some("A"), "draft"), now);
        s.clear(true);
        assert_eq!(
            s.speaker_recency().back(),
            Some(&SpeakerKey::Diarized("1".into()))
        );
    }

    #[test]
    fn same_sentence_id_does_not_cross_speaker_records() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_commit("1", "same", "speaker one"), now);
        s.apply(&s_commit("2", "same", "speaker two"), now);
        assert_eq!(s.sentences.len(), 2);
        assert_eq!(s.sentences[0].speaker, SpeakerKey::Diarized("1".into()));
        assert_eq!(s.sentences[1].speaker, SpeakerKey::Diarized("2".into()));
    }

    #[test]
    fn recent_ledger_stays_bounded_at_64() {
        let mut s = TranscriptState::default();
        for i in 0..80u64 {
            s.apply(&s_commit("1", &format!("{i}"), "x"), Instant::now());
        }
        assert_eq!(s.sentences.len(), 64);
    }

    #[test]
    fn anonymous_provider_uses_one_speaker_key() {
        let mut s = TranscriptState::default();
        for i in 0..3u64 {
            s.apply(
                &CaptionEvent::SourceCommitted(CommittedSource {
                    speaker: SpeakerKey::Anonymous,
                    sentence_id: Some(format!("{i}")),
                    text: "x".into(),
                    language: None,
                }),
                Instant::now(),
            );
        }
        assert!(s
            .sentences
            .iter()
            .all(|r| r.speaker == SpeakerKey::Anonymous));
        assert_eq!(s.sentences.len(), 3);
    }

    #[test]
    fn clear_true_finalizes_drafts_instead_of_dropping_them() {
        // committed source + draft target -> both kept, draft promoted
        let mut s = TranscriptState::default();
        s.apply(&s_commit("1", "A", "a"), Instant::now());
        s.apply(&t_draft("1", Some("A"), "draft"), Instant::now());
        s.clear(true);
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].source.phase, TrackPhase::Committed);
        assert_eq!(s.sentences[0].target.phase, TrackPhase::Committed);
        assert_eq!(s.sentences[0].target.text, "draft");

        // draft source + committed target -> both kept, draft promoted
        let mut s = TranscriptState::default();
        s.apply(&s_live("2", "draft src"), Instant::now());
        s.apply(&t_commit("2", "B", "final tgt"), Instant::now());
        s.clear(true);
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].source.phase, TrackPhase::Committed);
        assert_eq!(s.sentences[0].source.text, "draft src");
        assert_eq!(s.sentences[0].target.phase, TrackPhase::Committed);

        // an empty record with no content is dropped
        let mut s = TranscriptState::default();
        s.apply(&t_draft("3", Some("C"), "tgt only"), Instant::now());
        s.apply(
            &CaptionEvent::Retract {
                sentence_id: "C".into(),
                replacement_sentence_id: None,
                reason: None,
            },
            Instant::now(),
        );
        s.apply(&t_draft("3", Some("D"), "kept"), Instant::now());
        s.clear(true);
        assert_eq!(s.sentences.len(), 1);
        assert_eq!(s.sentences[0].target.text, "kept");
    }

    #[test]
    fn dismiss_settled_live_matches_and_consumes_once() {
        let mut s = TranscriptState::default();
        s.apply(&s_live("1", "hello wor"), Instant::now());
        s.apply(&s_commit("1", "A", "hello world"), Instant::now());
        assert!(!s.dismiss_settled_live(&SpeakerKey::Diarized("2".into()), None));
        assert!(s.dismiss_settled_live(&SpeakerKey::Diarized("1".into()), Some("A")));
        assert!(matches!(s.live_input, LiveInputRow::Hidden));
        assert!(!s.dismiss_settled_live(&SpeakerKey::Diarized("1".into()), Some("A")));
    }

    #[test]
    fn retracted_id_rejects_late_source_target_and_live_events() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(
            &CaptionEvent::SourceLive(LiveSourceSnapshot {
                speaker: SpeakerKey::Diarized("1".into()),
                sentence_id: Some("A".into()),
                text: "old live".into(),
                language: Some("en".into()),
            }),
            now,
        );
        s.apply(&s_commit("1", "A", "old source"), now);
        s.apply(
            &CaptionEvent::Retract {
                sentence_id: "A".into(),
                replacement_sentence_id: Some("M".into()),
                reason: Some("short_interrupt".into()),
            },
            now,
        );
        s.apply(&s_commit("1", "A", "late source"), now);
        s.apply(&t_commit("1", "A", "late target"), now);

        assert!(s.sentence_by_upstream_id("A").is_none());
        assert!(matches!(s.live_input, LiveInputRow::Hidden));
    }

    #[test]
    fn full_clear_discards_retracted_and_speculative_session_state() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(
            &CaptionEvent::Retract {
                sentence_id: "A".into(),
                replacement_sentence_id: None,
                reason: None,
            },
            now,
        );
        s.apply(
            &CaptionEvent::SpecTranslation(SpecTranslation {
                source: "hello".into(),
                translation: "stale".into(),
                target_lang: Some("fr".into()),
            }),
            now,
        );

        s.clear(false);
        s.apply(&s_commit("1", "A", "hello"), now);

        let record = s
            .sentence_by_upstream_id("A")
            .expect("new session accepts A");
        assert!(record.target.text.is_empty());
        assert!(!s.is_retracted("A"));
    }

    #[test]
    fn speculative_translation_trims_source_and_reuses_cached_result() {
        let mut s = TranscriptState::default();
        let now = Instant::now();
        s.apply(&s_commit("1", "A", " hello "), now);
        s.apply(
            &CaptionEvent::SpecTranslation(SpecTranslation {
                source: "hello".into(),
                translation: "bonjour".into(),
                target_lang: Some("fr".into()),
            }),
            now,
        );
        assert_eq!(
            s.sentence_by_upstream_id("A").unwrap().target.text,
            "bonjour"
        );

        s.apply(&s_commit("1", "B", "hello"), now);
        assert_eq!(
            s.sentence_by_upstream_id("B").unwrap().target.text,
            "bonjour"
        );
    }
}
