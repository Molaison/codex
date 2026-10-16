//! Predict after live completion or history replay while the user is idle.
use super::*;
use crate::next_prompt_prediction::PredictionRequest;
use crate::next_prompt_prediction::PredictionTrigger;

#[derive(Default)]
pub(super) struct PredictionSchedule {
    attempted: Option<(ThreadId, String)>,
    deferred: Option<(ThreadId, String, PredictionTrigger)>,
}

impl ChatWidget {
    pub(super) fn maybe_request_next_prompt_prediction(
        &mut self,
        turn_id: String,
        trigger: PredictionTrigger,
    ) {
        let Some(thread_id) = self.thread_id else {
            return;
        };
        // Ignore older replay pages and duplicate live/snapshot completion events.
        if self.turn_lifecycle.last_turn_id.as_deref() != Some(turn_id.as_str())
            || self
                .prediction_schedule
                .attempted
                .as_ref()
                .is_some_and(|(thread, turn)| *thread == thread_id && *turn == turn_id)
        {
            return;
        }
        if !crate::next_prompt_prediction::enabled()
            || self.blocks_direct_input
            || self.is_user_turn_pending_or_running()
            || self.has_queued_follow_up_messages()
            || self.active_mode_kind() == ModeKind::Plan
            || self
                .current_goal_status
                .as_ref()
                .is_some_and(GoalStatusState::is_active)
            || self.should_hide_realtime_delegation(&turn_id)
        {
            return;
        }
        // History can arrive before the first MCP status. None then means startup
        // has not reported yet, not that it finished. Wait for configured servers
        // too, so their first Ready update cannot cancel an already queued request.
        let awaiting_initial_mcp = !self.mcp_startup_ignore_updates_until_next_start
            && self.mcp_startup_expected_servers.as_ref().map_or_else(
                || {
                    self.config
                        .mcp_servers
                        .get()
                        .values()
                        .any(|server| server.enabled)
                },
                |servers| !servers.is_empty(),
            );
        if self.mcp_startup_status.is_some() || awaiting_initial_mcp {
            if self.bottom_pane.composer_is_empty() {
                self.prediction_schedule.deferred = Some((thread_id, turn_id, trigger));
            }
            return;
        }
        self.prediction_schedule.deferred = None;
        let Some((request_id, cancellation)) = self.bottom_pane.begin_next_prompt_prediction()
        else {
            return;
        };
        self.prediction_schedule.attempted = Some((thread_id, turn_id.clone()));
        self.app_event_tx
            .send(AppEvent::RequestNextPromptPrediction {
                request: PredictionRequest {
                    thread_id,
                    turn_id,
                    request_id,
                    cancellation,
                    trigger,
                },
            });
    }

    pub(super) fn retry_deferred_next_prompt_prediction(&mut self) {
        if self.mcp_startup_status.is_none()
            && let Some((thread_id, turn_id, trigger)) = self.prediction_schedule.deferred.take()
            && self.thread_id == Some(thread_id)
        {
            self.maybe_request_next_prompt_prediction(turn_id, trigger);
        }
    }

    pub(super) fn reset_next_prompt_prediction(&mut self) {
        self.prediction_schedule = PredictionSchedule::default();
        self.bottom_pane.cancel_next_prompt_prediction();
    }

    pub(crate) fn next_prompt_prediction_is_current(&self, request: &PredictionRequest) -> bool {
        self.thread_id == Some(request.thread_id)
            && self.turn_lifecycle.last_turn_id.as_deref() == Some(request.turn_id.as_str())
            && !self.is_user_turn_pending_or_running()
            && !request.cancellation.is_cancelled()
            && self
                .bottom_pane
                .next_prompt_prediction_is_current(request.request_id)
    }

    pub(crate) fn apply_next_prompt_prediction(
        &mut self,
        request: &PredictionRequest,
        suggestion: Option<String>,
    ) {
        if self.next_prompt_prediction_is_current(request) {
            self.bottom_pane
                .complete_next_prompt_prediction(request.request_id, suggestion);
            self.request_redraw();
        } else {
            request.cancellation.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn next_prompt_prediction_waits_for_first_mcp_status_after_replay() {
        use codex_app_server_protocol::{
            McpServerStartupState, McpServerStatusUpdatedNotification,
        };
        for expected_known in [false, true] {
            let (mut widget, _, mut events, _) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            let server: codex_config::types::McpServerConfig =
                toml::from_str::<toml::Value>("command = 'true'")
                    .unwrap()
                    .try_into()
                    .unwrap();
            widget
                .config
                .mcp_servers
                .set(HashMap::from([
                    ("alpha".to_string(), server.clone()),
                    ("beta".to_string(), server),
                ]))
                .unwrap();
            widget.thread_id = Some(ThreadId::new());
            widget.turn_lifecycle.last_turn_id = Some("completed-turn".into());
            if expected_known {
                widget.set_mcp_startup_expected_servers(["alpha".into(), "beta".into()]);
            }
            assert!(widget.mcp_startup_status.is_none());
            widget.maybe_request_next_prompt_prediction(
                "completed-turn".into(),
                PredictionTrigger::CatchUp,
            );
            assert!(
                !std::iter::from_fn(|| events.try_recv().ok())
                    .any(|event| matches!(event, AppEvent::RequestNextPromptPrediction { .. })),
                "history replay must wait even before the first MCP status arrives",
            );
            widget.set_mcp_startup_expected_servers(["alpha".into(), "beta".into()]);
            let mut requests = Vec::new();
            for name in ["alpha", "beta"] {
                widget.on_mcp_server_status_updated(McpServerStatusUpdatedNotification {
                    thread_id: widget.thread_id.map(|id| id.to_string()),
                    name: name.into(),
                    status: McpServerStartupState::Ready,
                    error: None,
                    failure_reason: None,
                });
                while let Ok(event) = events.try_recv() {
                    if let AppEvent::RequestNextPromptPrediction { request } = event {
                        requests.push(request);
                    }
                }
                if name == "alpha" {
                    assert!(
                        requests.is_empty(),
                        "the remaining MCP server is still pending"
                    );
                }
            }
            assert_eq!(requests.len(), 1);
            assert!(widget.next_prompt_prediction_is_current(&requests[0]));
        }
    }

    #[tokio::test]
    async fn next_prompt_prediction_duplicate_completion_keeps_original_request() {
        let (mut widget, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        widget.thread_id = Some(ThreadId::new());
        widget.turn_lifecycle.last_turn_id = Some("completed-turn".into());
        for trigger in [
            PredictionTrigger::Live,
            PredictionTrigger::CatchUp,
            PredictionTrigger::Live,
        ] {
            widget.maybe_request_next_prompt_prediction("completed-turn".into(), trigger);
        }
        let mut requests = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let AppEvent::RequestNextPromptPrediction { request } = event {
                requests.push(request);
            }
        }
        assert_eq!(
            requests.len(),
            1,
            "duplicate completion must not cancel and restart prediction"
        );
        assert!(widget.next_prompt_prediction_is_current(&requests[0]));
        widget.apply_next_prompt_prediction(&requests[0], None);
        widget.maybe_request_next_prompt_prediction(
            "completed-turn".into(),
            PredictionTrigger::CatchUp,
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, AppEvent::RequestNextPromptPrediction { .. }),
                "a null or sampled-out result must not be sampled again"
            );
        }
    }

    #[tokio::test]
    async fn next_prompt_prediction_resume_waits_for_mcp_startup() {
        let (mut widget, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        widget.thread_id = Some(ThreadId::new());
        widget.turn_lifecycle.last_turn_id = Some("completed-turn".into());
        widget.mcp_startup_status = Some(HashMap::new());
        widget.update_task_running_state();
        widget.maybe_request_next_prompt_prediction(
            "completed-turn".into(),
            PredictionTrigger::CatchUp,
        );
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event,
                AppEvent::RequestNextPromptPrediction { .. }
            ));
        }
        widget.finish_mcp_startup(Vec::new(), Vec::new());
        let mut requests = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let AppEvent::RequestNextPromptPrediction { request } = event {
                requests.push(request);
            }
        }
        assert_eq!(
            requests.len(),
            1,
            "catch-up must not be lost during MCP initialization"
        );
        assert!(widget.next_prompt_prediction_is_current(&requests[0]));
    }

    #[tokio::test]
    async fn next_prompt_prediction_resume_only_predicts_latest_completed_turn() {
        use codex_app_server_protocol::{Turn, TurnItemsView, TurnStatus};
        for latest_status in [
            TurnStatus::Completed,
            TurnStatus::Interrupted,
            TurnStatus::InProgress,
        ] {
            let (mut widget, _, mut events, _) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            widget.thread_id = Some(ThreadId::new());
            let turns = [TurnStatus::Completed, latest_status.clone()]
                .into_iter()
                .enumerate()
                .map(|(index, status)| Turn {
                    id: format!("turn-{index}"),
                    root_turn_id: None,
                    items_view: TurnItemsView::Full,
                    items: vec![ThreadItem::AgentMessage {
                        id: format!("answer-{index}"),
                        text: "Would you like an example?".into(),
                        phase: None,
                        memory_citation: None,
                        delivery: None,
                        questions: None,
                    }],
                    status,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                })
                .collect();
            widget.replay_thread_turns(turns, ReplayKind::ResumeInitialMessages);
            let mut current = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let AppEvent::RequestNextPromptPrediction { request } = event {
                    assert_eq!(request.trigger, PredictionTrigger::CatchUp);
                    if widget.next_prompt_prediction_is_current(&request) {
                        current.push(request);
                    }
                }
            }
            if latest_status == TurnStatus::Completed {
                assert_eq!(current.len(), 1);
                assert_eq!(current[0].turn_id, "turn-1");
                // Restoring a draft after history replay must cancel catch-up too.
                widget.bottom_pane.set_composer_text(
                    "restored draft".into(),
                    Vec::new(),
                    Vec::new(),
                );
                assert!(!widget.next_prompt_prediction_is_current(&current[0]));
            } else {
                assert!(
                    current.is_empty(),
                    "failed/running latest turns must not predict an older answer"
                );
            }
        }
    }

    #[tokio::test]
    async fn next_prompt_prediction_supports_ywl_and_rejects_other_thread_result() {
        let (mut widget, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        widget.config.model_provider_id = "ywl".into();
        widget.thread_id = Some(ThreadId::new());
        widget.turn_lifecycle.last_turn_id = Some("completed-turn".into());
        widget
            .maybe_request_next_prompt_prediction("completed-turn".into(), PredictionTrigger::Live);
        let mut request = None;
        while let Ok(event) = events.try_recv() {
            if let AppEvent::RequestNextPromptPrediction { request: value } = event {
                request = Some(value);
            }
        }
        let request = request.expect("ywl must not be excluded by provider/auth gates");
        assert!(widget.next_prompt_prediction_is_current(&request));
        widget.thread_id = Some(ThreadId::new());
        widget.apply_next_prompt_prediction(&request, Some("stale".into()));
        assert!(request.cancellation.is_cancelled());
        assert!(widget.bottom_pane.composer_is_empty());
    }

    #[tokio::test]
    async fn next_prompt_prediction_disconnect_cancels_and_allows_fresh_catchup() {
        let (mut widget, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        widget.thread_id = Some(ThreadId::new());
        widget.turn_lifecycle.last_turn_id = Some("completed-turn".into());
        widget
            .maybe_request_next_prompt_prediction("completed-turn".into(), PredictionTrigger::Live);
        let request = std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|event| {
                if let AppEvent::RequestNextPromptPrediction { request } = event {
                    Some(request)
                } else {
                    None
                }
            })
            .unwrap();
        widget.pause_for_disconnect();
        assert!(request.cancellation.is_cancelled());
        widget.maybe_request_next_prompt_prediction(
            "completed-turn".into(),
            PredictionTrigger::CatchUp,
        );
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::RequestNextPromptPrediction { .. }))
        );
    }

    #[tokio::test]
    async fn next_prompt_prediction_deferred_startup_respects_drafts_and_new_turns() {
        for draft in [true, false] {
            let (mut widget, _, mut events, _) =
                crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
            widget.thread_id = Some(ThreadId::new());
            widget.turn_lifecycle.last_turn_id = Some("completed-turn".into());
            widget.mcp_startup_status = Some(HashMap::new());
            widget.update_task_running_state();
            widget.maybe_request_next_prompt_prediction(
                "completed-turn".into(),
                PredictionTrigger::CatchUp,
            );
            if draft {
                widget
                    .bottom_pane
                    .set_composer_text("my draft".into(), Vec::new(), Vec::new());
            } else {
                widget.turn_lifecycle.last_turn_id = Some("next-turn".into());
                widget.on_task_started();
            }
            widget.finish_mcp_startup(Vec::new(), Vec::new());
            assert!(
                !std::iter::from_fn(|| events.try_recv().ok())
                    .any(|event| matches!(event, AppEvent::RequestNextPromptPrediction { .. }))
            );
        }
    }

    #[tokio::test]
    async fn next_prompt_prediction_does_not_request_over_a_user_draft() {
        let (mut widget, _, mut events, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        widget.thread_id = Some(ThreadId::new());
        widget
            .bottom_pane
            .set_composer_text("my draft".into(), Vec::new(), Vec::new());
        widget.maybe_request_next_prompt_prediction(
            "completed-turn".into(),
            PredictionTrigger::CatchUp,
        );
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event,
                AppEvent::RequestNextPromptPrediction { .. }
            ));
        }
        assert_eq!(widget.bottom_pane.composer_text(), "my draft");
    }
}
