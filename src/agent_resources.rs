//! Per-agent process-tree memory enforcement.
//!
//! Linux uses delegated cgroup v2 leaves. macOS (and Linux installations
//! without delegation) fall back to a bounded process-tree RSS watchdog.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use crate::{config::ResourceConfig, detect::Agent, layout::PaneId, platform::Signal};

static CLAUDE_MEMORY_LIMIT_BYTES: AtomicU64 =
    AtomicU64::new(crate::config::DEFAULT_AGENT_MEMORY_LIMIT_BYTES);
static CODEX_MEMORY_LIMIT_BYTES: AtomicU64 =
    AtomicU64::new(crate::config::DEFAULT_AGENT_MEMORY_LIMIT_BYTES);
static KILL_TREE_ON_MEMORY_LIMIT: AtomicBool = AtomicBool::new(false);
static MEMORY_WARN_PERCENT: AtomicU8 =
    AtomicU8::new(crate::config::DEFAULT_AGENT_MEMORY_WARN_PERCENT);
static PREPARE_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);
const RSS_WATCHDOG_INTERVAL: Duration = Duration::from_secs(2);
/// How far usage must fall below the warning watermark before it can fire
/// again. Without the gap, a tree parked at the watermark warns on every tick.
const MEMORY_WARN_REARM_MARGIN_PERCENT: u8 = 15;

/// Why a pane's memory is worth telling the user about.
///
/// The kernel enforces limits on its own schedule and reports after the fact,
/// so every variant here describes something that has already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMemoryNotice {
    /// Usage crossed the warning watermark; the tree is still running.
    Pressure { used_bytes: u64, limit_bytes: u64 },
    /// The kernel killed one or more processes in the tree.
    Killed { processes: u64, limit_bytes: u64 },
    /// The portable fallback watchdog terminated the tree itself.
    WatchdogTerminated { used_bytes: u64, limit_bytes: u64 },
}

pub(crate) fn configure(config: &ResourceConfig) {
    CLAUDE_MEMORY_LIMIT_BYTES.store(config.claude_memory_limit_bytes, Ordering::Release);
    CODEX_MEMORY_LIMIT_BYTES.store(config.codex_memory_limit_bytes, Ordering::Release);
    KILL_TREE_ON_MEMORY_LIMIT.store(config.kill_tree_on_memory_limit, Ordering::Release);
    MEMORY_WARN_PERCENT.store(config.memory_warn_percent, Ordering::Release);

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
    /// Cumulative kernel kill count already reported for the current leaf.
    reported_oom_kills: u64,
    /// Whether the warning watermark has fired without usage falling back.
    warned_high: bool,
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
                reported_oom_kills: 0,
                warned_high: false,
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
                state.warned_high = false;
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
        state.warned_high = false;

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
            KILL_TREE_ON_MEMORY_LIMIT.load(Ordering::Acquire),
        ) {
            Ok(Some(scope)) => {
                // A leaf carried across a live handoff, or reused by a second
                // agent in the same pane, already has kills on its counter.
                // Seeding from the current reading keeps history out of the
                // notifications this controller is about to raise.
                state.reported_oom_kills = crate::platform::agent_memory_snapshot(&scope)
                    .map(|snapshot| snapshot.oom_kills)
                    .unwrap_or(0);
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

    /// Sample this pane's memory on a detector tick.
    ///
    /// Returns the one thing worth telling the user about, if anything did
    /// happen. Kernel enforcement and the portable watchdog are mutually
    /// exclusive, so at most one of them can have anything to say.
    pub(crate) fn poll_memory(&self) -> Option<(Agent, AgentMemoryNotice)> {
        let agent = self.state.lock().ok()?.agent?;
        let notice = self
            .enforce_watchdog()
            .or_else(|| self.poll_kernel_memory())?;
        Some((agent, notice))
    }

    /// Enforce the portable fallback on detector ticks.
    pub(crate) fn enforce_watchdog(&self) -> Option<AgentMemoryNotice> {
        let (agent, limit_bytes) = {
            let mut state = self.state.lock().ok()?;
            if state.kernel_enforced || state.watchdog_tripped {
                return None;
            }
            let agent = state.agent?;
            if state.limit_bytes == 0 {
                return None;
            }
            let now = Instant::now();
            if !reserve_watchdog_sample(&mut state.last_watchdog_sample, now) {
                return None;
            }
            (agent, state.limit_bytes)
        };

        let child_pid = self.child_pid.load(Ordering::Acquire);
        let usage = crate::platform::agent_process_usage(child_pid)?;
        if usage.resident_bytes <= limit_bytes {
            return None;
        }

        let should_kill = {
            let mut state = self.state.lock().ok()?;
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
            return None;
        }

        tracing::error!(
            pane = self.pane_id.raw(),
            ?agent,
            resident_bytes = usage.resident_bytes,
            limit_bytes,
            "agent exceeded its memory limit; terminating its process tree"
        );
        crate::platform::signal_processes(&usage.pids, Signal::Kill);
        Some(AgentMemoryNotice::WatchdogTerminated {
            used_bytes: usage.resident_bytes,
            limit_bytes,
        })
    }

    /// Read what the kernel did to this pane's leaf since the last tick.
    fn poll_kernel_memory(&self) -> Option<AgentMemoryNotice> {
        let (agent, scope, limit_bytes) = {
            let state = self.state.lock().ok()?;
            if !state.kernel_enforced {
                return None;
            }
            (state.agent?, state.kernel_scope.clone()?, state.limit_bytes)
        };
        if limit_bytes == 0 {
            return None;
        }
        let snapshot = crate::platform::agent_memory_snapshot(&scope)?;

        let mut state = self.state.lock().ok()?;
        // The agent, its limit or its leaf can all have changed while the
        // snapshot was being read; reporting against the old one would name the
        // wrong pane contents.
        if state.agent != Some(agent)
            || state.limit_bytes != limit_bytes
            || state.kernel_scope.as_deref() != Some(scope.as_str())
        {
            return None;
        }

        if snapshot.oom_kills > state.reported_oom_kills {
            let processes = snapshot.oom_kills - state.reported_oom_kills;
            state.reported_oom_kills = snapshot.oom_kills;
            // A kill is the loud event; suppressing the watermark warning that
            // would otherwise follow keeps one incident to one notification.
            state.warned_high = true;
            tracing::error!(
                pane = self.pane_id.raw(),
                ?agent,
                processes,
                limit_bytes,
                "the kernel killed processes in this agent's memory cgroup"
            );
            return Some(AgentMemoryNotice::Killed {
                processes,
                limit_bytes,
            });
        }

        let warn_percent = MEMORY_WARN_PERCENT.load(Ordering::Acquire);
        let Some((warn_bytes, rearm_bytes)) = memory_warn_watermarks(limit_bytes, warn_percent)
        else {
            state.warned_high = false;
            return None;
        };
        if state.warned_high {
            if snapshot.current_bytes <= rearm_bytes {
                state.warned_high = false;
            }
            return None;
        }
        if snapshot.current_bytes < warn_bytes {
            return None;
        }
        state.warned_high = true;
        tracing::warn!(
            pane = self.pane_id.raw(),
            ?agent,
            used_bytes = snapshot.current_bytes,
            limit_bytes,
            "agent memory is close to its limit"
        );
        Some(AgentMemoryNotice::Pressure {
            used_bytes: snapshot.current_bytes,
            limit_bytes,
        })
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

/// Render a memory size the way a person reading a notice would say it.
pub(crate) fn format_memory_limit(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        let tenths = (bytes * 10).div_ceil(GIB);
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    } else {
        format!("{} MiB", bytes.div_ceil(MIB))
    }
}

/// The line left behind in a pane whose processes were killed.
///
/// A SIGKILL from the kernel leaves nothing on screen: no exit status, no
/// message, just a shell that suddenly has no children. Without this the death
/// is indistinguishable from the agent deciding to quit, so anything that ends
/// a process tree writes its own headstone into the pane it emptied.
pub(crate) fn tombstone_line(agent: Agent, notice: AgentMemoryNotice) -> Option<String> {
    let label = crate::detect::agent_label(agent);
    let detail = match notice {
        AgentMemoryNotice::Killed {
            processes,
            limit_bytes,
        } => format!(
            "{label} reached its {} memory limit; the kernel killed {processes} process{} in this pane",
            format_memory_limit(limit_bytes),
            if processes == 1 { "" } else { "es" }
        ),
        AgentMemoryNotice::WatchdogTerminated {
            used_bytes,
            limit_bytes,
        } => format!(
            "{label} reached {} against its {} memory limit; Herdr terminated its process tree",
            format_memory_limit(used_bytes),
            format_memory_limit(limit_bytes)
        ),
        // Pressure is a warning about a tree that is still running. Writing
        // into a live program's screen would corrupt it for no gain.
        AgentMemoryNotice::Pressure { .. } => return None,
    };
    // Start on a fresh line so a half-drawn prompt is not absorbed into the
    // message, and reset SGR afterwards so the shell keeps its own colours.
    Some(format!("\r\n\x1b[1;31m[herdr]\x1b[0m {detail}\r\n"))
}

/// Warning and re-arm thresholds in bytes, or `None` when warnings are off.
fn memory_warn_watermarks(limit_bytes: u64, warn_percent: u8) -> Option<(u64, u64)> {
    if limit_bytes == 0 || warn_percent == 0 || warn_percent > 100 {
        return None;
    }
    let rearm_percent = warn_percent.saturating_sub(MEMORY_WARN_REARM_MARGIN_PERCENT);
    // Divide before multiplying: an agent limit is far below where u64 could
    // wrap, and this keeps that true for any limit a config can name.
    let scale = |percent: u8| limit_bytes / 100 * u64::from(percent);
    Some((scale(warn_percent), scale(rearm_percent)))
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
    fn a_killed_tree_leaves_a_headstone_naming_the_limit() {
        let line = tombstone_line(
            Agent::Claude,
            AgentMemoryNotice::Killed {
                processes: 26,
                limit_bytes: 12 * 1024 * 1024 * 1024,
            },
        )
        .expect("a kill is worth a headstone");
        assert!(line.contains("claude"), "{line}");
        assert!(line.contains("12.0 GiB"), "{line}");
        assert!(line.contains("killed 26 processes"), "{line}");
        assert!(line.starts_with("\r\n"), "{line:?}");
        assert!(line.ends_with("\r\n"), "{line:?}");
    }

    #[test]
    fn a_single_killed_process_is_not_pluralized() {
        let line = tombstone_line(
            Agent::Codex,
            AgentMemoryNotice::Killed {
                processes: 1,
                limit_bytes: 4 * 1024 * 1024 * 1024,
            },
        )
        .expect("a kill is worth a headstone");
        assert!(line.contains("killed 1 process in"), "{line}");
    }

    #[test]
    fn a_surviving_tree_under_pressure_leaves_no_headstone() {
        assert_eq!(
            tombstone_line(
                Agent::Claude,
                AgentMemoryNotice::Pressure {
                    used_bytes: 1,
                    limit_bytes: 2,
                },
            ),
            None
        );
    }

    #[test]
    fn memory_sizes_read_as_a_person_would_say_them() {
        assert_eq!(format_memory_limit(12 * 1024 * 1024 * 1024), "12.0 GiB");
        assert_eq!(format_memory_limit(1536 * 1024 * 1024), "1.5 GiB");
        assert_eq!(format_memory_limit(512 * 1024 * 1024), "512 MiB");
    }

    #[test]
    fn memory_warnings_rearm_below_the_watermark() {
        let (warn, rearm) = memory_warn_watermarks(100, 80).expect("warnings are enabled");
        assert_eq!(warn, 80);
        assert_eq!(rearm, 65);
    }

    #[test]
    fn memory_warnings_are_disabled_without_a_percentage_or_a_limit() {
        assert_eq!(memory_warn_watermarks(100, 0), None);
        assert_eq!(memory_warn_watermarks(0, 80), None);
        assert_eq!(memory_warn_watermarks(100, 101), None);
    }

    #[test]
    fn a_low_warning_percentage_rearms_at_zero_rather_than_wrapping() {
        let (warn, rearm) = memory_warn_watermarks(100, 10).expect("warnings are enabled");
        assert_eq!(warn, 10);
        assert_eq!(rearm, 0);
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
