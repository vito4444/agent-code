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
    let expect_pid = child
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.id());
    let ready = wait_for_health(&url, Duration::from_secs(20), expect_pid);

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
            if !matches!(ready, Readiness::Ready) {
                // A page rather than a dialog, and rather than nothing. The reader needs the log path
                // more than an apology, and a modal they dismiss leaves them looking at a blank
                // window with no way back to this information.
                let page = failure_page(&log_path(), &ready);
                let encoded: String = page
                    .bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                            (b as char).to_string()
                        }
                        other => format!("%{other:02X}"),
                    })
                    .collect();
                // The charset has to be declared. Percent-encoding is per byte, so the UTF-8 is
                // intact on the way in; without this the browser decodes those bytes as latin-1 and
                // every em dash in the page arrives as three characters of noise. A diagnostic page
                // that looks corrupted is one the reader stops trusting halfway through.
                if let Ok(u) = format!("data:text/html;charset=utf-8,{encoded}").parse() {
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

/// Polls the health endpoint until the daemon we started answers.
///
/// Polling rather than waiting on a signal from the child, because the useful question is not "did
/// the process start" but "is it serving". A daemon that started and then failed a migration is a
/// running process that will never answer, and a shell that trusted the spawn would show an empty
/// window instead of the log path.
///
/// The pid check is the other half, and it is not hypothetical: the port here is fixed, so a stale
/// daemon still holding it makes the new one fail to bind and exit — after which "something answers
/// on 8787" is true and points at a process this shell does not control and cannot restart. The
/// symptoms are all indirect: agents that were configured are missing, a flag has no effect, the
/// interface is a version behind. Refusing to adopt a stranger turns all of that into one sentence.
fn wait_for_health(base: &str, timeout: Duration, expect_pid: Option<u32>) -> Readiness {
    let url = format!("{base}api/health");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match health(&url) {
            Some(pid) => match (expect_pid, pid) {
                // Older daemons do not report a pid. Accepting that is deliberate: refusing would
                // make the shell unusable against a daemon that is merely old, which is a worse
                // failure than the one being prevented.
                (Some(want), Some(got)) if want != got => {
                    eprintln!(
                        "a daemon we did not start is already serving 127.0.0.1:{PORT} \
                         (pid {got}, expected {want}); refusing to use it"
                    );
                    return Readiness::PortTaken { pid: got };
                }
                _ => return Readiness::Ready,
            },
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    }
    Readiness::NoAnswer
}

/// Why the shell is or is not going to show the interface.
///
/// Three outcomes rather than a boolean, because the two failures have different remedies and a
/// diagnostic that names the wrong cause sends the reader to the wrong place. "It did not start" means
/// read the log; "somebody else is on the port" means close the other window.
enum Readiness {
    Ready,
    NoAnswer,
    PortTaken { pid: u32 },
}

/// `Some(pid)` when the daemon answered, where the inner option is its reported pid.
///
/// Two levels of option because "did not answer" and "answered without saying which process it is"
/// are different answers and lead to different behaviour.
fn health(url: &str) -> Option<Option<u32>> {
    let body = http_get(url)?;
    Some(pid_from_health(&body))
}

/// Pulls `"pid": N` out of a health response.
///
/// Read by hand rather than with a JSON parser, to keep one out of a process that makes a single
/// request against loopback. The key is matched with its quotes and colon so that a substring like
/// `"stupid"` cannot supply the number — which is the failure a looser match invites, and it would
/// make the shell refuse to run for a reason nobody could see.
fn pid_from_health(body: &str) -> Option<u32> {
    let at = body.find("\"pid\"")?;
    let rest = &body[at + 5..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// The smallest HTTP GET that answers the question.
///
/// Hand-rolled to keep an HTTP client out of the shell's dependency tree for one request against
/// loopback.
fn http_get(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let mut stream = std::net::TcpStream::connect(authority).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    use std::io::Write;
    stream
        .write_all(
            format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/1.") || !head.lines().next()?.contains("200") {
        return None;
    }
    Some(body.to_string())
}

fn failure_page(log: &PathBuf, why: &Readiness) -> String {
    // The cream ground and the serif are the interface's, hand-written here because this page loads
    // before — or instead of — the stylesheet. A failure page that does not look like the application
    // reads as a crash in something else.
    let frame = "font:15px/1.7 'Source Han Serif SC','Noto Serif',Georgia,serif;\
                 background:#f5f3ee;color:#2b2926;margin:0;padding:48px;max-width:66ch";
    let code = "font-family:ui-monospace,'SF Mono',Menlo,monospace;color:#9c3b2e";
    let pre = "background:#efece4;padding:12px 14px;border-radius:6px;overflow:auto;font-size:13px";

    let body = match why {
        Readiness::PortTaken { pid } => format!(
            "<h1 style=\"font-size:22px;margin:0 0 16px\">Another workbench is already using this \
             port.</h1>\
             <p>Process <span style=\"{code}\">{pid}</span> is serving \
             <span style=\"{code}\">127.0.0.1:{PORT}</span>, and this window did not start it. It \
             is not being used, because a daemon this shell does not control is one it cannot \
             configure or restart — and every symptom of adopting it would be indirect: agents that \
             are missing, flags with no effect, an interface a version behind.</p>\
             <p>Either use the window that already has it, or stop that process and reopen this \
             one.</p>"
        ),
        _ => format!(
            "<h1 style=\"font-size:22px;margin:0 0 16px\">The workbench could not start its core \
             process.</h1>\
             <p>The window is open so that you can read this. Nothing has been lost: the event log \
             is append-only and the previous session is still on disk.</p>\
             <p>The daemon's output is at:</p>\
             <pre style=\"{pre}\">{log}</pre>\
             <p>Running <span style=\"{code}\">wkbd-core --listen 127.0.0.1:{PORT}</span> in a \
             terminal shows the same failure with its output attached.</p>",
            log = log.display()
        ),
    };

    format!("<div style=\"{frame}\">{body}</div>")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check that stops the shell adopting a daemon it did not start. Parsed by hand because this
    /// process makes one request and a JSON dependency for it would be the larger mistake — but "by
    /// hand" is exactly why it needs tests.
    #[test]
    fn reads_the_pid_out_of_a_health_body() {
        let body = r#"{"ok":true,"read_only":false,"degraded":null,"agents":2,"pid":4321}"#;
        assert_eq!(pid_from_health(body), Some(4321));
    }

    #[test]
    fn tolerates_the_pid_arriving_first_or_last() {
        assert_eq!(pid_from_health(r#"{"pid":7,"ok":true}"#), Some(7));
        assert_eq!(pid_from_health(r#"{"ok":true,"pid":7}"#), Some(7));
    }

    #[test]
    fn tolerates_whitespace_a_formatter_might_add() {
        assert_eq!(pid_from_health("{ \"pid\" : 99 }"), Some(99));
    }

    /// An older daemon does not report one. That has to be `None` rather than an error: refusing to
    /// run against a daemon that is merely old is a worse failure than the one being prevented.
    #[test]
    fn a_body_without_a_pid_is_not_an_error() {
        assert_eq!(pid_from_health(r#"{"ok":true,"agents":0}"#), None);
    }

    /// Substring matching would find the `pid` inside another key and read whatever followed it.
    #[test]
    fn does_not_match_a_pid_inside_another_key() {
        assert_eq!(pid_from_health(r#"{"stupid":5,"pid":6}"#), Some(6));
        assert_eq!(pid_from_health(r#"{"rapid_mode":true}"#), None);
    }

    #[test]
    fn a_non_numeric_pid_is_ignored_rather_than_guessed_at() {
        assert_eq!(pid_from_health(r#"{"pid":"nine"}"#), None);
    }
}
