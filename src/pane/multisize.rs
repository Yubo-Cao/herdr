//! Experimental time-multiplexed multi-size viewing of one pane.
//!
//! Enabled with `HERDR_EXPERIMENTAL_MULTISIZE=1`. A pane keeps its normal
//! ("primary") emulator at the geometry controller's size, plus one shadow
//! emulator per secondary viewer size class. When a secondary class is stale,
//! the scheduler briefly resizes the PTY (not the primary emulator) to that
//! class, the app repaints, and bytes read while the PTY has that size are fed
//! only to the matching shadow emulator. Viewers whose pane area matches a
//! class render from its shadow; everyone else renders the primary as today.
//!
//! The scheduler only cycles alternate-screen apps (full-screen TUIs repaint
//! completely on SIGWINCH), never while the pane receives input, and only when
//! a class has missed output since its last slot, so an idle pane never cycles.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const ENV: &str = "HERDR_EXPERIMENTAL_MULTISIZE";

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var(ENV).is_ok_and(|value| value == "1"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub(crate) const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    pub(crate) fn pack(self) -> u32 {
        (u32::from(self.cols) << 16) | u32::from(self.rows)
    }

    pub(crate) fn unpack(packed: u32) -> Option<Self> {
        (packed != 0).then(|| Self::new((packed >> 16) as u16, packed as u16))
    }

    /// Sizes close to the primary are served by cropping, as today; only
    /// clearly different viewports (a phone next to a desktop) get a class.
    fn differs_enough_from(self, primary: Self) -> bool {
        let dc = self.cols.abs_diff(primary.cols);
        let dr = self.rows.abs_diff(primary.rows);
        u32::from(dc) * 5 >= u32::from(primary.cols) || u32::from(dr) * 5 >= u32::from(primary.rows)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SlotPolicy {
    /// Minimum time at the primary size between secondary slots.
    pub primary_min: Duration,
    /// Time the PTY spends at a secondary size per slot.
    pub secondary_slot: Duration,
    /// Output this soon after a slot change is the app's resize repaint,
    /// not new content, so it does not make other classes stale.
    pub redraw_grace: Duration,
    /// No cycling while the pane received input this recently.
    pub input_quiet: Duration,
    /// A class disappears when no viewer rendered it for this long.
    pub viewer_ttl: Duration,
    pub max_classes: usize,
}

impl Default for SlotPolicy {
    fn default() -> Self {
        Self {
            primary_min: Duration::from_millis(1800),
            secondary_slot: Duration::from_millis(200),
            redraw_grace: Duration::from_millis(150),
            input_quiet: Duration::from_millis(1500),
            viewer_ttl: Duration::from_secs(30),
            max_classes: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Primary,
    Secondary(GridSize),
}

#[derive(Debug)]
struct ClassState {
    last_seen: Instant,
    stale: bool,
}

/// Pure slot policy; no I/O, so it is unit-tested with synthetic clocks.
#[derive(Debug)]
pub(crate) struct SlotScheduler {
    policy: SlotPolicy,
    classes: BTreeMap<GridSize, ClassState>,
    slot: Slot,
    slot_started: Instant,
    last_input: Option<Instant>,
    next_class: usize,
}

impl SlotScheduler {
    pub(crate) fn new(policy: SlotPolicy, now: Instant) -> Self {
        Self {
            policy,
            classes: BTreeMap::new(),
            slot: Slot::Primary,
            slot_started: now,
            last_input: None,
            next_class: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn slot(&self) -> Slot {
        self.slot
    }

    pub(crate) fn has_classes(&self) -> bool {
        !self.classes.is_empty()
    }

    /// Records that a viewer rendered this size. Returns false when the size
    /// does not get its own class (too close to primary, or too many classes).
    pub(crate) fn note_viewer(&mut self, size: GridSize, primary: GridSize, now: Instant) -> bool {
        if size == primary || !size.differs_enough_from(primary) {
            return false;
        }
        if let Some(class) = self.classes.get_mut(&size) {
            class.last_seen = now;
            return true;
        }
        if self.classes.len() >= self.policy.max_classes {
            return false;
        }
        self.classes.insert(
            size,
            ClassState {
                last_seen: now,
                stale: true,
            },
        );
        true
    }

    /// Output arrived while the PTY had the size of `slot`.
    pub(crate) fn note_output(&mut self, slot: Slot, now: Instant) {
        if now.duration_since(self.slot_started) < self.policy.redraw_grace {
            return;
        }
        for (size, class) in &mut self.classes {
            if slot != Slot::Secondary(*size) {
                class.stale = true;
            }
        }
    }

    pub(crate) fn note_input(&mut self, now: Instant) {
        self.last_input = Some(now);
    }

    /// The primary size changed: drop classes that now match or sit too close.
    pub(crate) fn primary_changed(&mut self, primary: GridSize) {
        self.classes
            .retain(|size, _| *size != primary && size.differs_enough_from(primary));
        if let Slot::Secondary(size) = self.slot {
            if !self.classes.contains_key(&size) {
                self.slot = Slot::Primary;
            }
        }
    }

    fn input_recent(&self, now: Instant) -> bool {
        self.last_input
            .is_some_and(|at| now.duration_since(at) < self.policy.input_quiet)
    }

    /// Advances the schedule. Returns the new slot when the PTY must change size.
    pub(crate) fn tick(&mut self, now: Instant, alternate_screen: bool) -> Option<Slot> {
        let ttl = self.policy.viewer_ttl;
        self.classes
            .retain(|_, class| now.duration_since(class.last_seen) < ttl);
        let elapsed = now.duration_since(self.slot_started);
        let next = match self.slot {
            Slot::Secondary(size) => {
                let gone = !self.classes.contains_key(&size);
                let interrupted = self.input_recent(now) || !alternate_screen;
                if gone || interrupted {
                    Some(Slot::Primary)
                } else if elapsed >= self.policy.secondary_slot {
                    if let Some(class) = self.classes.get_mut(&size) {
                        class.stale = false;
                    }
                    Some(Slot::Primary)
                } else {
                    None
                }
            }
            Slot::Primary => {
                if !alternate_screen || self.input_recent(now) || elapsed < self.policy.primary_min
                {
                    None
                } else {
                    let stale: Vec<GridSize> = self
                        .classes
                        .iter()
                        .filter(|(_, class)| class.stale)
                        .map(|(size, _)| *size)
                        .collect();
                    if stale.is_empty() {
                        None
                    } else {
                        let pick = stale[self.next_class % stale.len()];
                        self.next_class = self.next_class.wrapping_add(1);
                        Some(Slot::Secondary(pick))
                    }
                }
            }
        };
        if let Some(slot) = next {
            self.slot = slot;
            self.slot_started = now;
        }
        next
    }
}

/// Per-pane multi-size state shared by the PTY reader, renderers and the scheduler task.
pub(crate) struct MultiSize<T> {
    /// PTY size last applied by the actor thread (packed [`GridSize`]); the
    /// read path compares it with `primary` to route bytes.
    applied: Arc<AtomicU32>,
    primary: AtomicU32,
    cell_px: AtomicU64,
    inner: Mutex<Inner<T>>,
    make_shadow: Box<dyn Fn(GridSize) -> Option<T> + Send + Sync>,
}

struct Inner<T> {
    scheduler: SlotScheduler,
    shadows: HashMap<GridSize, Arc<T>>,
}

impl<T> MultiSize<T> {
    pub(crate) fn new(
        primary: GridSize,
        policy: SlotPolicy,
        make_shadow: Box<dyn Fn(GridSize) -> Option<T> + Send + Sync>,
    ) -> Self {
        Self {
            applied: Arc::new(AtomicU32::new(primary.pack())),
            primary: AtomicU32::new(primary.pack()),
            cell_px: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                scheduler: SlotScheduler::new(policy, Instant::now()),
                shadows: HashMap::new(),
            }),
            make_shadow,
        }
    }

    pub(crate) fn applied_size_cell(&self) -> Arc<AtomicU32> {
        self.applied.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn primary(&self) -> GridSize {
        GridSize::unpack(self.primary.load(Ordering::Acquire)).unwrap_or(GridSize::new(80, 24))
    }

    pub(crate) fn cell_px(&self) -> (u32, u32) {
        let packed = self.cell_px.load(Ordering::Acquire);
        ((packed >> 32) as u32, packed as u32)
    }

    pub(crate) fn set_primary(&self, primary: GridSize, cell_width_px: u32, cell_height_px: u32) {
        self.cell_px.store(
            (u64::from(cell_width_px) << 32) | u64::from(cell_height_px),
            Ordering::Release,
        );
        self.primary.store(primary.pack(), Ordering::Release);
        let mut inner = self.lock();
        inner.scheduler.primary_changed(primary);
        let Inner {
            scheduler, shadows, ..
        } = &mut *inner;
        shadows.retain(|size, _| scheduler.classes.contains_key(size));
    }

    /// Called on the PTY actor thread for every read. Returns the shadow that
    /// must receive the bytes, or `None` when they belong to the primary.
    pub(crate) fn route_read(&self, now: Instant) -> Option<Arc<T>> {
        let applied = GridSize::unpack(self.applied.load(Ordering::Acquire));
        let mut inner = self.lock();
        let shadow = applied
            .filter(|size| *size != self.primary())
            .and_then(|size| inner.shadows.get(&size).cloned());
        let slot = match (&shadow, applied) {
            (Some(_), Some(size)) => Slot::Secondary(size),
            _ => Slot::Primary,
        };
        inner.scheduler.note_output(slot, now);
        shadow
    }

    /// A viewer rendered the pane at `size`; returns the shadow to render, if any.
    pub(crate) fn shadow_for_viewer(&self, size: GridSize, now: Instant) -> Option<Arc<T>> {
        let primary = self.primary();
        let mut inner = self.lock();
        if !inner.scheduler.note_viewer(size, primary, now) {
            return None;
        }
        if let Some(shadow) = inner.shadows.get(&size) {
            return Some(shadow.clone());
        }
        let shadow = Arc::new((self.make_shadow)(size)?);
        inner.shadows.insert(size, shadow.clone());
        Some(shadow)
    }

    pub(crate) fn has_classes(&self) -> bool {
        self.lock().scheduler.has_classes()
    }

    pub(crate) fn note_input(&self, now: Instant) {
        self.lock().scheduler.note_input(now);
    }

    /// Returns the PTY size to apply when the slot changes.
    pub(crate) fn tick(&self, now: Instant, alternate_screen: bool) -> Option<GridSize> {
        let primary = self.primary();
        let mut inner = self.lock();
        let slot = inner.scheduler.tick(now, alternate_screen)?;
        let Inner {
            scheduler, shadows, ..
        } = &mut *inner;
        shadows.retain(|size, _| scheduler.classes.contains_key(size));
        Some(match slot {
            Slot::Primary => primary,
            Slot::Secondary(size) => size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: GridSize = GridSize::new(160, 45);
    const S: GridSize = GridSize::new(50, 40);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn scheduler(t0: Instant) -> SlotScheduler {
        SlotScheduler::new(SlotPolicy::default(), t0)
    }

    #[test]
    fn close_sizes_do_not_get_a_class() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        assert!(!s.note_viewer(L, L, t0));
        assert!(!s.note_viewer(GridSize::new(150, 44), L, t0));
        assert!(s.note_viewer(S, L, t0));
        assert!(s.has_classes());
    }

    #[test]
    fn class_count_is_bounded() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        assert!(s.note_viewer(GridSize::new(50, 40), L, t0));
        assert!(s.note_viewer(GridSize::new(90, 30), L, t0));
        assert!(!s.note_viewer(GridSize::new(40, 20), L, t0));
    }

    #[test]
    fn new_class_gets_one_slot_after_primary_minimum_then_idles() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        assert_eq!(
            s.tick(t0 + ms(100), true),
            None,
            "primary minimum not reached"
        );
        assert_eq!(s.tick(t0 + ms(1800), true), Some(Slot::Secondary(S)));
        assert_eq!(s.tick(t0 + ms(1900), true), None);
        assert_eq!(s.tick(t0 + ms(2000), true), Some(Slot::Primary));
        // The repaint that follows the switch back is not new content.
        s.note_output(Slot::Primary, t0 + ms(2010));
        assert_eq!(s.tick(t0 + ms(9000), true), None, "idle pane never cycles");
    }

    #[test]
    fn primary_output_makes_secondary_stale_again() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        s.tick(t0 + ms(1800), true);
        s.tick(t0 + ms(2000), true);
        s.note_output(Slot::Primary, t0 + ms(2500));
        assert_eq!(s.tick(t0 + ms(3000), true), None, "primary minimum again");
        assert_eq!(s.tick(t0 + ms(3800), true), Some(Slot::Secondary(S)));
    }

    #[test]
    fn output_in_own_slot_does_not_restale_that_class() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        s.tick(t0 + ms(1800), true);
        s.note_output(Slot::Secondary(S), t0 + ms(1990));
        s.tick(t0 + ms(2000), true);
        assert_eq!(s.tick(t0 + ms(5000), true), None);
    }

    #[test]
    fn input_pauses_cycling_and_ends_a_secondary_slot() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        s.note_input(t0 + ms(1700));
        assert_eq!(s.tick(t0 + ms(1800), true), None);
        assert_eq!(s.tick(t0 + ms(3300), true), Some(Slot::Secondary(S)));
        s.note_input(t0 + ms(3350));
        assert_eq!(s.tick(t0 + ms(3360), true), Some(Slot::Primary));
        // The interrupted slot did not refresh the class, so it is still due.
        assert_eq!(s.tick(t0 + ms(6000), true), Some(Slot::Secondary(S)));
    }

    #[test]
    fn primary_screen_apps_never_cycle() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        assert_eq!(s.tick(t0 + ms(5000), false), None);
    }

    #[test]
    fn viewers_expire() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        assert_eq!(s.tick(t0 + ms(31_000), true), None);
        assert!(!s.has_classes());
    }

    #[test]
    fn primary_change_to_the_class_size_returns_to_primary() {
        let t0 = Instant::now();
        let mut s = scheduler(t0);
        s.note_viewer(S, L, t0);
        s.tick(t0 + ms(1800), true);
        s.primary_changed(S);
        assert_eq!(s.slot(), Slot::Primary);
        assert!(!s.has_classes());
    }

    #[test]
    fn classes_alternate_round_robin() {
        let t0 = Instant::now();
        let a = GridSize::new(50, 40);
        let b = GridSize::new(90, 30);
        let mut s = scheduler(t0);
        s.note_viewer(a, L, t0);
        s.note_viewer(b, L, t0);
        let first = s.tick(t0 + ms(1800), true);
        s.tick(t0 + ms(2000), true);
        let second = s.tick(t0 + ms(3800), true);
        assert_ne!(first, second);
    }

    #[test]
    fn multisize_routes_reads_by_applied_pty_size() {
        let primary = L;
        let state: MultiSize<Mutex<Vec<u8>>> = MultiSize::new(
            primary,
            SlotPolicy::default(),
            Box::new(|_| Some(Mutex::new(Vec::new()))),
        );
        let now = Instant::now();
        let shadow = state.shadow_for_viewer(S, now).expect("phone class");
        assert!(state.route_read(now).is_none(), "PTY still at primary size");
        state.applied_size_cell().store(S.pack(), Ordering::Release);
        let routed = state.route_read(now).expect("PTY at phone size");
        assert!(Arc::ptr_eq(&routed, &shadow));
        // A nudge or unknown size belongs to the primary.
        state
            .applied_size_cell()
            .store(GridSize::new(160, 44).pack(), Ordering::Release);
        assert!(state.route_read(now).is_none());
    }

    fn ghostty_pane_terminal(
        size: GridSize,
        tx: &tokio::sync::mpsc::Sender<bytes::Bytes>,
    ) -> Option<crate::pane::PaneTerminal> {
        let terminal = crate::ghostty::Terminal::new(size.cols, size.rows, 1 << 20).ok()?;
        let ghostty = crate::pane::GhosttyPaneTerminal::new(terminal, tx.clone()).ok()?;
        Some(crate::pane::PaneTerminal::new(ghostty))
    }

    #[test]
    fn shadow_and_primary_emulators_keep_their_own_frames() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let pane_id = crate::layout::PaneId::from_raw(1);
        let primary = ghostty_pane_terminal(L, &tx).expect("primary emulator");
        let make_tx = tx.clone();
        let state = MultiSize::new(
            L,
            SlotPolicy::default(),
            Box::new(move |size| ghostty_pane_terminal(size, &make_tx)),
        );
        let now = Instant::now();
        let shadow = state.shadow_for_viewer(S, now).expect("phone shadow");

        // The primary shows the desktop frame.
        assert!(state.route_read(now).is_none());
        primary.process_pty_bytes(pane_id, 0, b"\x1b[?1049h\x1b[2J\x1b[Hdesktop frame", &tx);

        // A phone slot: the app repaints for 50x40 and only the shadow sees it.
        state.applied_size_cell().store(S.pack(), Ordering::Release);
        let routed = state.route_read(now).expect("routed to shadow");
        routed.process_pty_bytes(pane_id, 0, b"\x1b[?1049h\x1b[2J\x1b[Hphone frame", &tx);

        // Back at the desktop size, later output goes to the primary again.
        state.applied_size_cell().store(L.pack(), Ordering::Release);
        assert!(state.route_read(now).is_none());
        primary.process_pty_bytes(pane_id, 0, b"\x1b[2J\x1b[Hdesktop repaint", &tx);

        let phone = shadow.visible_text();
        let desktop = primary.visible_text();
        assert!(phone.contains("phone frame"), "{phone:?}");
        assert!(!phone.contains("desktop"), "{phone:?}");
        assert!(desktop.contains("desktop repaint"), "{desktop:?}");
        assert!(!desktop.contains("phone"), "{desktop:?}");
    }

    /// Replays a recorded PTY session (JSONL from the multisize harness) through
    /// per-size ghostty emulators, routing bytes by the PTY size at read time,
    /// and checks every slot ends with the emulator matching a settled
    /// reference. Run with `HERDR_MULTISIZE_REPLAY=/path/log.jsonl`.
    #[test]
    #[ignore = "needs a recorded session log"]
    fn replay_recorded_session_through_shadow_emulators() {
        use base64::Engine as _;
        let Ok(path) = std::env::var("HERDR_MULTISIZE_REPLAY") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("log");
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let pane_id = crate::layout::PaneId::from_raw(1);
        let mut emulators: HashMap<GridSize, crate::pane::PaneTerminal> = HashMap::new();
        let mut active = L;
        emulators.insert(L, ghostty_pane_terminal(L, &tx).expect("emulator"));
        let mut refs: HashMap<GridSize, String> = HashMap::new();
        let mut slot_ends: Vec<(GridSize, String)> = Vec::new();
        let mut cycling = false;
        for line in text.lines().skip(1) {
            let event: serde_json::Value = serde_json::from_str(line).expect("json");
            match event["k"].as_str() {
                Some("out") => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(event["p"].as_str().unwrap_or_default())
                        .expect("base64");
                    emulators[&active].process_pty_bytes(pane_id, 0, &bytes, &tx);
                }
                Some("resize") => {
                    if cycling {
                        slot_ends.push((active, emulators[&active].visible_text()));
                    }
                    let cols = event["p"][0].as_u64().unwrap_or(80) as u16;
                    let rows = event["p"][1].as_u64().unwrap_or(24) as u16;
                    active = GridSize::new(cols, rows);
                    emulators
                        .entry(active)
                        .or_insert_with(|| ghostty_pane_terminal(active, &tx).expect("emulator"));
                }
                Some("mark") => match event["p"]["label"].as_str() {
                    Some("ref") => {
                        refs.insert(active, emulators[&active].visible_text());
                    }
                    Some("alt_start") => cycling = true,
                    Some("alt_end") => cycling = false,
                    _ => {}
                },
                _ => {}
            }
        }
        let checked: Vec<bool> = slot_ends
            .iter()
            .filter_map(|(size, screen)| refs.get(size).map(|reference| reference == screen))
            .collect();
        let correct = checked.iter().filter(|ok| **ok).count();
        eprintln!(
            "replay: {correct}/{} slot ends match the settled reference",
            checked.len()
        );
        assert!(!checked.is_empty());
        assert_eq!(correct, checked.len());
    }
}
