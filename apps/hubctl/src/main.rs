//! `hubctl`: the Display Hub PC helper. See `docs/providers.md`.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hubctl::commands;

/// Forwards usage figures that Claude Code and Codex already report on this PC to your Display Hub.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Keep the helper's settings here instead of the user's configuration folder.
    #[arg(long, global = true, hide = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with the hub. Paste the helper string from the manager's Settings when asked (it is read
    /// from standard input, never from the command line).
    Login,
    /// Show the hub, what this PC reports, and recent activity.
    Status,
    /// Forget the hub on this PC.
    Logout,
    /// Claude Code status line: set `"statusLine": {"type": "command", "command": "hubctl claude-statusline"}`
    /// in Claude Code's settings. Prints a short line and forwards the plan limits in the background.
    ClaudeStatusline {
        /// Print nothing; only forward. For piping from a status line script you already have.
        #[arg(long)]
        quiet: bool,
    },
    /// Ask Codex (`codex app-server`) for your ChatGPT plan's limits and send them. For Task Scheduler.
    CodexSync {
        /// The Codex executable, if `codex` isn't on PATH.
        #[arg(long)]
        codex: Option<PathBuf>,
        /// Show what Codex reports without sending it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Send the queued Claude Code figures (started by `claude-statusline`).
    #[command(hide = true)]
    SendPending,
}

fn finish(r: Result<String, String>) -> ExitCode {
    match r {
        Ok(m) => {
            println!("{m}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hubctl: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dir = commands::dir(cli.config_dir);
    let store = commands::store();
    match cli.command {
        Cmd::Login => {
            if std::io::stdin().is_terminal() {
                eprint!("Paste the helper string from the manager's Settings, then press Enter: ");
                let _ = std::io::stderr().flush();
            }
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
                eprintln!("hubctl: no helper string given");
                return ExitCode::from(2);
            }
            let r = commands::login(&dir, &store, &line);
            zeroize::Zeroize::zeroize(&mut line);
            finish(r)
        }
        Cmd::Status => finish(commands::status(&dir, &store)),
        Cmd::Logout => finish(commands::logout(&dir, &store)),
        Cmd::ClaudeStatusline { quiet } => {
            // Always exits 0 with a line: Claude Code blanks the status line otherwise.
            commands::claude_statusline(
                &dir,
                &mut std::io::stdin().lock(),
                &mut std::io::stdout().lock(),
                quiet,
                true,
            );
            ExitCode::SUCCESS
        }
        Cmd::CodexSync { codex, dry_run } => {
            finish(commands::codex_sync(&dir, &store, codex.as_deref(), dry_run))
        }
        Cmd::SendPending => match commands::send_pending(&dir, &store) {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        },
    }
}
