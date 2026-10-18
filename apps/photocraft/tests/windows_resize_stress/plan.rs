//! The deterministic core of the resize stress test: where the cursor travels, what a soak run does for a
//! given seed, and how a run is judged. Plain data and arithmetic, so it is unit-tested on every platform;
//! `win.rs` and `main.rs` only drive Windows with it.

/// Where the test parks the window before a drag (its top-left, px): clear of the screen's corner, so the
/// left and top borders can be grabbed too.
pub const PARK: (i32, i32) = (80, 40);
/// Space kept free between a parked window and the screen's right and bottom edges.
pub const MARGIN: i32 = 60;
/// px the cursor moves after the press, so that egui starts the drag and asks the OS to resize.
pub const NUDGE_PX: i32 = 8;
/// Seconds between `ui.inspect` probes while the window is being resized.
pub const PROBE_INTERVAL_S: f64 = 0.25;

/// SplitMix64: a small seeded generator, so a soak run is reproducible from its seed without a new dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `lo..=hi` (`lo` when the range is empty).
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        let span = u64::try_from(i64::from(hi) - i64::from(lo) + 1).unwrap_or(1).max(1);
        lo.saturating_add(i32::try_from(self.next_u64() % span).unwrap_or(0))
    }

    /// A value in `lo..hi`.
    pub fn float(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + (hi - lo) * unit
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        let n = u64::try_from(items.len()).ok().filter(|n| *n > 0)?;
        items.get(usize::try_from(self.next_u64() % n).ok()?).copied()
    }

    /// An item with probability proportional to its weight.
    pub fn weighted<T: Copy>(&mut self, items: &[(T, u32)]) -> Option<T> {
        let total: u64 = items.iter().map(|(_, w)| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        let mut at = self.next_u64() % total;
        for (item, weight) in items {
            let weight = u64::from(*weight);
            if at < weight {
                return Some(*item);
            }
            at -= weight;
        }
        None
    }
}

/// A triangle wave in `0.0..=1.0` with period 1: 0 at phase 0, 1 at phase 0.5.
pub fn triangle(phase: f64) -> f64 {
    1.0 - (2.0 * phase.rem_euclid(1.0) - 1.0).abs()
}

/// How far the grabbed border travels, and how fast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sweep {
    /// px the border travels back and forth. Negative drags the other way (a left border).
    pub amplitude: i32,
    /// Seconds for one there-and-back.
    pub period_s: f64,
    /// px added to every offset: negative starts the sweep inside the starting size, so it crosses the
    /// window's minimum size.
    pub shift: i32,
}

impl Sweep {
    /// The cursor's offset from the grab point `t` seconds into the sweep.
    pub fn offset(&self, t: f64) -> i32 {
        let travel = f64::from(self.amplitude) * triangle(t / self.period_s.max(f64::EPSILON));
        NUDGE_PX.saturating_add(self.shift).saturating_add(travel.round() as i32)
    }
}

/// Which cursor coordinates a drag moves: x for a left or right border, both for a corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axes {
    pub x: bool,
    pub y: bool,
}

/// The point `offset` px along a drag that started at `grab`.
pub fn point_at(grab: (i32, i32), axes: Axes, offset: i32) -> (i32, i32) {
    (grab.0.saturating_add(if axes.x { offset } else { 0 }), grab.1.saturating_add(if axes.y { offset } else { 0 }))
}

/// Whether the cursor is off the line a drag moves it along, which only something else moving the mouse
/// can cause. The test stops then instead of fighting for the pointer. Compares the coordinate that
/// should not move, so it does not depend on how far a fast sweep has got.
pub fn off_path(cursor: (i32, i32), grab: (i32, i32), axes: Axes) -> bool {
    let (dx, dy) = (cursor.0 - grab.0, cursor.1 - grab.1);
    match (axes.x, axes.y) {
        (true, false) => dy.abs() > 6,
        (false, true) => dx.abs() > 6,
        (true, true) => (dx - dy).abs() > 8,
        (false, false) => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    RightEdge,
    LeftEdge,
    Corner,
    /// Many short press-drag-release cycles, as a person re-grabbing the border.
    ShortGrabs,
    MaximizeRestore,
}

/// One step of a soak run.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub kind: Kind,
    /// The window's outer size and position before the segment.
    pub size: (i32, i32),
    pub park: (i32, i32),
    pub sweep: Sweep,
    pub seconds: f64,
    /// Seconds between synthetic mouse moves (0.001 is a 1 kHz mouse).
    pub step_s: f64,
}

const START_SIZES: [(i32, i32); 4] = [(1000, 650), (1200, 720), (900, 600), (1400, 800)];
const PERIODS: [f64; 6] = [0.06, 0.1, 0.15, 0.25, 0.5, 1.0];

/// A window size that fits a `screen`-sized primary display with room to grow to the right.
fn fit(size: (i32, i32), screen: (i32, i32)) -> (i32, i32) {
    ((size.0).min(screen.0 - PARK.0 - 300).max(600), (size.1).min(screen.1 - PARK.1 - MARGIN).max(400))
}

/// A seeded plan of segments adding up to at least `seconds`, for a primary display of `screen` px. The
/// same seed and screen give the same plan.
pub fn soak_plan(seed: u64, seconds: f64, screen: (i32, i32)) -> Vec<Segment> {
    let mut rng = Rng::new(seed);
    let mut plan = Vec::new();
    let mut planned = 0.0;
    while planned < seconds {
        let kind = rng
            .weighted(&[(Kind::RightEdge, 5), (Kind::LeftEdge, 2), (Kind::Corner, 3), (Kind::ShortGrabs, 2), (Kind::MaximizeRestore, 1)])
            .unwrap_or(Kind::RightEdge);
        let size = fit(rng.pick(&START_SIZES).unwrap_or((1000, 650)), screen);
        // A left border needs room to travel left, so that window sits against the screen's right side.
        let park = if kind == Kind::LeftEdge { ((screen.0 - MARGIN - size.0).max(PARK.0), PARK.1) } else { PARK };
        let room_right = (screen.0 - park.0 - size.0 - MARGIN).max(0);
        let room_below = (screen.1 - park.1 - size.1 - MARGIN).max(0);
        let room_left = (park.0 - MARGIN / 2).max(0);
        let period_s = rng.pick(&PERIODS).unwrap_or(0.25);
        let (amplitude, shift, step_s) = match kind {
            Kind::RightEdge => {
                (rng.range(250, 900).min(room_right).max(60), rng.pick(&[0, -150, -350]).unwrap_or(0), rng.pick(&[0.001, 0.004, 0.008]).unwrap_or(0.004))
            }
            Kind::LeftEdge => (-rng.range(150, 400).min(room_left).max(60), 0, rng.pick(&[0.002, 0.004]).unwrap_or(0.004)),
            Kind::Corner => (rng.range(150, 300).min(room_right).min(room_below).max(40), rng.pick(&[0, -100, -250]).unwrap_or(0), 0.004),
            Kind::ShortGrabs => (rng.range(60, 300).min(room_right).max(40), 0, 0.004),
            Kind::MaximizeRestore => (0, 0, 0.004),
        };
        let seconds_here = rng.float(2.5, 7.0);
        // A segment also spends about a second and a half parking the window and settling.
        planned += seconds_here + 1.5;
        plan.push(Segment { kind, size, park, sweep: Sweep { amplitude, period_s, shift }, seconds: seconds_here, step_s });
    }
    plan
}

/// `count` outer window sizes for the API-driven storm: seeded, within what `screen` holds, every tenth
/// one an extreme, so the sequence jumps between the smallest and the largest window.
pub fn storm_sizes(seed: u64, count: usize, screen: (i32, i32)) -> Vec<(i32, i32)> {
    let mut rng = Rng::new(seed);
    let (min_w, min_h) = (700, 460);
    let max_w = (screen.0 - PARK.0 - 20).max(min_w + 1);
    let max_h = (screen.1 - PARK.1 - MARGIN).max(min_h + 1);
    (0..count)
        .map(|i| match i % 10 {
            0 if i % 20 == 0 => (min_w, min_h),
            0 => (max_w, max_h),
            _ => (rng.range(min_w, max_w), rng.range(min_h, max_h)),
        })
        .collect()
}

/// One answer to `ui.inspect` while the window was being resized.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Probe {
    /// When the answer arrived, in seconds since probing began.
    pub at_s: f64,
    pub latency_ms: f64,
    /// The app's frame counter, for the report.
    pub frame: u64,
}

/// What a run saw, for [`judge`].
#[derive(Clone, Debug, Default)]
pub struct Observations {
    /// How the app ended, if it did before the run did.
    pub died: Option<String>,
    /// Steps that failed on their own: a drag that did not start, a segment that ended at the wrong size.
    pub step_failures: Vec<String>,
    pub probes: Vec<Probe>,
    pub probe_failures: Vec<String>,
    /// How long probing ran, in seconds.
    pub probe_span_s: f64,
    /// After the run settled: the window's client size, the size of the frame the app rendered
    /// (`ui.screenshot`), and the size it laid its UI out at, all in physical px.
    pub client_px: (i32, i32),
    pub surface_px: Option<(u32, u32)>,
    pub laid_out_px: Option<(f64, f64)>,
    /// Why a command sent after the run (new document, fill) failed.
    pub edit_error: Option<String>,
    /// The app's private memory in MB before the run and after it.
    pub memory_mb: Option<(f64, f64)>,
}

/// What counts as a failure. The defaults leave room for a slow debug build or a busy machine, so
/// they flag a hang and a leak, not a hiccup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// The longest the UI thread may go without answering a probe.
    pub max_stall_ms: f64,
    pub memory_growth_mb: f64,
    pub size_tolerance_px: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_stall_ms: 3000.0, memory_growth_mb: 200.0, size_tolerance_px: 2.0 }
    }
}

/// The longest stretch, in ms, with no answered probe.
pub fn max_gap_ms(probes: &[Probe], span_s: f64) -> f64 {
    let mut longest = 0.0f64;
    let mut last = 0.0;
    for probe in probes {
        longest = longest.max(probe.at_s - last);
        last = probe.at_s;
    }
    longest.max(span_s - last) * 1000.0
}

/// The problems in a run; empty when it passed.
pub fn judge(obs: &Observations, limits: &Limits) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(how) = &obs.died {
        problems.push(format!("the app exited during the run ({how})"));
    }
    problems.extend(obs.step_failures.iter().cloned());
    problems.extend(obs.probe_failures.iter().map(|f| format!("a probe during the resize got no answer: {f}")));
    let gap = max_gap_ms(&obs.probes, obs.probe_span_s);
    if gap > limits.max_stall_ms {
        problems.push(format!("the UI thread answered nothing for {gap:.0} ms (limit {:.0} ms)", limits.max_stall_ms));
    }
    let (client_w, client_h) = (f64::from(obs.client_px.0), f64::from(obs.client_px.1));
    match obs.surface_px {
        None => problems.push("no frame was rendered after the resize (ui.screenshot failed)".into()),
        Some((w, h)) => {
            if (f64::from(w) - client_w).abs() > limits.size_tolerance_px || (f64::from(h) - client_h).abs() > limits.size_tolerance_px {
                problems.push(format!(
                    "the render surface is {w}x{h} px but the window is {}x{} px: it was not resized to match (a stale swap chain)",
                    obs.client_px.0, obs.client_px.1
                ));
            }
        }
    }
    if let Some((w, h)) = obs.laid_out_px
        && ((w - client_w).abs() > limits.size_tolerance_px || (h - client_h).abs() > limits.size_tolerance_px)
    {
        problems.push(format!("the UI is laid out at {w:.0}x{h:.0} px but the window is {}x{} px", obs.client_px.0, obs.client_px.1));
    }
    if let Some(error) = &obs.edit_error {
        problems.push(format!("the app stopped accepting commands after the resize: {error}"));
    }
    if let Some((before, after)) = obs.memory_mb
        && after - before > limits.memory_growth_mb
    {
        problems.push(format!("private memory grew from {before:.0} MB to {after:.0} MB (limit +{:.0} MB)", limits.memory_growth_mb));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: (i32, i32) = (1920, 1080);

    fn healthy() -> Observations {
        Observations {
            probes: (1u32..=20).map(|i| Probe { at_s: f64::from(i) * 0.25, latency_ms: 4.0, frame: u64::from(i) * 15 }).collect(),
            probe_span_s: 5.1,
            client_px: (1107, 700),
            surface_px: Some((1107, 700)),
            laid_out_px: Some((1107.0, 700.0)),
            memory_mb: Some((380.0, 383.0)),
            ..Observations::default()
        }
    }

    #[test]
    fn the_generator_repeats_from_a_seed_and_stays_in_range() {
        let draw = |seed| {
            let mut rng = Rng::new(seed);
            (0..64).map(|_| rng.range(-5, 5)).collect::<Vec<_>>()
        };
        assert_eq!(draw(7252), draw(7252));
        assert_ne!(draw(7252), draw(7253));
        let mut rng = Rng::new(1);
        for _ in 0..10_000 {
            assert!((-5..=5).contains(&rng.range(-5, 5)));
            assert!((2.5..7.0).contains(&rng.float(2.5, 7.0)));
        }
        assert_eq!(rng.range(3, 3), 3);
        assert_eq!(rng.range(9, 2), 9, "an empty range gives its low end");
        assert_eq!(rng.pick::<u8>(&[]), None);
        assert_eq!(rng.weighted::<u8>(&[]), None);
        assert_eq!(rng.weighted(&[('a', 0), ('b', 3)]), Some('b'));
    }

    #[test]
    fn the_triangle_wave_rises_to_one_and_returns() {
        assert_eq!(triangle(0.0), 0.0);
        assert_eq!(triangle(0.5), 1.0);
        assert_eq!(triangle(1.0), 0.0);
        assert_eq!(triangle(2.25), triangle(0.25), "periodic");
        assert_eq!(triangle(-0.25), triangle(0.75), "and defined before zero");
        assert!((0..1000).all(|i| (0.0..=1.0).contains(&triangle(f64::from(i) * 0.037))));
    }

    #[test]
    fn a_sweep_travels_its_amplitude_and_back_each_period() {
        let sweep = Sweep { amplitude: 400, period_s: 0.4, shift: 0 };
        assert_eq!(sweep.offset(0.0), NUDGE_PX);
        assert_eq!(sweep.offset(0.2), NUDGE_PX + 400);
        assert_eq!(sweep.offset(0.4), NUDGE_PX);
        let left = Sweep { amplitude: -300, period_s: 1.0, shift: 0 };
        assert_eq!(left.offset(0.5), NUDGE_PX - 300, "a left border is dragged left");
        let inward = Sweep { amplitude: 800, period_s: 0.12, shift: -250 };
        assert_eq!(inward.offset(0.0), NUDGE_PX - 250, "a negative shift starts inside the starting size");
        assert_eq!(Sweep { amplitude: 10, period_s: 0.0, shift: 0 }.offset(1.0).signum(), 1, "a zero period does not divide by zero");
    }

    #[test]
    fn a_drag_stops_when_something_else_moves_the_mouse() {
        let x_only = Axes { x: true, y: false };
        let corner = Axes { x: true, y: true };
        let grab = (1176, 390);
        // Anywhere along the line of travel, however far, is ours.
        assert!(!off_path((1176, 390), grab, x_only));
        assert!(!off_path((1576, 393), grab, x_only), "a pixel or two of rounding is not a user");
        assert!(!off_path((300, 390), grab, x_only), "a left sweep");
        assert!(off_path((1300, 420), grab, x_only), "someone moved the mouse sideways");
        assert!(!off_path((1276, 490), grab, corner), "a corner moves both coordinates together");
        assert!(off_path((1276, 390), grab, corner));
        assert!(!off_path((5, 5000), grab, Axes { x: false, y: false }), "a drag that moves nothing cannot be off its path");
        assert_eq!(point_at(grab, x_only, 50), (1226, 390));
        assert_eq!(point_at(grab, corner, -20), (1156, 370));
        assert_eq!(point_at(grab, Axes { x: false, y: true }, 7), (1176, 397));
    }

    #[test]
    fn a_soak_plan_is_reproducible_and_covers_every_kind() {
        let plan = soak_plan(7252, 300.0, SCREEN);
        assert_eq!(plan, soak_plan(7252, 300.0, SCREEN));
        assert_ne!(plan, soak_plan(7253, 300.0, SCREEN));
        let kinds: Vec<Kind> = plan.iter().map(|s| s.kind).collect();
        for kind in [Kind::RightEdge, Kind::LeftEdge, Kind::Corner, Kind::ShortGrabs, Kind::MaximizeRestore] {
            assert!(kinds.contains(&kind), "{kind:?} missing from {} segments", plan.len());
        }
        let spent: f64 = plan.iter().map(|s| s.seconds + 1.5).sum();
        assert!(spent >= 300.0, "the plan covers the requested time");
        assert!(spent < 300.0 + 8.5 + 1.5, "and stops at the first segment past it: {spent}");
    }

    #[test]
    fn soak_segments_keep_the_cursor_on_the_screen() {
        for screen in [SCREEN, (1366, 768), (2560, 1440), (3840, 2160)] {
            for seed in 0..40 {
                for s in soak_plan(seed, 120.0, screen) {
                    let right = s.park.0 + s.size.0;
                    let bottom = s.park.1 + s.size.1;
                    assert!(s.size.0 >= 600 && s.size.1 >= 400, "{s:?}");
                    assert!(right <= screen.0 && bottom <= screen.1, "the parked window fits {screen:?}: {s:?}");
                    let travel = (0..=200).map(|i| s.sweep.offset(f64::from(i) * s.sweep.period_s / 100.0)).collect::<Vec<_>>();
                    let (lo, hi) = (travel.iter().min().copied().unwrap_or(0), travel.iter().max().copied().unwrap_or(0));
                    match s.kind {
                        Kind::LeftEdge => assert!(s.park.0 + lo >= 0, "the left border stays on screen: {s:?} {screen:?}"),
                        Kind::MaximizeRestore => {}
                        _ => assert!(right + hi <= screen.0, "the right border stays on screen: {s:?} {screen:?}"),
                    }
                    if s.kind == Kind::Corner {
                        assert!(bottom + hi <= screen.1, "the bottom border stays on screen: {s:?} {screen:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn storm_sizes_repeat_stay_in_bounds_and_jump_between_extremes() {
        let sizes = storm_sizes(7252, 400, SCREEN);
        assert_eq!(sizes, storm_sizes(7252, 400, SCREEN));
        assert_eq!(sizes.len(), 400);
        assert!(sizes.iter().all(|&(w, h)| (700..=SCREEN.0 - PARK.0 - 20).contains(&w) && (460..=SCREEN.1 - PARK.1 - MARGIN).contains(&h)));
        assert_eq!(sizes.first(), Some(&(700, 460)));
        assert_eq!(sizes.get(10), Some(&(SCREEN.0 - PARK.0 - 20, SCREEN.1 - PARK.1 - MARGIN)));
        assert!(storm_sizes(1, 0, SCREEN).is_empty());
    }

    #[test]
    fn a_healthy_run_has_no_problems() {
        assert_eq!(judge(&healthy(), &Limits::default()), Vec::<String>::new());
    }

    #[test]
    fn a_dead_app_a_missed_drag_and_an_unanswered_probe_are_each_reported() {
        let obs = Observations {
            died: Some("exit code: 0xc0000005".into()),
            step_failures: vec!["segment 2: the OS never entered its modal sizing loop".into()],
            probe_failures: vec!["ui.inspect: timed out".into()],
            ..healthy()
        };
        let problems = judge(&obs, &Limits::default());
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("0xc0000005"));
        assert!(problems[1].contains("modal sizing loop"));
        assert!(problems[2].contains("timed out"));
    }

    #[test]
    fn a_frozen_ui_thread_is_a_gap_in_the_answers() {
        let mut obs = healthy();
        assert!(max_gap_ms(&obs.probes, obs.probe_span_s) < 400.0);
        // Answers at 0.25, 0.5 and 0.75 s, then nothing until 4.75 s.
        obs.probes.retain(|p| p.at_s < 1.0 || p.at_s > 4.5);
        let gap = max_gap_ms(&obs.probes, obs.probe_span_s);
        assert!((3900.0..4100.0).contains(&gap), "{gap}");
        assert!(judge(&obs, &Limits::default()).iter().any(|p| p.contains("answered nothing")));
        assert!(judge(&obs, &Limits { max_stall_ms: 5000.0, ..Limits::default() }).is_empty(), "the limit is configurable");
        // No answer at all, for the whole run, is a stall too (and a run that never probed is not).
        assert_eq!(max_gap_ms(&[], 4.0), 4000.0);
        assert_eq!(max_gap_ms(&[], 0.0), 0.0);
        // A stall before the first answer and after the last one counts as well.
        assert_eq!(max_gap_ms(&[Probe { at_s: 2.0, latency_ms: 1.0, frame: 1 }], 2.5), 2000.0);
        assert_eq!(max_gap_ms(&[Probe { at_s: 0.5, latency_ms: 1.0, frame: 1 }], 4.5), 4000.0);
    }

    #[test]
    fn a_surface_left_at_the_old_size_is_a_stale_swap_chain() {
        // The reporter's screenshot in #7252: content drawn at the old width, black where the window grew.
        let obs = Observations { client_px: (1260, 895), surface_px: Some((1110, 895)), laid_out_px: Some((1110.0, 895.0)), ..healthy() };
        let problems = judge(&obs, &Limits::default());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("1110x895") && problems[0].contains("1260x895") && problems[0].contains("stale"));
        assert!(problems[1].contains("laid out"));
        // Within the tolerance is not a mismatch (client sizes round).
        let near = Observations { client_px: (1108, 701), surface_px: Some((1107, 700)), laid_out_px: Some((1107.5, 700.4)), ..healthy() };
        assert!(judge(&near, &Limits::default()).is_empty());
        // No frame at all is reported, not skipped.
        let none = Observations { surface_px: None, ..healthy() };
        assert!(judge(&none, &Limits::default()).iter().any(|p| p.contains("no frame was rendered")));
    }

    #[test]
    fn commands_that_stop_working_and_memory_that_keeps_growing_are_reported() {
        let obs = Observations { edit_error: Some("engine.execute: timed out".into()), memory_mb: Some((380.0, 700.0)), ..healthy() };
        let problems = judge(&obs, &Limits::default());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("stopped accepting commands"));
        assert!(problems[1].contains("380 MB to 700 MB"));
        // Shrinking, or growing by less than the limit, is fine.
        assert!(judge(&Observations { memory_mb: Some((380.0, 120.0)), ..healthy() }, &Limits::default()).is_empty());
        assert!(judge(&Observations { memory_mb: Some((380.0, 579.0)), ..healthy() }, &Limits::default()).is_empty());
    }
}
