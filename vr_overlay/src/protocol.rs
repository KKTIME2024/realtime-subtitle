use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::ErrorKind;

use futures_util::StreamExt;
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::manifest::OverlayManifest;
use crate::transcript::{
    CaptionEvent, CommittedSource, LiveSourceSnapshot, Refinement, SeparatorKind, SpeakerKey,
    SpecTranslation, TargetUpdate,
};
use crate::views::VrViewSettings;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("bridge connect failed: {0}")]
    Connect(String),
    #[error("bridge url path must be /ws: {0}")]
    UnsupportedPath(String),
    #[error("bridge protocol error: {0}")]
    Protocol(String),
    #[error("bridge disconnected")]
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrackKind {
    Source,
    Translation,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct AccumKey {
    sentence_id: Option<String>,
    speaker: SpeakerKey,
    track: TrackKind,
}

pub struct DesktopProtocol {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pending: VecDeque<CaptionEvent>,
    accumulators: BTreeMap<AccumKey, String>,
    retracted_ids: BTreeSet<String>,
}

impl DesktopProtocol {
    pub async fn connect(manifest: &OverlayManifest) -> Result<Self, BridgeError> {
        if !is_desktop_ws_url(&manifest.bridge_url) {
            return Err(BridgeError::UnsupportedPath(manifest.bridge_url.clone()));
        }
        let (stream, _) = connect_async(&manifest.bridge_url)
            .await
            .map_err(|error| BridgeError::Connect(error.to_string()))?;
        Ok(Self {
            stream,
            pending: VecDeque::new(),
            accumulators: BTreeMap::new(),
            retracted_ids: BTreeSet::new(),
        })
    }

    pub async fn next_event(&mut self) -> Result<CaptionEvent, BridgeError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            self.fill_pending().await?;
        }
    }

    /// Await-free drain: pops a queued event, else polls the socket once.
    pub async fn try_next_event(&mut self) -> Option<Result<CaptionEvent, BridgeError>> {
        if let Some(event) = self.pending.pop_front() {
            return Some(Ok(event));
        }
        match futures_util::future::poll_immediate(self.fill_pending()).await {
            Some(Ok(())) => self.pending.pop_front().map(Ok),
            Some(Err(error)) => Some(Err(error)),
            None => None,
        }
    }

    async fn fill_pending(&mut self) -> Result<(), BridgeError> {
        loop {
            let message = self.stream.next().await.ok_or(BridgeError::Disconnected)?;
            let message = message.map_err(map_ws_error)?;
            match message {
                Message::Text(text) => {
                    self.process_text(&text);
                    return Ok(());
                }
                Message::Close(_) => return Err(BridgeError::Disconnected),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Binary(_) => {
                    self.pending.push_back(CaptionEvent::Activity);
                    return Ok(());
                }
            }
        }
    }

    fn process_text(&mut self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        };
        let Some(map) = value.as_object().cloned() else {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        };
        match map.get("type").and_then(Value::as_str) {
            Some("update") => self.process_update(&map),
            Some("refine_result") => self.process_refine(&map),
            Some("subtitle_retract") => self.process_retract(&map),
            Some("spec_translation_pending") => self.process_spec_pending(&map),
            Some("spec_translation") => self.process_spec_translation(&map),
            Some("clear") => {
                let preserve_existing = match map.get("preserve_existing") {
                    None => false,
                    Some(value) => match value.as_bool() {
                        Some(value) => value,
                        None => {
                            self.pending.push_back(CaptionEvent::Activity);
                            return;
                        }
                    },
                };
                // A preserve_existing rollover keeps sentence ids alive: the
                // same sentence may keep receiving final tokens on the new
                // stream, so the per-sentence accumulators must survive.
                // Only a full clear starts a fresh id space (the backend
                // regenerates its session id on a full session restart).
                if !preserve_existing {
                    self.accumulators.clear();
                    self.retracted_ids.clear();
                }
                self.pending
                    .push_back(CaptionEvent::Clear { preserve_existing });
            }
            Some("shutdown") => self.pending.push_back(CaptionEvent::Shutdown),
            Some("vr_view_settings") => {
                let mut settings_map = map.clone();
                settings_map.remove("type");
                match serde_json::from_value::<VrViewSettings>(Value::Object(settings_map)) {
                    Ok(settings) if settings.validate().is_ok() => {
                        self.pending
                            .push_back(CaptionEvent::ViewSettingsChanged(settings));
                    }
                    _ => self.pending.push_back(CaptionEvent::Activity),
                }
            }
            _ => self.pending.push_back(CaptionEvent::Activity),
        }
        if self.pending.is_empty() {
            self.pending.push_back(CaptionEvent::Activity);
        }
    }

    fn process_update(&mut self, map: &Map<String, Value>) {
        let final_tokens = map
            .get("final_tokens")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        self.accumulate_final_tokens(final_tokens);
        self.emit_non_final(map);
    }

    /// `final_tokens` is an ordered incremental delta: the backend sends each
    /// final token exactly once (it tracks `sent_count` per stream) and the
    /// desktop reference appends unconditionally. Each frame's tokens are
    /// therefore appended to the per-`(sentence_id, speaker, track)`
    /// accumulator; the emitted event always carries the full accumulated
    /// text. Repeated text across frames is valid content and is never
    /// deduplicated.
    fn accumulate_final_tokens(&mut self, tokens: &[Value]) {
        let mut groups: Vec<(AccumKey, String, Option<String>)> = Vec::new();
        for token in tokens {
            let Some(token) = token.as_object() else {
                continue;
            };
            if token
                .get("is_separator")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                self.flush_final_groups(&mut groups);
                self.emit_separator(token);
                continue;
            }
            if token.get("is_final").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let text = token.get("text").and_then(Value::as_str).unwrap_or("");
            let sentence_id = token
                .get("llm_sentence_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            if sentence_id
                .as_deref()
                .is_some_and(|id| self.retracted_ids.contains(id))
            {
                continue;
            }
            let speaker = speaker_from(token);
            let language = token
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if text == "<end>" {
                self.flush_final_groups(&mut groups);
                self.pending.push_back(CaptionEvent::SourceEnd {
                    speaker,
                    sentence_id,
                });
                continue;
            }
            if text.is_empty() {
                continue;
            }
            let key = AccumKey {
                sentence_id,
                speaker,
                track: track_from(token),
            };
            match groups.iter_mut().find(|(known, _, _)| known == &key) {
                Some((_, accumulated, known_language)) => {
                    accumulated.push_str(text);
                    if known_language.is_none() {
                        *known_language = language;
                    }
                }
                None => groups.push((key, text.to_owned(), language)),
            }
        }

        self.flush_final_groups(&mut groups);
    }

    /// A separator is a display line break, not a sentence boundary in the
    /// id-keyed model: id-keyed accumulators are untouched (late translations
    /// for a closed sentence keep their old id and must keep appending).
    /// Only the id-less source track (IPC-injected text, which has no
    /// sentence id) resets, so each displayed IPC segment starts fresh.
    fn emit_separator(&mut self, token: &Map<String, Value>) {
        let kind = match token.get("separator_type").and_then(Value::as_str) {
            Some("punctuation") => SeparatorKind::Punctuation,
            Some("translation") => SeparatorKind::Translation,
            Some("endpoint") => SeparatorKind::Endpoint,
            Some("speaker_change") => SeparatorKind::SpeakerChange,
            Some("interrupt_repair") => SeparatorKind::InterruptRepair,
            Some("reconnect") => SeparatorKind::Reconnect,
            _ => SeparatorKind::Unknown,
        };
        let idless_source_keys: Vec<AccumKey> = self
            .accumulators
            .keys()
            .filter(|key| key.sentence_id.is_none() && key.track == TrackKind::Source)
            .cloned()
            .collect();
        for key in idless_source_keys {
            self.accumulators.remove(&key);
        }
        self.pending.push_back(CaptionEvent::Separator { kind });
    }

    fn flush_final_groups(&mut self, groups: &mut Vec<(AccumKey, String, Option<String>)>) {
        for (key, text, language) in groups.drain(..) {
            let accumulator = self.accumulators.entry(key.clone()).or_default();
            accumulator.push_str(&text);
            let full_text = accumulator.clone();
            let event = match key.track {
                TrackKind::Source => CaptionEvent::SourceCommitted(CommittedSource {
                    speaker: key.speaker,
                    sentence_id: key.sentence_id,
                    text: full_text,
                    language,
                }),
                TrackKind::Translation => CaptionEvent::TargetCommitted(TargetUpdate {
                    speaker: key.speaker,
                    sentence_id: key.sentence_id,
                    text: full_text,
                    language,
                }),
            };
            self.pending.push_back(event);
        }
    }

    /// `non_final_tokens` is a replaceable snapshot. Exactly one live source
    /// row is emitted for the last contiguous speaker run; translation drafts
    /// are emitted once per speaking speaker.
    fn emit_non_final(&mut self, map: &Map<String, Value>) {
        let tokens = map
            .get("non_final_tokens")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut current: Option<(SpeakerKey, String, Option<String>, Option<String>)> = None;
        let mut translation_groups: Vec<(SpeakerKey, String, Option<String>, Option<String>)> =
            Vec::new();
        for token in tokens {
            let Some(token) = token.as_object() else {
                continue;
            };
            let text = token.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() || text == "<end>" {
                continue;
            }
            let speaker = speaker_from(token);
            let sentence_id = token
                .get("llm_sentence_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            let language = token
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if track_from(token) == TrackKind::Translation {
                match translation_groups
                    .iter_mut()
                    .find(|(known, _, _, _)| known == &speaker)
                {
                    Some((_, accumulated, known_id, known_language)) => {
                        accumulated.push_str(text);
                        if known_id.is_none() {
                            *known_id = sentence_id;
                        }
                        if known_language.is_none() {
                            *known_language = language;
                        }
                    }
                    None => {
                        translation_groups.push((speaker, text.to_owned(), sentence_id, language))
                    }
                }
                continue;
            }
            match current.as_mut() {
                Some((known, accumulated, known_id, known_language)) if known == &speaker => {
                    accumulated.push_str(text);
                    if known_id.is_none() {
                        *known_id = sentence_id;
                    }
                    if known_language.is_none() {
                        *known_language = language;
                    }
                }
                _ => current = Some((speaker, text.to_owned(), sentence_id, language)),
            }
        }
        if let Some((speaker, text, sentence_id, language)) = current {
            self.pending
                .push_back(CaptionEvent::SourceLive(LiveSourceSnapshot {
                    speaker,
                    text,
                    sentence_id,
                    language,
                }));
        }
        for (speaker, text, sentence_id, language) in translation_groups {
            self.pending
                .push_back(CaptionEvent::TargetDraft(TargetUpdate {
                    speaker,
                    text,
                    sentence_id,
                    language,
                }));
        }
    }

    /// `refine_result` carries the sentence id, the source text, the draft
    /// (`original_translation`), the refined value (`refined_translation`,
    /// null when `no_change`), and the target language. `no_change: true`
    /// confirms the existing translation; it is not a no-op.
    fn process_refine(&mut self, map: &Map<String, Value>) {
        let Some(sentence_id) = map
            .get("sentence_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        };
        let source = map
            .get("source")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let language = map
            .get("target_lang")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let no_change = map
            .get("no_change")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if no_change {
            self.pending.push_back(CaptionEvent::TargetConfirmed {
                sentence_id: sentence_id.to_owned(),
                source,
            });
            return;
        }
        let refined = map
            .get("refined_translation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let original = map
            .get("original_translation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let text = if refined.is_empty() {
            original
        } else {
            refined
        };
        if text.is_empty() {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        }
        self.pending
            .push_back(CaptionEvent::RefinedTarget(Refinement {
                sentence_id: sentence_id.to_owned(),
                text: text.to_owned(),
                language,
                source,
            }));
    }

    /// `subtitle_retract` removes a retracted sentence by id. The backend
    /// re-emits the merged replacement as fresh final tokens under
    /// `replacement_sentence_id`, so no local reconstruction is needed.
    fn process_retract(&mut self, map: &Map<String, Value>) {
        let Some(sentence_id) = map
            .get("sentence_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        };
        let sentence_id = sentence_id.trim();
        if sentence_id.is_empty() {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        }
        self.retracted_ids.insert(sentence_id.to_owned());
        self.accumulators
            .retain(|key, _| key.sentence_id.as_deref() != Some(sentence_id));
        let replacement = map
            .get("replacement_sentence_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let reason = map.get("reason").and_then(Value::as_str).map(str::to_owned);
        self.pending.push_back(CaptionEvent::Retract {
            sentence_id: sentence_id.to_owned(),
            replacement_sentence_id: replacement,
            reason,
        });
    }

    fn process_spec_pending(&mut self, map: &Map<String, Value>) {
        let Some(source) = map
            .get("source")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        };
        let target_lang = map
            .get("target_lang")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        self.pending.push_back(CaptionEvent::SpecPending {
            source: source.to_owned(),
            target_lang,
        });
    }

    fn process_spec_translation(&mut self, map: &Map<String, Value>) {
        let Some(source) = map
            .get("source")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        };
        let translation = map
            .get("translation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if translation.is_empty() {
            self.pending.push_back(CaptionEvent::Activity);
            return;
        }
        let target_lang = map
            .get("target_lang")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        self.pending
            .push_back(CaptionEvent::SpecTranslation(SpecTranslation {
                source: source.to_owned(),
                translation,
                target_lang,
            }));
    }
}

fn speaker_from(token: &Map<String, Value>) -> SpeakerKey {
    match token.get("speaker").and_then(Value::as_str) {
        Some(speaker) if !speaker.is_empty() => SpeakerKey::Diarized(speaker.to_owned()),
        _ => SpeakerKey::Anonymous,
    }
}

fn track_from(token: &Map<String, Value>) -> TrackKind {
    if token.get("translation_status").and_then(Value::as_str) == Some("translation") {
        TrackKind::Translation
    } else {
        TrackKind::Source
    }
}

fn map_ws_error(error: tokio_tungstenite::tungstenite::Error) -> BridgeError {
    use tokio_tungstenite::tungstenite::Error as WsError;
    match &error {
        WsError::ConnectionClosed | WsError::AlreadyClosed => BridgeError::Disconnected,
        WsError::Io(io)
            if matches!(
                io.kind(),
                ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionReset
                    | ErrorKind::NotConnected
                    | ErrorKind::UnexpectedEof
            ) =>
        {
            BridgeError::Disconnected
        }
        other => BridgeError::Protocol(other.to_string()),
    }
}

pub(crate) fn is_desktop_ws_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://") else {
        return false;
    };
    let Some(path_offset) = rest.find('/') else {
        return false;
    };
    let authority = &rest[..path_offset];
    let path = &rest[path_offset..];
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    host == "127.0.0.1" && port.parse::<u16>().is_ok_and(|port| port != 0) && path == "/ws"
}

#[cfg(test)]
mod tests {
    use super::is_desktop_ws_url;

    #[test]
    fn desktop_ws_url_matches_only_the_local_unauthenticated_endpoint() {
        assert!(is_desktop_ws_url("ws://127.0.0.1:1/ws"));
        assert!(is_desktop_ws_url("ws://127.0.0.1:65535/ws"));
        assert!(!is_desktop_ws_url("ws://127.0.0.1:1/vr_ws"));
        assert!(!is_desktop_ws_url("ws://127.0.0.1:1/workspace"));
        assert!(!is_desktop_ws_url("ws://127.0.0.1:1"));
        assert!(!is_desktop_ws_url("ws://127.0.0.1:0/ws"));
        assert!(!is_desktop_ws_url("ws://127.0.0.1:1/ws?token=x"));
        assert!(!is_desktop_ws_url("wss://127.0.0.1:1/ws"));
        assert!(!is_desktop_ws_url("ws://192.168.0.2:1/ws"));
        assert!(!is_desktop_ws_url("ws://example.test:1/ws"));
    }
}
