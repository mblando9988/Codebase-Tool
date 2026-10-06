//! A small window around the command-line tool: pick a folder, build its code graph, read
//! the result. All the work is done by the `codebase-context-graph` binary that sits next to
//! this one, so the app and the command line always run the same engine.

use eframe::egui;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const CLI_NAME: &str = "codebase-context-graph";
const MAX_OUTPUT_BYTES: usize = 200_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Index,
    Doctor,
}

impl Action {
    fn command(self) -> &'static str {
        match self {
            Action::Index => "index",
            Action::Doctor => "doctor",
        }
    }

    fn running_label(self) -> &'static str {
        match self {
            Action::Index => "Indexing",
            Action::Doctor => "Checking indexers",
        }
    }
}

enum Event {
    Line(String),
    /// The process ended; `None` if it was killed by a signal.
    Finished(Option<i32>),
}

enum Status {
    Idle,
    Running { action: Action, started: Instant },
    Finished { ok: bool, message: String },
}

struct GuiApp {
    project: String,
    output: String,
    status: Status,
    /// The few lines of the last index run worth showing on their own.
    summary: Vec<String>,
    last_error: Option<String>,
    events: Option<Receiver<Event>>,
    stop_requested: Arc<AtomicBool>,
}

impl Default for GuiApp {
    fn default() -> Self {
        Self {
            // Empty on purpose: an app started from Finder runs in "/", which must never be
            // indexed by accident.
            project: String::new(),
            output: String::new(),
            status: Status::Idle,
            summary: Vec::new(),
            last_error: None,
            events: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The command-line binary installed next to the given executable.
fn cli_next_to(exe: &Path) -> Option<PathBuf> {
    let candidate = exe.parent()?.join(CLI_NAME);
    (candidate.is_file() && candidate != exe).then_some(candidate)
}

fn cli_binary() -> Result<PathBuf, String> {
    // The command-line tool ships in the same folder as this app, so it is found from the
    // app's own path. Nothing security-relevant depends on the result.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this app: {e}"))?;
    cli_next_to(&exe)
        .ok_or_else(|| format!("`{CLI_NAME}` was not found next to {}", exe.display()))
}

fn cli_args(action: Action, project: &str) -> Vec<String> {
    vec![
        action.command().to_string(),
        "--project-root".to_string(),
        project.to_string(),
    ]
}

fn is_project_folder(path: &str) -> bool {
    let path = path.trim();
    !path.is_empty() && Path::new(path).is_dir()
}

/// Lines of the index output that are shown in the result box.
fn is_summary_line(line: &str) -> bool {
    let line = line.trim_start();
    ["Nodes:", "Edges:", "Coverage:", "not covered:", "indexers that did not run:"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Keeps the output pane bounded by dropping the oldest half when it grows too large.
fn trim_output(output: &mut String) {
    if output.len() > MAX_OUTPUT_BYTES {
        let mut cut = output.len() - MAX_OUTPUT_BYTES / 2;
        while !output.is_char_boundary(cut) {
            cut += 1;
        }
        output.drain(..cut);
    }
}

fn open_folder(path: &Path) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(opener).arg(path).spawn();
}

/// Stops the command-line tool and the indexers it started. They share a process group
/// (see `start`), so one signal reaches all of them.
#[cfg(unix)]
fn terminate_group(pid: u32) {
    let signalled = Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .status()
        .is_ok_and(|s| s.success());
    if !signalled {
        let _ = Command::new("pkill").args(["-TERM", "-g", &pid.to_string()]).status();
    }
}

#[cfg(not(unix))]
fn terminate_group(_pid: u32) {}

/// Forwards a pipe to the UI line by line, whatever bytes it contains.
fn pump<R: Read + Send + 'static>(reader: R, tx: Sender<Event>, ctx: egui::Context) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();
        while reader.read_until(b'\n', &mut buffer).is_ok_and(|n| n > 0) {
            let line = String::from_utf8_lossy(&buffer).trim_end().to_string();
            buffer.clear();
            if tx.send(Event::Line(line)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    })
}

impl GuiApp {
    fn running(&self) -> bool {
        matches!(self.status, Status::Running { .. })
    }

    fn append(&mut self, text: &str) {
        self.output.push_str(text);
        self.output.push('\n');
        trim_output(&mut self.output);
    }

    fn fail(&mut self, message: String) {
        self.append(&format!("Error: {message}"));
        self.status = Status::Finished { ok: false, message };
    }

    fn start(&mut self, ctx: &egui::Context, action: Action) {
        let project = self.project.trim().to_string();
        let root = if is_project_folder(&project) { project } else { ".".to_string() };
        let cli = match cli_binary() {
            Ok(path) => path,
            Err(message) => return self.fail(message),
        };

        let mut command = Command::new(&cli);
        command
            .args(cli_args(action, &root))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => return self.fail(format!("could not start {}: {e}", cli.display())),
        };

        self.append(&format!("$ {CLI_NAME} {} --project-root {root}", action.command()));
        self.summary.clear();
        self.last_error = None;
        self.stop_requested.store(false, Ordering::SeqCst);
        self.status = Status::Running { action, started: Instant::now() };

        let (tx, rx) = mpsc::channel();
        self.events = Some(rx);
        let pid = child.id();
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(pump(out, tx.clone(), ctx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(pump(err, tx.clone(), ctx.clone()));
        }

        let stop = self.stop_requested.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let mut signalled = false;
            let code = loop {
                if stop.load(Ordering::SeqCst) && !signalled {
                    signalled = true;
                    terminate_group(pid);
                    let _ = child.kill(); // in case the group signal did not go through
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(_) => break None,
                }
            };
            for reader in readers {
                let _ = reader.join(); // so no output is lost behind the "finished" event
            }
            let _ = tx.send(Event::Finished(code));
            ctx.request_repaint();
        });
    }

    fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        self.append("Stopping…");
    }

    fn drain_events(&mut self) {
        let mut finished = None;
        if let Some(rx) = &self.events {
            let lines: Vec<Event> = rx.try_iter().collect();
            for event in lines {
                match event {
                    Event::Line(line) => {
                        if is_summary_line(&line) {
                            self.summary.push(line.trim().to_string());
                        }
                        if let Some(message) = line.strip_prefix("Error:") {
                            self.last_error = Some(message.trim().to_string());
                        }
                        self.append(&line);
                    }
                    Event::Finished(code) => finished = Some(code),
                }
            }
        }
        if let Some(code) = finished {
            self.events = None;
            let ok = code == Some(0);
            let message = if self.stop_requested.load(Ordering::SeqCst) {
                "Stopped".to_string()
            } else if ok {
                "Done".to_string()
            } else if let Some(error) = self.last_error.take() {
                error
            } else {
                format!("Failed ({})", code.map_or("no exit code".to_string(), |c| format!("exit {c}")))
            };
            self.status = Status::Finished { ok: ok && !self.stop_requested.load(Ordering::SeqCst), message };
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        if self.running() {
            ctx.request_repaint_after(Duration::from_millis(500)); // keeps the timer moving
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Codebase Context Graph");
            ui.label(
                "Maps who defines what and who calls whom, using the standard indexers for \
                 Rust, TypeScript/JavaScript and Python.",
            );
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                ui.label("Project folder");
                ui.add(
                    egui::TextEdit::singleline(&mut self.project)
                        .hint_text("Choose a folder…")
                        .desired_width((ui.available_width() - 90.0).max(120.0)),
                );
                if ui.button("Browse…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.project = path.to_string_lossy().to_string();
                    }
                }
            });
            ui.add_space(6.0);

            let running = self.running();
            ui.horizontal(|ui| {
                let can_index = !running && is_project_folder(&self.project);
                let index = egui::Button::new("Index project").min_size(egui::vec2(130.0, 30.0));
                if ui.add_enabled(can_index, index).clicked() {
                    self.start(ctx, Action::Index);
                }
                if ui.add_enabled(!running, egui::Button::new("Check indexers")).clicked() {
                    self.start(ctx, Action::Doctor);
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.stop();
                }
            });
            ui.add_space(6.0);

            match &self.status {
                Status::Idle => {
                    ui.label("Ready");
                }
                Status::Running { action, started } => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("{}… {}", action.running_label(), format_elapsed(started.elapsed())));
                    });
                }
                Status::Finished { ok, message } => {
                    let color = if *ok {
                        egui::Color32::from_rgb(90, 190, 120)
                    } else {
                        egui::Color32::from_rgb(235, 130, 110)
                    };
                    ui.colored_label(color, message);
                }
            }

            if !self.summary.is_empty() {
                ui.add_space(6.0);
                ui.group(|ui| {
                    for line in &self.summary {
                        ui.label(line);
                    }
                    let results = Path::new(self.project.trim()).join(".codebase-context");
                    if !running && results.is_dir() && ui.button("Open results folder").clicked() {
                        open_folder(&results);
                    }
                });
            }

            ui.add_space(8.0);
            ui.separator();
            ui.label("Output");
            egui::ScrollArea::both().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                if self.output.is_empty() {
                    ui.weak("Pick a folder and press “Index project”. “Check indexers” shows what is installed.");
                } else {
                    // A read-only text box, so the output can be selected and copied.
                    let mut text: &str = &self.output;
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("Codebase Context Graph"),
        ..Default::default()
    };

    eframe::run_native(
        "Codebase Context Graph",
        options,
        Box::new(|_cc| Ok(Box::new(GuiApp::default()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_action_runs_the_matching_cli_command_on_the_chosen_folder() {
        assert_eq!(cli_args(Action::Index, "/p"), ["index", "--project-root", "/p"]);
        assert_eq!(cli_args(Action::Doctor, "."), ["doctor", "--project-root", "."]);
    }

    #[test]
    fn only_existing_folders_count_as_projects() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(is_project_folder(dir.path().to_str().unwrap()));
        assert!(is_project_folder(&format!("  {}  ", dir.path().display())), "pasted paths often have spaces");
        assert!(!is_project_folder(file.to_str().unwrap()));
        assert!(!is_project_folder(""));
        assert!(!is_project_folder("   "));
        assert!(!is_project_folder("/definitely/not/here"));
    }

    #[test]
    fn the_cli_is_found_next_to_the_app_and_never_resolves_to_the_app_itself() {
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("codebase-context-graph-gui");
        std::fs::write(&gui, "").unwrap();
        assert_eq!(cli_next_to(&gui), None, "no CLI installed: report it instead of starting another window");

        let cli = dir.path().join(CLI_NAME);
        std::fs::write(&cli, "").unwrap();
        assert_eq!(cli_next_to(&gui), Some(cli.clone()));
        assert_eq!(cli_next_to(&cli), None, "if the app were the CLI itself it must not call itself");
    }

    #[test]
    fn the_result_box_shows_the_summary_and_the_coverage_problems() {
        for line in [
            "Nodes: FILE 4, FUNCTION 7",
            "Edges: CALLS 6, CONTAINS 10",
            "Coverage: 10 of 12 source files have semantic data",
            "  not covered: a.rs, b.rs",
            "  indexers that did not run: scip-python",
        ] {
            assert!(is_summary_line(line), "{line}");
        }
        for line in ["Indexers:", "  rust-analyzer    rust-cli   ok   8 documents", "Scanned /p: 12 source files", ""] {
            assert!(!is_summary_line(line), "{line}");
        }
    }

    #[test]
    fn elapsed_time_reads_as_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0:00");
        assert_eq!(format_elapsed(Duration::from_secs(27)), "0:27");
        assert_eq!(format_elapsed(Duration::from_secs(754)), "12:34");
    }

    #[test]
    fn long_output_is_trimmed_from_the_front_on_a_character_boundary() {
        let mut output = "é".repeat(MAX_OUTPUT_BYTES); // 2 bytes each
        trim_output(&mut output);
        assert!(output.len() <= MAX_OUTPUT_BYTES / 2 + 1);
        assert!(output.chars().all(|c| c == 'é'));

        let mut short = "kept".to_string();
        trim_output(&mut short);
        assert_eq!(short, "kept");
    }

    #[test]
    fn a_finished_run_reports_done_failed_or_stopped() {
        let (tx, rx) = mpsc::channel();
        let mut app = GuiApp { events: Some(rx), ..GuiApp::default() };
        tx.send(Event::Line("Error: no semantic index was produced".into())).unwrap();
        tx.send(Event::Finished(Some(1))).unwrap();
        app.drain_events();
        match &app.status {
            Status::Finished { ok: false, message } => assert_eq!(message, "no semantic index was produced"),
            _ => panic!("expected a failed run"),
        }

        let (tx, rx) = mpsc::channel();
        let mut app = GuiApp { events: Some(rx), ..GuiApp::default() };
        tx.send(Event::Line("Coverage: 1 of 1 source files have semantic data".into())).unwrap();
        tx.send(Event::Finished(Some(0))).unwrap();
        app.drain_events();
        assert!(matches!(&app.status, Status::Finished { ok: true, message } if message == "Done"));
        assert_eq!(app.summary, ["Coverage: 1 of 1 source files have semantic data"]);

        let (tx, rx) = mpsc::channel();
        let mut app = GuiApp { events: Some(rx), ..GuiApp::default() };
        app.stop_requested.store(true, Ordering::SeqCst);
        tx.send(Event::Finished(None)).unwrap();
        app.drain_events();
        assert!(matches!(&app.status, Status::Finished { ok: false, message } if message == "Stopped"));
    }
}
