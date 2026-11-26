//! How a run changes, worked out without the database: from its state, its steps
//! and what happened, the state it goes to and what to send Claude. The functions
//! outside load the run, pass in what this needs (the time, a fresh paste token,
//! how often it took each step), and save and send what comes back.

use super::{
    awaits_prompt, continue_offered, node_of, By, Change, Phase, Resume, RunState, Say, Status, Taken, BRIEFING_TIMEOUT_SECS,
    CLEAR_TIMEOUT_SECS, MAX_RUN_STEPS, NO_REPLY, NO_TOKEN,
};
use crate::flows::{self, Graph, Kind, Node};

/// What a change is worked out from besides the run's state: its steps, how often
/// it took each (see `Taken`), and a fresh token for a paste it may come to owe.
pub(super) struct Cx<'a> {
    pub(super) graph: &'a Graph,
    pub(super) taken: &'a Taken,
    pub(super) token: i64,
}

impl<'a> Cx<'a> {
    fn node(&self, id: &str) -> Result<&'a Node, String> {
        node_of(self.graph, id)
    }

    /// How often the run has taken `step` so far, counting a visit it is on now.
    fn runs_of(&self, state: &RunState, step: &str) -> i64 {
        self.taken.0.get(step).copied().unwrap_or(0) + i64::from(state.node_id == step)
    }

    /// `state`, at the manual prompt `node`, owing the app the paste of its text
    /// (see `prefill_text`), if there is any: Start's base info unless Claude has
    /// it, then the step's draft. Each time under a new token, so the record of an
    /// earlier paste cannot cancel it (see `prefilled`).
    fn owe_prefill(&self, state: RunState, node: &Node) -> RunState {
        if flows::prefill_text(self.graph, node, !state.briefed).is_empty() {
            return state;
        }
        state.with_prefill(self.token)
    }

    /// `state`, owing the app a fill of Claude's input again if the run waits at a
    /// manual prompt, as that input was lost (a clear, a restart): with the base
    /// info, if Claude lacks it now.
    fn refill(&self, state: RunState) -> Result<RunState, String> {
        let node = self.node(&state.node_id)?;
        Ok(if awaits_prompt(&state, node) { self.owe_prefill(state, node) } else { state })
    }
}

/// Fails unless the run is at `step_id`, the step the person saw it at.
pub(super) fn at(graph: &Graph, state: &RunState, step_id: &str) -> Result<(), String> {
    if state.node_id == step_id {
        return Ok(());
    }
    let name = |id: &str| node_of(graph, id).map_or(id.to_owned(), |n| n.name().to_owned());
    Err(format!("the run has moved on from \"{}\" to \"{}\"; pick again for where it is now", name(step_id), name(&state.node_id)))
}

/// The run's first turn, from Start, at `now`: its briefing (the run's rules and
/// Start's base info), the run then waiting at Start for Claude's reply (see
/// `on_stop`); or, for a flow that leads straight to its end, nothing, the run
/// being done.
pub(super) fn first_turn(cx: &Cx, state: &RunState, start: &Node, now: i64) -> Change {
    let next = flows::next_step(cx.graph, &start.id, flows::START_OUTPUT);
    if next.is_none_or(|n| n.kind == Kind::End) {
        return go(cx, state, start, flows::START_OUTPUT, 0, By::Flow);
    }
    let briefing = RunState { briefed: true, ..state.moved(&start.id, Phase::Briefing { sent_at: now }, 0) };
    Change { state: briefing, left: None, say: Some(Say::Briefing) }
}

/// Moves the run from `from` along `output` to the step it leads to, if any, with
/// the run's step count at `steps`, with the prompt to send when that is a prompt
/// step (a run that ends sends nothing; see `RunState::finished`). The run
/// instead waits at `from` for the person when going on would pass
/// `MAX_RUN_STEPS` (into a prompt step) or, for `By::Flow` only, when the next
/// step has already run its `max_runs`.
pub(super) fn go(cx: &Cx, state: &RunState, from: &Node, output: &str, steps: i64, by: By) -> Change {
    let next = flows::next_step(cx.graph, &from.id, output);
    // Going on with `from` would only redo it and stop here again.
    let wait = |why: String| Change::to(state.waiting(&from.id, steps, &why, None));
    if next.is_some_and(|n| matches!(n.kind, Kind::Prompt | Kind::Title)) && steps >= MAX_RUN_STEPS {
        return wait(format!("The run has taken {MAX_RUN_STEPS} steps without you; pick an output to go on."));
    }
    if let (Some(next), By::Flow) = (next, by)
        && let Some(max) = next.max_runs
        && cx.runs_of(state, &next.id) >= i64::from(max)
    {
        let name = next.name();
        let times = if max == 1 { "once".to_string() } else { format!("{max} times") };
        return wait(format!(
            "\"{name}\" has already run {times}, its limit. Pick \"{output}\" to run it once more, or another output."
        ));
    }
    let (state, say) = match next {
        // The output leads nowhere: the flow is done.
        None => (state.finished(&from.id, steps), None),
        Some(next) => match next.kind {
            Kind::Prompt | Kind::Title => {
                let mut state = state.moved(&next.id, Phase::Working, steps + 1);
                let say = state.prompt(next);
                (state, Some(say))
            }
            Kind::Pause => (state.moved(&next.id, Phase::Waiting { prefill: false }, steps), None),
            Kind::ManualPrompt => (cx.owe_prefill(state.moved(&next.id, Phase::Waiting { prefill: false }, steps), next), None),
            Kind::End | Kind::Start => (state.finished(&next.id, steps), None),
        },
    };
    Change { state, left: Some((from.id.clone(), output.to_owned())), say }
}

/// Claude ended its turn with `reply` (None when Claude Code did not report it):
/// the change, or None to leave the run alone (see `super::on_stop`).
pub(super) fn on_stop(cx: &Cx, state: &RunState, reply: Option<&str>) -> Result<Option<Change>, String> {
    // A turn that ends before the clear (one Claude was in when ▶ was pressed) is
    // not the reply to any step.
    if matches!(state.phase, Phase::Clearing { .. }) {
        return Ok(None);
    }
    // The step's prompt, typed when the person moved the run on, was not yet sent:
    // the first turn to end is the one it waited behind; another, with no prompt of
    // the step's sent in between, means it never will be (see `Phase::Typed`).
    match state.phase {
        Phase::Typed { turn_ended: false } => {
            return Ok(Some(Change::to(RunState { phase: Phase::Typed { turn_ended: true }, ..state.clone() })));
        }
        Phase::Typed { turn_ended: true } => return Ok(Some(Change::to(state.waiting_here(TYPED_NOT_SENT, Some(Resume::Prompt))))),
        _ => {}
    }
    let node = cx.node(&state.node_id)?;
    if node.kind == Kind::Start {
        // Only Claude's reply to the run's briefing ends it (a turn after a clear or a
        // restart is not one): "Ready" takes the first step. A reply that did more,
        // as Claude may start on the job, waits for the person to look first.
        if !matches!(state.phase, Phase::Briefing { .. }) {
            return Ok(None);
        }
        let wait = |why: &str| Change::to(state.waiting(&node.id, 0, why, None));
        return Ok(Some(match reply {
            None => wait(NO_REPLY),
            Some(reply) if !flows::says_ready(reply) => wait(
                "Claude did more than reply \"Ready\" to the flow's briefing. Read its reply, then pick an output to go on, or stop the run.",
            ),
            Some(_) => go(cx, state, node, flows::START_OUTPUT, 0, By::Flow),
        }));
    }
    if node.kind == Kind::ManualPrompt {
        // Working only once the person sent their prompt (see `on_prompt`): this is
        // Claude's answer to it. Waiting for the person, any turn is left alone.
        let Some(output) = node.outputs.first().filter(|_| state.phase == Phase::Working) else {
            return Ok(None);
        };
        // The person stepped in, which restarts the count toward MAX_RUN_STEPS.
        return Ok(Some(go(cx, state, node, output, 0, By::Flow)));
    }
    match &state.phase {
        Phase::NeedsPerson { .. } => {
            // The person may be chatting with Claude while the run waits. Only a reply
            // that ends with one of the step's outputs moves it on, and only where
            // Continue is offered: at a step limit only a pick passes the wait. Any
            // other turn leaves the wait, its note and Continue as they were.
            let output = reply.and_then(|reply| reply_output(node, reply)).filter(|m| {
                continue_offered(&state.phase, node.kind).is_some() && m != flows::ATTENTION && node.outputs.contains(m)
            });
            Ok(output.map(|output| go(cx, state, node, &output, state.steps, By::Flow)))
        }
        Phase::Working => {
            let wait = |why: String| Change::to(state.waiting(&node.id, state.steps, &why, Some(Resume::Remind)));
            let Some(reply) = reply else {
                // Its next reply would not be reported either.
                return Ok(Some(Change::to(state.waiting(&node.id, state.steps, NO_REPLY, None))));
            };
            if node.kind == Kind::Title && flows::parse_marker(reply).as_deref() != Some(flows::ATTENTION) {
                return Ok(Some(match reply_output(node, reply) {
                    Some(output) => go(cx, state, node, &output, state.steps, By::Flow),
                    None => wait("Claude's reply did not end with a TITLE line, so the tab cannot be named.".into()),
                }));
            }
            Ok(Some(match flows::parse_marker(reply) {
                None => wait("Claude's reply did not end with a FLOW line, so the flow cannot tell how to go on.".into()),
                Some(m) if m == flows::ATTENTION => wait("Claude asked for a person; see its last reply.".into()),
                Some(m) if !node.outputs.contains(&m) => {
                    wait(format!("Claude ended with \"FLOW: {m}\", which is not an output of \"{}\".", node.name()))
                }
                Some(m) => go(cx, state, node, &m, state.steps, By::Flow),
            }))
        }
        // Waiting at a pause, a chat turn is not its reply; an ended run has none.
        // (Clearing, Briefing, which is only at Start, and Typed are seen to above.)
        _ => Ok(None),
    }
}

/// The output a running step's `reply` picks: the `FLOW:` line's, or at a tab title
/// step, its one output once the reply ends with a title (see `flows::parse_title`).
fn reply_output(node: &Node, reply: &str) -> Option<String> {
    if node.kind == Kind::Title {
        return flows::parse_title(reply).and(node.outputs.first().cloned());
    }
    flows::parse_marker(reply)
}

/// The person picked `output` for `step_id` (see `super::proceed`).
pub(super) fn proceed(cx: &Cx, state: &RunState, step_id: &str, output: &str) -> Result<Change, String> {
    at(cx.graph, state, step_id)?;
    match state.phase {
        Phase::Clearing { .. } => {
            return Err("the run is clearing the conversation before its first step; wait for it, or stop the run".into());
        }
        // The first step would land behind the briefing, whose reply would be taken for the step's.
        Phase::Briefing { .. } => return Err("Claude is reading the flow's briefing; wait for its reply, or stop the run".into()),
        _ => {}
    }
    let node = cx.node(&state.node_id)?;
    if !node.outputs.iter().any(|o| o == output) {
        return Err(format!("\"{}\" has no output {output:?}", node.name()));
    }
    let mut change = go(cx, state, node, output, 0, By::Person);
    // Claude may still be answering: the next step's reply comes only after Claude
    // Code has submitted its prompt (see `Phase::Typed`). A repeat pick types a new
    // prompt, which waits afresh.
    if matches!(state.phase, Phase::Working | Phase::Typed { .. }) && change.state.phase == Phase::Working {
        change.state.phase = Phase::Typed { turn_ended: false };
    }
    Ok(change)
}

/// The app pasted the manual prompt named by `token` at `step_id`: the run no
/// longer owes it, unless a paste was owed since (see `super::prefilled`).
pub(super) fn prefilled(state: &RunState, step_id: &str, token: i64) -> Option<RunState> {
    if state.node_id != step_id || state.prefill_token.unwrap_or(NO_TOKEN) != token {
        return None;
    }
    Some(RunState { phase: state.phase.clone().with_prefill(false), ..state.clone() })
}

/// `prompt` was sent to Claude: a run waiting at a manual prompt runs it (see
/// `super::on_prompt`). A run waiting for the prompt the app typed (`Phase::Typed`)
/// counts the next turn as the step's reply if `prompt` is that prompt (see
/// `is_step_prompt`); another prompt (one the person queued, or the prompt of an
/// earlier pick) leaves it waiting, its turn yet to end. None for any other run.
/// `super::on_prompt` looks for the phases this changes in SQL before taking the
/// write lock: a phase added here must be added there.
pub(super) fn prompt_sent(cx: &Cx, state: &RunState, prompt: &str) -> Result<Option<RunState>, String> {
    let node = cx.node(&state.node_id)?;
    if let Phase::Typed { turn_ended } = state.phase {
        return Ok(if is_step_prompt(prompt, node) {
            Some(RunState { phase: Phase::Working, ..state.clone() })
        } else {
            // A turn is to end before the step's prompt goes: not a sign it was lost.
            turn_ended.then(|| RunState { phase: Phase::Typed { turn_ended: false }, ..state.clone() })
        });
    }
    // The prompt may carry Start's base info, but never the run's rules: after a
    // clear, the next prompt step still briefs Claude again.
    Ok(awaits_prompt(state, node).then(|| state.moved(&node.id, Phase::Working, state.steps)))
}

/// Whether `prompt`, as Claude Code reported it sent, is prompt step `node`'s prompt
/// as the app typed it (see `super::render`): its text, maybe led by the run's rules
/// and Start's base info. The app pastes it whole, but Claude Code may change its
/// line ends and spacing, so only the words are compared.
fn is_step_prompt(prompt: &str, node: &Node) -> bool {
    let typed = flows::prompt_text(node);
    let (sent, typed): (Vec<_>, Vec<_>) = (prompt.split_whitespace().collect(), typed.split_whitespace().collect());
    !typed.is_empty() && sent.ends_with(&typed)
}

/// Why a run waits whose typed prompt was not seen sent (see `on_stop`).
const TYPED_NOT_SENT: &str = "This step's prompt, which misag typed when you moved the run on, was not seen being sent to Claude: \
     it may have gone along with the turn before, or been cleared from Claude's input. Press Continue to send it again, or pick an output to go on.";

/// A prompt could not be recorded, failing with `error` (see `super::prompt_unseen`).
pub(super) fn prompt_unseen(cx: &Cx, state: &RunState, error: &str) -> Result<Option<RunState>, String> {
    // The prompt misag typed was likely sent, but Claude's answer would not count.
    if let Phase::Typed { .. } = state.phase {
        let why = format!(
            "misag could not record a prompt sent to Claude ({error}). If it was this step's prompt, which misag typed when you \
             moved the run on, Claude's answer will not move the run on: once Claude has answered, press Continue to have it \
             go on with the step, or pick an output to go on."
        );
        return Ok(Some(state.waiting_here(&why, Some(Resume::Remind))));
    }
    if !matches!(state.phase, Phase::Waiting { .. }) || cx.node(&state.node_id)?.kind != Kind::ManualPrompt {
        return Ok(None);
    }
    let why = format!("misag could not record your prompt ({error}), so Claude's answer will not move the run on. Pick an output to go on.");
    Ok(Some(state.waiting_here(&why, None)))
}

/// The person has Claude go on with `step_id`, which needs attention (see
/// `super::resume`).
pub(super) fn resume(cx: &Cx, state: &RunState, step_id: &str) -> Result<Change, String> {
    at(cx.graph, state, step_id)?;
    let node = cx.node(&state.node_id)?;
    if !matches!(state.phase, Phase::NeedsPerson { .. }) || !matches!(node.kind, Kind::Prompt | Kind::Title) {
        return Err(format!("\"{}\" does not need attention; pick an output instead", node.name()));
    }
    let Some(how) = continue_offered(&state.phase, node.kind) else {
        return Err(format!("Claude going on with \"{}\" would not help; pick an output instead", node.name()));
    };
    let mut next = state.moved(&node.id, Phase::Working, state.steps);
    let say = match how {
        Resume::Remind => Say::Remind { node_id: node.id.clone() },
        // With the rules and base info, if Claude lacks them (after a clear, or an
        // unsent prompt that carried them).
        Resume::Prompt => next.prompt(node),
    };
    Ok(Change { state: next, left: None, say: Some(say) })
}

/// The prompt for `step_id` could not be typed (see `super::unsent`); None for a
/// run that has moved on or no longer runs the step.
pub(super) fn unsent(state: &RunState, step_id: &str, briefs: bool, why: &str) -> Option<RunState> {
    if state.node_id != step_id || state.phase.status() != Status::Running {
        return None;
    }
    let waiting = state.waiting(step_id, state.steps, why, Some(Resume::Prompt));
    Some(RunState { briefed: waiting.briefed && !briefs, ..waiting })
}

/// Claude cleared its conversation (see `super::clear_tab`).
pub(super) fn cleared_tab(cx: &Cx, state: &RunState, why: &str) -> Result<RunState, String> {
    let lost = RunState { briefed: false, ..state.clone() };
    let state = if lost.phase == (Phase::Clearing { done: false }) {
        RunState { phase: Phase::Clearing { done: true }, ..lost }
    } else {
        pause(cx, lost, why)?
    };
    cx.refill(state)
}

/// `state` once the Stop hook failed (see `super::pause_tab`): as `pause`, but a
/// run waiting for the prompt the app typed is left as it is, as the turn whose
/// Stop that was is the one the prompt waited behind (see `Phase::Typed`). Pausing
/// it would have Continue type the prompt again while Claude Code still holds it.
pub(super) fn stop_failed(cx: &Cx, state: RunState, why: &str) -> Result<RunState, String> {
    if let Phase::Typed { .. } = state.phase {
        return Ok(state);
    }
    pause(cx, state, why)
}

/// `state` once Claude will not reply to its running step (see `cleared_tab` and
/// `stop_failed`); unchanged unless it runs a step Claude was to reply to (see
/// `answer_lost` for a manual prompt). A run waiting for the prompt the app typed
/// may have had it sent before, so `why` must not say Claude never got it.
pub(super) fn pause(cx: &Cx, state: RunState, why: &str) -> Result<RunState, String> {
    if let Some(lost) = answer_lost(cx, &state, why)? {
        return Ok(lost);
    }
    Ok(match state.phase {
        Phase::Briefing { .. } => state.briefing_lost(why),
        Phase::Working | Phase::Typed { .. } => state.waiting_here(why, Some(Resume::Prompt)),
        _ => state,
    })
}

/// Claude's answer to the person's manual prompt the run runs will not be seen
/// (`why`: the conversation was cleared, misag restarted): the run waits for the
/// prompt again, or an output. Continue has no step prompt to remind Claude of.
/// None unless the run is working on a manual prompt.
fn answer_lost(cx: &Cx, state: &RunState, why: &str) -> Result<Option<RunState>, String> {
    if state.phase != Phase::Working || cx.node(&state.node_id)?.kind != Kind::ManualPrompt {
        return Ok(None);
    }
    let note = format!("{why} Claude's answer to your prompt will not be seen: send your prompt again, or pick an output to go on.");
    Ok(Some(state.waiting_here(&note, None)))
}

/// `state` once misag has restarted (see `super::after_restart`).
pub(super) fn restarted(cx: &Cx, state: &RunState) -> Result<RunState, String> {
    let state = match state.phase {
        // The app that would have sent the briefing after the clear is gone.
        Phase::Clearing { .. } => state.clear_lost("Mischief of Agents was restarted before the conversation was cleared. Pick an output to go on without clearing, or stop the run and press ▶ again."),
        Phase::Briefing { .. } => state.briefing_lost("Mischief of Agents was restarted while Claude read the flow's briefing. Pick an output to go on; the first step briefs Claude again."),
        Phase::Working => match answer_lost(cx, state, "Mischief of Agents was restarted while Claude answered your prompt.")? {
            Some(lost) => lost,
            // Claude comes back in its conversation, so it can be reminded.
            None => state.waiting_here(
                "Mischief of Agents was restarted while Claude worked on this step, so its reply was not seen. Pick an output to go on.",
                Some(Resume::Remind),
            ),
        },
        // A prompt Claude Code still held went with the Claude that misag stopped.
        Phase::Typed { .. } => state.waiting_here(
            "Mischief of Agents was restarted while this step's prompt waited to be sent, so Claude may not have received it. \
             Press Continue to send it again, or pick an output to go on.",
            Some(Resume::Prompt),
        ),
        _ => state.clone(),
    };
    cx.refill(state)
}

/// `state` at `now` if it has timed out at Start, for a run started at
/// `started_at` (see `super::expire`); None if it has not.
pub(super) fn timed_out(state: &RunState, started_at: i64, now: i64) -> Option<RunState> {
    match state.phase {
        Phase::Clearing { .. } if started_at <= now - CLEAR_TIMEOUT_SECS => Some(state.clear_lost(
            // The 30 seconds are the app's own timeout, `CLEAR_TIMEOUT_MS` in
            // src/runController.ts, which the person sees; `CLEAR_TIMEOUT_SECS` is
            // deliberately longer, so the app's stop comes first.
            "Claude did not clear the conversation within 30 seconds. Pick an output to go on without clearing, or stop the run and press ▶ again.",
        )),
        Phase::Briefing { sent_at } if sent_at <= now - BRIEFING_TIMEOUT_SECS => Some(state.briefing_lost(&format!(
            "Claude did not reply to the flow's briefing within {} minutes (it may have been interrupted). Pick an output to go on; the first step briefs Claude again.",
            BRIEFING_TIMEOUT_SECS / 60
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::{at_step, graph, manual_first, manual_prompt_first, needs, review_loop};
    use super::super::NOTHING_TAKEN;
    use crate::flows::tests::{edge, node, runnable};
    use std::collections::BTreeMap;

    const TOKEN: i64 = 7;

    fn cx<'a>(graph: &'a Graph, taken: &'a Taken) -> Cx<'a> {
        Cx { graph, taken, token: TOKEN }
    }

    fn reply(graph: &Graph, state: &RunState, text: Option<&str>) -> Option<Change> {
        on_stop(&cx(graph, NOTHING_TAKEN), state, text).unwrap()
    }

    fn waits(state: &RunState, node_id: &str, steps: i64, why: &str, resume: Option<Resume>) -> Change {
        Change::to(RunState { node_id: node_id.into(), steps, phase: needs(why, resume, false), ..state.clone() })
    }

    #[test]
    fn the_first_turn_briefs_claude_or_ends_a_flow_that_leads_to_its_end() {
        let g = runnable();
        let initial = RunState::initial(&g.nodes[0], false);
        let change = first_turn(&cx(&g, NOTHING_TAKEN), &initial, &g.nodes[0], 100);
        let briefing = RunState { briefed: true, phase: Phase::Briefing { sent_at: 100 }, ..initial.clone() };
        assert_eq!(change, Change { state: briefing, left: None, say: Some(Say::Briefing) });
        let straight = graph(vec![node("e", Kind::End, &[])], vec![edge("start", "next", "e")]);
        let change = first_turn(&cx(&straight, NOTHING_TAKEN), &initial, &straight.nodes[0], 100);
        let done = RunState { node_id: "e".into(), phase: Phase::Ended { stopped: false, owes_closing: false }, ..initial };
        assert_eq!(change, Change { state: done, left: Some(("start".into(), "next".into())), say: None });
    }

    #[test]
    fn a_briefing_reply_of_ready_takes_the_first_step_and_any_other_waits() {
        let g = runnable();
        let briefing = RunState { steps: 0, ..at_step("start", Phase::Briefing { sent_at: 100 }) };
        let first = RunState { node_id: "p".into(), steps: 1, phase: Phase::Working, ..briefing.clone() };
        assert_eq!(
            reply(&g, &briefing, Some("**Ready.**")),
            Some(Change { state: first, left: Some(("start".into(), "next".into())), say: Some(Say::Prompt { node_id: "p".into(), briefs: false }) })
        );
        let more = reply(&g, &briefing, Some("Ready. I also fixed the bug.")).unwrap();
        let Phase::NeedsPerson { why, .. } = &more.state.phase else { panic!("{more:?}") };
        assert_eq!(more, waits(&briefing, "start", 0, why, None));
        assert!(why.contains("more than reply \"Ready\""), "{why}");
        assert_eq!(reply(&g, &briefing, None), Some(waits(&briefing, "start", 0, NO_REPLY, None)));
        // A turn at Start that is not the briefing's reply is left alone.
        assert_eq!(reply(&g, &RunState { phase: Phase::Working, ..briefing }, Some("Ready")), None);
    }

    #[test]
    fn a_flow_line_at_a_prompt_step_moves_the_run_on_or_ends_it() {
        let g = runnable();
        let working = at_step("p", Phase::Working);
        let again = RunState { steps: 4, ..working.clone() };
        assert_eq!(
            reply(&g, &working, Some("Not yet.\n\nFLOW: fail")),
            Some(Change { state: again, left: Some(("p".into(), "fail".into())), say: Some(Say::Prompt { node_id: "p".into(), briefs: false }) })
        );
        // After a clear, the prompt carries the rules again.
        let cleared = RunState { briefed: false, ..working.clone() };
        let change = reply(&g, &cleared, Some("FLOW: fail")).unwrap();
        assert_eq!((change.state.briefed, change.say), (true, Some(Say::Prompt { node_id: "p".into(), briefs: true })));
        let done = RunState { node_id: "e".into(), phase: Phase::Ended { stopped: false, owes_closing: true }, ..working.clone() };
        assert_eq!(reply(&g, &working, Some("FLOW: pass")), Some(Change { state: done, left: Some(("p".into(), "pass".into())), say: None }));
        let unknown = reply(&g, &working, Some("FLOW: maybe")).unwrap();
        assert!(matches!(&unknown.state.phase, Phase::NeedsPerson { why, resume: Some(Resume::Remind), .. } if why.contains("not an output")));
        let no_line = reply(&g, &working, Some("Done.")).unwrap();
        assert!(matches!(&no_line.state.phase, Phase::NeedsPerson { why, resume: Some(Resume::Remind), .. } if why.contains("FLOW line")));
        assert_eq!(reply(&g, &working, None), Some(waits(&working, "p", 3, NO_REPLY, None)));
    }

    #[test]
    fn claude_asking_for_a_person_waits_with_continue() {
        let g = runnable();
        let working = at_step("p", Phase::Working);
        assert_eq!(
            reply(&g, &working, Some("FLOW: attention")),
            Some(waits(&working, "p", 3, "Claude asked for a person; see its last reply.", Some(Resume::Remind)))
        );
    }

    #[test]
    fn the_step_limit_and_a_step_s_max_runs_wait_for_the_person() {
        let g = runnable();
        let looping = RunState { steps: MAX_RUN_STEPS, ..at_step("p", Phase::Working) };
        let change = reply(&g, &looping, Some("FLOW: fail")).unwrap();
        assert_eq!(change.left, None, "the run did not leave the step");
        assert!(matches!(&change.state.phase, Phase::NeedsPerson { why, resume: None, .. } if why.contains("50 steps")));
        let g = review_loop();
        let fixing = at_step("fix", Phase::Working);
        let once = Taken(BTreeMap::from([("review".to_owned(), 1)]));
        let change = on_stop(&cx(&g, &once), &fixing, Some("FLOW: done")).unwrap().unwrap();
        assert_eq!((change.state.node_id.as_str(), change.state.phase), ("review", Phase::Working));
        let twice = Taken(BTreeMap::from([("review".to_owned(), 2)]));
        let change = on_stop(&cx(&g, &twice), &fixing, Some("FLOW: done")).unwrap().unwrap();
        assert!(matches!(&change.state.phase, Phase::NeedsPerson { why, resume: None, .. } if why.contains("already run 2 times")));
        // The person is not held back by it.
        let waiting = RunState { phase: needs("x", None, false), ..fixing };
        let change = proceed(&cx(&g, &twice), &waiting, "fix", "done").unwrap();
        assert_eq!((change.state.node_id.as_str(), change.state.steps), ("review", 1));
    }

    #[test]
    fn a_chat_turn_while_the_run_waits_changes_nothing() {
        let g = manual_first();
        let at_pause = at_step("m", Phase::Waiting { prefill: false });
        assert_eq!(reply(&g, &at_pause, Some("FLOW: ok")), None);
        let needing = at_step("p", needs("cleared", Some(Resume::Prompt), false));
        for text in [Some("Just chatting."), Some("FLOW: attention"), Some("FLOW: nope"), None] {
            assert_eq!(reply(&g, &needing, text), None, "{text:?}");
        }
        // Where Continue is offered, a valid FLOW line moves it on.
        let change = reply(&g, &needing, Some("FLOW: done")).unwrap();
        assert_eq!(change.left, Some(("p".into(), "done".into())));
        // At a step limit, it does not.
        assert_eq!(reply(&g, &at_step("p", needs("limit", None, false)), Some("FLOW: done")), None);
    }

    const TYPED: Phase = Phase::Typed { turn_ended: false };
    const TURN_ENDED: Phase = Phase::Typed { turn_ended: true };

    #[test]
    fn a_running_step_moved_on_by_hand_waits_for_its_prompt_to_be_submitted() {
        let g = runnable();
        let cx = cx(&g, NOTHING_TAKEN);
        let working = at_step("p", Phase::Working);
        let typed = proceed(&cx, &working, "p", "fail").unwrap().state;
        assert_eq!(typed.phase, TYPED);
        let ended = RunState { phase: TURN_ENDED, ..typed.clone() };
        assert_eq!(on_stop(&cx, &typed, Some("FLOW: pass")).unwrap(), Some(Change::to(ended.clone())), "the turn before the prompt");
        // Picking again while it waits types a new prompt, which waits afresh; a waiting run's pick does not.
        assert_eq!(proceed(&cx, &typed, "p", "fail").unwrap().state.phase, TYPED);
        assert_eq!(proceed(&cx, &ended, "p", "fail").unwrap().state.phase, TYPED);
        let needing = at_step("p", needs("x", None, false));
        assert_eq!(proceed(&cx, &needing, "p", "fail").unwrap().state.phase, Phase::Working);
        let prompt = flows::prompt_text(cx.node("p").unwrap());
        for state in [&typed, &ended] {
            assert_eq!(prompt_sent(&cx, state, &prompt).unwrap(), Some(RunState { phase: Phase::Working, ..typed.clone() }));
        }
    }

    #[test]
    fn only_the_step_s_own_prompt_ends_the_wait_for_it() {
        let g = review_loop();
        let cx = cx(&g, NOTHING_TAKEN);
        let typed = at_step("review", TYPED);
        let ended = at_step("review", TURN_ENDED);
        let review = flows::prompt_text(cx.node("review").unwrap());
        let working = Some(RunState { phase: Phase::Working, ..typed.clone() });
        // As Claude Code may space it, or led by the rules and base info after a clear.
        assert_eq!(prompt_sent(&cx, &typed, &format!("  {}\r\n", review.replace('\n', "\r\n"))).unwrap(), working);
        let briefed = flows::first_prompt_text(&g, "F", cx.node("review").unwrap());
        assert_eq!(prompt_sent(&cx, &typed, &briefed).unwrap(), working);
        // A prompt the person queued, or the one an earlier pick typed, has a turn of
        // its own to end first.
        let fix = flows::prompt_text(cx.node("fix").unwrap());
        let cut: String = review.chars().take(review.chars().count() / 2).collect();
        for other in ["Also check the logs.", fix.as_str(), "", cut.as_str()] {
            assert_eq!(prompt_sent(&cx, &typed, other).unwrap(), None, "{other:?}");
            assert_eq!(prompt_sent(&cx, &ended, other).unwrap(), Some(typed.clone()), "{other:?}");
        }
    }

    #[test]
    fn a_second_turn_without_the_typed_prompt_waits_for_the_person_to_send_it_again() {
        let g = runnable();
        let ended = at_step("p", TURN_ENDED);
        for text in [Some("FLOW: pass"), Some("Done."), None] {
            let change = reply(&g, &ended, text).unwrap();
            assert_eq!(change, waits(&ended, "p", 3, TYPED_NOT_SENT, Some(Resume::Prompt)), "{text:?}");
        }
        assert!(TYPED_NOT_SENT.contains("not seen being sent") && TYPED_NOT_SENT.contains("Continue"));
    }

    #[test]
    fn a_lost_reply_or_prompt_while_the_typed_prompt_waits() {
        let g = runnable();
        let cx = cx(&g, NOTHING_TAKEN);
        let typed = at_step("p", TYPED);
        // The failed Stop was the turn the prompt waited behind: no Continue to type it twice.
        assert_eq!(stop_failed(&cx, typed.clone(), "lost").unwrap(), typed);
        assert_eq!(stop_failed(&cx, at_step("p", Phase::Working), "lost").unwrap().phase, needs("lost", Some(Resume::Prompt), false));
        // A clear or a restart: the prompt may or may not have reached Claude.
        assert_eq!(pause(&cx, typed.clone(), "lost").unwrap().phase, needs("lost", Some(Resume::Prompt), false));
        let after = restarted(&cx, &typed).unwrap();
        let Phase::NeedsPerson { why, resume: Some(Resume::Prompt), .. } = &after.phase else { panic!("{after:?}") };
        assert!(why.contains("may not have received") && why.contains("Press Continue to send it again"), "{why}");
        // A prompt the hook could not record may have been the one misag typed.
        let unseen = prompt_unseen(&cx, &typed, "boom").unwrap().unwrap();
        let Phase::NeedsPerson { why, resume: Some(Resume::Remind), .. } = &unseen.phase else { panic!("{unseen:?}") };
        assert!(why.contains("(boom)") && why.contains("which misag typed") && !why.contains("your prompt"), "{why}");
    }

    #[test]
    fn a_pause_makes_a_running_step_wait_and_leaves_the_rest() {
        let g = manual_prompt_first("Fix it");
        let cx = cx(&g, NOTHING_TAKEN);
        let working = at_step("p", Phase::Working);
        assert_eq!(pause(&cx, working.clone(), "lost").unwrap(), RunState { phase: needs("lost", Some(Resume::Prompt), false), ..working });
        let briefing = at_step("start", Phase::Briefing { sent_at: 1 });
        assert_eq!(
            pause(&cx, briefing.clone(), "lost").unwrap(),
            RunState { briefed: false, phase: needs("lost", None, false), ..briefing }
        );
        let answering = at_step("mp", Phase::Working);
        let lost = pause(&cx, answering, "Cleared.").unwrap();
        assert!(matches!(&lost.phase, Phase::NeedsPerson { why, resume: None, .. } if why.contains("send your prompt again")));
        for left_alone in [at_step("mp", Phase::Waiting { prefill: true }), at_step("start", Phase::Clearing { done: true })] {
            assert_eq!(pause(&cx, left_alone.clone(), "lost").unwrap(), left_alone);
        }
    }

    #[test]
    fn a_clear_during_a_step_makes_it_wait_and_owes_the_rules_again() {
        let g = manual_prompt_first("Fix it");
        let cx = cx(&g, NOTHING_TAKEN);
        let working = at_step("p", Phase::Working);
        assert_eq!(
            cleared_tab(&cx, &working, "Cleared.").unwrap(),
            RunState { briefed: false, phase: needs("Cleared.", Some(Resume::Prompt), false), ..working }
        );
        let clearing = at_step("start", Phase::Clearing { done: false });
        assert_eq!(
            cleared_tab(&cx, &clearing, "Cleared.").unwrap(),
            RunState { briefed: false, phase: Phase::Clearing { done: true }, ..clearing }
        );
        // A manual prompt owes its paste again, now with the base info, under the new token.
        let waiting = at_step("mp", Phase::Waiting { prefill: false });
        assert_eq!(
            cleared_tab(&cx, &waiting, "Cleared.").unwrap(),
            RunState { briefed: false, prefill_token: Some(TOKEN), phase: Phase::Waiting { prefill: true }, ..waiting }
        );
    }

    #[test]
    fn a_restart_during_a_step_waits_with_what_continue_can_still_do() {
        let g = manual_prompt_first("Fix it");
        let cx = cx(&g, NOTHING_TAKEN);
        let restarted = |state: RunState| restarted(&cx, &state).unwrap();
        let working = restarted(at_step("p", Phase::Working));
        assert!(matches!(&working.phase, Phase::NeedsPerson { why, resume: Some(Resume::Remind), .. } if why.contains("worked on this step")));
        let answering = restarted(at_step("mp", Phase::Working));
        assert!(matches!(&answering.phase, Phase::NeedsPerson { resume: None, prefill: true, .. }), "{answering:?}");
        assert_eq!(answering.prefill_token, Some(TOKEN));
        let clearing = restarted(at_step("start", Phase::Clearing { done: false }));
        assert!(matches!(&clearing.phase, Phase::NeedsPerson { why, resume: None, .. } if why.contains("before the conversation was cleared")));
        let briefing = restarted(at_step("start", Phase::Briefing { sent_at: 1 }));
        assert!(!briefing.briefed && matches!(&briefing.phase, Phase::NeedsPerson { resume: None, .. }));
        let waiting = at_step("p", needs("x", Some(Resume::Remind), false));
        assert_eq!(restarted(waiting.clone()), waiting);
    }

    #[test]
    fn a_clear_or_a_briefing_times_out_at_start() {
        let clearing = at_step("start", Phase::Clearing { done: true });
        assert_eq!(timed_out(&clearing, 1000, 1000 + CLEAR_TIMEOUT_SECS - 1), None);
        let expired = timed_out(&clearing, 1000, 1000 + CLEAR_TIMEOUT_SECS).unwrap();
        assert!(matches!(&expired.phase, Phase::NeedsPerson { why, resume: None, .. } if why.contains("30 seconds")));
        // A briefing counts from when it was sent, not from the start.
        let briefing = at_step("start", Phase::Briefing { sent_at: 5000 });
        assert_eq!(timed_out(&briefing, 0, 5000 + BRIEFING_TIMEOUT_SECS - 1), None);
        let expired = timed_out(&briefing, 0, 5000 + BRIEFING_TIMEOUT_SECS).unwrap();
        assert!(!expired.briefed && matches!(&expired.phase, Phase::NeedsPerson { why, .. } if why.contains("2 minutes")));
        assert_eq!(timed_out(&at_step("p", Phase::Working), 0, i64::MAX / 2), None);
    }
}
