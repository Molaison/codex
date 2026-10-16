//! Use the pinned desktop backend's ChatGPT composer-prediction mode only.
//! The main app-server, its provider credentials, and its session database are untouched.
use super::App;
use super::thread_events::ThreadBufferedEvent;
use crate::app_event::AppEvent;
use crate::next_prompt_prediction::{
    PredictionRequest, PredictionTrigger, parse_prediction, prediction_context,
};
use codex_app_server_protocol::ServerNotification;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

impl App {
    pub(super) async fn request_next_prompt_prediction(&mut self, request: PredictionRequest) {
        if !self.chat_widget.next_prompt_prediction_is_current(&request) {
            return;
        }
        let Some(channel) = self.thread_event_channels.get(&request.thread_id) else {
            self.chat_widget
                .apply_next_prompt_prediction(&request, None);
            return;
        };
        let context = {
            let store = channel.store.lock().await;
            let mut seen = std::collections::HashSet::new();
            prediction_context(
                store
                    .turns
                    .iter()
                    .flat_map(|turn| turn.items.iter())
                    .chain(store.buffer.iter().filter_map(|event| {
                        let ThreadBufferedEvent::Notification(notification) = event else {
                            return None;
                        };
                        let ServerNotification::ItemCompleted(notification) = notification.as_ref()
                        else {
                            return None;
                        };
                        Some(&notification.item)
                    }))
                    .filter(|item| seen.insert(item.id().to_string())),
            )
        };
        let Some(items) = context else {
            self.chat_widget
                .apply_next_prompt_prediction(&request, None);
            return;
        };
        let codex_home = self.chat_widget.config_ref().codex_home.to_path_buf();
        let model = self.chat_widget.current_model().to_string();
        let event_sender = self.app_event_tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = request.cancellation.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_millis(700)) => {}
            }
            let result = tokio::select! {
                _ = request.cancellation.cancelled() => return,
                result = run_official_prediction(codex_home, model, items, request.thread_id.to_string(), request.turn_id.clone(), request.trigger) => result,
            };
            event_sender.send(AppEvent::NextPromptPredictionGenerated { request, result });
        });
    }

    pub(super) fn next_prompt_prediction_generated(
        &mut self,
        request: PredictionRequest,
        result: Result<String, String>,
    ) {
        let suggestion = match result {
            Ok(response) => parse_prediction(&response),
            Err(error) => {
                tracing::warn!(%error, "Official CLI prediction unavailable; no paid fallback");
                None
            }
        };
        if suggestion.is_some() && self.chat_widget.next_prompt_prediction_is_current(&request) {
            tracing::info!(thread_id = %request.thread_id, turn_id = %request.turn_id, trigger = request.trigger.as_str(),
                "Official ChatGPT composer prediction ready; ywl main provider unchanged");
        }
        self.chat_widget
            .apply_next_prompt_prediction(&request, suggestion);
    }
}

async fn run_official_prediction(
    codex_home: PathBuf,
    model: String,
    items: Vec<serde_json::Value>,
    source_thread_id: String,
    source_turn_id: String,
    trigger: PredictionTrigger,
) -> Result<String, String> {
    if !cfg!(target_os = "linux") {
        return Err("Official prediction companion is verified on Linux only".into());
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let binary_directory = executable.parent().ok_or("CLI directory unavailable")?;
    // RAII cleanup also removes the copied access token if the helper is cancelled.
    let runtime = tempfile::Builder::new()
        .prefix("codex-cli-prediction-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "codex_home": codex_home, "binary_directory": binary_directory,
        "runtime_root": runtime.path(), "model": model, "items": items,
        "source_thread_id": source_thread_id, "source_turn_id": source_turn_id,
        "prediction_trigger": trigger.as_str(),
    }))
    .map_err(|error| error.to_string())?;
    let mut child = tokio::process::Command::new("python3")
        .args(["-I", "-c", include_str!("official_prediction.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut input = child
        .stdin
        .take()
        .ok_or("Prediction helper stdin unavailable")?;
    input
        .write_all(&payload)
        .await
        .map_err(|error| error.to_string())?;
    input
        .write_all(b"\n")
        .await
        .map_err(|error| error.to_string())?;
    drop(input);
    // Dropping this future kills the helper; Linux parent-death signalling also stops its backend.
    let output = tokio::time::timeout(std::time::Duration::from_secs(45), child.wait_with_output())
        .await
        .map_err(|_| "Official prediction timed out; no fallback".to_string())?
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout.len() > 8192 {
        return Err("Official prediction output too large".into());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}
