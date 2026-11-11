//! The Display Hub PC helper. It forwards usage figures that tools on this PC already report to their
//! signed-in user (Claude Code's status line, Codex's app server) to the hub, labelled as
//! provider-reported. It reads no browser data, copies no other app's tokens, and calls no
//! undocumented endpoints (`docs/providers.md`).

pub mod claude;
pub mod codex;
pub mod commands;
pub mod helper;
