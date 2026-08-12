//! The desktop shell.
//!
//! Deliberately thin. It starts the daemon, waits for it to answer, and points a webview at it. No
//! application logic lives here, and that is a decision rather than laziness: everything in this
//! process is unavailable on any other platform and untestable without a display, so anything that
//! ends up here stops being covered by the test suite.
//!
//! ## Why a local HTTP server rather than the webview's asset protocol
//!
//! The daemon already serves the interface, and the interface already talks to it over HTTP and a
//! WebSocket. Serving the assets from the same origin means the browser during development and the
//! shell in production run the same code down to the request URLs. The alternative — assets over a
//! custom protocol, API over HTTP — gives the shell a different origin from the daemon and every
//! cross-origin question has to be answered twice.
//!
//! ## The window opens even when nothing else works
//!
//! If the daemon will not start, this shows a page saying so, with the log path. It does not exit,
//! and it does not show an empty webview. An application that cannot open cannot be used to diagnose
//! why it cannot open, and a shell that dies on a bad daemon takes the only interface to the logs
//! with it.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where the daemon listens. Fixed rather than negotiated: the daemon writes the port it chose into
/// its state directory, but reading that requires knowing the state directory, which is the thing the
/// daemon decides. One constant is easier to reason about than two lookups.
const PORT: u16 = 8787;

fn main() {
    // Held for the process lifetime. Dropping it would leave the daemon running after the window
    // closes, and the next launch would find the port taken by a daemon whose log nobody can find.
    // Behind a mutex because the window-event handler is `Fn` rather than `FnMut`.
    let child = std::sync::Arc::new(std::sync::Mutex::new(match start_daemon() {
        Started::Running(c) => Some(c),
        Started::Failed { reason } => {
            // Not fatal. The window still opens and says so — see `failure_page`.
            eprintln!("could not start the daemon: {reason}");
            None
        }
    }));

    let url = format!("http://127.0.0.1:{PORT}/");
    let ready = wait_for_health(&url, Duration::from_secs(20));

    tauri::Builder::default()
        .setup(move |app| {
            use tauri::Manager;
            let window = app
                .get_webview_window("main")
                .expect("the main window is declared in tauri.conf.json");

            // The window is declared pointing at the daemon, so the happy path needs nothing here.
            // Doing it with `eval` instead was the first attempt and it produced a white window: a
            // script sent during setup races the webview's own initialisation and is simply lost, with
            // no error anywhere. Declaring the URL means the webview navigates as part of creating
            // itself, which cannot race.
            if !ready {
                // A page rather than a dialog, and rather than nothing. The reader needs the log path
                // more than an apology, and a modal they dismiss leaves them looking at a blank
                // window with no way back to this information.
                let page = failure_page(&log_path());
                let encoded: String = page
                    .bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                            (b as char).to_string()
                        }
                        other => format!("%{other:02X}"),
                    })
                    .collect();
                if let Ok(u) = format!("data:text/html,{encoded}").parse() {
                    let _ = window.navigate(u);
                }
            }
            let _ = &url;
            Ok(())
        })
        .on_window_event({
            let child = child.clone();
            move |_window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // Terminated explicitly. The daemon reaps its own agents on shutdown, so letting it
                // linger would leave a tree of agent processes belonging to a window that is gone —
                // which is the exact leak the process registry exists to clean up after, and there is
                // no reason to rely on that path when this one is available.
                if let Some(c) = child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        }})
        .run(tauri::generate_context!())
        .expect("the shell could not start");
}

enum Started {
    Running(Child),
    Failed { reason: String },
}

fn start_daemon() -> Started {
    let Some(binary) = daemon_binary() else {
        return Started::Failed {
            reason: "the daemon binary was not found next to the shell".to_string(),
        };
    };

    // Output goes to a file rather than to inherited handles. A desktop application has no terminal
    // attached, so inherited stdio goes nowhere and the first thing anyone needs when it will not
    // start is the log.
    let log = log_path();
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let sink = std::fs::File::create(&log);

    let mut cmd = Command::new(binary);
    cmd.arg("--listen").arg(format!("127.0.0.1:{PORT}"));
    // Everything after `--` goes to the daemon untouched. Without this the shell would have to grow
    // a duplicate of every daemon flag, and the duplicate would be the one that falls behind — which
    // is worse than having no shell option at all, because a flag that is silently dropped looks
    // like a daemon that ignores its configuration.
    let mut passthrough = std::env::args().skip_while(|a| a != "--").skip(1).peekable();
    if passthrough.peek().is_some() {
        cmd.args(passthrough);
    }
    match sink {
        Ok(f) => {
            let err = f.try_clone();
            cmd.stdout(Stdio::from(f));
            if let Ok(e) = err {
                cmd.stderr(Stdio::from(e));
            }
        }
        Err(_) => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }

    match cmd.spawn() {
        Ok(child) => Started::Running(child),
        Err(e) => Started::Failed { reason: e.to_string() },
    }
}

/// Next to the shell first, then on the path.
///
/// Next to it first because that is where a packaged build puts it, and a stale copy on the path
/// would otherwise win — producing a version mismatch between the shell and the daemon that is very
/// hard to see and explains nothing about the symptoms.
fn daemon_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(if cfg!(windows) { "wkbd-core.exe" } else { "wkbd-core" });
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    which("wkbd-core")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("wkbd").join("daemon.log")
}

/// Polls the health endpoint until it answers.
///
/// Polling rather than waiting on a signal from the child, because the useful question is not "did
/// the process start" but "is it serving". A daemon that started and then failed a migration is a
/// running process that will never answer, and a shell that trusted the spawn would show an empty
/// window instead of the log path.
fn wait_for_health(base: &str, timeout: Duration) -> bool {
    let url = format!("{base}api/health");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if http_ok(&url) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// The smallest HTTP GET that answers the question.
///
/// Hand-rolled to keep an HTTP client out of the shell's dependency tree for one request against
/// loopback. It reads only enough to see the status line.
fn http_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let Ok(mut stream) = std::net::TcpStream::connect(authority) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    use std::io::Write;
    if stream
        .write_all(
            format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) if n > 12 => buf[..n].starts_with(b"HTTP/1.") && buf[..n].windows(3).any(|w| w == b"200"),
        _ => false,
    }
}

fn failure_page(log: &PathBuf) -> String {
    format!(
        "<div style=\"font:14px/1.6 system-ui,sans-serif;padding:32px;max-width:60ch\">\
         <h1 style=\"font-size:20px\">The workbench could not start its core process.</h1>\
         <p>The window is open so that you can read this. Nothing has been lost: the event log is \
         append-only and the previous session is still on disk.</p>\
         <p>The daemon's output is at:</p>\
         <pre style=\"background:#f6f8fa;padding:12px;border-radius:6px;overflow:auto\">{}</pre>\
         <p>Starting <code>wkbd-core --listen 127.0.0.1:{}</code> from a terminal will show the same \
         failure with the output attached.</p></div>",
        log.display(),
        PORT
    )
}
