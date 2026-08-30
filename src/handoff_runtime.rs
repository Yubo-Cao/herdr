#[cfg(unix)]
use serde::{Deserialize, Serialize};

/// Long-lived pane runtime transferred during server replacement.
///
/// Handoff preserves server-owned session state such as PTYs, processes, agent
/// identity, and durable plugin/session metadata. It intentionally does not
/// preserve transient coordination such as in-flight requests, waits,
/// subscriptions, client sockets, or pane-to-pane messages; clients reconnect
/// and retry those operations after replacement.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffRuntimeState {
    pub pane_id: u32,
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    #[serde(default)]
    pub keyboard_protocol_flags: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_protocol_ansi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_state: Option<crate::pane::InputState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_history_ansi: Option<String>,
    /// Linux cgroup leaf retained across a process-preserving server handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_memory_scope: Option<String>,
}

#[cfg(unix)]
impl HandoffRuntimeState {
    pub fn with_pane_id(mut self, pane_id: crate::layout::PaneId) -> Self {
        self.pane_id = pane_id.raw();
        self
    }
}

#[derive(Debug)]
pub(crate) struct ImportedHandoffRuntime {
    #[cfg(unix)]
    pub master_fd: std::os::fd::RawFd,
    #[cfg(unix)]
    pub state: HandoffRuntimeState,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn legacy_handoff_state_defaults_resource_scope_to_none() {
        let state: HandoffRuntimeState = serde_json::from_str(
            r#"{
                "pane_id": 7,
                "child_pid": 42,
                "rows": 24,
                "cols": 80,
                "cell_width_px": 0,
                "cell_height_px": 0
            }"#,
        )
        .unwrap();

        assert!(state.agent_memory_scope.is_none());
    }

    #[test]
    fn pane_id_remap_preserves_resource_scope() {
        let mut state: HandoffRuntimeState = serde_json::from_str(
            r#"{
                "pane_id": 7,
                "child_pid": 42,
                "rows": 24,
                "cols": 80,
                "cell_width_px": 0,
                "cell_height_px": 0,
                "agent_memory_scope": "pane-7-42"
            }"#,
        )
        .unwrap();
        state = state.with_pane_id(crate::layout::PaneId::from_raw(99));

        assert_eq!(state.pane_id, 99);
        assert_eq!(state.agent_memory_scope.as_deref(), Some("pane-7-42"));
    }
}
