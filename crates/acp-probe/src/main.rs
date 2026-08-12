//! Measures what an ACP agent actually does, as opposed to what the protocol permits.
//!
//! The output is the capability matrix the interface is built against. Every field starts
//! at "not observed" and only becomes true when this tool watched it happen, because the
//! alternative — assuming a capability exists and discovering otherwise in front of a user
//! — produces controls that appear functional and do nothing.
//!
//! Two halves, with different requirements:
//!
//! - `initialize` and `session/new` need no model credentials. Protocol version, announced
//!   capabilities, and which config options exist are all obtainable offline.
//! - Everything about a turn — whether chunks carry `messageId`, how many thought segments
//!   one turn produces, whether usage is ever reported — requires a real prompt, and
//!   therefore credentials for whatever model the agent is configured to use.
//!
//! Pass `--prompt` to run the second half. Without it the report is explicit that those
//! rows were not measured, which is a different statement from "the agent does not do it".

use anyhow::Result;
use clap::Parser;
use std::collections::BTreeMap;
use wkbd_agent::{probe_agent, CapabilityReport};

#[derive(Parser, Debug)]
#[command(name = "acp-probe", about = "Measure an ACP agent's real capabilities")]
struct Cli {
    /// Agent command line, e.g. `--agent 'claude=npx @agentclientprotocol/claude-agent-acp'`.
    /// Repeatable.
    #[arg(long = "agent", value_name = "ID=COMMAND")]
    agents: Vec<String>,

    /// Also run a real prompt turn. Requires whatever credentials the agent needs.
    #[arg(long)]
    prompt: Option<String>,

    /// Working directory handed to `session/new`.
    #[arg(long, default_value = ".")]
    cwd: String,

    /// Option id to select when the agent asks for permission. Defaults to the first
    /// option offered, which for most agents is "allow once".
    #[arg(long)]
    permission: Option<String>,

    /// Emit JSON instead of the table.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if cli.agents.is_empty() {
        eprintln!(
            "no agents given. example:\n  \
             acp-probe --agent 'fake=./target/debug/fake-acp-agent' --prompt 'hello'"
        );
        std::process::exit(2);
    }

    let cwd = std::fs::canonicalize(&cli.cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| cli.cwd.clone());

    let mut reports = Vec::new();
    for entry in &cli.agents {
        let (id, command) = match entry.split_once('=') {
            Some((id, cmd)) => (id.to_string(), cmd.to_string()),
            None => (entry.clone(), entry.clone()),
        };

        let parts = shell_words(&command);
        if parts.is_empty() {
            eprintln!("skipping {id}: empty command");
            continue;
        }

        let command_for_build = parts.clone();
        let report = probe_agent(
            &id,
            &command,
            move || {
                let mut cmd = tokio::process::Command::new(&command_for_build[0]);
                cmd.args(&command_for_build[1..]);
                // The environment is inherited here, unlike in the daemon: probing a real
                // agent needs its credentials, and the point of this tool is to observe an
                // agent configured the way the user configured it.
                cmd.kill_on_drop(true);
                cmd
            },
            &cwd,
            cli.prompt.as_deref(),
            cli.permission.as_deref(),
        )
        .await;

        match report {
            Ok(r) => reports.push(r),
            Err(e) => {
                let mut r = CapabilityReport {
                    agent_id: id,
                    command,
                    ..Default::default()
                };
                r.initialize_failed = Some(format!("could not launch: {e}"));
                reports.push(r);
            }
        }
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }

    print_table(&reports, cli.prompt.is_some());
    Ok(())
}

/// Minimal argv splitter. Handles quotes so a command with a quoted path works; it does not
/// try to be a shell, because running the agent through a shell would add a process that
/// nothing supervises.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn print_table(reports: &[CapabilityReport], turn_run: bool) {
    println!("ACP capability matrix");
    println!("{}", "=".repeat(100));
    if !turn_run {
        println!(
            "NOTE: no --prompt given, so every turn-dependent row below reads 'not observed'.\n      \
             That is not the same as the agent not supporting it."
        );
        println!("{}", "-".repeat(100));
    }

    for r in reports {
        println!("\n{}  ({})", r.agent_id, r.command);
        if let Some(err) = &r.initialize_failed {
            println!("  UNREACHABLE: {err}");
            continue;
        }
        println!(
            "  protocol version      {}",
            r.protocol_version.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
        );
        if let Some(info) = &r.agent_info {
            println!("  agent info            {info}");
        }
        println!(
            "  auth methods          {}",
            if r.auth_methods.is_empty() { "none".into() } else { r.auth_methods.join(", ") }
        );
        println!("  session/new           {}", if r.session_created { "ok" } else { "failed" });

        if r.config_option_ids.is_empty() {
            println!(
                "  config options        none declared -> the UI must draw no model or \
                 thinking selector"
            );
        } else {
            println!("  config options        {}", r.config_option_ids.join(", "));
            println!(
                "  categories            {}",
                if r.config_categories.is_empty() {
                    "none (all must render as plain selects)".into()
                } else {
                    r.config_categories.join(", ")
                }
            );
            for (id, support) in &r.config_support {
                println!("    {id:<20} {support:?}");
            }
        }

        if !r.turn_observed {
            println!("  turn behaviour        not observed (no --prompt)");
            continue;
        }

        println!(
            "  messageId             {}",
            match (r.sent_message_ids, r.omitted_message_ids) {
                (true, false) => "always -> segmentation is the agent's own".to_string(),
                (true, true) => "sometimes -> segmentation is best effort".to_string(),
                (false, _) => "never -> segmentation is entirely inferred".to_string(),
            }
        );
        println!(
            "  thought segments      {}{}",
            r.thought_segments_in_turn,
            if r.thought_segments_in_turn > 1 {
                "  (several per turn: only the newest may auto-expand)"
            } else {
                ""
            }
        );
        println!("  answer segments       {}", r.message_segments_in_turn);
        println!(
            "  max concurrent live   {}{}",
            r.max_concurrent_live_segments,
            if r.max_concurrent_live_segments > 1 { "  <-- INVARIANT VIOLATED" } else { "" }
        );
        println!(
            "  usage_update          {}",
            if r.sent_usage_update {
                "yes -> show the context ring"
            } else {
                "no -> the context ring must be absent, not zero"
            }
        );
        println!("  structured diff       {}", yes_no(r.sent_diff_content));
        println!("  embedded terminal     {}", yes_no(r.sent_terminal_content));
        println!("  plan                  {}", yes_no(r.sent_plan));
        println!("  asked permission      {}", yes_no(r.requested_permission));
        println!(
            "  stop reason           {}",
            r.stop_reason.clone().unwrap_or_else(|| "-".into())
        );
        if !r.unknown_updates.is_empty() {
            println!("  unmodelled updates    {}", r.unknown_updates.join(", "));
        }
        if r.malformed_lines > 0 {
            println!(
                "  malformed stdout      {} line(s) skipped  <-- the agent is polluting \
                 its own JSON-RPC stream",
                r.malformed_lines
            );
        }
    }

    println!("\n{}", "=".repeat(100));
    println!("summary");
    let mut by_id: BTreeMap<&str, &CapabilityReport> = BTreeMap::new();
    for r in reports {
        by_id.insert(&r.agent_id, r);
    }
    for r in by_id.values() {
        println!("  {}", r.summary_line());
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}
