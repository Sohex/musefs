//! Confirmation prompts, for the one command that changes a store in a way it
//! cannot take back (#705).
//!
//! `dialoguer` is the console-rs sibling of the `indicatif` dependency the scan
//! progress bar already uses, which matters rather than being incidental: both
//! draw to stderr through the same `console` backend, so a prompt can be lifted
//! clear of a live progress frame by the same [`crate::progress::suspend`] path
//! a log record goes through.

use std::io::IsTerminal;

use anyhow::{Context as _, Result};

/// Whether a question can be asked at all.
///
/// Both ends have to be a terminal: stdin because the answer is read from it,
/// stderr because that is where the question is drawn. A pipeline gets neither,
/// and must never be left blocking on an answer nobody is there to give.
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask `question`, taking `default` on a bare Enter. Callers check
/// [`interactive`] first; off a terminal they name a flag instead.
pub(crate) fn confirm(question: &str, default: bool) -> Result<bool> {
    crate::progress::suspend(|| {
        dialoguer::Confirm::new()
            .with_prompt(question)
            .default(default)
            .interact()
            .context("reading the answer from the terminal")
    })
}

/// Resolve a yes/no the user may have answered on the command line, may be
/// asked for now, or may not be in a position to answer at all.
///
/// An explicit flag always wins, so a script is never stuck. Absent one, a
/// terminal gets the question and a pipeline gets `false` — every caller of
/// this is an *offer* of extra work, so declining is both the safe answer and
/// the one that leaves the user something they can still run by hand.
pub(crate) fn decide(flag: Option<bool>, question: &str, default: bool) -> Result<bool> {
    match flag {
        Some(answer) => Ok(answer),
        None if interactive() => confirm(question, default),
        None => Ok(false),
    }
}
