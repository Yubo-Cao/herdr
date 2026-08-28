use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::app::state::{
    AppState, ContextMenuState, CopyModeState, KeybindHelpState, Mode, NavigatorState,
    PaneFocusTarget, ProductAnnouncementState, ReleaseNotesState, SettingsState, ViewState,
    WorktreeCreateState, WorktreeOpenState, WorktreeRemoveState,
};
use crate::layout::{PaneId, TileFocusState};
use crate::selection::Selection;
use crate::terminal::TerminalId;

#[derive(Clone)]
struct TabViewState {
    focus: TileFocusState,
    zoomed: bool,
}

/// Ephemeral TUI projection owned by one connected app client.
///
/// Workspace, tab, pane, and terminal topology remain shared in `AppState`.
/// This value contains only the presentation choices that are temporarily
/// materialized into that shared state while routing or rendering one client.
#[derive(Clone)]
pub(crate) struct ClientViewState {
    active_workspace_id: Option<String>,
    selected_workspace_id: Option<String>,
    active_tabs: HashMap<String, usize>,
    tabs: HashMap<(String, usize), TabViewState>,
    mode: Mode,
    previous_pane_focus: Option<PaneFocusTarget>,
    confirm_close_workspace_id: Option<String>,
    creating_new_tab: bool,
    requested_new_tab_name: Option<String>,
    pending_workspace_create_cwd: Option<std::path::PathBuf>,
    rename_pane_target: Option<PaneId>,
    worktree_create: Option<WorktreeCreateState>,
    worktree_open: Option<WorktreeOpenState>,
    worktree_remove: Option<WorktreeRemoveState>,
    collapsed_space_keys: std::collections::HashSet<String>,
    name_input: String,
    name_input_replace_on_type: bool,
    release_notes: Option<ReleaseNotesState>,
    product_announcement: Option<ProductAnnouncementState>,
    keybind_help: KeybindHelpState,
    navigator: NavigatorState,
    copy_mode: Option<CopyModeState>,
    workspace_scroll: usize,
    agent_panel_scroll: usize,
    tab_scroll: usize,
    tab_scroll_follow_active: bool,
    mobile_switcher_scroll: usize,
    view: ViewState,
    drag: Option<crate::app::state::DragState>,
    selection: Option<Selection>,
    selection_autoscroll: Option<crate::app::state::SelectionAutoscroll>,
    context_menu: Option<ContextMenuState>,
    copy_feedback: Option<crate::app::state::CopyFeedback>,
    sidebar_width: u16,
    sidebar_width_source: crate::app::state::SidebarWidthSource,
    sidebar_width_auto: bool,
    sidebar_collapsed: bool,
    sidebar_section_split: f32,
    settings: SettingsState,
    global_menu: crate::app::state::MenuListState,
}

impl ClientViewState {
    pub(crate) fn capture(state: &AppState) -> Self {
        let active_workspace_id = state
            .active
            .and_then(|idx| state.workspaces.get(idx))
            .map(|workspace| workspace.id.clone());
        let selected_workspace_id = state
            .workspaces
            .get(state.selected)
            .map(|workspace| workspace.id.clone());
        let mut active_tabs = HashMap::new();
        let mut tabs = HashMap::new();
        for workspace in &state.workspaces {
            if let Some(tab) = workspace.tabs.get(workspace.active_tab) {
                active_tabs.insert(workspace.id.clone(), tab.number);
            }
            for tab in &workspace.tabs {
                tabs.insert(
                    (workspace.id.clone(), tab.number),
                    TabViewState {
                        focus: tab.layout.focus_state(),
                        zoomed: tab.zoomed,
                    },
                );
            }
        }
        Self {
            active_workspace_id,
            selected_workspace_id,
            active_tabs,
            tabs,
            mode: state.mode,
            previous_pane_focus: state.previous_pane_focus.clone(),
            confirm_close_workspace_id: state.confirm_close_workspace_id.clone(),
            creating_new_tab: state.creating_new_tab,
            requested_new_tab_name: state.requested_new_tab_name.clone(),
            pending_workspace_create_cwd: state.pending_workspace_create_cwd.clone(),
            rename_pane_target: state.rename_pane_target,
            worktree_create: state.worktree_create.clone(),
            worktree_open: state.worktree_open.clone(),
            worktree_remove: state.worktree_remove.clone(),
            collapsed_space_keys: state.collapsed_space_keys.clone(),
            name_input: state.name_input.clone(),
            name_input_replace_on_type: state.name_input_replace_on_type,
            release_notes: state.release_notes.clone(),
            product_announcement: state.product_announcement.clone(),
            keybind_help: state.keybind_help.clone(),
            navigator: state.navigator.clone(),
            copy_mode: state.copy_mode.clone(),
            workspace_scroll: state.workspace_scroll,
            agent_panel_scroll: state.agent_panel_scroll,
            tab_scroll: state.tab_scroll,
            tab_scroll_follow_active: state.tab_scroll_follow_active,
            mobile_switcher_scroll: state.mobile_switcher_scroll,
            view: state.view.clone(),
            drag: state.drag.clone(),
            selection: state.selection.clone(),
            selection_autoscroll: state.selection_autoscroll.clone(),
            context_menu: state.context_menu.clone(),
            copy_feedback: state.copy_feedback.clone(),
            sidebar_width: state.sidebar_width,
            sidebar_width_source: state.sidebar_width_source,
            sidebar_width_auto: state.sidebar_width_auto,
            sidebar_collapsed: state.sidebar_collapsed,
            sidebar_section_split: state.sidebar_section_split,
            settings: state.settings.clone(),
            global_menu: state.global_menu,
        }
    }

    pub(crate) fn apply(&self, state: &mut AppState) {
        for workspace in &mut state.workspaces {
            if let Some(tab_number) = self.active_tabs.get(&workspace.id) {
                if let Some(idx) = workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.number == *tab_number)
                {
                    workspace.active_tab = idx;
                }
            }
            if workspace.active_tab >= workspace.tabs.len() {
                workspace.active_tab = workspace.tabs.len().saturating_sub(1);
            }
            for tab in &mut workspace.tabs {
                if let Some(tab_view) = self.tabs.get(&(workspace.id.clone(), tab.number)) {
                    tab.layout.restore_focus_state(tab_view.focus);
                    tab.zoomed = tab_view.zoomed;
                }
            }
        }

        state.active = self
            .active_workspace_id
            .as_ref()
            .and_then(|id| {
                state
                    .workspaces
                    .iter()
                    .position(|workspace| &workspace.id == id)
            })
            .or_else(|| (!state.workspaces.is_empty()).then_some(0));
        state.selected = self
            .selected_workspace_id
            .as_ref()
            .and_then(|id| {
                state
                    .workspaces
                    .iter()
                    .position(|workspace| &workspace.id == id)
            })
            .or(state.active)
            .unwrap_or(0);
        state.mode = self.mode;
        state.previous_pane_focus = self.previous_pane_focus.clone();
        state.confirm_close_workspace_id = self.confirm_close_workspace_id.clone();
        state.creating_new_tab = self.creating_new_tab;
        state.requested_new_tab_name = self.requested_new_tab_name.clone();
        state.pending_workspace_create_cwd = self.pending_workspace_create_cwd.clone();
        state.rename_pane_target = self.rename_pane_target;
        state.worktree_create = self.worktree_create.clone();
        state.worktree_open = self.worktree_open.clone();
        state.worktree_remove = self.worktree_remove.clone();
        state.collapsed_space_keys = self.collapsed_space_keys.clone();
        state.name_input.clone_from(&self.name_input);
        state.name_input_replace_on_type = self.name_input_replace_on_type;
        state.release_notes = self.release_notes.clone();
        state.product_announcement = self.product_announcement.clone();
        state.keybind_help = self.keybind_help.clone();
        state.navigator = self.navigator.clone();
        state.copy_mode = self.copy_mode.clone();
        state.workspace_scroll = self.workspace_scroll;
        state.agent_panel_scroll = self.agent_panel_scroll;
        state.tab_scroll = self.tab_scroll;
        state.tab_scroll_follow_active = self.tab_scroll_follow_active;
        state.mobile_switcher_scroll = self.mobile_switcher_scroll;
        state.view = self.view.clone();
        state.drag = self.drag.clone();
        state.selection = self.selection.clone();
        state.selection_autoscroll = self.selection_autoscroll.clone();
        state.context_menu = self.context_menu.clone();
        state.copy_feedback = self.copy_feedback.clone();
        state.sidebar_width = self.sidebar_width;
        state.sidebar_width_source = self.sidebar_width_source;
        state.sidebar_width_auto = self.sidebar_width_auto;
        state.sidebar_collapsed = self.sidebar_collapsed;
        state.sidebar_section_split = self.sidebar_section_split;
        state.settings = self.settings.clone();
        state.global_menu = self.global_menu;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientLocation {
    pub workspace_id: String,
    pub tab_number: usize,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

pub(crate) fn materialized_location(state: &AppState) -> Option<ClientLocation> {
    let workspace = state.workspaces.get(state.active?)?;
    let tab = workspace.tabs.get(workspace.active_tab)?;
    let pane_id = tab.layout.focused();
    let terminal_id = tab.panes.get(&pane_id)?.attached_terminal_id.clone();
    Some(ClientLocation {
        workspace_id: workspace.id.clone(),
        tab_number: tab.number,
        pane_id,
        terminal_id,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct GhostPointer {
    pub pane_id: PaneId,
    pub column: u16,
    pub row: u16,
    pub updated_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct ParticipantPresence {
    pub display_name: String,
    pub color_index: usize,
    pub location: Option<ClientLocation>,
    pub pointer: Option<GhostPointer>,
}

impl ParticipantPresence {
    pub(crate) fn anonymous(client_id: u64) -> Self {
        Self {
            display_name: format!("guest-{client_id}"),
            color_index: client_id as usize,
            location: None,
            pointer: None,
        }
    }

    pub(crate) fn set_display_name(&mut self, display_name: &str, client_id: u64) {
        self.display_name =
            sanitize_display_name(display_name).unwrap_or_else(|| format!("guest-{client_id}"));
    }
}

fn spoofing_format_character(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

pub(crate) fn sanitize_display_name(value: &str) -> Option<String> {
    let mut output = String::new();
    let mut pending_space = false;
    for ch in value.trim().chars() {
        if ch.is_control() || spoofing_format_character(ch) {
            continue;
        }
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if output.chars().count() >= 32 {
            break;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(ch);
    }
    (!output.is_empty()).then_some(output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseAcquire {
    Acquired,
    Refreshed,
    Blocked { owner: u64 },
}

#[derive(Debug, Clone)]
struct ControlLease {
    client_id: u64,
    last_activity: Instant,
}

pub(crate) struct ControlLeaseRegistry {
    leases: HashMap<TerminalId, ControlLease>,
    idle_timeout: Duration,
}

impl ControlLeaseRegistry {
    pub(crate) fn new(idle_timeout: Duration) -> Self {
        Self {
            leases: HashMap::new(),
            idle_timeout,
        }
    }

    pub(crate) fn acquire(
        &mut self,
        terminal_id: &TerminalId,
        client_id: u64,
        now: Instant,
    ) -> LeaseAcquire {
        if let Some(lease) = self.leases.get_mut(terminal_id) {
            if lease.client_id == client_id {
                lease.last_activity = now;
                return LeaseAcquire::Refreshed;
            }
            if now.saturating_duration_since(lease.last_activity) < self.idle_timeout {
                return LeaseAcquire::Blocked {
                    owner: lease.client_id,
                };
            }
        }
        self.leases.insert(
            terminal_id.clone(),
            ControlLease {
                client_id,
                last_activity: now,
            },
        );
        LeaseAcquire::Acquired
    }

    pub(crate) fn owner(&self, terminal_id: &TerminalId, now: Instant) -> Option<u64> {
        self.leases.get(terminal_id).and_then(|lease| {
            (now.saturating_duration_since(lease.last_activity) < self.idle_timeout)
                .then_some(lease.client_id)
        })
    }

    pub(crate) fn remove_client(&mut self, client_id: u64) {
        self.leases.retain(|_, lease| lease.client_id != client_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_sanitization_bounds_and_removes_spoofing_controls() {
        let input = "  Alice\u{202e}\n   Example  ";
        assert_eq!(
            sanitize_display_name(input).as_deref(),
            Some("Alice Example")
        );
        assert_eq!(sanitize_display_name("\u{200b}\n"), None);
        assert_eq!(
            sanitize_display_name("abcdefghijklmnopqrstuvwxyz0123456789")
                .expect("bounded name")
                .chars()
                .count(),
            32
        );
    }

    #[test]
    fn control_lease_is_first_writer_wins_then_expires() {
        let terminal_id = TerminalId::alloc();
        let start = Instant::now();
        let mut leases = ControlLeaseRegistry::new(Duration::from_secs(30));
        assert_eq!(
            leases.acquire(&terminal_id, 1, start),
            LeaseAcquire::Acquired
        );
        assert_eq!(
            leases.acquire(&terminal_id, 2, start + Duration::from_secs(1)),
            LeaseAcquire::Blocked { owner: 1 }
        );
        assert_eq!(
            leases.acquire(&terminal_id, 2, start + Duration::from_secs(31)),
            LeaseAcquire::Acquired
        );
    }

    #[test]
    fn removing_client_releases_its_control_leases() {
        let terminal_id = TerminalId::alloc();
        let now = Instant::now();
        let mut leases = ControlLeaseRegistry::new(Duration::from_secs(30));
        leases.acquire(&terminal_id, 7, now);
        leases.remove_client(7);
        assert_eq!(leases.owner(&terminal_id, now), None);
    }
}
