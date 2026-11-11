//! `hubctl-sync`: `hubctl codex-sync` without a console window, for Windows Task Scheduler. It takes
//! the same `--codex <path>`, and writes what happened to the helper's log (`hubctl status` shows it).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut codex: Option<PathBuf> = None;
    let mut dir: Option<PathBuf> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--codex") => codex = args.next().map(PathBuf::from),
            Some("--config-dir") => dir = args.next().map(PathBuf::from),
            _ => return ExitCode::from(2),
        }
    }
    let dir = hubctl::commands::dir(dir);
    match hubctl::commands::codex_sync(&dir, &hubctl::commands::store(), codex.as_deref(), false) {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
