use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::schema::{
    CollaborationClaimParams, CollaborationLeaveParams, CollaborationReleaseParams,
    CollaborationSnapshot, CollaborationUpdateParams, EventData, EventEnvelope, EventKind,
    ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:./".contains(&byte))
}

fn valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_optional_identifier(value: Option<&str>) -> bool {
    value.is_none_or(valid_identifier)
}

impl App {
    pub(crate) fn emit_collaboration_updated(&mut self, snapshot: CollaborationSnapshot) {
        self.emit_event(EventEnvelope {
            event: EventKind::CollaborationUpdated,
            data: EventData::CollaborationUpdated { snapshot },
        });
    }

    pub(super) fn handle_collaboration_update(
        &mut self,
        id: String,
        mut params: CollaborationUpdateParams,
    ) -> String {
        if !valid_identifier(&params.participant_id) {
            return encode_error(id, "invalid_participant_id", "invalid participant_id");
        }
        params.display_name = params.display_name.trim().chars().take(80).collect();
        if params.display_name.is_empty() {
            return encode_error(id, "invalid_display_name", "display_name is required");
        }
        if !valid_color(&params.color) {
            return encode_error(id, "invalid_color", "color must be a #rrggbb value");
        }
        if !valid_optional_identifier(params.workspace_id.as_deref())
            || !valid_optional_identifier(params.tab_id.as_deref())
            || !valid_optional_identifier(params.pane_id.as_deref())
        {
            return encode_error(
                id,
                "invalid_collaboration_target",
                "workspace_id, tab_id, and pane_id must be valid identifiers",
            );
        }
        params.surface = params.surface.trim().chars().take(32).collect();
        if params.surface.is_empty() {
            params.surface = "api".into();
        }
        let now = unix_ms();
        let participant = self.state.collaboration.update_participant(params, now);
        let snapshot = self.state.collaboration.snapshot(now);
        debug_assert!(snapshot
            .participants
            .iter()
            .any(|entry| entry.participant_id == participant.participant_id));
        self.emit_collaboration_updated(snapshot.clone());
        encode_success(id, ResponseResult::CollaborationSnapshot { snapshot })
    }

    pub(super) fn handle_collaboration_list(&mut self, id: String) -> String {
        let snapshot = self.state.collaboration.snapshot(unix_ms());
        encode_success(id, ResponseResult::CollaborationSnapshot { snapshot })
    }

    pub(super) fn handle_collaboration_leave(
        &mut self,
        id: String,
        params: CollaborationLeaveParams,
    ) -> String {
        if !valid_identifier(&params.participant_id) {
            return encode_error(id, "invalid_participant_id", "invalid participant_id");
        }
        let released = self.state.collaboration.leave(&params.participant_id);
        if released {
            let snapshot = self.state.collaboration.snapshot(unix_ms());
            self.emit_collaboration_updated(snapshot);
        }
        encode_success(id, ResponseResult::CollaborationReleased { released })
    }

    pub(super) fn handle_collaboration_claim(
        &mut self,
        id: String,
        params: CollaborationClaimParams,
    ) -> String {
        if !valid_identifier(&params.participant_id) {
            return encode_error(id, "invalid_participant_id", "invalid participant_id");
        }
        if !valid_identifier(&params.pane_id) {
            return encode_error(id, "invalid_pane_id", "invalid pane_id");
        }
        match self.state.collaboration.claim_pane(
            &params.participant_id,
            &params.pane_id,
            params.takeover,
            params.protect_ms,
            unix_ms(),
        ) {
            Ok((granted, claim)) => {
                if granted {
                    let snapshot = self.state.collaboration.snapshot(unix_ms());
                    self.emit_collaboration_updated(snapshot);
                }
                encode_success(id, ResponseResult::CollaborationClaim { granted, claim })
            }
            Err("participant_not_registered") => encode_error(
                id,
                "participant_not_registered",
                "call collaboration.update before claiming a pane",
            ),
            Err("participant_is_viewer") => encode_error(
                id,
                "permission_denied",
                "viewer participants cannot control panes",
            ),
            Err(message) => encode_error(id, "collaboration_error", message),
        }
    }

    pub(super) fn handle_collaboration_release(
        &mut self,
        id: String,
        params: CollaborationReleaseParams,
    ) -> String {
        if !valid_identifier(&params.participant_id) {
            return encode_error(id, "invalid_participant_id", "invalid participant_id");
        }
        if !valid_identifier(&params.pane_id) {
            return encode_error(id, "invalid_pane_id", "invalid pane_id");
        }
        let released = self
            .state
            .collaboration
            .release_pane(&params.participant_id, &params.pane_id);
        if released {
            let snapshot = self.state.collaboration.snapshot(unix_ms());
            self.emit_collaboration_updated(snapshot);
        }
        encode_success(id, ResponseResult::CollaborationReleased { released })
    }
}
