//! Integration: rebasing a card branch onto the run tip when other cards were accepted while it
//! waited (`engine.py`: `_accept`'s integration, `_rebase`, `_integration_step`'s rebase). The
//! I/O the shell carries out for `decide`; which step is due, the integrate attempt after a
//! conflict, the re-review and the verify that follow are `next`'s.
//!
//! The rebase runs in the run's worktree, on the card branch only: its commits are never signed,
//! it never updates other branches and never autostashes (`git::cards::rebase_onto`). The
//! card's tip before integration is kept first on the integration's `keep` ref
//! (`refs/leadhand/<run id>/integration/<card>-<k>`), and whatever an implementer left in the
//! worktree is kept and cleared before the rebase (R30). In-place runs are gone (deliberate
//! change 1), so nothing here looks for ignored files a rebase could overwrite.
//!
//! Crash points sit at the Python's positions (S2-R34): `before_rebase` after the `keep` ref
//! is written, `after_rebase` before `rebased` or `conflict` is recorded.

use super::cards::{Log, Run, card_branch, is_card, os};
use super::violations::{ensure_clean, run_id};
use crate::error::{Error, Result};
use crate::git::cards::{branch_ref, is_ancestor, rebase_onto};
use crate::git::workspace::head;
use crate::state::events::{self, IntegrationStarted};
use crate::state::{CardRef, Event, append_event};
use crate::testing::crash_point;
use crate::wire::PyStr;

/// The card's branch as the run records it, else the name it is cut under.
fn branch_of(run: &Run, log: &Log, card: u32) -> Result<PyStr> {
    let cards = log.cards(&run.numbers());
    // `fold_cards(events, ctx.numbers)[card]`.
    let c = cards
        .get(&card)
        .ok_or_else(|| Error::msg(format!("KeyError: {card}")))?;
    Ok(card_branch(run, c))
}

/// Integrates a card whose branch does not contain the run tip (`_accept`'s first branch):
/// records `integration_started` with the run tip to rebase onto, the card's tip and the ref
/// that keeps it, then rebases.
pub(super) fn integrate(run: &Run, card: u32) -> Result<()> {
    let log = Log::read(&run.paths)?;
    let branch = branch_of(run, &log, card)?;
    let git = run.git()?;
    let k = 1 + log
        .events
        .iter()
        .filter(|e| matches!(e, Event::IntegrationStarted(i) if is_card(i.card.as_ref(), card)))
        .count();
    let onto = head(&git, &run.run_ref().to_os()?)?;
    let tip = head(&git, &branch_ref(&branch)?)?;
    let keep = format!("refs/leadhand/{}/integration/{card}-{k}", run_id(run)?);
    let (integ, _) = append_event(
        &run.paths,
        Event::IntegrationStarted(IntegrationStarted {
            card: Some(CardRef::Number(card.into())),
            onto: Some(onto),
            tip: Some(tip),
            keep: Some(keep.into()),
            ..IntegrationStarted::default()
        }),
    )?;
    let Event::IntegrationStarted(integ) = integ else {
        unreachable!("append_event returns the event it appended");
    };
    rebase(run, card, &branch, &integ)
}

/// Rebases an integrating card again, from its latest `integration_started` (`_integration_step`
/// after a crash between the intent and its outcome).
pub(super) fn rebase_again(run: &Run, card: u32) -> Result<()> {
    let log = Log::read(&run.paths)?;
    let branch = branch_of(run, &log, card)?;
    let integ = log
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::IntegrationStarted(i) if is_card(i.card.as_ref(), card) => Some(i.clone()),
            _ => None,
        })
        .ok_or_else(|| Error::msg("list index out of range"))?;
    rebase(run, card, &branch, &integ)
}

/// Rebases the card branch onto the run tip `integ` recorded, then records `rebased` or
/// `conflict` (`_rebase`). Repeat-safe: a card branch that already contains the run tip counts
/// as rebased; on a conflict `rebase_onto` has aborted the rebase. The card's tip before
/// integration is kept on `integ`'s `keep` ref first; anything left in the workspace (an
/// implementer's leftovers, or a rebase a dead process left in progress) is kept and cleared,
/// which puts the card branch back where it was.
fn rebase(run: &Run, card: u32, branch: &PyStr, integ: &IntegrationStarted) -> Result<()> {
    let git = run.git()?;
    if let Some(keep) = integ.keep.as_ref().filter(|k| !k.is_empty()) {
        let tip = integ
            .tip
            .clone()
            .ok_or_else(|| Error::msg("KeyError: 'tip'"))?;
        git.run(&[
            os("update-ref"),
            os("-m"),
            os("leadhand: card tip before integration"),
            &keep.to_os()?,
            os(&tip),
        ])?;
    }
    crash_point("before_rebase");
    let why = PyStr::from(format!("before rebasing card {card}"));
    ensure_clean(run, branch, &why, None, false, true)?;
    let onto = integ
        .onto
        .clone()
        .ok_or_else(|| Error::msg("KeyError: 'onto'"))?;
    let card_ref = branch_ref(branch)?;
    let conflicts = if is_ancestor(&git, os(&onto), &card_ref)? {
        Vec::new()
    } else {
        rebase_onto(&git, branch, &onto)?
    };
    crash_point("after_rebase");
    let card_ref_event = Some(CardRef::Number(card.into()));
    let event = if conflicts.is_empty() {
        Event::Rebased(events::Rebased {
            card: card_ref_event,
            onto: Some(onto),
            tip: Some(head(&git, &card_ref)?),
            ..events::Rebased::default()
        })
    } else {
        Event::Conflict(events::Conflict {
            card: card_ref_event,
            onto: Some(onto),
            paths: Some(conflicts),
            ..events::Conflict::default()
        })
    };
    append_event(&run.paths, event)?;
    Ok(())
}
