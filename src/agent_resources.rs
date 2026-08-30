//! Per-agent process-tree memory enforcement.
//!
//! Linux uses delegated cgroup v2 leaves. macOS (and Linux installations
//! without delegation) fall back to a bounded process-tree RSS watchdog.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use crate::{config::ResourceConfig, detect::Agent, layout::PaneId, platform::Signal};

static CLAUDE_MEMORY_LIMIT_BYTES: AtomicU64 =
    AtomicU64::new(crate::config::DEFAULT_AGENT_MEMORY_LIMIT_BYTES);
static CODEX_MEMORY_LIMIT_BYTES: AtomicU64 =
    AtomicU64::new(crate::config::DEFAULT_AGENT_MEMORY_LIMIT_BYTES);
static PREPARE_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);
const RSS_WATCHDOG_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn configure(config: &ResourceConfig) {
    CLAUDE_MEMORY_LIMIT_BYTES.store(config.claude_memory_limit_bytes, Ordering::Release);
    CODEX_MEMORY_LIMIT_BYTES.store(config.codex_memory_limit_bytes, Ordering::Release);

    tracing::info!(
        claude_memory_limit_bytes = config.claude_memory_limit_bytes,
        codex_memory_limit_bytes = config.codex_memory_limit_bytes,
        "using per-agent memory limits"
    );

    if config.claude_memory_limit_bytes == 0 && config.codex_memory_limit_bytes == 0 {
        return;
    }

    match crate::platform::prepare_agent_memory_controller() {
        Ok(true) => tracing::info!("using delegated cgroup v2 agent memory enforcement"),
        Ok(false) => {
            #[cfg(target_os = "macos")]
            tracing::info!("using process-tree RSS watchdog for agent memory enforcement");
        }
        Err(err) => {
            if !PREPARE_WARNING_EMITTED.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    %err,
                    "cgroup v2 agent memory enforcement is unavailable; using the RSS watchdog"
                );
            }
        }
    }
}

fn memory_limit_for(agent: Agent) -> u64 {
    memory_limit_for_values(
        agent,
        CLAUDE_MEMORY_LIMIT_BYTES.load(Ordering::Acquire),
        CODEX_MEMORY_LIMIT_BYTES.load(Ordering::Acquire),
    )
}

fn memory_limit_for_values(agent: Agent, claude: u64, codex: u64) -> u64 {
    match agent {
        Agent::Claude => claude,
        Agent::Codex => codex,
        _ => 0,
    }
}

struct ControllerState {
    agent: Option<Agent>,
    limit_bytes: u64,
    kernel_scope: Option<String>,
    kernel_enforced: bool,
    watchdog_tripped: bool,
    last_watchdog_sample: Option<Instant>,
}

/// Resource ownership attached to one long-lived pane runtime.
///
/// The cgroup leaf name is serialized during live handoff, while this controller
/// and its detector-side watchdog are recreated by the replacement server.
pub(crate) struct PaneResourceController {
    pane_id: PaneId,
    child_pid: Arc<AtomicU32>,
    state: Mutex<ControllerState>,
}

impl PaneResourceController {
    pub(crate) fn new(
        pane_id: PaneId,
        child_pid: Arc<AtomicU32>,
        imported_scope: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pane_id,
            child_pid,
            state: Mutex::new(ControllerState {
                agent: None,
                limit_bytes: 0,
                kernel_scope: imported_scope,
                kernel_enforced: false,
                watchdog_tripped: false,
                last_watchdog_sample: None,
            }),
        })
    }

    /// Apply policy to a newly identified foreground agent process tree.
    ///
    /// Calls are intentionally forced even when the agent kind is unchanged: a
    /// second Claude/Codex process can replace the first one in the same pane.
    pub(crate) fn observe_agent_process(&self, agent: Option<Agent>) {
        let Some(agent) = agent else {
            if let Ok(mut state) = self.state.lock() {
                state.agent = None;
                state.limit_bytes = 0;
                state.kernel_enforced = false;
                state.watchdog_tripped = false;
                state.last_watchdog_sample = None;
            }
            return;
        };

        let limit_bytes = memory_limit_for(agent);
        let child_pid = self.child_pid.load(Ordering::Acquire);
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.agent = Some(agent);
        state.limit_bytes = limit_bytes;
        state.watchdog_tripped = false;
        state.last_watchdog_sample = None;

        if limit_bytes == 0 || child_pid == 0 {
            if let Some(scope) = state.kernel_scope.as_deref() {
                if let Err(err) = crate::platform::disable_agent_memory_limit(scope) {
                    tracing::warn!(
                        pane = self.pane_id.raw(),
                        ?agent,
                        %err,
                        "failed to disable an existing agent memory limit"
                    );
                }
            }
            state.kernel_enforced = false;
            return;
        }

        match crate::platform::apply_agent_memory_limit(
            state.kernel_scope.as_deref(),
            self.pane_id.raw(),
            child_pid,
            limit_bytes,
        ) {
            Ok(Some(scope)) => {
                state.kernel_scope = Some(scope);
                state.kernel_enforced = true;
                tracing::info!(
                    pane = self.pane_id.raw(),
                    ?agent,
                    limit_bytes,
                    "applied kernel agent memory limit"
                );
            }
            Ok(None) => {
                state.kernel_enforced = false;
            }
            Err(err) => {
                state.kernel_enforced = false;
                tracing::warn!(
                    pane = self.pane_id.raw(),
                    ?agent,
                    limit_bytes,
                    %err,
                    "failed to apply kernel agent memory limit; using the RSS watchdog"
                );
            }
        }
    }

    pub(crate) fn refresh(&self) {
        let agent = self.state.lock().ok().and_then(|state| state.agent);
        self.observe_agent_process(agent);
    }

    /// Enforce the portable fallback on detector ticks.
    pub(crate) fn enforce_watchdog(&self) {
        let (agent, limit_bytes) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.kernel_enforced || state.watchdog_tripped {
                return;
            }
            let Some(agent) = state.agent else {
                return;
            };
            if state.limit_bytes == 0 {
                return;
            }
            let now = Instant::now();
            if !reserve_watchdog_sample(&mut state.last_watchdog_sample, now) {
                return;
            }
            (agent, state.limit_bytes)
        };

        let child_pid = self.child_pid.load(Ordering::Acquire);
        let Some(usage) = crate::platform::agent_process_usage(child_pid) else {
            return;
        };
        if usage.resident_bytes <= limit_bytes {
            return;
        }

        let should_kill = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.agent != Some(agent)
                || state.limit_bytes != limit_bytes
                || state.kernel_enforced
                || state.watchdog_tripped
            {
                false
            } else {
                state.watchdog_tripped = true;
                true
            }
        };
        if !should_kill {
            return;
        }

        tracing::error!(
            pane = self.pane_id.raw(),
            ?agent,
            resident_bytes = usage.resident_bytes,
            limit_bytes,
            "agent exceeded its memory limit; terminating its process tree"
        );
        crate::platform::signal_processes(&usage.pids, Signal::Kill);
    }

    pub(crate) fn handoff_scope_name(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.kernel_scope.clone())
    }

    /// Remove the leaf after the pane's process session has been terminated.
    pub(crate) fn cleanup(&self) {
        let scope = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.kernel_scope.take());
        if let Some(scope) = scope {
            if let Err(err) = crate::platform::cleanup_agent_memory_scope(&scope) {
                tracing::warn!(
                    pane = self.pane_id.raw(),
                    %err,
                    "failed to remove agent memory cgroup"
                );
            }
        }
    }
}

fn reserve_watchdog_sample(last_sample: &mut Option<Instant>, now: Instant) -> bool {
    if last_sample.is_some_and(|last| now.saturating_duration_since(last) < RSS_WATCHDOG_INTERVAL) {
        return false;
    }
    *last_sample = Some(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_limits_are_selected_independently() {
        assert_eq!(memory_limit_for_values(Agent::Claude, 11, 22), 11);
        assert_eq!(memory_limit_for_values(Agent::Codex, 11, 22), 22);
        assert_eq!(memory_limit_for_values(Agent::Gemini, 11, 22), 0);
    }

    #[test]
    fn rss_watchdog_samples_are_rate_limited() {
        let start = Instant::now();
        let mut last_sample = None;
        assert!(reserve_watchdog_sample(&mut last_sample, start));
        assert!(!reserve_watchdog_sample(
            &mut last_sample,
            start + RSS_WATCHDOG_INTERVAL - Duration::from_millis(1)
        ));
        assert!(reserve_watchdog_sample(
            &mut last_sample,
            start + RSS_WATCHDOG_INTERVAL
        ));
    }
}
