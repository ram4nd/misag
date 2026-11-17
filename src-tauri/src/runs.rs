//! Flow runs: a flow being worked through in a tab, one at a time per tab.
//!
//! A run starts at the flow's Start step and moves along outputs. It first briefs
//! Claude (the run's rules and Start's base info) and waits at Start for Claude's
//! reply, "Ready", which takes the first step (see `on_stop`); a flow that leads
//! straight to its end skips that. A briefing with no reply within
//! `BRIEFING_TIMEOUT_SECS` (Claude was interrupted, or crashed) waits for the
//! person (see `active`). A prompt step sends its text to Claude; when Claude ends its turn, the Stop hook
//! (`misag --claude-hook`, a separate process) reads the `FLOW: <output>` line
//! Claude ended with and moves on, handing Claude the next prompt as hook context.
//! A pause waits for the person, who goes on by its one output (▶ in the Flows
//! box) or stops the run. A manual prompt has the app put the step's draft (led by
//! Start's base info after a clear) into Claude's input without sending it
//! (`prefill_text`, `prefilled`); the person's own prompt, reported by the
//! UserPromptSubmit hook (`on_prompt`), has the step run, and Claude's answer
//! moves it on by its one output, with no `FLOW:` line. ▶ skips it. A reply the
//! runner cannot follow waits too: the person picks an output in the editor's run
//! bar (and can do so for a running step too, if Claude was interrupted) or has
//! Claude go on with the step. A pick or Continue names the step it was made for,
//! so one that arrives after the run moved on is refused. An output with no next
//! step, or an End step, ends the run; that, or stopping it, has Claude told the
//! flow is finished with the person's next prompt (see `owe_closing`).
//!
//! A flow whose Start clears begins in the tab's new conversation: its run holds
//! the tab at Start while the app types `/clear`, the SessionStart hook reports the
//! clear (`clear_tab`), and the app then sends the briefing (`cleared`). A run
//! still at Start `CLEAR_TIMEOUT_SECS` after it began (the page that would go on
//! was reloaded, or failed) waits for the person instead (see `active`).
//!
//! Any turn that ends while a step runs counts as its reply, including one the
//! person started by chatting with Claude.
//!
//! The app (through `Db`) and the hook (on its own connection) both call these
//! functions on the same database.

use crate::flows::{self, Graph, Kind, Node};
use crate::layout::TabId;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::HashSet;
use std::time::SystemTime;

/// A run that has taken this many steps waits for the person, in case it loops.
pub const MAX_RUN_STEPS: i64 = 50;

/// How long a run whose Start clears may stay at Start before it waits for the
/// person: the app's own 30 seconds (`CLEAR_TIMEOUT_MS` in src/main.ts, after which
/// it stops the run, saying why), and a little more, so its stop comes first.
pub const CLEAR_TIMEOUT_SECS: i64 = 35;

/// How long a run waits at Start for Claude's reply to its briefing, which asks
/// only for "Ready", before it waits for the person: Claude was interrupted (which
/// runs no Stop hook) or failed.
pub const BRIEFING_TIMEOUT_SECS: i64 = 120;

/// Its active statuses are mirrored by `RunStatus` in src/flows.ts.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Claude is working on a prompt step.
    Running,
    /// Waiting for the person: at a pause, to go on (▶, by its one output) or stop
    /// the run; at a manual prompt, also to send their prompt, which makes the step
    /// `Running` until Claude answers.
    Manual,
    /// The run needs a person (its note says why): Claude's reply could not be
    /// followed, it asked for one, or the run hit `MAX_RUN_STEPS`. The person picks
    /// an output or presses Continue (see `Resume`); where Continue is offered,
    /// Claude's next reply with a valid `FLOW:` line also moves the run on.
    Attention,
    Done,
    Stopped,
}

/// An active run, for the sidebar. Mirrored by `Run` in src/flows.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: i64,
    pub flow_id: i64,
    pub flow_name: String,
    pub tab_id: String,
    /// The step the run is at, and its display name.
    pub step_id: String,
    pub step: String,
    pub status: Status,
    /// Why the run waits, when it needs attention.
    pub note: Option<String>,
    /// The current step's outputs, for the person to pick from.
    pub outputs: Vec<String>,
    pub steps: i64,
    /// How many of the flow's steps (all but Start and End) the run has finished,
    /// each counted once however often it ran, and how many there are.
    pub done: i64,
    pub total: i64,
    /// Whether Claude can be asked to go on with the step (see `resume`).
    pub resumable: bool,
    /// Whether the app has yet to fill Claude's input for the manual prompt the run
    /// waits at (see `prefill_text`).
    pub prefill: bool,
    /// The kind of the step the run is at; None for a run that cannot be read.
    pub step_kind: Option<Kind>,
    /// Whether the run waits at Start for Claude's reply to its briefing.
    pub briefing: bool,
}

/// How Continue has Claude go on with a step that needs attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resume {
    /// Claude still has the step in mind (it asked for a person, or its reply could
    /// not be followed): remind it to go on and how to end.
    Remind,
    /// Claude no longer has the step in mind (the conversation was cleared): send
    /// the step's prompt again.
    Prompt,
}

impl Resume {
    fn as_str(self) -> &'static str {
        match self {
            Resume::Remind => "remind",
            Resume::Prompt => "prompt",
        }
    }

    /// Like `Status::parse`, a value this version does not know is an error, not
    /// "none", so a run from a newer version is not driven on a guess.
    fn parse(s: Option<String>) -> Result<Option<Self>, String> {
        match s.as_deref() {
            None => Ok(None),
            Some("remind") => Ok(Some(Resume::Remind)),
            Some("prompt") => Ok(Some(Resume::Prompt)),
            Some(other) => Err(format!("a run has the resume state {other:?}, which this version does not know")),
        }
    }
}

/// Where a run whose Start clears is with the clear, while it is at Start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clear {
    /// `/clear` is being typed; Claude has not reported it yet.
    Pending,
    /// Claude has cleared; the app takes the first step.
    Done,
}

impl Clear {
    /// See `Resume::parse`.
    fn parse(s: Option<String>) -> Result<Option<Self>, String> {
        match s.as_deref() {
            None => Ok(None),
            Some("pending") => Ok(Some(Clear::Pending)),
            Some("done") => Ok(Some(Clear::Done)),
            Some(other) => Err(format!("a run has the clear state {other:?}, which this version does not know")),
        }
    }
}

/// A run just started. Mirrored by `Started` in src/main.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Started {
    pub run_id: i64,
    /// Type `/clear`, then wait for `cleared` to give the briefing.
    pub clear: bool,
    /// What to send Claude, if the run does not clear: its briefing, or nothing for
    /// a flow that leads straight to its end.
    pub prompt: Option<String>,
}

/// How a run whose Start clears is getting on. Mirrored in src/main.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Cleared {
    /// Claude has not reported the clear yet.
    Pending,
    /// Claude has cleared and the run briefs it; send `prompt`, if any (none for a
    /// flow that leads straight to its end).
    Ready { prompt: Option<String> },
    /// The run is not waiting for a clear (anymore): stopped, or moved on otherwise.
    Gone,
}

/// Statuses in which a run holds its tab, as SQL. The `one_active_run_per_tab`
/// index in migration 7 lists the same ones; a test checks they agree.
pub(crate) const ACTIVE: &str = "('running', 'manual', 'attention')";

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Manual => "manual",
            Status::Attention => "attention",
            Status::Done => "done",
            Status::Stopped => "stopped",
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "running" => Status::Running,
            "manual" => Status::Manual,
            "attention" => Status::Attention,
            "done" => Status::Done,
            "stopped" => Status::Stopped,
            other => return Err(format!("a run has the status {other:?}, which this version does not know")),
        })
    }
}

fn sql(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

fn now() -> i64 {
    i64::try_from(crate::db::unix_secs(SystemTime::now())).unwrap_or(i64::MAX)
}

fn parse_graph(json: &str) -> Result<Graph, String> {
    serde_json::from_str(json).map_err(|e| format!("the run's steps cannot be read ({e})"))
}

/// An active run as stored.
struct Stored {
    id: i64,
    graph: Graph,
    node_id: String,
    status: Status,
    steps: i64,
    resume: Option<Resume>,
    clear: Option<Clear>,
    /// Whether Claude has the run's rules and Start's base info (see `first_turn`):
    /// false until the briefing is sent, and after a clear.
    briefed: bool,
    /// Whether the app has yet to fill Claude's input for the manual prompt.
    prefill: bool,
    /// Whether the run waits at Start for Claude's reply to its briefing.
    briefing: bool,
}

impl Stored {
    /// Fails unless the run is at `step_id`, the step the person saw it at.
    fn at(&self, step_id: &str) -> Result<(), String> {
        if self.node_id == step_id {
            return Ok(());
        }
        let name = |id: &str| self.graph.nodes.iter().find(|n| n.id == id).map_or(id.to_owned(), |n| n.name().to_owned());
        Err(format!("the run has moved on from \"{}\" to \"{}\"; pick again for where it is now", name(step_id), name(&self.node_id)))
    }

    fn node(&self) -> Result<&Node, String> {
        let id = &self.node_id;
        self.graph.nodes.iter().find(|n| &n.id == id).ok_or_else(|| format!("the run is at a missing step {id:?}"))
    }
}

/// The active run matching `filter` (`tab_id = ?1` or `id = ?1`).
fn active_run(conn: &Connection, filter: &str, key: &dyn rusqlite::ToSql) -> Result<Option<Stored>, String> {
    let row = conn
        .query_row(
            &format!("SELECT id, graph, node_id, status, steps, resume, clear, briefed, prefill, briefing FROM flow_runs WHERE {filter} AND status IN {ACTIVE}"),
            [key],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(sql)?;
    row.map(|(id, graph, node_id, status, steps, resume, clear, briefed, prefill, briefing)| {
        Ok(Stored {
            id,
            graph: parse_graph(&graph)?,
            node_id,
            status: Status::parse(&status)?,
            steps,
            resume: Resume::parse(resume)?,
            clear: Clear::parse(clear)?,
            briefed,
            prefill,
            briefing,
        })
    })
    .transpose()
}

fn set(tx: &Transaction, run: i64, node_id: &str, status: Status, steps: i64, note: Option<&str>) -> Result<(), String> {
    tx.execute(
        "UPDATE flow_runs SET node_id = ?2, status = ?3, steps = ?4, note = ?5, resume = NULL, clear = NULL, prefill = 0, briefing = 0, updated_at = ?6 WHERE id = ?1",
        params![run, node_id, status.as_str(), steps, note, now()],
    )
    .map(drop)
    .map_err(sql)
}

/// Makes the run wait at `node_id` for the person, saying why, and how Continue
/// goes on from there (None: it cannot help, and the person picks an output).
fn wait_for_person(tx: &Transaction, run: i64, node_id: &str, steps: i64, why: &str, resume: Option<Resume>) -> Result<Option<String>, String> {
    set(tx, run, node_id, Status::Attention, steps, Some(why))?;
    tx.execute("UPDATE flow_runs SET resume = ?2 WHERE id = ?1", params![run, resume.map(Resume::as_str)])
        .map_err(sql)?;
    Ok(None)
}

/// Records that the run left `node_id` by `output`.
fn left(tx: &Transaction, run: i64, node_id: &str, output: &str) -> Result<(), String> {
    tx.execute(
        "INSERT INTO flow_run_events (run_id, node_id, output, at) VALUES (?1, ?2, ?3, ?4)",
        params![run, node_id, output, now()],
    )
    .map(drop)
    .map_err(sql)
}

/// Who moves a run on.
#[derive(Clone, Copy)]
enum By {
    /// The flow itself: Claude's reply, or the start.
    Flow,
    /// The person, who is not held back by a step's `max_runs`. (Their picks also
    /// restart the count toward `MAX_RUN_STEPS`, through the `steps` they pass.)
    Person,
}

/// How often the run has taken `step` so far, counting a visit it is on now.
fn runs_of(tx: &Transaction, run: &Stored, step: &str) -> Result<i64, String> {
    let earlier: i64 = tx
        .query_row("SELECT COUNT(*) FROM flow_run_events WHERE run_id = ?1 AND node_id = ?2", params![run.id, step], |r| r.get(0))
        .map_err(sql)?;
    Ok(earlier + i64::from(run.node_id == step))
}

/// Whether an ending run owes Claude the note that its flow is finished: Claude has
/// the run's rules (`briefed`; not after a clear, or while the run clears at its start).
const OWES_CLOSING: &str = "briefed = 1 AND clear IS NULL";

/// Has run `run_id`, ending, tell Claude the flow is finished with the person's
/// next prompt (see `on_prompt`), if it owes that (`OWES_CLOSING`); call it before
/// `set`, which drops the run's clear. Nothing is typed and no turn is spent:
/// stopping must not send into a turn or a permission prompt.
fn owe_closing(conn: &Connection, run_id: i64) -> Result<(), String> {
    conn.execute(&format!("UPDATE flow_runs SET closing = 1 WHERE id = ?1 AND {OWES_CLOSING}"), [run_id]).map(drop).map_err(sql)
}

/// Drops the note owed to Claude in tab `tab_id` that a flow is finished.
fn drop_closing(conn: &Connection, tab_id: &TabId) -> Result<(), String> {
    conn.execute("UPDATE flow_runs SET closing = 0 WHERE tab_id = ?1 AND closing = 1", [tab_id.as_str()]).map(drop).map_err(sql)
}

/// The name of run `run_id`'s flow, for Claude.
fn flow_name(conn: &Connection, run_id: i64) -> Result<String, String> {
    conn.query_row("SELECT f.name FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.id = ?1", [run_id], |r| r.get(0))
        .map_err(sql)
}

/// The prompt of `node` for the run, led by the run's rules and Start's base info
/// if Claude lacks them (after a clear), which it then has.
fn prompt_for(tx: &Transaction, run: &Stored, node: &Node) -> Result<String, String> {
    if run.briefed {
        return Ok(flows::prompt_text(node));
    }
    tx.execute("UPDATE flow_runs SET briefed = 1 WHERE id = ?1", [run.id]).map_err(sql)?;
    Ok(flows::first_prompt_text(&run.graph, &flow_name(tx, run.id)?, node))
}

/// The run's first turn, from Start: its briefing (the run's rules and Start's base
/// info), the run then waiting at Start for Claude's reply (see `on_stop`); or, for a
/// flow that leads straight to its end, nothing, the run being done. Returns what to
/// send Claude.
fn first_turn(tx: &Transaction, run_id: i64, graph: &Graph, start: &Node) -> Result<Option<String>, String> {
    let next = flows::next_step(graph, &start.id, flows::START_OUTPUT);
    if next.is_none_or(|n| n.kind == Kind::End) {
        let run = Stored {
            id: run_id,
            graph: graph.clone(),
            node_id: start.id.clone(),
            status: Status::Running,
            steps: 0,
            resume: None,
            clear: None,
            briefed: false,
            prefill: false,
            briefing: false,
        };
        return go(tx, &run, start, flows::START_OUTPUT, 0, By::Flow);
    }
    set(tx, run_id, &start.id, Status::Running, 0, None)?;
    tx.execute("UPDATE flow_runs SET briefed = 1, briefing = 1 WHERE id = ?1", [run_id]).map_err(sql)?;
    Ok(Some(flows::briefing_text(graph, &flow_name(tx, run_id)?)))
}

/// Moves the run from `from` along `output` to the step it leads to, if any, with
/// the run's step count at `steps`; returns the prompt to send when that is a
/// prompt step (a run that ends sends nothing; see `owe_closing`). The run instead waits at `from` for the person when going on
/// would pass `MAX_RUN_STEPS` (into a prompt step) or, for `By::Flow` only, when
/// the next step has already run its `max_runs`.
fn go(tx: &Transaction, run: &Stored, from: &Node, output: &str, steps: i64, by: By) -> Result<Option<String>, String> {
    let next = flows::next_step(&run.graph, &from.id, output);
    // Going on with `from` would only redo it and stop here again.
    let wait = |why: String| wait_for_person(tx, run.id, &from.id, steps, &why, None);
    if next.is_some_and(|n| n.kind == Kind::Prompt) && steps >= MAX_RUN_STEPS {
        return wait(format!("The run has taken {MAX_RUN_STEPS} steps without you; pick an output to go on."));
    }
    if let (Some(next), By::Flow) = (next, by) {
        if let Some(max) = next.max_runs {
            if runs_of(tx, run, &next.id)? >= i64::from(max) {
                let name = next.name();
                let times = if max == 1 { "once".to_string() } else { format!("{max} times") };
                return wait(format!(
                    "\"{name}\" has already run {times}, its limit. Pick \"{output}\" to run it once more, or another output."
                ));
            }
        }
    }
    left(tx, run.id, &from.id, output)?;
    // Done: Claude, if it has the run's rules, is told it no longer needs to end
    // its replies with a FLOW line, with the person's next prompt.
    let finish = |at: &str| -> Result<(), String> {
        owe_closing(tx, run.id)?;
        set(tx, run.id, at, Status::Done, steps, None)
    };
    let Some(next) = next else {
        // The output leads nowhere: the flow is done.
        return finish(&from.id).map(|()| None);
    };
    match next.kind {
        Kind::Prompt => {
            set(tx, run.id, &next.id, Status::Running, steps + 1, None)?;
            prompt_for(tx, run, next).map(Some)
        }
        Kind::Pause => set(tx, run.id, &next.id, Status::Manual, steps, None).map(|()| None),
        Kind::ManualPrompt => {
            set(tx, run.id, &next.id, Status::Manual, steps, None)?;
            owe_prefill(tx, run.id, &run.graph, next, run.briefed)?;
            Ok(None)
        }
        Kind::End | Kind::Start => finish(&next.id).map(|()| None),
    }
}

/// Starts flow `flow_id` in tab `tab_id`: with the briefing to send (see
/// `first_turn`), or, if its Start clears, at Start until Claude has cleared (see
/// the module docs).
pub fn start(conn: &mut Connection, flow_id: i64, tab_id: &TabId) -> Result<Started, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let (graph, unconfirmed): (String, bool) = tx
        .query_row("SELECT graph, unconfirmed FROM flows WHERE id = ?1", [flow_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(sql)?
        .ok_or_else(|| format!("there is no flow {flow_id}"))?;
    if unconfirmed {
        return Err("a program in a tab added this flow; confirm it in the app before it runs".into());
    }
    let graph = parse_graph(&graph)?;
    let problems = flows::problems(&graph);
    if !problems.is_empty() {
        return Err(format!("the flow is not ready to run: {}", problems.join(" ")));
    }
    let start = graph.nodes.iter().find(|n| n.kind == Kind::Start).ok_or("the flow has no Start step")?.clone();
    // The new run's briefing supersedes an earlier run's note that it is finished.
    drop_closing(&tx, tab_id)?;
    let now = now();
    let clear = flows::clears(&graph);
    let inserted = tx.execute(
        "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, steps, clear, briefed, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'running', 0, ?5, 0, ?6, ?6)",
        params![
            flow_id,
            tab_id.as_str(),
            serde_json::to_string(&graph).map_err(|e| e.to_string())?,
            start.id,
            clear.then_some("pending"),
            now
        ],
    );
    match inserted {
        // The flow exists and the values are checked, so the constraint is the one-run index.
        Err(e) if e.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) => {
            return Err("this tab is already running a flow; stop that run first".into());
        }
        other => other.map_err(sql)?,
    };
    let run_id = tx.last_insert_rowid();
    if clear {
        tx.commit().map_err(sql)?;
        return Ok(Started { run_id, clear, prompt: None });
    }
    let prompt = first_turn(&tx, run_id, &graph, &start)?;
    tx.commit().map_err(sql)?;
    Ok(Started { run_id, clear, prompt })
}

/// How run `run_id`, which clears at its start, is getting on; once Claude has
/// cleared, gives the briefing (once: later calls are `Gone`).
pub fn cleared(conn: &mut Connection, run_id: i64) -> Result<Cleared, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "id = ?1", &run_id)? else { return Ok(Cleared::Gone) };
    match run.clear {
        None => Ok(Cleared::Gone),
        Some(Clear::Pending) => Ok(Cleared::Pending),
        Some(Clear::Done) => {
            let prompt = first_turn(&tx, run.id, &run.graph, run.node()?)?;
            tx.commit().map_err(sql)?;
            Ok(Cleared::Ready { prompt })
        }
    }
}

/// Why a run waits when Claude Code does not report Claude's replies.
const NO_REPLY: &str = "Claude Code did not report Claude's reply to Mischief of Agents; update Claude Code to run flows.";

/// Claude in tab `tab_id` ended its turn with `reply` (None when Claude Code did
/// not report it): moves the tab's run on. Returns the next prompt to send, if
/// any. A tab with no run, or whose run waits for the person, is left alone. A
/// manual prompt the person sent goes on by its one output, whatever the reply.
pub fn on_stop(conn: &mut Connection, tab_id: &TabId, reply: Option<&str>) -> Result<Option<String>, String> {
    // Every turn of every tab ends here: most tabs run no flow, so look before
    // taking the write lock.
    let listening = conn
        .query_row(
            "SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND status IN ('running', 'attention')",
            [tab_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql)?;
    if listening.is_none() {
        return Ok(None);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? else { return Ok(None) };
    // A turn that ends before the clear (one Claude was in when ▶ was pressed) is
    // not the reply to any step.
    if run.clear.is_some() {
        return Ok(None);
    }
    let node = run.node()?.clone();
    if node.kind == Kind::Start {
        // Only Claude's reply to the run's briefing ends it (a turn after a clear or a
        // restart is not one): "Ready" takes the first step. A reply that did more,
        // as Claude may start on the job, waits for the person to look first.
        if run.status != Status::Running || !run.briefing {
            return Ok(None);
        }
        let prompt = match reply {
            None => wait_for_person(&tx, run.id, &node.id, 0, NO_REPLY, None)?,
            Some(reply) if !flows::says_ready(reply) => wait_for_person(
                &tx,
                run.id,
                &node.id,
                0,
                "Claude did more than reply \"Ready\" to the flow's briefing. Read its reply, then pick an output to go on, or stop the run.",
                None,
            )?,
            Some(_) => go(&tx, &run, &node, flows::START_OUTPUT, 0, By::Flow)?,
        };
        tx.commit().map_err(sql)?;
        return Ok(prompt);
    }
    if node.kind == Kind::ManualPrompt {
        // Running only once the person sent their prompt (see `on_prompt`): this is
        // Claude's answer to it. Waiting for the person, any turn is left alone.
        let Some(output) = node.outputs.first().filter(|_| run.status == Status::Running) else {
            return Ok(None);
        };
        // The person stepped in, which restarts the count toward MAX_RUN_STEPS.
        let prompt = go(&tx, &run, &node, output, 0, By::Flow)?;
        tx.commit().map_err(sql)?;
        return Ok(prompt);
    }
    let wait = |why: String| wait_for_person(&tx, run.id, &node.id, run.steps, &why, Some(Resume::Remind));
    let Some(reply) = reply else {
        // Its next reply would not be reported either.
        let prompt = wait_for_person(&tx, run.id, &node.id, run.steps, NO_REPLY, None)?;
        tx.commit().map_err(sql)?;
        return Ok(prompt);
    };
    let prompt = match flows::parse_marker(reply) {
        None => wait("Claude's reply did not end with a FLOW line, so the flow cannot tell how to go on.".into()),
        Some(m) if m == flows::ATTENTION => wait("Claude asked for a person; see its last reply.".into()),
        Some(m) if !node.outputs.contains(&m) => wait(format!(
            "Claude ended with \"FLOW: {m}\", which is not an output of \"{}\".",
            node.name()
        )),
        Some(m) => go(&tx, &run, &node, &m, run.steps, By::Flow),
    }?;
    tx.commit().map_err(sql)?;
    Ok(prompt)
}

/// The person picked `output` for `step_id`, the step they saw the run at; returns
/// the next prompt to send, if any. Usually the run waits (manual or needs
/// attention), but a running step can be moved on too, for when Claude was
/// interrupted and no reply will come. Refused if the run has moved on from
/// `step_id` (a second click, or a pick from a page not yet redrawn), and while the
/// run is still clearing the conversation before its first step. The person
/// stepping in restarts the count toward `MAX_RUN_STEPS`.
pub fn proceed(conn: &mut Connection, run_id: i64, step_id: &str, output: &str) -> Result<Option<String>, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let run = active_run(&tx, "id = ?1", &run_id)?.ok_or_else(|| format!("there is no active run {run_id}"))?;
    run.at(step_id)?;
    if run.clear.is_some() {
        return Err("the run is clearing the conversation before its first step; wait for it, or stop the run".into());
    }
    if run.briefing {
        // The first step would land behind the briefing, whose reply would be taken for the step's.
        return Err("Claude is reading the flow's briefing; wait for its reply, or stop the run".into());
    }
    let node = run.node()?.clone();
    if !node.outputs.iter().any(|o| o == output) {
        return Err(format!("\"{}\" has no output {output:?}", node.name()));
    }
    let prompt = go(&tx, &run, &node, output, 0, By::Person)?;
    tx.commit().map_err(sql)?;
    Ok(prompt)
}

/// Has the app put the manual prompt `node`'s text into Claude's input (see
/// `prefill_text`), if there is any: Start's base info unless Claude has it
/// (`briefed`), then the step's draft. Each time with a new token, so the record of
/// an earlier paste cannot cancel it (see `prefilled`).
fn owe_prefill(conn: &Connection, run_id: i64, graph: &Graph, node: &Node, briefed: bool) -> Result<(), String> {
    if flows::prefill_text(graph, node, !briefed).is_empty() {
        return Ok(());
    }
    // Kept within what JavaScript numbers hold exactly.
    conn.execute(
        "UPDATE flow_runs SET prefill = 1, prefill_token = abs(random() % 9007199254740991) WHERE id = ?1",
        [run_id],
    )
    .map(drop)
    .map_err(sql)
}

/// Whether the run waits for the person at a manual prompt: for their prompt, or,
/// once Claude's answer to it was lost, for it again or an output.
fn awaits_prompt(run: &Stored, node: &Node) -> bool {
    node.kind == Kind::ManualPrompt && matches!(run.status, Status::Manual | Status::Attention)
}

/// What the app puts into Claude's input for a manual prompt (see `prefill_text`).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Prefill {
    pub text: String,
    /// Names this paste to `prefilled`.
    pub token: i64,
}

/// The text to put into Claude's input for the manual prompt run `run_id` waits at,
/// `step_id`: Start's base info if Claude lacks it, then the step's draft. None if
/// the app owes no paste there (anymore): the run moved on or was stopped, or the
/// text was pasted. Reading it changes nothing; `prefilled` records the paste, so
/// one that fails is tried again.
pub fn prefill_text(conn: &mut Connection, run_id: i64, step_id: &str) -> Result<Option<Prefill>, String> {
    let Some(run) = active_run(conn, "id = ?1", &run_id)? else { return Ok(None) };
    if run.node_id != step_id || !run.prefill || !awaits_prompt(&run, run.node()?) {
        return Ok(None);
    }
    let token: Option<i64> = conn
        .query_row("SELECT prefill_token FROM flow_runs WHERE id = ?1", [run.id], |r| r.get(0))
        .map_err(sql)?;
    let text = flows::prefill_text(&run.graph, run.node()?, !run.briefed);
    Ok(Some(Prefill { text, token: token.unwrap_or(0) }).filter(|p| !p.text.is_empty()))
}

/// The app put the manual prompt's text named by `token` into Claude's input (see
/// `prefill_text`). A paste owed since (after a clear, say) stays owed. Claude has
/// the base info only once the person sends it (see `on_prompt`).
pub fn prefilled(conn: &mut Connection, run_id: i64, step_id: &str, token: i64) -> Result<(), String> {
    conn.execute(
        "UPDATE flow_runs SET prefill = 0, updated_at = ?3 WHERE id = ?1 AND node_id = ?2 AND prefill_token = ?4",
        params![run_id, step_id, now(), token],
    )
    .map(drop)
    .map_err(sql)
}

/// The person sent `prompt` to Claude in tab `tab_id` (the UserPromptSubmit hook):
/// a run waiting at a manual prompt runs it, so Claude's answer moves it on (see
/// `on_stop`). Claude has Start's base info if the prompt carried it. A slash
/// command is not a prompt to Claude, and prompts at any other step change nothing.
/// Returns what to add to Claude's context with the prompt: that a flow which ran
/// in the tab is finished, owed since it ended (see `owe_closing`).
pub fn on_prompt(conn: &mut Connection, tab_id: &TabId, prompt: &str) -> Result<Option<String>, String> {
    if prompt.trim_start().starts_with('/') {
        return Ok(None);
    }
    // Every prompt in every tab comes here: look before taking the write lock.
    let waiting = conn
        .query_row(
            "SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND status IN ('manual', 'attention')
             UNION ALL SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND closing = 1 LIMIT 1",
            [tab_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql)?;
    if waiting.is_none() {
        return Ok(None);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    // A finished run's note goes along with this prompt, once (a tab owes at most
    // one, as a new run drops it).
    let finished: Option<i64> = tx
        .query_row("UPDATE flow_runs SET closing = 0 WHERE tab_id = ?1 AND closing = 1 RETURNING id", [tab_id.as_str()], |r| r.get(0))
        .optional()
        .map_err(sql)?;
    let closing = finished.map(|run| flow_name(&tx, run)).transpose()?.map(|name| flows::closing_text(&name));
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? {
        let node = run.node()?;
        if awaits_prompt(&run, node) {
            // The prompt may carry Start's base info, but never the run's rules: after a
            // clear, the next prompt step still briefs Claude again.
            set(&tx, run.id, &node.id, Status::Running, run.steps, None)?;
        }
    }
    tx.commit().map_err(sql)?;
    Ok(closing)
}

/// The UserPromptSubmit hook could not record the person's prompt in tab `tab_id`:
/// a run waiting at a manual prompt would never see Claude's answer, so it waits
/// for the person, saying `why`, rather than for a prompt already sent. Its paste,
/// if still owed, stays owed.
pub fn prompt_unseen(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let Some(run) = active_run(conn, "tab_id = ?1", &tab_id.as_str())? else { return Ok(()) };
    if run.status != Status::Manual || run.node()?.kind != Kind::ManualPrompt {
        return Ok(());
    }
    conn.execute(
        "UPDATE flow_runs SET status = 'attention', note = ?2, resume = NULL, updated_at = ?3 WHERE id = ?1 AND status = 'manual'",
        params![run.id, why, now()],
    )
    .map(drop)
    .map_err(sql)
}

/// Claude's answer to the person's manual prompt in tab `tab_id` will not be seen
/// (`why`: the conversation was cleared, misag restarted): the run waits for the
/// prompt again, or an output. Continue has no step prompt to remind Claude of.
fn answer_lost(conn: &Connection, tab_id: &str, why: &str) -> Result<(), String> {
    let Some(run) = active_run(conn, "tab_id = ?1", &tab_id)? else { return Ok(()) };
    if run.status != Status::Running || run.clear.is_some() || run.node()?.kind != Kind::ManualPrompt {
        return Ok(());
    }
    let note = format!("{why} Claude's answer to your prompt will not be seen: send your prompt again, or pick an output to go on.");
    conn.execute(
        "UPDATE flow_runs SET status = 'attention', note = ?2, resume = NULL, updated_at = ?3 WHERE id = ?1",
        params![run.id, note, now()],
    )
    .map(drop)
    .map_err(sql)
}

/// Has the app fill the input of the manual prompt the tab's run waits at again,
/// as Claude's input was lost (a clear, a restart): with the base info, if Claude
/// lacks it now.
fn refill(conn: &Connection, tab_id: &str) -> Result<(), String> {
    let Some(run) = active_run(conn, "tab_id = ?1", &tab_id)? else { return Ok(()) };
    let node = run.node()?;
    if awaits_prompt(&run, node) {
        owe_prefill(conn, run.id, &run.graph, node, run.briefed)?;
    }
    Ok(())
}

/// The person has seen to a prompt step that needs attention (answered Claude, or
/// fixed what it asked for): the run goes back to running the step, and the
/// returned text has Claude go on with it (see `Resume`), logged in
/// `flow_run_resumes`. Refused where that cannot help, as at a step limit, where the
/// person picks an output instead, and if the run has moved on from `step_id` (see
/// `proceed`).
pub fn resume(conn: &mut Connection, run_id: i64, step_id: &str) -> Result<String, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let run = active_run(&tx, "id = ?1", &run_id)?.ok_or_else(|| format!("there is no active run {run_id}"))?;
    run.at(step_id)?;
    let node = run.node()?;
    if run.status != Status::Attention || node.kind != Kind::Prompt {
        return Err(format!("\"{}\" does not need attention; pick an output instead", node.name()));
    }
    let Some(how) = run.resume else {
        return Err(format!("Claude going on with \"{}\" would not help; pick an output instead", node.name()));
    };
    // Logged with why it waited, before going on clears that.
    tx.execute(
        "INSERT INTO flow_run_resumes (run_id, node_id, note, at) SELECT id, node_id, note, ?2 FROM flow_runs WHERE id = ?1",
        params![run.id, now()],
    )
    .map_err(sql)?;
    set(&tx, run.id, &node.id, Status::Running, run.steps, None)?;
    let prompt = match how {
        Resume::Remind => flows::resume_text(node),
        // With the base info, if a clear took that too.
        Resume::Prompt => prompt_for(&tx, &run, node)?,
    };
    tx.commit().map_err(sql)?;
    Ok(prompt)
}

/// Claude in tab `tab_id` cleared its conversation: a run waiting for that clear
/// can send its briefing (see `cleared`); a running step will get no reply, so
/// its run waits for the person, saying `why` (see `pause_tab`). Either way, the
/// run's next prompt carries its rules and Start's base info again. A finished
/// run's note is dropped, as Claude no longer has the flow's rules.
pub fn clear_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    tx.execute(&format!("UPDATE flow_runs SET briefed = 0 WHERE tab_id = ?1 AND status IN {ACTIVE}"), [tab_id.as_str()])
        .map_err(sql)?;
    drop_closing(&tx, tab_id)?;
    let changed = tx
        .execute(
            "UPDATE flow_runs SET clear = 'done', updated_at = ?2 WHERE tab_id = ?1 AND status = 'running' AND clear = 'pending'",
            params![tab_id.as_str(), now()],
        )
        .map_err(sql)?;
    if changed == 0 {
        pause(&tx, tab_id, why)?;
    }
    refill(&tx, tab_id.as_str())?;
    tx.commit().map_err(sql)
}

/// Makes the tab's running run wait for the person, saying why: Claude will not
/// reply to its step (the conversation was cleared, or the hook failed), so
/// Continue sends the step's prompt again. A run still at Start waiting for its
/// clear has no step to reply to, and is left alone.
pub fn pause_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    pause(conn, tab_id, why)
}

fn pause(conn: &Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    answer_lost(conn, tab_id.as_str(), why)?;
    conn.execute(
        // Claude may not have read a briefing it was given: the first step carries it again.
        "UPDATE flow_runs SET status = 'attention', note = ?2, resume = 'prompt', briefed = CASE WHEN briefing = 1 THEN 0 ELSE briefed END,
         briefing = 0, updated_at = ?3 WHERE tab_id = ?1 AND status = 'running' AND clear IS NULL",
        params![tab_id.as_str(), why, now()],
    )
    .map(drop)
    .map_err(sql)
}

/// Stops the tab's active run, if it has one: the tab is closing.
pub fn stop_tab(conn: &Connection, tab_id: &TabId) -> Result<(), String> {
    conn.execute(
        &format!("UPDATE flow_runs SET status = 'stopped', updated_at = ?2 WHERE tab_id = ?1 AND status IN {ACTIVE}"),
        params![tab_id.as_str(), now()],
    )
    .map(drop)
    .map_err(sql)
}

/// At startup: stops the runs of tabs that are no longer open, and makes running
/// ones wait, as their Claude was stopped with misag and will not reply.
pub fn after_restart(conn: &mut Connection, open_tabs: &[TabId]) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let tabs: Vec<String> = {
        let mut stmt = tx.prepare(&format!("SELECT DISTINCT tab_id FROM flow_runs WHERE status IN {ACTIVE}")).map_err(sql)?;
        let tabs = stmt.query_map([], |r| r.get(0)).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?;
        tabs
    };
    for tab in tabs {
        if open_tabs.iter().any(|open| open.as_str() == tab) {
            // The app that would have sent the briefing after the clear is gone.
            tx.execute(
                "UPDATE flow_runs SET status = 'attention', note = ?2, resume = NULL, clear = NULL, updated_at = ?3
                 WHERE tab_id = ?1 AND status = 'running' AND clear IS NOT NULL",
                params![tab, "Mischief of Agents was restarted before the conversation was cleared. Pick an output to go on without clearing, or stop the run and press ▶ again.", now()],
            )
            .map_err(sql)?;
            answer_lost(&tx, &tab, "Mischief of Agents was restarted while Claude answered your prompt.")?;
            // Claude may not have read the briefing: the first step carries it again.
            tx.execute(
                "UPDATE flow_runs SET status = 'attention', note = ?2, resume = NULL, briefing = 0, briefed = 0, updated_at = ?3
                 WHERE tab_id = ?1 AND status = 'running' AND briefing = 1",
                params![tab, "Mischief of Agents was restarted while Claude read the flow's briefing. Pick an output to go on; the first step briefs Claude again.", now()],
            )
            .map_err(sql)?;
            tx.execute(
                // Claude comes back in its conversation, so it can be reminded.
                "UPDATE flow_runs SET status = 'attention', note = ?2, resume = 'remind', updated_at = ?3 WHERE tab_id = ?1 AND status = 'running'",
                params![tab, "Mischief of Agents was restarted while Claude worked on this step, so its reply was not seen. Pick an output to go on.", now()],
            )
            .map_err(sql)?;
            refill(&tx, &tab)?;
        } else {
            tx.execute(
                &format!("UPDATE flow_runs SET status = 'stopped', updated_at = ?2 WHERE tab_id = ?1 AND status IN {ACTIVE}"),
                params![tab, now()],
            )
            .map_err(sql)?;
        }
    }
    tx.commit().map_err(sql)
}

/// Stops run `run_id`, which has Claude told the flow is finished with the
/// person's next prompt (see `owe_closing`). A run whose steps cannot be read can
/// still be stopped.
pub fn stop(conn: &Connection, run_id: i64) -> Result<(), String> {
    let changed = conn
        .execute(
            &format!("UPDATE flow_runs SET status = 'stopped', closing = ({OWES_CLOSING}), updated_at = ?2 WHERE id = ?1 AND status IN {ACTIVE}"),
            params![run_id, now()],
        )
        .map_err(sql)?;
    if changed != 1 {
        return Err(format!("there is no active run {run_id}"));
    }
    Ok(())
}

/// How many of `graph`'s steps (all but Start and End, and steps Start cannot
/// reach, which never run) run `run` has finished, each counted once, so loops do
/// not add to it; and how many steps there are. Branches the run did not take
/// stay unfinished, so a run may end short of the total.
fn progress(conn: &Connection, run: i64, graph: &Graph) -> Result<(i64, i64), String> {
    let Some(start) = graph.nodes.iter().find(|n| n.kind == Kind::Start) else { return Ok((0, 0)) };
    let reached = flows::reachable(graph, start);
    let mut stmt = conn.prepare_cached("SELECT DISTINCT node_id FROM flow_run_events WHERE run_id = ?1").map_err(sql)?;
    let left = stmt
        .query_map([run], |r| r.get::<_, String>(0))
        .map_err(sql)?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(sql)?;
    let (done, total) = graph
        .nodes
        .iter()
        .filter(|n| !matches!(n.kind, Kind::Start | Kind::End) && reached.contains(n.id.as_str()))
        .fold((0, 0), |(done, total), n| (done + i64::from(left.contains(&n.id)), total + 1));
    Ok((done, total))
}

/// Every active run, oldest first. A run still at Start `CLEAR_TIMEOUT_SECS` after
/// it began, waiting for a clear the app no longer sees through, first waits for
/// the person, as after a restart.
pub fn active(conn: &Connection) -> Result<Vec<Run>, String> {
    conn.execute(
        "UPDATE flow_runs SET status = 'attention', note = ?1, resume = NULL, clear = NULL, updated_at = ?2
         WHERE status = 'running' AND clear IS NOT NULL AND started_at <= ?2 - ?3",
        params![
            "Claude did not clear the conversation within 30 seconds. Pick an output to go on without clearing, or stop the run and press ▶ again.",
            now(),
            CLEAR_TIMEOUT_SECS
        ],
    )
    .map_err(sql)?;
    // Claude may not have read the briefing: the first step carries it again.
    conn.execute(
        "UPDATE flow_runs SET status = 'attention', note = ?1, resume = NULL, briefing = 0, briefed = 0, updated_at = ?2
         WHERE status = 'running' AND briefing = 1 AND updated_at <= ?2 - ?3",
        params![
            "Claude did not reply to the flow's briefing within 2 minutes (it may have been interrupted). Pick an output to go on; the first step briefs Claude again.",
            now(),
            BRIEFING_TIMEOUT_SECS
        ],
    )
    .map_err(sql)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT r.id, r.flow_id, f.name, r.tab_id, r.graph, r.node_id, r.status, r.note, r.steps, r.resume, r.clear, r.prefill, r.briefing
             FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.status IN {ACTIVE} ORDER BY r.id"
        ))
        .map_err(sql)?;
    type Row = (i64, i64, String, String, String, String, String, Option<String>, i64, Option<String>, Option<String>, bool, bool);
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
            ))
        })
        .map_err(sql)?
        .collect::<Result<Vec<Row>, _>>()
        .map_err(sql)?;
    rows.into_iter()
        .map(|(id, flow_id, flow_name, tab_id, graph, step_id, status, note, steps, resume, clear, prefill, briefing)| {
            // A run that cannot be read is still listed, saying why, so it can be stopped.
            let graph = parse_graph(&graph);
            // Only shown, so a failure to count leaves the run listed, and stoppable.
            let (done, total) = match &graph {
                Ok(graph) => progress(conn, id, graph).unwrap_or_else(|e| {
                    eprintln!("misag: could not count the steps run {id} has done: {e}");
                    (0, 0)
                }),
                Err(_) => (0, 0),
            };
            let node = graph.and_then(|graph| {
                let node = graph.nodes.into_iter().find(|n| n.id == step_id);
                node.ok_or_else(|| format!("the run is at a missing step {step_id:?}"))
            });
            let status = Status::parse(&status)?;
            let states = Resume::parse(resume).and_then(|resume| Ok((resume, Clear::parse(clear)?)));
            let (step, outputs, note, resumable, step_kind) = match node.and_then(|node| states.map(|states| (node, states))) {
                Ok((node, (resume, clear))) => {
                    // The same test as `resume`'s.
                    let resumable = status == Status::Attention && node.kind == Kind::Prompt && resume.is_some();
                    if clear.is_some() || briefing {
                        // Nothing to pick until the first step is taken (see `proceed`).
                        let note = clear.is_some().then(|| "Clearing the conversation before the first step.".to_owned());
                        (node.name().to_owned(), Vec::new(), note, false, Some(node.kind))
                    } else {
                        (node.name().to_owned(), node.outputs, note, resumable, Some(node.kind))
                    }
                }
                Err(e) => (step_id.clone(), Vec::new(), Some(format!("This run cannot be read: {e}. Stop it.")), false, None),
            };
            Ok(Run { id, flow_id, flow_name, tab_id, step, outputs, step_id, status, note, steps, done, total, resumable, prefill, step_kind, briefing })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Db};
    use crate::flows::tests::{edge, node, runnable};
    use crate::flows::Edge;

    fn tab(id: &str) -> TabId {
        TabId::parse(id).unwrap()
    }

    /// A database with `graph` stored as flow "F"; the run functions get their own
    /// connection, as the hook does.
    fn setup(name: &str, graph: &Graph) -> (Db, Connection, i64) {
        let path = db::tests::temp_db(name);
        let db = Db::open(path.clone()).unwrap();
        let id = db.create_flow("F").unwrap().id;
        db.save_flow_graph(id, graph).unwrap();
        (db, db::connect_existing(&path).unwrap(), id)
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        let mut all = vec![node("start", Kind::Start, &["next"])];
        all.extend(nodes);
        Graph { version: 1, nodes: all, edges }
    }

    /// Start → a pause → a prompt.
    fn manual_first() -> Graph {
        graph(
            vec![node("m", Kind::Pause, &["ok"]), node("p", Kind::Prompt, &["done"])],
            vec![edge("start", "next", "m"), edge("m", "ok", "p")],
        )
    }

    /// The step run `run` is at, as the app shows it; empty for no such run.
    fn at(conn: &Connection, run: i64) -> String {
        conn.query_row("SELECT node_id FROM flow_runs WHERE id = ?1", [run], |r| r.get(0)).unwrap_or_default()
    }

    /// `proceed` from the step the run is at.
    fn pick(conn: &mut Connection, run: i64, output: &str) -> Result<Option<String>, String> {
        let step = at(conn, run);
        proceed(conn, run, &step, output)
    }

    /// `resume` at the step the run is at.
    fn go_on(conn: &mut Connection, run: i64) -> Result<String, String> {
        let step = at(conn, run);
        resume(conn, run, &step)
    }

    fn only_run(conn: &Connection) -> Run {
        let runs = active(conn).unwrap();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }

    /// `start`, then Claude's reply to the run's briefing, for a run that does not
    /// clear first: as if it began at its first step, with that step's prompt.
    fn begin(conn: &mut Connection, flow: i64, tab_id: &TabId) -> Result<Started, String> {
        let mut started = start(conn, flow, tab_id)?;
        if !started.clear {
            assert!(started.prompt.as_deref().unwrap().ends_with("Reply only with: Ready"), "{:?}", started.prompt);
            started.prompt = on_stop(conn, tab_id, Some("Ready")).unwrap();
        }
        Ok(started)
    }

    #[test]
    fn a_run_begins_by_briefing_claude_and_takes_its_first_step_once_claude_is_ready() {
        let g = briefing(runnable(), "The app is in src/.");
        let (_db, mut conn, flow) = setup("runs-briefing", &g);
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(started.prompt, Some(flows::briefing_text(&g, "F")));
        let run = only_run(&conn);
        assert_eq!((run.step_id.as_str(), run.status, run.steps), ("start", Status::Running, 0));
        // "Ready" takes the first step, without the base info Claude has.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap(), prompt_of(&g, "p"));
        assert_eq!(only_run(&conn).steps, 1);
    }

    #[test]
    fn a_run_that_clears_is_briefed_after_the_clear() {
        let mut g = briefing(runnable(), "The app is in src/.");
        g.nodes[0].clear = true;
        let (_db, mut conn, flow) = setup("runs-briefing-clear", &g);
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!((started.clear, started.prompt), (true, None));
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Ready { prompt: Some(flows::briefing_text(&g, "F")) });
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn while_claude_reads_the_briefing_start_s_output_is_not_offered() {
        // Picking it would send the first step behind the briefing, whose reply
        // would then be taken for the step's.
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-briefing-skip", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let briefing = only_run(&conn);
        assert!(briefing.briefing);
        assert!(briefing.outputs.is_empty());
        assert!(pick(&mut conn, run, "next").unwrap_err().contains("briefing"));
        on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap();
        assert!(!only_run(&conn).briefing);
    }

    #[test]
    fn a_briefing_reply_that_does_more_than_say_ready_waits_for_the_person() {
        let g = runnable();
        for reply in ["Ready. I'll start by fixing the tests:\n\nDone, all green.\nFLOW: pass", &"Ready ".repeat(40)] {
            let (_db, mut conn, flow) = setup("runs-briefing-eager", &g);
            let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
            assert_eq!(on_stop(&mut conn, &tab("t1"), Some(reply)).unwrap(), None);
            let waiting = only_run(&conn);
            assert_eq!((waiting.step_id.as_str(), waiting.status, waiting.briefing), ("start", Status::Attention, false));
            assert!(waiting.note.as_deref().unwrap().contains("more than"), "{:?}", waiting.note);
            // The person checks the reply and goes on.
            assert_eq!(pick(&mut conn, run, "next").unwrap(), prompt_of(&g, "p"));
        }
        for reply in ["Ready", "**Ready.**", "ready"] {
            let (_db, mut conn, flow) = setup("runs-briefing-ready", &g);
            start(&mut conn, flow, &tab("t1")).unwrap();
            assert_eq!(on_stop(&mut conn, &tab("t1"), Some(reply)).unwrap(), prompt_of(&g, "p"), "{reply}");
        }
    }

    #[test]
    fn a_briefing_reply_claude_code_does_not_report_waits_for_the_person() {
        let (_db, mut conn, flow) = setup("runs-briefing-no-reply", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), None).unwrap(), None);
        let waiting = only_run(&conn);
        assert_eq!(waiting.status, Status::Attention);
        assert!(waiting.note.unwrap().contains("update Claude Code"));
    }

    #[test]
    fn a_clear_during_the_briefing_has_the_first_step_brief_claude_again() {
        let g = briefing(runnable(), "The app is in src/.");
        let (_db, mut conn, flow) = setup("runs-briefing-cleared", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        clear_tab(&mut conn, &tab("t1"), "The conversation was cleared.").unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.resumable, waiting.outputs.clone()), (Status::Attention, false, vec!["next".to_string()]));
        // A late reply is not the briefing's anymore.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap(), None);
        assert_eq!(pick(&mut conn, run, "next").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[1])));
    }

    #[test]
    fn a_restart_during_the_briefing_says_so_and_briefs_claude_again() {
        let g = briefing(runnable(), "The app is in src/.");
        let (_db, mut conn, flow) = setup("runs-briefing-restart", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        after_restart(&mut conn, &[tab("t1")]).unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.resumable, waiting.briefing), (Status::Attention, false, false));
        assert!(waiting.note.as_deref().unwrap().contains("briefing"), "{:?}", waiting.note);
        assert_eq!(pick(&mut conn, run, "next").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[1])));
    }

    #[test]
    fn a_stopped_briefed_run_tells_claude_with_the_person_s_next_prompt() {
        let (_db, mut conn, flow) = setup("runs-stop-closing", &runnable());
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        stop(&conn, run).unwrap();
        assert!(stop(&conn, run).unwrap_err().contains("no active run"));
        // A slash command is not a message to Claude, so it does not carry it.
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "/model").unwrap(), None);
        assert_eq!(on_prompt(&mut conn, &tab("t2"), "Hi").unwrap(), None, "another tab");
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), finished());
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "And now?").unwrap(), None, "once");
        // Claude got the briefing even if it has yet to reply.
        let (_db, mut conn, flow) = setup("runs-stop-briefing", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        stop(&conn, run).unwrap();
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), finished());
        // A run still clearing never briefed Claude.
        let mut g = runnable();
        g.nodes[0].clear = true;
        let (_db, mut conn, flow) = setup("runs-stop-clearing", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        stop(&conn, run).unwrap();
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), None);
    }

    #[test]
    fn a_new_run_or_a_clear_drops_the_finished_flow_s_note() {
        let (_db, mut conn, flow) = setup("runs-closing-dropped", &runnable());
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        stop(&conn, run).unwrap();
        // The new run's briefing supersedes it.
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        stop(&conn, run).unwrap();
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), None);
    }

    #[test]
    fn a_run_that_ends_after_a_clear_does_not_tell_claude_about_a_flow_it_lost() {
        let (_db, mut conn, flow) = setup("runs-closing-cleared", &runnable());
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(pick(&mut conn, run, "pass").unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), None);
    }

    #[test]
    fn the_end_of_a_run_tells_claude_with_the_person_s_next_prompt_and_costs_no_turn() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-closing", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        // "pass" leads to the End step: Claude is not made to reply again.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: pass")).unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), finished());
        // So does an output that leads nowhere, and the person's pick.
        let g = graph(vec![node("p", Kind::Prompt, &["done"])], vec![edge("start", "next", "p")]);
        let (_db, mut conn, flow) = setup("runs-closing-nowhere", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert_eq!(pick(&mut conn, run, "done").unwrap(), None);
        assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), finished());
    }

    #[test]
    fn a_briefing_with_no_reply_waits_for_the_person_and_the_first_step_briefs_again() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-briefing-timeout", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert!(only_run(&conn).briefing);
        // As if Claude was interrupted (no Stop) long enough ago.
        conn.execute("UPDATE flow_runs SET updated_at = updated_at - ?1", [BRIEFING_TIMEOUT_SECS + 1]).unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.briefing, waiting.outputs.clone()), (Status::Attention, false, vec!["next".to_string()]));
        assert!(waiting.note.as_deref().unwrap().contains("briefing"), "{:?}", waiting.note);
        assert_eq!(pick(&mut conn, run, "next").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[1])));
    }

    #[test]
    fn a_failed_stop_hook_during_the_briefing_has_the_first_step_brief_again() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-briefing-pause", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "The Stop hook failed.").unwrap();
        assert_eq!(pick(&mut conn, run, "next").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[1])));
    }

    /// What Claude is told with the person's next prompt once a run has ended.
    fn finished() -> Option<String> {
        Some(flows::closing_text("F"))
    }

    fn prompt_of(graph: &Graph, id: &str) -> Option<String> {
        Some(flows::prompt_text(graph.nodes.iter().find(|n| n.id == id).unwrap()))
    }

    #[test]
    fn a_run_starts_with_the_first_prompt() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-start", &g);
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!((started.clear, started.prompt), (false, prompt_of(&g, "p")));
        let run = only_run(&conn);
        assert_eq!(started.run_id, run.id);
        assert_eq!(
            (run.flow_name.as_str(), run.tab_id.as_str(), run.step.as_str(), run.status, run.steps),
            ("F", "t1", "P", Status::Running, 1)
        );
        assert_eq!(run.outputs, ["pass", "fail"]);
    }

    #[test]
    fn start_s_base_info_goes_in_the_briefing_and_again_after_a_clear() {
        let mut g = runnable();
        g.nodes[0].text = "The app is in src/.".into();
        let (_db, mut conn, flow) = setup("runs-base-info", &g);
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        // The briefing had it.
        assert_eq!(started.prompt, prompt_of(&g, "p"));
        // Looping back to the step, Claude still has it.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), prompt_of(&g, "p"));
        // After a clear, Claude has lost it with the step.
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(go_on(&mut conn, started.run_id).unwrap(), flows::first_prompt_text(&g, "F", &g.nodes[1]));
    }

    #[test]
    fn a_run_that_starts_at_pauses_is_briefed_before_them() {
        let mut g = graph(
            vec![node("m", Kind::Pause, &["ok"]), node("m2", Kind::Pause, &["ok"]), node("p", Kind::Prompt, &["done"])],
            vec![edge("start", "next", "m"), edge("m", "ok", "m2"), edge("m2", "ok", "p")],
        );
        g.nodes[0].text = "The app is in src/.".into();
        let (_db, mut conn, flow) = setup("runs-base-info-manual", &g);
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(started.prompt, None);
        assert_eq!(pick(&mut conn, started.run_id, "ok").unwrap(), None);
        assert_eq!(pick(&mut conn, started.run_id, "ok").unwrap(), prompt_of(&g, "p"));
    }

    /// `graph` with `info` as Start's base info.
    fn briefing(mut graph: Graph, info: &str) -> Graph {
        graph.nodes[0].text = info.into();
        graph
    }

    #[test]
    fn picking_an_output_after_a_clear_sends_base_info_again() {
        let g = briefing(runnable(), "The app is in src/.");
        let (_db, mut conn, flow) = setup("runs-base-info-pick", &g);
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        let prompt = pick(&mut conn, started.run_id, "fail").unwrap().unwrap();
        assert_eq!(prompt, flows::first_prompt_text(&g, "F", &g.nodes[1]));
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn a_clear_at_a_manual_step_owes_base_info_to_the_next_prompt() {
        let g = briefing(
            graph(
                vec![node("m", Kind::Pause, &["ok"]), node("p", Kind::Prompt, &["done"])],
                vec![edge("start", "next", "m"), edge("m", "ok", "p"), edge("p", "done", "m")],
            ),
            "The app is in src/.",
        );
        let (_db, mut conn, flow) = setup("runs-base-info-manual-clear", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), prompt_of(&g, "p"));
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        // Back at the pause, Claude still has it.
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), prompt_of(&g, "p"));
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual, "a clear does not hold up a manual step");
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[2])));
    }

    #[test]
    fn continue_after_a_failed_hook_does_not_send_base_info_again() {
        // The conversation still has it.
        let g = briefing(runnable(), "The app is in src/.");
        let (_db, mut conn, flow) = setup("runs-base-info-hook", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "The Stop hook failed.").unwrap();
        assert_eq!(Some(go_on(&mut conn, run).unwrap()), prompt_of(&g, "p"));
    }

    #[test]
    fn blank_base_info_sends_the_prompt_as_it_is() {
        let g = briefing(runnable(), " \n ");
        let (_db, mut conn, flow) = setup("runs-base-info-blank", &g);
        assert_eq!(begin(&mut conn, flow, &tab("t1")).unwrap().prompt, prompt_of(&g, "p"));
    }

    #[test]
    fn a_flow_that_is_not_ready_does_not_start() {
        let (_db, mut conn, flow) = setup("runs-not-ready", &Graph::default());
        let err = begin(&mut conn, flow, &tab("t1")).unwrap_err();
        assert!(err.contains("not ready") && err.contains("Start is not connected"), "{err}");
        assert!(active(&conn).unwrap().is_empty());
        assert!(begin(&mut conn, flow + 100, &tab("t1")).unwrap_err().contains("no flow"));
    }

    #[test]
    fn a_run_counts_its_finished_steps_once_each_out_of_the_flow_s_steps() {
        // Start → pause m → prompt p (fail loops to p, pass → End).
        // Step u is not reachable from Start, so it never runs and is not counted.
        let g = graph(
            vec![
                node("m", Kind::Pause, &["ok"]),
                node("p", Kind::Prompt, &["pass", "fail"]),
                node("end", Kind::End, &[]),
                node("u", Kind::Prompt, &["done"]),
            ],
            vec![edge("start", "next", "m"), edge("m", "ok", "p"), edge("p", "pass", "end"), edge("p", "fail", "p")],
        );
        let (_db, mut conn, flow) = setup("runs-progress", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn);
        assert_eq!((run.done, run.total), (0, 2));
        pick(&mut conn, run.id, "ok").unwrap();
        assert_eq!(only_run(&conn).done, 1);
        on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap();
        assert_eq!(only_run(&conn).done, 2);
        on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap();
        assert_eq!(only_run(&conn).done, 2);
    }

    #[test]
    fn a_tab_runs_one_flow_at_a_time() {
        let (_db, mut conn, flow) = setup("runs-one", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        let err = begin(&mut conn, flow, &tab("t1")).unwrap_err();
        assert!(err.contains("already running"), "{err}");
        begin(&mut conn, flow, &tab("t2")).unwrap();
        let first = active(&conn).unwrap()[0].id;
        stop(&conn, first).unwrap();
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert!(stop(&conn, 999).unwrap_err().contains("no active run"));
    }

    #[test]
    fn claude_s_flow_line_picks_the_next_step() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-follow", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        // fail loops back to the same prompt.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Broken.\nFLOW: fail")).unwrap(), prompt_of(&g, "p"));
        assert_eq!(only_run(&conn).steps, 2);
        // pass leads to End, which finishes the run.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("All good.\nFLOW: pass")).unwrap(), None, "a run that ends sends nothing");
        assert!(active(&conn).unwrap().is_empty());
        let status: String = conn.query_row("SELECT status FROM flow_runs", [], |r| r.get(0)).unwrap();
        assert_eq!(status, "done");
        let mut stmt = conn.prepare("SELECT node_id, output FROM flow_run_events ORDER BY id").unwrap();
        let events: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        let expected = [("start", "next"), ("p", "fail"), ("p", "pass")].map(|(n, o)| (n.to_string(), o.to_string()));
        assert_eq!(events, expected);
    }

    #[test]
    fn an_output_without_a_next_step_ends_the_run() {
        let g = graph(vec![node("p", Kind::Prompt, &["done", "again"])], vec![edge("start", "next", "p"), edge("p", "again", "p")]);
        let (_db, mut conn, flow) = setup("runs-run-out", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None, "a run that ends sends nothing");
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_reply_that_cannot_be_followed_needs_attention_until_one_can() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-attention", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        for (reply, why) in [
            ("I changed some files.", "FLOW"),
            ("FLOW: maybe", "maybe"),
            ("Which database should I use?\nFLOW: attention", "asked for a person"),
        ] {
            assert_eq!(on_stop(&mut conn, &tab("t1"), Some(reply)).unwrap(), None);
            let run = only_run(&conn);
            assert_eq!(run.status, Status::Attention, "{reply:?}");
            assert!(run.note.as_deref().unwrap_or("").contains(why), "{:?} should mention {why:?}", run.note);
        }
        // The person answers Claude in the terminal; its next reply moves the run on.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Used SQLite.\nFLOW: fail")).unwrap(), prompt_of(&g, "p"));
        let run = only_run(&conn);
        assert_eq!((run.status, run.note), (Status::Running, None));
    }

    #[test]
    fn a_manual_step_from_an_older_flow_with_several_outputs_does_not_start() {
        let mut g = manual_first();
        g.nodes[1].outputs.push("redo".into());
        let (_db, mut conn, flow) = setup("runs-manual-several", &g);
        assert!(begin(&mut conn, flow, &tab("t1")).unwrap_err().contains("more than one output"));
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_manual_step_waits_for_the_person() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-manual", &g);
        assert_eq!(begin(&mut conn, flow, &tab("t1")).unwrap().prompt, None);
        let run = only_run(&conn);
        assert_eq!((run.status, run.outputs.clone()), (Status::Manual, vec!["ok".to_string()]));
        // Claude ending a turn (the person chatting) does not move a manual step.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: ok")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Manual);
        assert!(pick(&mut conn, run.id, "maybe").unwrap_err().contains("maybe"));
        assert_eq!(only_run(&conn).status, Status::Manual);
        assert_eq!(pick(&mut conn, run.id, "ok").unwrap(), prompt_of(&g, "p"));
        assert_eq!(only_run(&conn).status, Status::Running);
    }

    /// Start (with base info) → a manual prompt with a draft → a prompt.
    fn manual_prompt_first(draft: &str) -> Graph {
        let mut mp = node("mp", Kind::ManualPrompt, &["sent"]);
        mp.text = draft.into();
        briefing(
            graph(vec![mp, node("p", Kind::Prompt, &["done"])], vec![edge("start", "next", "mp"), edge("mp", "sent", "p")]),
            "The app is in src/.",
        )
    }

    /// What the app pastes for the run's manual prompt, then marks pasted.
    fn paste(conn: &mut Connection, run: i64) -> Option<String> {
        let step = at(conn, run);
        let fill = prefill_text(conn, run, &step).unwrap()?;
        prefilled(conn, run, &step, fill.token).unwrap();
        Some(fill.text)
    }

    /// The text `prefill_text` gives the run, without pasting it.
    fn owed(conn: &mut Connection, run: i64, step: &str) -> Option<String> {
        prefill_text(conn, run, step).unwrap().map(|fill| fill.text)
    }

    // The draft keeps its trailing space, for the person to go on typing.
    const DRAFT: &str = "Fix the bug in ";
    // After a clear, Claude lacks the base info, so it leads the draft.
    const INFO_AND_DRAFT: &str = "The app is in src/.\n\nFix the bug in ";

    #[test]
    fn a_manual_prompt_fills_claude_s_input_and_goes_on_once_claude_answers_the_person() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt", &g);
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(started.prompt, None, "nothing is sent");
        let run = only_run(&conn);
        assert_eq!((run.status, run.step_id.as_str(), run.prefill), (Status::Manual, "mp", true));
        assert!(owed(&mut conn, run.id, "p").is_none(), "a fill for another step gets nothing");
        // Reading the text does not use it up: a paste that fails is tried again.
        // Claude has the base info from the briefing.
        assert_eq!(owed(&mut conn, run.id, "mp").as_deref(), Some(DRAFT));
        assert_eq!(paste(&mut conn, run.id).as_deref(), Some(DRAFT));
        assert!(!only_run(&conn).prefill);
        assert_eq!(owed(&mut conn, run.id, "mp"), None, "it is pasted once");
        // A turn the person did not start (a background task waking Claude) is not their prompt.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Task done.")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Manual);
        on_prompt(&mut conn, &tab("t1"), "Fix the bug in main.rs").unwrap();
        assert_eq!(only_run(&conn).status, Status::Running, "Claude is answering the person");
        // Claude's reply needs no FLOW line.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Fixed it.")).unwrap(), prompt_of(&g, "p"));
        assert_eq!(only_run(&conn).step_id, "p");
    }

    #[test]
    fn after_a_clear_the_next_prompt_step_briefs_claude_even_after_a_manual_prompt() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-no-info", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(paste(&mut conn, run).as_deref(), Some(INFO_AND_DRAFT));
        // The person's prompt may carry the base info, but never the flow's rules.
        on_prompt(&mut conn, &tab("t1"), INFO_AND_DRAFT).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Fixed it.")).unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[2])));
    }

    #[test]
    fn a_manual_prompt_with_nothing_to_fill_waits_for_the_person_s_prompt() {
        let mut g = manual_prompt_first("");
        g.nodes[0].text.clear();
        let (_db, mut conn, flow) = setup("runs-manual-prompt-empty", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert!(!only_run(&conn).prefill);
        assert_eq!(owed(&mut conn, run, "mp"), None);
        // A turn already under way when the run got here is not the person's prompt.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Done.")).unwrap(), None);
        on_prompt(&mut conn, &tab("t1"), "Do it").unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Done.")).unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn a_prompt_can_lead_to_a_manual_prompt() {
        let mut mp = node("mp", Kind::ManualPrompt, &["sent"]);
        mp.text = "Now ask about ".into();
        let g = briefing(
            graph(vec![node("p", Kind::Prompt, &["done"]), mp], vec![edge("start", "next", "p"), edge("p", "done", "mp")]),
            "The app is in src/.",
        );
        let (_db, mut conn, flow) = setup("runs-to-manual-prompt", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        // Claude stops; the app fills the input once it sees the run there.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        assert_eq!(paste(&mut conn, run).as_deref(), Some("Now ask about "), "Claude has the base info");
        on_prompt(&mut conn, &tab("t1"), "Now ask about it").unwrap();
        // The flow ends after the person's prompt is answered.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Answer.")).unwrap(), None, "a run that ends sends nothing");
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn manual_prompts_can_follow_each_other() {
        let mut first = node("a", Kind::ManualPrompt, &["sent"]);
        first.text = "one".into();
        let mut second = node("b", Kind::ManualPrompt, &["sent"]);
        second.text = "two".into();
        let g = graph(vec![first, second], vec![edge("start", "next", "a"), edge("a", "sent", "b")]);
        let (_db, mut conn, flow) = setup("runs-manual-prompts", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert_eq!(paste(&mut conn, run).as_deref(), Some("one"));
        on_prompt(&mut conn, &tab("t1"), "one").unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("ok")).unwrap(), None);
        assert_eq!((only_run(&conn).step_id.as_str(), only_run(&conn).prefill), ("b", true));
        assert_eq!(paste(&mut conn, run).as_deref(), Some("two"));
    }

    #[test]
    fn a_clear_or_a_restart_fills_the_manual_prompt_again() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-refill", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        paste(&mut conn, run);
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual, "a clear does not hold up a manual prompt");
        assert_eq!(paste(&mut conn, run).as_deref(), Some(INFO_AND_DRAFT));
        after_restart(&mut conn, &[tab("t1")]).unwrap();
        assert_eq!(paste(&mut conn, run).as_deref(), Some(INFO_AND_DRAFT), "Claude's input was lost with the app");
    }

    #[test]
    fn a_prompt_at_any_other_step_changes_nothing() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-prompt-at-pause", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), "hi").unwrap();
        on_prompt(&mut conn, &tab("t2"), "hi").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual);
    }

    #[test]
    fn a_paste_recorded_late_does_not_cancel_a_refill() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-late", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let first = prefill_text(&mut conn, run, "mp").unwrap().unwrap();
        // A clear comes between the paste and its record: the input is empty again.
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        prefilled(&mut conn, run, "mp", first.token).unwrap();
        assert!(only_run(&conn).prefill, "the paste after the clear is still owed");
        assert_eq!(paste(&mut conn, run).as_deref(), Some(INFO_AND_DRAFT));
    }

    #[test]
    fn a_slash_command_is_not_the_person_s_prompt() {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-slash", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), "  /model opus").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual);
    }

    #[test]
    fn a_manual_prompt_whose_answer_was_lost_waits_for_the_prompt_again() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-lost", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        paste(&mut conn, run);
        on_prompt(&mut conn, &tab("t1"), "Fix the bug in a.rs").unwrap();
        // Claude's answer will not come: the conversation was cleared while it worked.
        clear_tab(&mut conn, &tab("t1"), "The conversation was cleared.").unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.resumable), (Status::Attention, false), "Continue has nothing to remind Claude of");
        assert!(waiting.note.as_deref().unwrap().contains("send your prompt again"), "{:?}", waiting.note);
        assert_eq!(paste(&mut conn, run).as_deref(), Some(INFO_AND_DRAFT), "the input is filled again");
        // A turn Claude takes by itself does not move it on; the person's prompt does.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("hi")).unwrap(), None);
        on_prompt(&mut conn, &tab("t1"), "The app is in src/.\n\nFix the bug in a.rs").unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Fixed.")).unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[2])), "the clear took the rules");
    }

    #[test]
    fn a_restart_while_claude_answers_a_manual_prompt_waits_for_the_prompt_again() {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-restart", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), "The app is in src/. Go").unwrap();
        after_restart(&mut conn, &[tab("t1")]).unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.resumable), (Status::Attention, false));
        assert!(waiting.note.as_deref().unwrap().contains("send your prompt again"), "{:?}", waiting.note);
    }

    #[test]
    fn a_prompt_the_hook_could_not_record_makes_the_manual_prompt_wait_for_the_person() {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-unseen", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        prompt_unseen(&mut conn, &tab("t1"), "Your prompt could not be recorded.").unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.note.as_deref()), (Status::Attention, Some("Your prompt could not be recorded.")));
        // A run at any other step is left as it is.
        let (_db2, mut conn2, flow2) = setup("runs-pause-unseen", &manual_first());
        begin(&mut conn2, flow2, &tab("t1")).unwrap();
        prompt_unseen(&mut conn2, &tab("t1"), "x").unwrap();
        assert_eq!(only_run(&conn2).status, Status::Manual);
    }

    #[test]
    fn a_run_says_what_kind_of_step_it_is_at() {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-step-kind", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(only_run(&conn).step_kind, Some(Kind::ManualPrompt));
    }

    #[test]
    fn the_person_can_skip_a_manual_prompt_and_base_info_still_goes() {
        let g = manual_prompt_first("draft");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-skip", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        // Pasted, but never sent: Claude still lacks the base info.
        paste(&mut conn, run);
        assert_eq!(pick(&mut conn, run, "sent").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[2])));
        assert!(!only_run(&conn).prefill);
    }

    #[test]
    fn a_run_stored_with_the_old_kind_name_goes_on_from_its_pause() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-old-manual", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let stored: String = conn.query_row("SELECT graph FROM flow_runs", [], |r| r.get(0)).unwrap();
        assert!(stored.contains(r#""kind":"manual""#), "older misags read it: {stored}");
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn the_person_can_move_a_running_run_on_by_hand() {
        // Claude was interrupted, crashed or never ran the hook: no Stop will come.
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-take-over", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn);
        assert_eq!(pick(&mut conn, run.id, "fail").unwrap(), prompt_of(&g, "p"));
        assert_eq!(pick(&mut conn, run.id, "pass").unwrap(), None, "a run that ends sends nothing");
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_prompt_can_lead_to_a_manual_step() {
        let g = graph(
            vec![node("p", Kind::Prompt, &["done"]), node("m", Kind::Pause, &["ok"])],
            vec![edge("start", "next", "p"), edge("p", "done", "m")],
        );
        let (_db, mut conn, flow) = setup("runs-to-manual", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        assert_eq!((only_run(&conn).status, only_run(&conn).step_id), (Status::Manual, "m".to_string()));
    }

    #[test]
    fn a_claude_code_that_does_not_report_its_reply_is_named_as_the_cause() {
        let (_db, mut conn, flow) = setup("runs-no-reply", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), None).unwrap(), None);
        let run = only_run(&conn);
        assert_eq!(run.status, Status::Attention);
        assert!(run.note.unwrap().contains("update Claude Code"));
    }

    #[test]
    fn clearing_the_conversation_pauses_a_running_run() {
        let (_db, mut conn, flow) = setup("runs-clear", &manual_first());
        begin(&mut conn, flow, &tab("t2")).unwrap();
        let (_db2, mut conn2, flow2) = setup("runs-clear-2", &runnable());
        begin(&mut conn2, flow2, &tab("t1")).unwrap();
        pause_tab(&mut conn2, &tab("t1"), "The conversation was cleared.").unwrap();
        let run = only_run(&conn2);
        assert_eq!((run.status, run.note.as_deref()), (Status::Attention, Some("The conversation was cleared.")));
        // A run waiting at a manual step already waits for the person.
        pause_tab(&mut conn, &tab("t2"), "cleared").unwrap();
        assert_eq!((only_run(&conn).status, only_run(&conn).note), (Status::Manual, None));
        pause_tab(&mut conn, &tab("t9"), "no run").unwrap();
    }

    fn clearing() -> Graph {
        let mut g = runnable();
        g.nodes[0].clear = true;
        g
    }

    #[test]
    fn a_run_that_clears_takes_its_first_step_once_claude_has_cleared() {
        let g = clearing();
        let (_db, mut conn, flow) = setup("runs-clear-first", &g);
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        // The run holds the tab before anything is typed, so a second start is refused.
        assert_eq!((started.clear, started.prompt), (true, None));
        assert!(begin(&mut conn, flow, &tab("t1")).unwrap_err().contains("already running"));
        let run = only_run(&conn);
        assert_eq!((run.step_id.as_str(), run.status, run.steps), ("start", Status::Running, 0));
        assert!(run.outputs.is_empty(), "nothing to pick while it clears");
        assert!(run.note.unwrap().contains("Clearing"));
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Pending);
        // A turn that ends before the clear is not the first step's reply.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: pass")).unwrap(), None);
        assert!(pick(&mut conn, started.run_id, "next").unwrap_err().contains("clearing"));
        // The clear does not make the run wait, as it would a running step.
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(only_run(&conn).status, Status::Running);
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Ready { prompt: Some(flows::briefing_text(&g, "F")) });
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap(), prompt_of(&g, "p"));
        let run = only_run(&conn);
        assert_eq!((run.step_id.as_str(), run.status, run.steps, run.note), ("p", Status::Running, 1, None));
        // Taken once; a clear after that makes the running step wait, as before.
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Gone);
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(only_run(&conn).status, Status::Attention);
    }

    #[test]
    fn a_stopped_run_that_clears_is_gone() {
        let (_db, mut conn, flow) = setup("runs-clear-stopped", &clearing());
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        stop(&conn, started.run_id).unwrap();
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Gone);
    }

    #[test]
    fn a_restart_while_clearing_waits_for_the_person() {
        let (_db, mut conn, flow) = setup("runs-clear-restart", &clearing());
        let started = begin(&mut conn, flow, &tab("t1")).unwrap();
        after_restart(&mut conn, &[tab("t1")]).unwrap();
        let run = only_run(&conn);
        assert_eq!((run.step_id.as_str(), run.status, run.outputs.clone()), ("start", Status::Attention, vec!["next".to_string()]));
        assert!(run.note.unwrap().contains("before the conversation was cleared"));
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Gone);
        // The person goes on without the clear.
        assert!(pick(&mut conn, started.run_id, "next").unwrap().is_some());
    }

    #[test]
    fn after_a_restart_runs_of_closed_tabs_stop_and_running_ones_wait() {
        let path = db::tests::temp_db("runs-restart");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        db.save_flow_graph(flow, &runnable()).unwrap();
        let manual = db.create_flow("M").unwrap().id;
        db.save_flow_graph(manual, &manual_first()).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        begin(&mut conn, flow, &tab("open")).unwrap();
        begin(&mut conn, manual, &tab("waiting")).unwrap();
        begin(&mut conn, flow, &tab("closed")).unwrap();
        after_restart(&mut conn, &[tab("open"), tab("waiting")]).unwrap();
        let runs = active(&conn).unwrap();
        let summary: Vec<_> = runs.iter().map(|r| (r.tab_id.as_str(), r.status)).collect();
        assert_eq!(summary, [("open", Status::Attention), ("waiting", Status::Manual)]);
        assert!(runs[0].note.as_deref().unwrap().contains("restarted"));
    }

    #[test]
    fn closing_a_tab_stops_its_run() {
        let (_db, mut conn, flow) = setup("runs-close", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        stop_tab(&conn, &tab("t1")).unwrap();
        assert!(active(&conn).unwrap().is_empty());
        stop_tab(&conn, &tab("t1")).unwrap();
    }

    #[test]
    fn a_run_whose_steps_cannot_be_read_is_still_listed() {
        let (_db, mut conn, flow) = setup("runs-bad-row", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        begin(&mut conn, flow, &tab("t2")).unwrap();
        conn.execute("UPDATE flow_runs SET graph = 'broken' WHERE tab_id = 't1'", []).unwrap();
        let runs = active(&conn).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].note.as_deref().unwrap().contains("cannot be read"));
        assert!(runs[0].outputs.is_empty());
        assert_eq!((runs[0].done, runs[0].total), (0, 0));
        // It can still be stopped.
        stop(&conn, runs[0].id).unwrap();
        assert_eq!(active(&conn).unwrap().len(), 1);
    }

    #[test]
    fn the_one_run_index_covers_the_active_statuses() {
        let (_db, conn, _) = setup("runs-index", &runnable());
        let index: String =
            conn.query_row("SELECT sql FROM sqlite_master WHERE name = 'one_active_run_per_tab'", [], |r| r.get(0)).unwrap();
        assert!(index.ends_with(&format!("WHERE status IN {ACTIVE}")), "{index}");
    }

    #[test]
    fn misag_s_step_limit_stays_below_claude_code_s() {
        // A run's Stop-hook continuations in a row: each step's prompt (the first
        // follows the briefing's reply), with room to spare.
        assert!(MAX_RUN_STEPS + 1 < i64::from(crate::claude::STOP_CAP));
    }

    #[test]
    fn the_person_can_pick_the_output_of_a_step_that_needs_attention() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-pick", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_stop(&mut conn, &tab("t1"), Some("Hmm.")).unwrap();
        let run = only_run(&conn);
        assert_eq!(pick(&mut conn, run.id, "fail").unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn a_step_that_needs_attention_can_be_resumed_by_asking_claude_to_go_on() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-resume", &g);
        let t = tab("t1");
        begin(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, Some("Which database?\nFLOW: attention")).unwrap();
        let run = only_run(&conn);
        assert!(run.resumable);
        let prompt = go_on(&mut conn, run.id).unwrap();
        assert!(prompt.contains("\"P\"") && prompt.contains("`FLOW: pass` or `FLOW: fail`"), "{prompt}");
        let resumed = only_run(&conn);
        assert!(!resumed.resumable);
        assert_eq!((resumed.status, resumed.note, resumed.step_id.as_str(), resumed.steps), (Status::Running, None, "p", run.steps));
        // Claude's reply moves it on as usual.
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: pass")).unwrap(), None, "a run that ends sends nothing");
        assert!(active(&conn).unwrap().is_empty());
        // Only a prompt step that needs attention is resumed.
        let (_db, mut conn, flow) = setup("runs-resume-manual", &manual_first());
        begin(&mut conn, flow, &t).unwrap();
        let run = only_run(&conn);
        assert!(!run.resumable);
        assert!(go_on(&mut conn, run.id).unwrap_err().contains("does not need attention"));
        assert_eq!(only_run(&conn).status, Status::Manual);
    }

    #[test]
    fn continue_is_offered_only_where_claude_going_on_can_help() {
        let g = runnable();
        let t = tab("t1");
        // A reply without a FLOW line, or one naming no output: Claude is reminded.
        for reply in ["Done.", "FLOW: maybe"] {
            let (_db, mut conn, flow) = setup("runs-resume-reply", &g);
            begin(&mut conn, flow, &t).unwrap();
            on_stop(&mut conn, &t, Some(reply)).unwrap();
            let run = only_run(&conn);
            assert!(run.resumable, "{reply:?}");
            assert!(go_on(&mut conn, run.id).unwrap().starts_with("Go on with the flow step"));
        }
        // The conversation was cleared: Claude gets the step's prompt again.
        let (_db, mut conn, flow) = setup("runs-resume-cleared", &g);
        begin(&mut conn, flow, &t).unwrap();
        pause_tab(&mut conn, &t, "The conversation was cleared.").unwrap();
        let run = only_run(&conn);
        assert!(run.resumable);
        assert_eq!(Some(go_on(&mut conn, run.id).unwrap()), prompt_of(&g, "p"));
        // Claude Code that does not report replies would only wait again.
        let (_db, mut conn, flow) = setup("runs-resume-unreported", &g);
        begin(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, None).unwrap();
        assert!(!only_run(&conn).resumable);
        // A step limit: going on would only redo the step and wait again.
        let (_db, mut conn, flow) = setup("runs-resume-limit", &g);
        begin(&mut conn, flow, &t).unwrap();
        for _ in 0..MAX_RUN_STEPS {
            on_stop(&mut conn, &t, Some("FLOW: fail")).unwrap();
        }
        let run = only_run(&conn);
        assert_eq!(run.status, Status::Attention);
        assert!(!run.resumable);
        assert!(go_on(&mut conn, run.id).is_err());
        assert_eq!(only_run(&conn), run);
    }

    #[test]
    fn a_continue_is_logged_with_why_the_run_waited_but_is_not_a_run_of_the_step() {
        let g = review_loop();
        let (_db, mut conn, flow) = setup("runs-resume-log", &g);
        let t = tab("t1");
        begin(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, Some("Which files?\nFLOW: attention")).unwrap();
        let id = only_run(&conn).id;
        go_on(&mut conn, id).unwrap();
        let log: Vec<(i64, String, Option<String>)> = {
            let mut stmt = conn.prepare("SELECT run_id, node_id, note FROM flow_run_resumes ORDER BY id").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
        };
        assert_eq!(log, [(id, "review".to_string(), Some("Claude asked for a person; see its last reply.".to_string()))]);
        // Review may run twice: the Continue was the same run of it, so a second round is still allowed.
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: issues")).unwrap(), prompt_of(&g, "fix"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: done")).unwrap(), prompt_of(&g, "review"));
    }

    #[test]
    fn a_second_continue_is_refused_and_changes_nothing() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-resume-twice", &g);
        let t = tab("t1");
        begin(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, Some("FLOW: attention")).unwrap();
        let id = only_run(&conn).id;
        go_on(&mut conn, id).unwrap();
        let running = only_run(&conn);
        assert!(go_on(&mut conn, id).unwrap_err().contains("does not need attention"));
        assert_eq!(only_run(&conn), running);
        stop(&conn, id).unwrap();
        assert!(go_on(&mut conn, id).unwrap_err().contains("no active run"));
    }

    #[test]
    fn after_a_restart_claude_is_reminded_of_its_step() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-resume-restart", &g);
        let t = tab("t1");
        begin(&mut conn, flow, &t).unwrap();
        after_restart(&mut conn, std::slice::from_ref(&t)).unwrap();
        let run = only_run(&conn);
        assert!(run.resumable);
        assert!(go_on(&mut conn, run.id).unwrap().starts_with("Go on with the flow step"));
    }

    #[test]
    fn a_run_that_keeps_going_waits_after_the_step_limit() {
        let (_db, mut conn, flow) = setup("runs-limit", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        for _ in 1..MAX_RUN_STEPS {
            assert!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap().is_some());
        }
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), None);
        let run = only_run(&conn);
        // It waits at the step Claude finished, whose outputs the person picks from.
        assert_eq!((run.status, run.step_id.as_str()), (Status::Attention, "p"));
        assert!(run.note.unwrap().contains(&MAX_RUN_STEPS.to_string()));
        // Stepping in sends the next prompt and starts the count over.
        assert!(pick(&mut conn, run.id, "fail").unwrap().is_some());
        assert_eq!(only_run(&conn).steps, 1);
    }

    /// Start → Review, whose issues go to Fix and back, at most twice; clean ends.
    fn review_loop() -> Graph {
        let mut review = node("review", Kind::Prompt, &["clean", "issues"]);
        review.max_runs = Some(2);
        graph(
            vec![review, node("fix", Kind::Prompt, &["done"])],
            vec![edge("start", "next", "review"), edge("review", "issues", "fix"), edge("fix", "done", "review")],
        )
    }

    #[test]
    fn a_step_run_its_most_times_waits_for_the_person() {
        let g = review_loop();
        let (_db, mut conn, flow) = setup("runs-max", &g);
        let t = tab("t1");
        assert_eq!(begin(&mut conn, flow, &t).unwrap().prompt, prompt_of(&g, "review"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: issues")).unwrap(), prompt_of(&g, "fix"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: done")).unwrap(), prompt_of(&g, "review"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: issues")).unwrap(), prompt_of(&g, "fix"));
        // A third review would be one too many: the run waits after the fix.
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: done")).unwrap(), None);
        let run = only_run(&conn);
        assert_eq!((run.status, run.step_id.as_str()), (Status::Attention, "fix"));
        let note = run.note.unwrap();
        assert!(note.contains("REVIEW") && note.contains("already run 2 times"), "{note}");
        // The person can send it round once more; the next round waits again.
        assert_eq!(pick(&mut conn, run.id, "done").unwrap(), prompt_of(&g, "review"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: issues")).unwrap(), prompt_of(&g, "fix"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: done")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Attention);
        assert_eq!(pick(&mut conn, run.id, "done").unwrap(), prompt_of(&g, "review"));
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: clean")).unwrap(), None, "a run that ends sends nothing");
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_step_that_may_run_once_says_so() {
        let mut g = review_loop();
        g.nodes[1].max_runs = Some(1);
        let (_db, mut conn, flow) = setup("runs-max-once", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_stop(&mut conn, &tab("t1"), Some("FLOW: issues")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        let note = only_run(&conn).note.unwrap();
        assert!(note.contains("already run once") && note.contains("Pick \"done\""), "{note}");
    }

    #[test]
    fn a_step_that_loops_onto_itself_counts_its_runs_too() {
        let mut p = node("p", Kind::Prompt, &["again", "done"]);
        p.max_runs = Some(2);
        let g = graph(vec![p], vec![edge("start", "next", "p"), edge("p", "again", "p")]);
        let (_db, mut conn, flow) = setup("runs-max-self", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert!(on_stop(&mut conn, &tab("t1"), Some("FLOW: again")).unwrap().is_some());
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: again")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Attention);
    }

    #[test]
    fn a_run_follows_the_flow_as_it_was_when_started() {
        let g = runnable();
        let (db, mut conn, flow) = setup("runs-snapshot", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        let mut edited = g.clone();
        edited.nodes[1].text = "something else".into();
        db.save_flow_graph(flow, &edited).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn other_tabs_and_finished_runs_are_left_alone() {
        let (_db, mut conn, flow) = setup("runs-other", &runnable());
        assert_eq!(on_stop(&mut conn, &tab("t9"), Some("FLOW: pass")).unwrap(), None);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn).id;
        stop(&conn, run).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), None);
        assert!(pick(&mut conn, run, "fail").unwrap_err().contains("no active run"));
    }

    #[test]
    fn a_flow_that_starts_at_its_end_finishes_at_once() {
        let g = graph(vec![node("e", Kind::End, &[])], vec![edge("start", "next", "e")]);
        let (_db, mut conn, flow) = setup("runs-empty", &g);
        // Nothing to brief Claude for, or to tell it is finished.
        assert_eq!(start(&mut conn, flow, &tab("t1")).unwrap().prompt, None);
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_flow_added_from_a_tab_runs_only_once_confirmed() {
        let path = db::tests::temp_db("runs-unconfirmed");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.import_unconfirmed_flow("Planted", &runnable()).unwrap();
        assert!(flow.unconfirmed);
        let mut conn = db::connect_existing(&path).unwrap();
        assert!(begin(&mut conn, flow.id, &tab("t")).unwrap_err().contains("confirm"));
        db.confirm_flow(flow.id).unwrap();
        assert!(!db.list_flows().unwrap()[0].unconfirmed);
        assert!(begin(&mut conn, flow.id, &tab("t")).is_ok());
    }

    #[test]
    fn a_pick_or_continue_for_a_step_the_run_has_left_is_refused() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-stale-pick", &g);
        let run = begin(&mut conn, flow, &tab("stale")).unwrap();
        // Two clicks on "ok" for the manual step: the second arrives at the prompt step.
        assert!(proceed(&mut conn, run.run_id, "m", "ok").unwrap().is_some());
        let stale = proceed(&mut conn, run.run_id, "m", "done").unwrap_err();
        assert!(stale.contains("moved on"), "{stale}");
        assert_eq!(only_run(&conn).step_id, "p");
        on_stop(&mut conn, &tab("stale"), Some("FLOW: attention")).unwrap();
        assert!(resume(&mut conn, run.run_id, "m").unwrap_err().contains("moved on"));
        assert!(resume(&mut conn, run.run_id, "p").is_ok());
    }

    #[test]
    fn an_unknown_resume_or_clear_state_is_an_error_not_unset() {
        let (_db, mut conn, flow) = setup("runs-unknown-state", &manual_first());
        let run = begin(&mut conn, flow, &tab("odd")).unwrap();
        // As a newer version might store them; the CHECK constraints are dropped for the test.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
        conn.execute("UPDATE flow_runs SET clear = 'later' WHERE id = ?1", [run.run_id]).unwrap();
        assert!(pick(&mut conn, run.run_id, "ok").unwrap_err().contains("later"));
        conn.execute("UPDATE flow_runs SET clear = NULL, resume = 'someday' WHERE id = ?1", [run.run_id]).unwrap();
        assert!(pick(&mut conn, run.run_id, "ok").unwrap_err().contains("someday"));
    }

    #[test]
    fn a_clear_that_never_finishes_makes_the_run_wait_for_you() {
        let mut g = manual_first();
        g.nodes[0].clear = true;
        let (_db, mut conn, flow) = setup("runs-clear-expired", &g);
        let pending = begin(&mut conn, flow, &tab("pending")).unwrap();
        let cleared = begin(&mut conn, flow, &tab("cleared")).unwrap();
        clear_tab(&mut conn, &tab("cleared"), "x").unwrap();
        // Still within the time the app gives the clear: left alone.
        let statuses = |conn: &Connection| active(conn).unwrap().into_iter().map(|r| (r.status, r.outputs.is_empty())).collect::<Vec<_>>();
        assert_eq!(statuses(&conn), [(Status::Running, true), (Status::Running, true)]);
        // The page that would have gone on was reloaded or failed: past the deadline, both wait at Start.
        conn.execute("UPDATE flow_runs SET started_at = started_at - ?1", [CLEAR_TIMEOUT_SECS + 1]).unwrap();
        let runs = active(&conn).unwrap();
        assert_eq!(runs.iter().map(|r| (r.status, r.outputs.clone())).collect::<Vec<_>>(), [
            (Status::Attention, vec![flows::START_OUTPUT.to_string()]),
            (Status::Attention, vec![flows::START_OUTPUT.to_string()]),
        ]);
        assert!(runs[0].note.as_deref().unwrap().contains("30 seconds"), "{runs:?}");
        assert_eq!(cleared_run_state(&mut conn, pending.run_id), Cleared::Gone);
        // Picking Start's output goes on without the clear.
        assert!(pick(&mut conn, cleared.run_id, flows::START_OUTPUT).is_ok());
    }

    fn cleared_run_state(conn: &mut Connection, run: i64) -> Cleared {
        cleared(conn, run).unwrap()
    }
}
