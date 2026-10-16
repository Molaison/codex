//! Optional CLI next-message predictions. No account or provider configuration is changed.
//! Each request belongs to one completed turn and an untouched composer. Cancellation
//! survives asynchronous startup, and dropping a composer cancels its pending request.

use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PredictionTrigger {
    Live,
    CatchUp,
}

impl PredictionTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::CatchUp => "catch_up",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PredictionRequest {
    pub(crate) thread_id: ThreadId,
    pub(crate) turn_id: String,
    pub(crate) request_id: Uuid,
    pub(crate) cancellation: CancellationToken,
    pub(crate) trigger: PredictionTrigger,
}

#[derive(Default)]
pub(crate) struct ComposerPrediction {
    pending: Option<(Uuid, CancellationToken)>,
    suggestion: Option<String>,
}

impl ComposerPrediction {
    pub(crate) fn begin(&mut self) -> (Uuid, CancellationToken) {
        self.clear();
        let request = (Uuid::new_v4(), CancellationToken::new());
        self.pending = Some(request.clone());
        request
    }

    pub(crate) fn is_current(&self, request_id: Uuid) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|(id, cancellation)| *id == request_id && !cancellation.is_cancelled())
    }

    pub(crate) fn complete(&mut self, request_id: Uuid, suggestion: Option<String>) -> bool {
        if !self.is_current(request_id) {
            return false;
        }
        self.clear();
        self.suggestion = suggestion;
        true
    }

    pub(crate) fn suggestion(&self) -> Option<&str> {
        self.suggestion.as_deref()
    }

    pub(crate) fn clear(&mut self) -> bool {
        if let Some((_, cancellation)) = self.pending.take() {
            cancellation.cancel();
        }
        self.suggestion.take().is_some()
    }
}

impl Drop for ComposerPrediction {
    fn drop(&mut self) {
        self.clear();
    }
}

pub(crate) fn enabled() -> bool {
    !std::env::var("CODEX_CLI_PREDICTIONS")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "off"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prediction {
    #[serde(deserialize_with = "Option::deserialize")]
    suggestion: Option<String>,
}

pub(crate) fn parse_prediction(response: &str) -> Option<String> {
    let parsed: Prediction = serde_json::from_str(response).ok()?;
    let suggestion = parsed.suggestion?.trim().to_string();
    (!suggestion.is_empty()
        && suggestion.chars().count() <= 240
        && !suggestion.chars().any(char::is_control)
        && !suggestion.starts_with('/')
        && !suggestion.starts_with('!'))
    .then_some(suggestion)
}

/// Include only recent user text and final assistant messages, never tools or reasoning.
/// Bound each message and the message count without splitting Unicode characters.
pub(crate) fn prediction_context<'a>(
    items: impl Iterator<Item = &'a ThreadItem>,
) -> Option<Vec<serde_json::Value>> {
    let messages: Vec<_> = items
        .filter_map(|item| match item {
            ThreadItem::UserMessage { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|input| match input {
                        UserInput::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.trim().is_empty()).then_some(("user", text))
            }
            ThreadItem::AgentMessage { text, phase, .. }
                if !matches!(phase, Some(MessagePhase::Commentary)) && !text.trim().is_empty() =>
            {
                Some(("assistant", text.clone()))
            }
            _ => None,
        })
        .collect();
    if messages.last().is_none_or(|(role, _)| *role != "assistant") {
        return None;
    }
    let mut messages: Vec<_> = messages
        .into_iter()
        .rev()
        .take(8)
        .map(|(role, text)| json!({"type":"message","role":role,"content":[{"type":if role == "user" {"input_text"} else {"output_text"},"text":text.chars().take(1500).collect::<String>()}]}))
        .collect();
    messages.reverse();
    Some(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_or_replaced_predictions_cannot_apply() {
        let mut state = ComposerPrediction::default();
        let (old_id, old_token) = state.begin();
        let (new_id, new_token) = state.begin();
        assert!(old_token.is_cancelled());
        assert!(!state.complete(old_id, Some("stale".into())));
        assert!(state.complete(new_id, Some("继续检查".into())));
        assert!(new_token.is_cancelled());
        assert_eq!(state.suggestion(), Some("继续检查"));
        assert!(!state.complete(new_id, Some("duplicate".into())));
        state.clear();
        assert_eq!(state.suggestion(), None);
    }

    #[test]
    fn dropping_composer_cancels_pending_generation() {
        let mut state = ComposerPrediction::default();
        let (_, cancellation) = state.begin();
        drop(state);
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn prediction_accepts_unicode_but_rejects_commands_and_control_sequences() {
        assert_eq!(
            parse_prediction(r#"{"suggestion":"  请检查边界情况  "}"#),
            Some("请检查边界情况".into())
        );
        for invalid in [
            r#"{"suggestion":null}"#,
            "{}",
            r#"{"suggestion":"/quit"}"#,
            r#"{"suggestion":"!rm file"}"#,
            r#"{"suggestion":"hello\nworld"}"#,
            r#"{"suggestion":"\u001b[31mhello"}"#,
            r#"{"suggestion":"ok","extra":true}"#,
        ] {
            assert_eq!(parse_prediction(invalid), None);
        }
        assert_eq!(
            parse_prediction(&json!({"suggestion":"中".repeat(241)}).to_string()),
            None
        );
    }

    #[test]
    fn context_excludes_commentary_and_bounds_unicode_messages() {
        let items = vec![
            ThreadItem::AgentMessage {
                id: "private".into(),
                text: "private reasoning text".into(),
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            ThreadItem::AgentMessage {
                id: "final".into(),
                text: "中".repeat(2000),
                phase: Some(MessagePhase::FinalAnswer),
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        ];
        let prompt = serde_json::to_string(&prediction_context(items.iter()).unwrap()).unwrap();
        assert!(!prompt.contains("private reasoning text"));
        assert_eq!(prompt.matches('中').count(), 1500);
        assert!(prompt.len() < 6000);
        assert!(prediction_context(std::iter::empty()).is_none());
    }
}
