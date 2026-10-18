//! Launches an isolated PhotoCraft, resizes its window the way a person does (real mouse input through the
//! OS's modal sizing loop) or the way a program does (`SetWindowPos`), watches it from outside, and judges
//! what it saw with [`plan::judge`].
//!
//! The app is only ever reached through its own control channel and its window handle, found by process id,
//! so a PhotoCraft you have open is never touched; its settings live in a throwaway directory under
//! `target/tmp`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::control::Control;
use crate::plan::{self, Axes, Kind, Limits, Observations, Probe, Segment, Sweep};
use crate::win::{self, Window};

const OPT_IN: &str = "PHOTOCRAFT_RESIZE_STRESS";

/// What the test reads from the environment (see docs/development.md › Windows resize stress test).
struct Config {
    exe: PathBuf,
    extra_args: Vec<String>,
    drag_seconds: f64,
    soak_seconds: f64,
    seed: u64,
    limits: Limits,
    out_dir: Option<PathBuf>,
}

impl Config {
    fn from_env() -> Config {
        let text = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let number = |name: &str, default: f64| text(name).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(default);
        let defaults = Limits::default();
        Config {
            exe: text("PHOTOCRAFT_STRESS_EXE").map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_photocraft")), PathBuf::from),
            extra_args: text("PHOTOCRAFT_STRESS_ARGS").map(|v| v.split_whitespace().map(str::to_owned).collect()).unwrap_or_default(),
            // A day is far more than a run needs; the cap keeps a typo from planning (or sleeping) forever.
            drag_seconds: number("PHOTOCRAFT_STRESS_SECONDS", 15.0).min(86_400.0),
            soak_seconds: number("PHOTOCRAFT_STRESS_SOAK_SECONDS", 60.0).min(86_400.0),
            seed: text("PHOTOCRAFT_STRESS_SEED").and_then(|v| v.parse().ok()).unwrap_or(7252),
            limits: Limits {
                max_stall_ms: number("PHOTOCRAFT_STRESS_MAX_STALL_MS", defaults.max_stall_ms),
                memory_growth_mb: number("PHOTOCRAFT_STRESS_MEMORY_GROWTH_MB", defaults.memory_growth_mb),
                ..defaults
            },
            out_dir: text("PHOTOCRAFT_STRESS_OUT").map(PathBuf::from),
        }
    }
}

/// Holds the right to use the real mouse and desktop.
pub struct Session {
    _serial: MutexGuard<'static, ()>,
}

/// The tests move the real mouse and open a window, so they refuse to run unless asked in so many words,
/// and run one at a time even when the test harness uses several threads.
pub fn begin() -> Session {
    let opted_in = std::env::var_os(OPT_IN).is_some_and(|v| !v.is_empty() && v != "0");
    assert!(
        opted_in,
        "this test takes over the real mouse and opens a PhotoCraft window: set {OPT_IN}=1 to run it (docs/development.md › Windows resize stress test)"
    );
    static SERIAL: Mutex<()> = Mutex::new(());
    Session { _serial: SERIAL.lock().unwrap_or_else(PoisonError::into_inner) }
}

type Log = Arc<Mutex<Vec<String>>>;

fn log_has(log: &Log, needle: &str) -> bool {
    log.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|l| l.contains(needle))
}

fn log_tail(log: &Log, n: usize) -> String {
    let lines = log.lock().unwrap_or_else(PoisonError::into_inner);
    lines.iter().skip(lines.len().saturating_sub(n)).cloned().collect::<Vec<_>>().join("\n")
}

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).and_then(|l| l.local_addr()).map(|a| a.port()).map_err(|e| format!("no free port: {e}"))
}

/// How many times a press is tried before a drag that will not start is a failure.
const GRAB_ATTEMPTS: u32 = 3;

/// Why a drag ended before its sweep.
enum DragError {
    /// The press did not start an OS resize.
    NotStarted,
    /// The test stopped itself, for the reason given: a refused press, or something else moving the mouse.
    Stopped(String),
}

impl DragError {
    fn stopped(why: impl Into<String>) -> DragError {
        DragError::Stopped(why.into())
    }
}

/// One isolated PhotoCraft process and its main window.
struct App {
    child: Child,
    window: Window,
    port: u16,
    token: String,
    dir: PathBuf,
    log: Log,
    keep_dir: bool,
    /// Presses that did not start a resize and were tried again (see [`App::drag_border`]).
    missed_grabs: u32,
}

impl App {
    fn launch(cfg: &Config, name: &str) -> Result<App, String> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("resize-stress-{name}-{}-{stamp}", std::process::id()));
        let (settings, out) = (dir.join("settings"), dir.join("out"));
        for d in [&settings, &out] {
            std::fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
        }
        let token_file = dir.join("control.token");
        let mut last = String::new();
        // Another process can take the port between choosing it and the app binding it: try again.
        for _ in 0..3 {
            let port = free_port()?;
            let mut child = Command::new(&cfg.exe)
                .args(["--new-instance", "--control"])
                .arg(port.to_string())
                .arg("--control-token-file")
                .arg(&token_file)
                .arg("--automation-write-root")
                .arg(&out)
                .args(&cfg.extra_args)
                .env("PHOTOCRAFT_CONFIG_DIR", &settings)
                .env("RUST_BACKTRACE", "1")
                // A control port, token or automation root from the caller's shell must not leak into the test.
                .env_remove("PHOTOCRAFT_CONTROL_PORT")
                .env_remove("PHOTOCRAFT_CONTROL_TOKEN")
                .env_remove("PHOTOCRAFT_CONTROL_TOKEN_FILE")
                .env_remove("PHOTOCRAFT_AUTOMATION_READ_ROOT")
                .env_remove("PHOTOCRAFT_AUTOMATION_WRITE_ROOT")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("start {}: {e}", cfg.exe.display()))?;
            let log: Log = Arc::default();
            if let Some(stderr) = child.stderr.take() {
                let sink = Arc::clone(&log);
                thread::spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        sink.lock().unwrap_or_else(PoisonError::into_inner).push(line);
                    }
                });
            }
            match wait_until_ready(&mut child, &log, port, &token_file) {
                Ok((token, window)) => return Ok(App { child, window, port, token, dir, log, keep_dir: false, missed_grabs: 0 }),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    last = format!("{error}\n--- the app's log ---\n{}", log_tail(&log, 40));
                    if !log_has(&log, "failed to bind") {
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        Err(last)
    }

    /// How the app ended, if it has.
    fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("the process cannot be queried: {e}")),
        }
    }

    fn connect(&self, timeout: Duration) -> Result<Control, String> {
        Control::connect(self.port, &self.token, timeout)
    }

    /// Press at `grab`, nudge until the OS enters its modal sizing loop, sweep the cursor back and forth, release.
    fn drag(&mut self, grab: (i32, i32), axes: Axes, sweep: Sweep, seconds: f64, step_s: f64) -> Result<(), DragError> {
        if !win::move_cursor(grab.0, grab.1) {
            return Err(DragError::stopped("SendInput refused to move the cursor"));
        }
        thread::sleep(Duration::from_millis(150));
        // Never press unless the test window is exactly what a click here would hit, and the cursor is where
        // the test put it: otherwise something else (you, or another window) is in the way.
        match win::cursor() {
            Some(at) if (at.0 - grab.0).abs() > 3 || (at.1 - grab.1).abs() > 3 => {
                return Err(DragError::stopped(format!("the cursor is at {at:?}, not at {grab:?}: something else is moving the mouse; not pressing")));
            }
            None => return Err(DragError::stopped("cannot read the cursor position")),
            Some(_) => {}
        }
        if !self.window.is_under(grab.0, grab.1) {
            let rect = self.window.rect();
            return Err(DragError::stopped(format!(
                "the test window (at {rect:?}) is not under the cursor at {grab:?}: another window is in front, or the window moved; not pressing"
            )));
        }
        let Some(_held) = win::HeldButton::press() else {
            return Err(DragError::stopped("SendInput refused the mouse press (a window of a more privileged process may have the foreground)"));
        };
        // egui starts the resize after a few px of movement.
        let direction = if sweep.amplitude < 0 { -1 } else { 1 };
        for i in 1..=plan::NUDGE_PX {
            let (x, y) = plan::point_at(grab, axes, direction * i);
            win::move_cursor(x, y);
            thread::sleep(Duration::from_millis(16));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !self.window.in_modal_loop() {
            thread::sleep(Duration::from_millis(10));
        }
        if !self.window.in_modal_loop() {
            return Err(DragError::NotStarted);
        }
        let started = Instant::now();
        let mut moves = 0u32;
        let mut next = started;
        while started.elapsed().as_secs_f64() < seconds {
            if self.exited().is_some() {
                break;
            }
            let (x, y) = plan::point_at(grab, axes, sweep.offset(started.elapsed().as_secs_f64()));
            win::move_cursor(x, y);
            moves += 1;
            if moves.is_multiple_of(25)
                && let Some(at) = win::cursor()
                && plan::off_path(at, grab, axes)
            {
                return Err(DragError::stopped(format!("the cursor left its path (at {at:?}): something else moved the mouse; stopped so as not to fight it")));
            }
            next += Duration::from_secs_f64(step_s);
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        let (x, y) = plan::point_at(grab, axes, plan::NUDGE_PX);
        win::move_cursor(x, y);
        Ok(())
    }

    /// Wait for the window to come to rest: the OS's sizing loop over and the window's rectangle unchanged for
    /// 150 ms. A slow renderer lags behind the mouse, so the OS can still be working through a drag long after
    /// the test let go of the button; a rectangle read then is out of date by the time it is used.
    fn wait_until_at_rest(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut last, mut since) = (None, Instant::now());
        while Instant::now() < deadline {
            let now = self.window.rect();
            if now != last || self.window.in_modal_loop() {
                (last, since) = (now, Instant::now());
            } else if since.elapsed() >= Duration::from_millis(150) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Drag the border or corner `kind` names, from where the window is now (once it is at rest). When the
    /// press does not start a resize the border is grabbed again, as a person would: very slow rendering
    /// (the software adapter) loses about one grab in forty. The retries are counted in `missed_grabs` and
    /// reported; only a drag that never starts is a failure.
    fn drag_border(&mut self, kind: Kind, sweep: Sweep, seconds: f64, step_s: f64) -> Result<(), String> {
        for attempt in 1..=GRAB_ATTEMPTS {
            self.wait_until_at_rest();
            let (left, top, right, bottom) = self.window.rect().ok_or("cannot read the window's rectangle")?;
            let mid = (top + bottom) / 2;
            let (grab, axes) = match kind {
                Kind::LeftEdge => ((left + 3, mid), Axes { x: true, y: false }),
                Kind::Corner => ((right - 4, bottom - 4), Axes { x: true, y: true }),
                _ => ((right - 4, mid), Axes { x: true, y: false }),
            };
            let result = self.drag(grab, axes, sweep, seconds, step_s);
            self.wait_until_at_rest();
            match result {
                Ok(()) => return Ok(()),
                Err(DragError::NotStarted) if attempt < GRAB_ATTEMPTS => self.missed_grabs += 1,
                Err(DragError::NotStarted) => {
                    self.missed_grabs += 1;
                    return Err(format!("the OS never entered its modal sizing loop in {GRAB_ATTEMPTS} tries: the press did not start a resize"));
                }
                Err(DragError::Stopped(why)) => return Err(why),
            }
        }
        Ok(())
    }

    fn maximize_restore(&mut self, seconds: f64) {
        let end = Instant::now() + Duration::from_secs_f64(seconds);
        let mut i = 0u64;
        while Instant::now() < end && self.exited().is_none() {
            let dwell = Duration::from_millis(100 + 50 * (i % 7));
            self.window.maximize();
            thread::sleep(dwell);
            self.window.restore();
            thread::sleep(dwell);
            i += 1;
        }
    }

    /// Resize the window from the program as fast as it will take it. Returns how many sizes were set.
    fn api_storm(&mut self, seconds: f64, seed: u64, screen: (i32, i32)) -> usize {
        let end = Instant::now() + Duration::from_secs_f64(seconds);
        let mut set = 0;
        for size in plan::storm_sizes(seed, 2000, screen).into_iter().cycle() {
            if Instant::now() >= end || self.exited().is_some() || !self.window.resize(size) {
                break;
            }
            set += 1;
            thread::sleep(Duration::from_millis(2));
        }
        set
    }

    /// One soak segment: park the window, then do what the plan says.
    fn run_segment(&mut self, seg: &Segment, screen: (i32, i32)) -> Result<(), String> {
        if !self.window.park(seg.park, seg.size) {
            return Err("cannot place the window".into());
        }
        thread::sleep(Duration::from_millis(400));
        match seg.kind {
            Kind::RightEdge | Kind::LeftEdge | Kind::Corner => self.drag_border(seg.kind, seg.sweep, seg.seconds, seg.step_s),
            Kind::ShortGrabs => {
                let end = Instant::now() + Duration::from_secs_f64(seg.seconds);
                let mut i = 0u32;
                while Instant::now() < end && self.exited().is_none() {
                    // Each short drag can leave the window a little wider: keep room for the next one.
                    if self.window.rect().is_some_and(|(_, _, right, _)| right + seg.sweep.amplitude + plan::MARGIN > screen.0) {
                        self.window.park(seg.park, seg.size);
                        thread::sleep(Duration::from_millis(300));
                    }
                    let sweep = Sweep { period_s: if i.is_multiple_of(2) { 0.1 } else { 0.2 }, ..seg.sweep };
                    self.drag_border(Kind::RightEdge, sweep, 0.3 + 0.1 * f64::from(i % 8), seg.step_s)?;
                    thread::sleep(Duration::from_millis(u64::from(20 + 30 * (i % 7))));
                    i += 1;
                }
                Ok(())
            }
            Kind::MaximizeRestore => {
                self.maximize_restore(seg.seconds);
                Ok(())
            }
        }
    }

    /// The size, in px, of the frame the app renders now (what `ui.screenshot` captures), next to the window's.
    fn rendered_size(&self) -> Result<(u32, u32), String> {
        let mut control = self.connect(Duration::from_secs(30))?;
        let shot = control.call("ui.screenshot", json!({"path": "final.png", "focus": false}))?;
        let side = |key: &str| shot[key].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| format!("ui.screenshot reported no {key}: {shot}"));
        Ok((side("width")?, side("height")?))
    }

    /// Whether the app's rendered frame is the window's size; the problem if it is not.
    fn surface_problem(&self, tolerance: f64) -> Option<String> {
        let client = self.window.client()?;
        match self.rendered_size() {
            Ok((w, h)) if (f64::from(w) - f64::from(client.0)).abs() > tolerance || (f64::from(h) - f64::from(client.1)).abs() > tolerance => {
                Some(format!("the render surface is {w}x{h} px but the window is {}x{} px (a stale swap chain)", client.0, client.1))
            }
            Ok(_) => None,
            Err(e) => Some(format!("no frame could be rendered: {e}")),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !self.keep_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// The app's private memory in MB once start-up work (the system font scan, the first document) has stopped
/// allocating: two readings a second apart within 4 MB of each other, or what it is after 20 s. Growth is
/// then measured from a settled start, not from a moment when the app was still warming up.
fn settled_memory_mb(pid: u32) -> Option<f64> {
    let mut last = win::private_mb(pid)?;
    let mut steady = 0;
    for _ in 0..20 {
        thread::sleep(Duration::from_secs(1));
        let now = win::private_mb(pid)?;
        steady = if (now - last).abs() < 4.0 { steady + 1 } else { 0 };
        last = now;
        if steady >= 2 {
            break;
        }
    }
    Some(last)
}

/// Wait for the control server and the main window; returns the control token and the window.
fn wait_until_ready(child: &mut Child, log: &Log, port: u16, token_file: &Path) -> Result<(String, Window), String> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the app exited while starting ({status})"));
        }
        if log_has(log, "failed to bind") {
            return Err("the control port was taken".into());
        }
        let token = std::fs::read_to_string(token_file).ok().map(|t| t.trim().to_owned()).filter(|t| t.len() == 64);
        if let Some(token) = token
            && let Some(window) = win::find_window(child.id())
            && let Ok(mut control) = Control::connect(port, &token, Duration::from_secs(5))
            && control.call("ui.inspect", json!({})).is_ok()
        {
            return Ok((token, window));
        }
        if Instant::now() > deadline {
            return Err("the app did not come up within 180 s".into());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// What the background prober saw.
#[derive(Default)]
struct ProbeLog {
    probes: Vec<Probe>,
    failures: Vec<String>,
    span_s: f64,
}

/// Asks the app `ui.inspect` four times a second from another thread. An answer proves that the UI thread
/// ran a frame, so a window that stops drawing while it is being resized shows up as late or missing answers.
struct Prober {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<ProbeLog>,
}

impl Prober {
    fn start(port: u16, token: String, timeout: Duration) -> Prober {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let began = Instant::now();
            let mut log = ProbeLog::default();
            let mut connection: Option<Control> = None;
            while !flag.load(Ordering::Relaxed) {
                let tick = Instant::now();
                if connection.is_none() {
                    match Control::connect(port, &token, timeout) {
                        Ok(c) => connection = Some(c),
                        Err(e) => log.failures.push(format!("t={:.1}s: {e}", began.elapsed().as_secs_f64())),
                    }
                }
                if let Some(c) = connection.as_mut() {
                    match c.call("ui.inspect", json!({})) {
                        Ok(v) => {
                            let latency_ms = tick.elapsed().as_secs_f64() * 1000.0;
                            log.probes.push(Probe { at_s: began.elapsed().as_secs_f64(), latency_ms, frame: v["frame"].as_u64().unwrap_or(0) });
                        }
                        Err(e) => {
                            log.failures.push(format!("t={:.1}s: {e}", began.elapsed().as_secs_f64()));
                            connection = None;
                        }
                    }
                }
                thread::sleep(Duration::from_secs_f64(plan::PROBE_INTERVAL_S).saturating_sub(tick.elapsed()));
            }
            log.span_s = began.elapsed().as_secs_f64();
            log
        });
        Prober { stop, handle }
    }

    fn finish(self) -> ProbeLog {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or_default()
    }
}

/// The window states the tests drive, one test each.
#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    RightBorder,
    LeftBorder,
    Corner,
    BelowMinimum,
    MaximizeRestore,
    ApiStorm,
    Soak,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::RightBorder => "right-border",
            Scenario::LeftBorder => "left-border",
            Scenario::Corner => "corner-large-document",
            Scenario::BelowMinimum => "below-minimum-size",
            Scenario::MaximizeRestore => "maximize-restore",
            Scenario::ApiStorm => "api-storm",
            Scenario::Soak => "soak",
        }
    }

    /// The document opened first. #7252 reports an empty window, so the border tests that mirror it open none.
    fn document(self) -> Option<(u32, u32)> {
        match self {
            Scenario::RightBorder | Scenario::BelowMinimum => None,
            Scenario::Corner => Some((6000, 4000)),
            _ => Some((640, 480)),
        }
    }

    fn start_size(self) -> (i32, i32) {
        if matches!(self, Scenario::BelowMinimum) { (900, 700) } else { (1100, 700) }
    }
}

/// Run one scenario against a fresh PhotoCraft and panic with the problems found, if any.
pub fn run(scenario: Scenario) {
    let _session = begin();
    win::init_dpi();
    let cfg = Config::from_env();
    let mut app = App::launch(&cfg, scenario.name()).unwrap_or_else(|e| panic!("{e}"));
    let (obs, details) = match drive(&mut app, &cfg, scenario) {
        Ok(done) => done,
        Err(error) => {
            app.keep_dir = true;
            panic!("{}: {error}\n--- the app's log ---\n{}\n(kept in {})", scenario.name(), log_tail(&app.log, 40), app.dir.display());
        }
    };
    let problems = plan::judge(&obs, &cfg.limits);
    let summary = summarize(&cfg, scenario, &obs, &problems, details);
    eprintln!(
        "[resize-stress] {}: {}\n{}",
        scenario.name(),
        if problems.is_empty() { "passed" } else { "FAILED" },
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    );
    if let Some(out) = &cfg.out_dir {
        let here = out.join(scenario.name());
        let saved = std::fs::create_dir_all(&here)
            .and_then(|()| std::fs::write(here.join("summary.json"), serde_json::to_vec_pretty(&summary).unwrap_or_default()))
            .and_then(|()| std::fs::write(here.join("app-stderr.log"), log_tail(&app.log, usize::MAX)));
        if let Err(e) = saved {
            eprintln!("[resize-stress] could not write to {}: {e}", here.display());
        }
    }
    if !problems.is_empty() {
        app.keep_dir = true;
        panic!(
            "{} found {} problem(s):\n - {}\n--- the app's log ---\n{}\n(kept in {})",
            scenario.name(),
            problems.len(),
            problems.join("\n - "),
            log_tail(&app.log, 30),
            app.dir.display()
        );
    }
}

/// Run the scenario and collect what to judge, plus details for the report.
fn drive(app: &mut App, cfg: &Config, scenario: Scenario) -> Result<(Observations, Value), String> {
    let screen = win::screen_size();
    let mut control = app.connect(Duration::from_secs(30))?;
    let gpu = control.call("ui.inspect", json!({}))?.pointer("/perf/timings/gpuInfo").cloned().unwrap_or(Value::Null);
    if let Some((width, height)) = scenario.document() {
        control.call("engine.execute", json!({"command": "file.new", "params": {"width": width, "height": height, "background": "white"}}))?;
    }
    drop(control);
    // The first screenshot allocates the capture buffers for good: take one now, so that the memory growth
    // measured below is the resizing's and not that one-time cost.
    let _ = app.rendered_size();
    let memory_before = settled_memory_mb(app.child.id());

    let size = scenario.start_size();
    let park = if matches!(scenario, Scenario::LeftBorder) { ((screen.0 - plan::MARGIN - size.0).max(plan::PARK.0), plan::PARK.1) } else { plan::PARK };
    if !app.window.park(park, size) {
        return Err("cannot place the window".into());
    }
    thread::sleep(Duration::from_millis(1500));

    let mut obs = Observations::default();
    let mut details = json!({});
    let prober = Prober::start(app.port, app.token.clone(), Duration::from_millis(cfg.limits.max_stall_ms as u64));
    let drag = |app: &mut App, kind: Kind, sweep: Sweep, obs: &mut Observations| {
        if let Err(e) = app.drag_border(kind, sweep, cfg.drag_seconds, 0.004) {
            obs.step_failures.push(format!("{}: {e}", scenario.name()));
        }
    };
    match scenario {
        Scenario::RightBorder => drag(app, Kind::RightEdge, Sweep { amplitude: 400, period_s: 0.3, shift: 0 }, &mut obs),
        Scenario::BelowMinimum => drag(app, Kind::RightEdge, Sweep { amplitude: 800, period_s: 0.2, shift: -250 }, &mut obs),
        Scenario::LeftBorder => drag(app, Kind::LeftEdge, Sweep { amplitude: -400, period_s: 0.4, shift: 0 }, &mut obs),
        Scenario::Corner => drag(app, Kind::Corner, Sweep { amplitude: 250, period_s: 0.4, shift: 0 }, &mut obs),
        Scenario::MaximizeRestore => app.maximize_restore(cfg.drag_seconds),
        Scenario::ApiStorm => {
            let sizes = app.api_storm(cfg.drag_seconds, cfg.seed, screen);
            details["sizes_set"] = json!(sizes);
        }
        Scenario::Soak => {
            let segments = plan::soak_plan(cfg.seed, cfg.soak_seconds, screen);
            details["segments"] = json!(segments.len());
            // The app's memory after each segment: the shape of any growth (steady, or a step) tells a leak from a one-off.
            let mut memory_after_segment = Vec::new();
            for (i, seg) in segments.iter().enumerate() {
                if app.exited().is_some() {
                    break;
                }
                if let Err(e) = app.run_segment(seg, screen) {
                    obs.step_failures.push(format!("segment {} ({:?}): {e}", i + 1, seg.kind));
                }
                thread::sleep(Duration::from_secs(1));
                // Every segment must end with the app drawing at the window's size.
                if let Some(problem) = app.surface_problem(cfg.limits.size_tolerance_px) {
                    obs.step_failures.push(format!("segment {} ({:?}): {problem}", i + 1, seg.kind));
                }
                memory_after_segment.push(win::private_mb(app.child.id()).map(|mb| mb.round()));
            }
            details["memory_mb_after_each_segment"] = json!(memory_after_segment);
        }
    }
    thread::sleep(Duration::from_secs(1));
    let log = prober.finish();
    (obs.probes, obs.probe_failures, obs.probe_span_s) = (log.probes, log.failures, log.span_s);
    obs.died = app.exited();
    // Before the checks below, which allocate on their own (a screenshot, a document).
    obs.memory_mb = memory_before.zip(win::private_mb(app.child.id()));

    // Recovery: the window is at rest. The app must draw at its size, lay out at it, and still take commands.
    obs.client_px = app.window.client().unwrap_or((0, 0));
    obs.surface_px = app.rendered_size().ok();
    match app.connect(Duration::from_secs(30)) {
        Ok(mut control) => {
            if let Ok(inspect) = control.call("ui.inspect", json!({})) {
                let ppp = inspect.pointer("/window/pixelsPerPoint").and_then(Value::as_f64).unwrap_or(1.0);
                let (w, h) = (inspect.pointer("/window/width").and_then(Value::as_f64), inspect.pointer("/window/height").and_then(Value::as_f64));
                obs.laid_out_px = w.zip(h).map(|(w, h)| (w * ppp, h * ppp));
            }
            let edit = control
                .call("engine.execute", json!({"command": "file.new", "params": {"width": 64, "height": 64, "background": "white"}}))
                .and_then(|_| control.call("engine.execute", json!({"command": "edit.fill", "params": {"color": "#2266dd"}})));
            obs.edit_error = edit.err();
        }
        Err(e) => obs.edit_error = Some(format!("cannot connect: {e}")),
    }
    details["gpu"] = gpu;
    details["missed_grabs"] = json!(app.missed_grabs);
    Ok((obs, details))
}

fn summarize(cfg: &Config, scenario: Scenario, obs: &Observations, problems: &[String], details: Value) -> Value {
    let latencies = obs.probes.iter().map(|p| p.latency_ms);
    let frames_per_s = match (obs.probes.first(), obs.probes.last()) {
        (Some(first), Some(last)) if last.at_s > first.at_s => Some(last.frame.saturating_sub(first.frame) as f64 / (last.at_s - first.at_s)),
        _ => None,
    };
    json!({
        "scenario": scenario.name(),
        "verdict": if problems.is_empty() { "pass" } else { "fail" },
        "problems": problems,
        "exe": cfg.exe.display().to_string(),
        "args": cfg.extra_args,
        "seed": cfg.seed,
        "drag_seconds": cfg.drag_seconds,
        "screen_px": win::screen_size(),
        "details": details,
        "probes": {
            "answered": obs.probes.len(),
            "failed": obs.probe_failures.len(),
            "max_latency_ms": latencies.fold(0.0, f64::max),
            "max_gap_ms": plan::max_gap_ms(&obs.probes, obs.probe_span_s),
            "app_frames_per_s": frames_per_s,
        },
        "final": {"client_px": obs.client_px, "surface_px": obs.surface_px, "laid_out_px": obs.laid_out_px},
        "memory_mb": obs.memory_mb,
        "limits": {"max_stall_ms": cfg.limits.max_stall_ms, "memory_growth_mb": cfg.limits.memory_growth_mb, "size_tolerance_px": cfg.limits.size_tolerance_px},
    })
}
