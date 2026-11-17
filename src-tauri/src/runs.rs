//! Flow runs: a flow being worked through in a tab, one at a time per tab.
//!
//! A run starts at the flow's Start step and moves along outputs. It first briefs
//! Claude (the run's rules and Start's base info) and waits at Start for Claude's
//! reply, "Ready", which takes the first step (see `on_stop`); a flow that leads
//! straight to its end skips that. A briefing with no reply within
//! `BRIEFING_TIMEOUT_SECS` (Claude was interrupted, or crashed) waits for the
//! person (see `expire`). A prompt step sends its text to Claude; when Claude ends its turn, the Stop hook
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
//! flow is finished with the person's next prompt (see `RunState::owes_closing`).
//!
//! A flow whose Start clears begins in the tab's new conversation: its run holds
//! the tab at Start while the app types `/clear`, the SessionStart hook reports the
//! clear (`clear_tab`), and the app then sends the briefing (`cleared`). A run
//! still at Start `CLEAR_TIMEOUT_SECS` after it began (the page that would go on
//! was reloaded, or failed) waits for the person instead (see `expire`).
//!
//! Any turn that ends while a step runs counts as its reply, including one the
//! person started by chatting with Claude.
//!
//! The app (through `Db`) and the hook (on its own connection) both call these
//! functions on the same database.

use crate::flows::{self, Graph, Kind, Node};
use crate::layout::TabId;
use rusqlite::{named_params, params, Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

/// A run that has taken this many steps waits for the person, in case it loops.
pub const MAX_RUN_STEPS: i64 = 50;

/// How long a run whose Start clears may stay at Start before it waits for the
/// person: the app's own 30 seconds (`CLEAR_TIMEOUT_MS` in src/main.ts, after which
/// it stops the run, saying why), and a little more, so its stop comes first.
pub const CLEAR_TIMEOUT_SECS: i64 = 35;

/// How long a run waits at Start for Claude's reply to its briefing, which asks
/// only for "Ready", before it waits for the person: Claude was interrupted (which
/// runs no Stop hook) or failed. `expire`'s note gives it in minutes.
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
    /// Claude no longer has the step in mind (the conversation was cleared, or the
    /// step's prompt could not be typed; see `unsent`): send the step's prompt again.
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
    fn as_str(self) -> &'static str {
        match self {
            Clear::Pending => "pending",
            Clear::Done => "done",
        }
    }

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

/// A step's prompt for the app to type into the run's tab. Mirrored by `StepPrompt`
/// in src/runController.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StepPrompt {
    /// The step the run is now at, which `unsent` names if the prompt cannot be typed.
    pub step_id: String,
    pub prompt: String,
    /// Whether the prompt carries the run's rules and Start's base info (Claude
    /// lacked them), which `unsent` then takes back.
    pub briefs: bool,
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

/// What changes about a run as it goes: every column of `flow_runs` but its `id`,
/// the ones fixed when it starts (its flow, tab, steps' copy and start time), and
/// `updated_at`, which `RunState::save` stamps. Every state change loads it, makes the
/// whole new state and saves it in one write (`RunState::save`), in the transaction
/// that read it, so no write depends on another's order. (Stopping, and dropping the
/// note owed for an ended run, stay raw UPDATEs; see `stop_in` and `drop_closing`.)
#[derive(Clone, Debug, PartialEq)]
struct RunState {
    node_id: String,
    status: Status,
    steps: i64,
    /// Why the run waits, when it needs attention.
    note: Option<String>,
    resume: Option<Resume>,
    clear: Option<Clear>,
    /// Whether Claude has the run's rules and Start's base info (see `first_turn` and
    /// `prompt_for`): false until the briefing is sent, and after a clear.
    briefed: bool,
    /// Whether the app has yet to fill Claude's input for the manual prompt.
    prefill: bool,
    /// Names that paste (see `prefilled`).
    prefill_token: Option<i64>,
    /// Whether the run waits at Start for Claude's reply to its briefing, sent at
    /// `briefing_at` (see `expire`).
    briefing: bool,
    briefing_at: Option<i64>,
    /// Whether the ended run has yet to tell Claude its flow is finished (see `RunState::owes_closing`).
    closing: bool,
}

impl RunState {
    /// The columns `from_row` reads.
    const COLUMNS: &str = "node_id, status, steps, note, resume, clear, briefed, prefill, prefill_token, briefing, briefing_at, closing";

    /// The state in `row`'s `COLUMNS`, read by name. The inner error is a value this
    /// version does not know.
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Result<Self, String>> {
        let status: String = row.get("status")?;
        let resume: Option<String> = row.get("resume")?;
        let clear: Option<String> = row.get("clear")?;
        let (node_id, steps, note) = (row.get("node_id")?, row.get("steps")?, row.get("note")?);
        let (briefed, prefill, prefill_token) = (row.get("briefed")?, row.get("prefill")?, row.get("prefill_token")?);
        let (briefing, briefing_at, closing) = (row.get("briefing")?, row.get("briefing_at")?, row.get("closing")?);
        Ok((|| {
            Ok(RunState {
                node_id,
                status: Status::parse(&status)?,
                steps,
                note,
                resume: Resume::parse(resume)?,
                clear: Clear::parse(clear)?,
                briefed,
                prefill,
                prefill_token,
                briefing,
                briefing_at,
                closing,
            })
        })())
    }

    /// The state of run `run_id`, whatever its status; None for no such run.
    fn load(conn: &Connection, run_id: i64) -> Result<Option<Self>, String> {
        Self::load_where(conn, "id = ?1", run_id)
    }

    /// The state of run `run_id` if it is active; None otherwise.
    fn load_active(conn: &Connection, run_id: i64) -> Result<Option<Self>, String> {
        Self::load_where(conn, &format!("id = ?1 AND status IN {ACTIVE}"), run_id)
    }

    fn load_where(conn: &Connection, filter: &str, run_id: i64) -> Result<Option<Self>, String> {
        conn.query_row(&format!("SELECT {} FROM flow_runs WHERE {filter}", Self::COLUMNS), [run_id], Self::from_row)
            .optional()
            .map_err(sql)?
            .transpose()
    }

    /// Makes this run `run_id`'s state, in one write that also stamps `updated_at`.
    /// An error if there is no run `run_id`.
    fn save(&self, conn: &Connection, run_id: i64) -> Result<(), String> {
        let changed = conn
            .execute(
                "UPDATE flow_runs SET node_id = :node_id, status = :status, steps = :steps, note = :note, resume = :resume,
                 clear = :clear, briefed = :briefed, prefill = :prefill, prefill_token = :prefill_token, briefing = :briefing,
                 briefing_at = :briefing_at, closing = :closing, updated_at = :updated_at WHERE id = :id",
                named_params! {
                    ":id": run_id,
                    ":node_id": self.node_id,
                    ":status": self.status.as_str(),
                    ":steps": self.steps,
                    ":note": self.note,
                    ":resume": self.resume.map(Resume::as_str),
                    ":clear": self.clear.map(Clear::as_str),
                    ":briefed": self.briefed,
                    ":prefill": self.prefill,
                    ":prefill_token": self.prefill_token,
                    ":briefing": self.briefing,
                    ":briefing_at": self.briefing_at,
                    ":closing": self.closing,
                    ":updated_at": now(),
                },
            )
            .map_err(sql)?;
        if changed != 1 {
            return Err(format!("run {run_id} could not be saved: there is no such run"));
        }
        Ok(())
    }

    /// The run at `node_id` with `status`, `steps` and `note`, owing nothing there
    /// yet: no Continue, clear, paste or briefing (nor its time). What Claude has
    /// (`briefed`), the last paste's token, and the `closing` flag of an ended run
    /// carry over.
    fn moved(&self, node_id: &str, status: Status, steps: i64, note: Option<String>) -> Self {
        RunState {
            node_id: node_id.to_owned(),
            status,
            steps,
            note,
            resume: None,
            clear: None,
            briefed: self.briefed,
            prefill: false,
            prefill_token: self.prefill_token,
            briefing: false,
            briefing_at: None,
            closing: self.closing,
        }
    }

    /// The run waiting at `node_id` for the person, saying why, and how Continue goes
    /// on from there (None: it cannot help, and the person picks an output).
    fn waiting(&self, node_id: &str, steps: i64, why: &str, resume: Option<Resume>) -> Self {
        RunState { resume, ..self.moved(node_id, Status::Attention, steps, Some(why.to_owned())) }
    }

    /// The run waiting where it is for the person, saying why, and how Continue goes on
    /// from there (see `waiting`); the rest of its state carries over.
    fn waiting_here(&self, why: &str, resume: Option<Resume>) -> Self {
        RunState { status: Status::Attention, note: Some(why.to_owned()), resume, ..self.clone() }
    }

    /// The run, at Start reading its briefing, waiting there for the person, saying
    /// why: Claude may not have read the briefing, so the first step carries it again,
    /// and Continue has no step prompt to send (see `continue_offered`).
    fn briefing_lost(&self, why: &str) -> Self {
        RunState { briefed: false, briefing: false, briefing_at: None, ..self.waiting_here(why, None) }
    }

    /// The run, still at Start waiting for its clear, waiting there for the person
    /// to go on without it, saying why.
    fn clear_lost(&self, why: &str) -> Self {
        RunState { clear: None, ..self.waiting_here(why, None) }
    }

    /// The run done at `node_id`; Claude, if it has the run's rules, is told it no
    /// longer needs to end its replies with a FLOW line, with the person's next
    /// prompt (see `RunState::owes_closing`).
    fn finished(&self, node_id: &str, steps: i64) -> Self {
        RunState { closing: self.closing || self.owes_closing(), ..self.moved(node_id, Status::Done, steps, None) }
    }

    /// Whether the run, ending, owes Claude the note that its flow is finished: Claude
    /// has the run's rules (`briefed`; not after a clear, or while the run clears at
    /// its start). Nothing is typed and no turn is spent: stopping must not send into
    /// a turn or a permission prompt. `OWES_CLOSING` says the same in SQL.
    fn owes_closing(&self) -> bool {
        self.briefed && self.clear.is_none()
    }
}

/// An active run as stored.
struct Stored {
    id: i64,
    graph: Graph,
    state: RunState,
}

impl Stored {
    /// Fails unless the run is at `step_id`, the step the person saw it at.
    fn at(&self, step_id: &str) -> Result<(), String> {
        if self.state.node_id == step_id {
            return Ok(());
        }
        let name = |id: &str| self.graph.nodes.iter().find(|n| n.id == id).map_or(id.to_owned(), |n| n.name().to_owned());
        Err(format!("the run has moved on from \"{}\" to \"{}\"; pick again for where it is now", name(step_id), name(&self.state.node_id)))
    }

    fn node(&self) -> Result<&Node, String> {
        let id = &self.state.node_id;
        self.graph.nodes.iter().find(|n| &n.id == id).ok_or_else(|| format!("the run is at a missing step {id:?}"))
    }

    /// Saves `state` as the run's; skips the write (and its `updated_at` stamp) when
    /// `state` is what the run already has.
    fn save(&self, conn: &Connection, state: &RunState) -> Result<(), String> {
        if *state == self.state {
            return Ok(());
        }
        state.save(conn, self.id)
    }
}

/// The active run matching `filter` (`tab_id = ?1` or `id = ?1`).
fn active_run(conn: &Connection, filter: &str, key: &dyn rusqlite::ToSql) -> Result<Option<Stored>, String> {
    let row = conn
        .query_row(
            &format!("SELECT id, graph, {} FROM flow_runs WHERE {filter} AND status IN {ACTIVE}", RunState::COLUMNS),
            [key],
            |r| Ok((r.get::<_, i64>("id")?, r.get::<_, String>("graph")?, RunState::from_row(r)?)),
        )
        .optional()
        .map_err(sql)?;
    row.map(|(id, graph, state)| Ok(Stored { id, graph: parse_graph(&graph)?, state: state? })).transpose()
}

/// Makes the run wait at `node_id` for the person (see `RunState::waiting`).
fn wait_for_person<T>(tx: &Transaction, run: &Stored, node_id: &str, steps: i64, why: &str, resume: Option<Resume>) -> Result<Option<T>, String> {
    run.state.waiting(node_id, steps, why, resume).save(tx, run.id)?;
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
    Ok(earlier + i64::from(run.state.node_id == step))
}

/// `RunState::owes_closing` in SQL, for `stop`, which works on the row itself.
const OWES_CLOSING: &str = "briefed = 1 AND clear IS NULL";

/// Drops the note owed to Claude in tab `tab_id` that a flow is finished. Kept as
/// one UPDATE of that column alone: it touches ended runs, which may hold states
/// this version cannot read.
fn drop_closing(conn: &Connection, tab_id: &TabId) -> Result<(), String> {
    conn.execute("UPDATE flow_runs SET closing = 0 WHERE tab_id = ?1 AND closing = 1", [tab_id.as_str()]).map(drop).map_err(sql)
}

/// The name of run `run_id`'s flow, for Claude.
fn flow_name(conn: &Connection, run_id: i64) -> Result<String, String> {
    conn.query_row("SELECT f.name FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.id = ?1", [run_id], |r| r.get(0))
        .map_err(sql)
}

/// The prompt of `node` for the run, led by the run's rules and Start's base info
/// if Claude lacks them (after a clear), which it then has (in `state`, the run's
/// state to be).
fn prompt_for(tx: &Transaction, run: &Stored, state: &mut RunState, node: &Node) -> Result<StepPrompt, String> {
    let briefs = !state.briefed;
    let prompt = if briefs { flows::first_prompt_text(&run.graph, &flow_name(tx, run.id)?, node) } else { flows::prompt_text(node) };
    state.briefed = true;
    Ok(StepPrompt { step_id: node.id.clone(), prompt, briefs })
}

/// The run's first turn, from Start: its briefing (the run's rules and Start's base
/// info), the run then waiting at Start for Claude's reply (see `on_stop`); or, for a
/// flow that leads straight to its end, nothing, the run being done. Returns what to
/// send Claude.
fn first_turn(tx: &Transaction, run: &Stored, start: &Node) -> Result<Option<String>, String> {
    let next = flows::next_step(&run.graph, &start.id, flows::START_OUTPUT);
    if next.is_none_or(|n| n.kind == Kind::End) {
        return go(tx, run, start, flows::START_OUTPUT, 0, By::Flow).map(texts);
    }
    let briefing = RunState {
        briefed: true,
        briefing: true,
        briefing_at: Some(now()),
        ..run.state.moved(&start.id, Status::Running, 0, None)
    };
    briefing.save(tx, run.id)?;
    Ok(Some(flows::briefing_text(&run.graph, &flow_name(tx, run.id)?)))
}

/// Moves the run from `from` along `output` to the step it leads to, if any, with
/// the run's step count at `steps`; returns the prompt to send when that is a
/// prompt step (a run that ends sends nothing; see `RunState::finished`). The run instead waits at `from` for the person when going on
/// would pass `MAX_RUN_STEPS` (into a prompt step) or, for `By::Flow` only, when
/// the next step has already run its `max_runs`.
fn go(tx: &Transaction, run: &Stored, from: &Node, output: &str, steps: i64, by: By) -> Result<Option<StepPrompt>, String> {
    let next = flows::next_step(&run.graph, &from.id, output);
    // Going on with `from` would only redo it and stop here again.
    let wait = |why: String| wait_for_person(tx, run, &from.id, steps, &why, None);
    if next.is_some_and(|n| n.kind == Kind::Prompt) && steps >= MAX_RUN_STEPS {
        return wait(format!("The run has taken {MAX_RUN_STEPS} steps without you; pick an output to go on."));
    }
    if let (Some(next), By::Flow) = (next, by)
        && let Some(max) = next.max_runs
        && runs_of(tx, run, &next.id)? >= i64::from(max)
    {
        let name = next.name();
        let times = if max == 1 { "once".to_string() } else { format!("{max} times") };
        return wait(format!(
            "\"{name}\" has already run {times}, its limit. Pick \"{output}\" to run it once more, or another output."
        ));
    }
    left(tx, run.id, &from.id, output)?;
    let (state, prompt) = match next {
        // The output leads nowhere: the flow is done.
        None => (run.state.finished(&from.id, steps), None),
        Some(next) => match next.kind {
            Kind::Prompt => {
                let mut state = run.state.moved(&next.id, Status::Running, steps + 1, None);
                let prompt = prompt_for(tx, run, &mut state, next)?;
                (state, Some(prompt))
            }
            Kind::Pause => (run.state.moved(&next.id, Status::Manual, steps, None), None),
            Kind::ManualPrompt => {
                let mut state = run.state.moved(&next.id, Status::Manual, steps, None);
                owe_prefill(tx, &mut state, &run.graph, next)?;
                (state, None)
            }
            Kind::End | Kind::Start => (run.state.finished(&next.id, steps), None),
        },
    };
    state.save(tx, run.id)?;
    Ok(prompt)
}

/// The text of the prompt `go` returned, for where no step id goes along.
fn texts(next: Option<StepPrompt>) -> Option<String> {
    next.map(|n| n.prompt)
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
    let state = RunState::load(&tx, run_id)?.ok_or("the new run is gone")?;
    let prompt = first_turn(&tx, &Stored { id: run_id, graph, state }, &start)?;
    tx.commit().map_err(sql)?;
    Ok(Started { run_id, clear, prompt })
}

/// How run `run_id`, which clears at its start, is getting on; once Claude has
/// cleared, gives the briefing (once: later calls are `Gone`).
pub fn cleared(conn: &mut Connection, run_id: i64) -> Result<Cleared, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "id = ?1", &run_id)? else { return Ok(Cleared::Gone) };
    match run.state.clear {
        None => Ok(Cleared::Gone),
        Some(Clear::Pending) => Ok(Cleared::Pending),
        Some(Clear::Done) => {
            let prompt = first_turn(&tx, &run, run.node()?)?;
            tx.commit().map_err(sql)?;
            Ok(Cleared::Ready { prompt })
        }
    }
}

/// Whether Continue is offered for a run with `status` at a step of `kind`
/// (see `resume`): only at a prompt step that needs attention, where Claude
/// going on can help. A reply that ends with one of the step's outputs moves
/// such a run on too (see `on_stop`).
fn continue_offered(status: Status, kind: Kind, resume: Option<Resume>) -> bool {
    status == Status::Attention && kind == Kind::Prompt && resume.is_some()
}

/// Why a run waits when Claude Code does not report Claude's replies.
const NO_REPLY: &str = "Claude Code did not report Claude's reply to Mischief of Agents; update Claude Code to run flows.";

/// Claude in tab `tab_id` ended its turn with `reply` (None when Claude Code did
/// not report it): moves the tab's run on. Returns the next prompt to send, if
/// any. A tab with no run is left alone. A run that waits for the person at a
/// prompt step is left alone too, unless Claude's reply ends with one of the
/// step's outputs and Continue is offered there (see `continue_offered`); one
/// that waits at Start or a manual prompt always is. A manual prompt the person
/// sent goes on by its one output, whatever the reply.
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
    let state = &run.state;
    // A turn that ends before the clear (one Claude was in when ▶ was pressed) is
    // not the reply to any step.
    if state.clear.is_some() {
        return Ok(None);
    }
    let node = run.node()?.clone();
    if node.kind == Kind::Start {
        // Only Claude's reply to the run's briefing ends it (a turn after a clear or a
        // restart is not one): "Ready" takes the first step. A reply that did more,
        // as Claude may start on the job, waits for the person to look first.
        if state.status != Status::Running || !state.briefing {
            return Ok(None);
        }
        let prompt = match reply {
            None => wait_for_person(&tx, &run, &node.id, 0, NO_REPLY, None)?,
            Some(reply) if !flows::says_ready(reply) => wait_for_person(
                &tx,
                &run,
                &node.id,
                0,
                "Claude did more than reply \"Ready\" to the flow's briefing. Read its reply, then pick an output to go on, or stop the run.",
                None,
            )?,
            Some(_) => go(&tx, &run, &node, flows::START_OUTPUT, 0, By::Flow)?,
        };
        tx.commit().map_err(sql)?;
        return Ok(texts(prompt));
    }
    if node.kind == Kind::ManualPrompt {
        // Running only once the person sent their prompt (see `on_prompt`): this is
        // Claude's answer to it. Waiting for the person, any turn is left alone.
        let Some(output) = node.outputs.first().filter(|_| state.status == Status::Running) else {
            return Ok(None);
        };
        // The person stepped in, which restarts the count toward MAX_RUN_STEPS.
        let prompt = go(&tx, &run, &node, output, 0, By::Flow)?;
        tx.commit().map_err(sql)?;
        return Ok(texts(prompt));
    }
    if state.status == Status::Attention {
        // The person may be chatting with Claude while the run waits. Only a reply
        // that ends with one of the step's outputs moves it on, and only where
        // Continue is offered: at a step limit only a pick passes the wait. Any other
        // turn leaves the wait, its note and Continue as they were.
        let Some(output) = reply
            .and_then(flows::parse_marker)
            .filter(|m| continue_offered(state.status, node.kind, state.resume) && m != flows::ATTENTION && node.outputs.contains(m))
        else {
            return Ok(None);
        };
        let prompt = go(&tx, &run, &node, &output, state.steps, By::Flow)?;
        tx.commit().map_err(sql)?;
        return Ok(texts(prompt));
    }
    let wait = |why: String| wait_for_person(&tx, &run, &node.id, state.steps, &why, Some(Resume::Remind));
    let Some(reply) = reply else {
        // Its next reply would not be reported either.
        wait_for_person::<()>(&tx, &run, &node.id, state.steps, NO_REPLY, None)?;
        tx.commit().map_err(sql)?;
        return Ok(None);
    };
    let prompt = match flows::parse_marker(reply) {
        None => wait("Claude's reply did not end with a FLOW line, so the flow cannot tell how to go on.".into()),
        Some(m) if m == flows::ATTENTION => wait("Claude asked for a person; see its last reply.".into()),
        Some(m) if !node.outputs.contains(&m) => wait(format!(
            "Claude ended with \"FLOW: {m}\", which is not an output of \"{}\".",
            node.name()
        )),
        Some(m) => go(&tx, &run, &node, &m, state.steps, By::Flow),
    }?;
    tx.commit().map_err(sql)?;
    Ok(texts(prompt))
}

/// The person picked `output` for `step_id`, the step they saw the run at; returns
/// the next prompt to send, if any. Usually the run waits (manual or needs
/// attention), but a running step can be moved on too, for when Claude was
/// interrupted and no reply will come. Refused if the run has moved on from
/// `step_id` (a second click, or a pick from a page not yet redrawn), and while the
/// run is still clearing the conversation before its first step. The person
/// stepping in restarts the count toward `MAX_RUN_STEPS`.
pub fn proceed(conn: &mut Connection, run_id: i64, step_id: &str, output: &str) -> Result<Option<StepPrompt>, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let run = active_run(&tx, "id = ?1", &run_id)?.ok_or_else(|| format!("there is no active run {run_id}"))?;
    run.at(step_id)?;
    if run.state.clear.is_some() {
        return Err("the run is clearing the conversation before its first step; wait for it, or stop the run".into());
    }
    if run.state.briefing {
        // The first step would land behind the briefing, whose reply would be taken for the step's.
        return Err("Claude is reading the flow's briefing; wait for its reply, or stop the run".into());
    }
    let node = run.node()?.clone();
    if !node.outputs.iter().any(|o| o == output) {
        return Err(format!("\"{}\" has no output {output:?}", node.name()));
    }
    let next = go(&tx, &run, &node, output, 0, By::Person)?;
    tx.commit().map_err(sql)?;
    Ok(next)
}

/// Has the app put the manual prompt `node`'s text into Claude's input (see
/// `prefill_text`), if there is any: Start's base info unless Claude has it
/// (`briefed`), then the step's draft. Each time with a new token, so the record of
/// an earlier paste cannot cancel it (see `prefilled`).
fn owe_prefill(conn: &Connection, state: &mut RunState, graph: &Graph, node: &Node) -> Result<(), String> {
    if flows::prefill_text(graph, node, !state.briefed).is_empty() {
        return Ok(());
    }
    // Kept within what JavaScript numbers hold exactly.
    let token = conn.query_row("SELECT abs(random() % 9007199254740991)", [], |r| r.get(0)).map_err(sql)?;
    state.prefill = true;
    state.prefill_token = Some(token);
    Ok(())
}

/// Whether the run waits for the person at a manual prompt (`node`, where it is):
/// for their prompt, or, once Claude's answer to it was lost, for it again or an
/// output.
fn awaits_prompt(state: &RunState, node: &Node) -> bool {
    node.kind == Kind::ManualPrompt && matches!(state.status, Status::Manual | Status::Attention)
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
    let state = &run.state;
    if state.node_id != step_id || !state.prefill || !awaits_prompt(state, run.node()?) {
        return Ok(None);
    }
    let text = flows::prefill_text(&run.graph, run.node()?, !state.briefed);
    Ok(Some(Prefill { text, token: state.prefill_token.unwrap_or(0) }).filter(|p| !p.text.is_empty()))
}

/// The app put the manual prompt's text named by `token` into Claude's input (see
/// `prefill_text`). A paste owed since (after a clear, say) stays owed. Claude has
/// the base info only once the person sends it (see `on_prompt`).
pub fn prefilled(conn: &mut Connection, run_id: i64, step_id: &str, token: i64) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(state) = RunState::load_active(&tx, run_id)? else { return Ok(()) };
    if state.node_id != step_id || state.prefill_token != Some(token) {
        return Ok(());
    }
    RunState { prefill: false, ..state }.save(&tx, run_id)?;
    tx.commit().map_err(sql)
}

/// The person sent `prompt` to Claude in tab `tab_id` (the UserPromptSubmit hook):
/// a run waiting at a manual prompt runs it, so Claude's answer moves it on (see
/// `on_stop`). Claude has Start's base info if the prompt carried it. A slash
/// command is not a prompt to Claude, and prompts at any other step change nothing.
/// Returns what to add to Claude's context with the prompt: that a flow which ran
/// in the tab is finished, owed since it ended (see `RunState::owes_closing`).
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
    // one, as a new run drops it). Like `drop_closing`, one UPDATE of that column
    // alone, on an ended run that may hold states this version cannot read.
    let finished: Option<i64> = tx
        .query_row("UPDATE flow_runs SET closing = 0 WHERE tab_id = ?1 AND closing = 1 RETURNING id", [tab_id.as_str()], |r| r.get(0))
        .optional()
        .map_err(sql)?;
    let closing = finished.map(|run| flow_name(&tx, run)).transpose()?.map(|name| flows::closing_text(&name));
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? {
        let node = run.node()?;
        if awaits_prompt(&run.state, node) {
            // The prompt may carry Start's base info, but never the run's rules: after a
            // clear, the next prompt step still briefs Claude again.
            run.state.moved(&node.id, Status::Running, run.state.steps, None).save(&tx, run.id)?;
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
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? else { return Ok(()) };
    if run.state.status != Status::Manual || run.node()?.kind != Kind::ManualPrompt {
        return Ok(());
    }
    run.state.waiting_here(why, None).save(&tx, run.id)?;
    tx.commit().map_err(sql)
}

/// Claude's answer to the person's manual prompt `run` runs (its state being
/// `state`) will not be seen (`why`: the conversation was cleared, misag
/// restarted): the run waits for the prompt again, or an output. Continue has no
/// step prompt to remind Claude of. None unless `state` is running (no clear
/// pending) a manual prompt.
fn answer_lost(run: &Stored, state: &RunState, why: &str) -> Result<Option<RunState>, String> {
    if state.status != Status::Running || state.clear.is_some() || run.node()?.kind != Kind::ManualPrompt {
        return Ok(None);
    }
    let note = format!("{why} Claude's answer to your prompt will not be seen: send your prompt again, or pick an output to go on.");
    Ok(Some(state.waiting_here(&note, None)))
}

/// `state` for `run`, owing the app a fill of Claude's input again if the run waits
/// at a manual prompt, as that input was lost (a clear, a restart): with the base
/// info, if Claude lacks it now. Otherwise `state` as it is.
fn refill(conn: &Connection, run: &Stored, mut state: RunState) -> Result<RunState, String> {
    let node = run.node()?;
    if awaits_prompt(&state, node) {
        owe_prefill(conn, &mut state, &run.graph, node)?;
    }
    Ok(state)
}

/// The person has seen to a prompt step that needs attention (answered Claude, or
/// fixed what it asked for): the run goes back to running the step, and the
/// returned text has Claude go on with it (see `Resume`), logged in
/// `flow_run_resumes`. Refused where that cannot help, as at a step limit, where the
/// person picks an output instead, and if the run has moved on from `step_id` (see
/// `proceed`).
pub fn resume(conn: &mut Connection, run_id: i64, step_id: &str) -> Result<StepPrompt, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let run = active_run(&tx, "id = ?1", &run_id)?.ok_or_else(|| format!("there is no active run {run_id}"))?;
    run.at(step_id)?;
    let node = run.node()?;
    if run.state.status != Status::Attention || node.kind != Kind::Prompt {
        return Err(format!("\"{}\" does not need attention; pick an output instead", node.name()));
    }
    let Some(how) = run.state.resume.filter(|_| continue_offered(run.state.status, node.kind, run.state.resume)) else {
        return Err(format!("Claude going on with \"{}\" would not help; pick an output instead", node.name()));
    };
    // Logged with why it waited, which going on clears.
    tx.execute(
        "INSERT INTO flow_run_resumes (run_id, node_id, note, at) VALUES (?1, ?2, ?3, ?4)",
        params![run.id, node.id, run.state.note, now()],
    )
    .map_err(sql)?;
    let mut state = run.state.moved(&node.id, Status::Running, run.state.steps, None);
    let next = match how {
        Resume::Remind => StepPrompt { step_id: node.id.clone(), prompt: flows::resume_text(node), briefs: false },
        // With the rules and base info, if Claude lacks them (after a clear, or an
        // unsent prompt that carried them).
        Resume::Prompt => prompt_for(&tx, &run, &mut state, node)?,
    };
    state.save(&tx, run.id)?;
    tx.commit().map_err(sql)?;
    Ok(next)
}

/// The prompt `proceed` or `resume` returned for step `step_id` could not be typed
/// (Claude left the tab first): the run would wait for a reply that cannot come, so
/// it waits for the person, saying `why`, and Continue sends the step's prompt again.
/// If that prompt carried the run's rules (`briefs`), Claude never got them, so the
/// one Continue sends carries them again; otherwise Claude, resumed in the tab,
/// still has them. Returns whether the run now waits: one that has moved on or no
/// longer runs the step (a Stop hook got there first) is left alone.
pub fn unsent(conn: &mut Connection, run_id: i64, step_id: &str, briefs: bool, why: &str) -> Result<bool, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "id = ?1", &run_id)? else { return Ok(false) };
    if run.state.node_id != step_id || run.state.status != Status::Running {
        return Ok(false);
    }
    let waiting = run.state.waiting(step_id, run.state.steps, why, Some(Resume::Prompt));
    RunState { briefed: waiting.briefed && !briefs, ..waiting }.save(&tx, run.id)?;
    tx.commit().map_err(sql)?;
    Ok(true)
}

/// Claude in tab `tab_id` cleared its conversation: a run waiting for that clear
/// can send its briefing (see `cleared`); a running step will get no reply, so
/// its run waits for the person, saying `why` (see `pause_tab`). Either way, the
/// run's next prompt carries its rules and Start's base info again. A finished
/// run's note is dropped, as Claude no longer has the flow's rules.
pub fn clear_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    drop_closing(&tx, tab_id)?;
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? {
        let lost = RunState { briefed: false, ..run.state.clone() };
        let state = if lost.status == Status::Running && lost.clear == Some(Clear::Pending) {
            RunState { clear: Some(Clear::Done), ..lost }
        } else {
            pause(&run, lost, why)?
        };
        run.save(&tx, &refill(&tx, &run, state)?)?;
    }
    tx.commit().map_err(sql)
}

/// Makes the tab's running run wait for the person, saying why: Claude will not
/// reply to its step (the conversation was cleared, or the hook failed), so
/// Continue sends the step's prompt again, except while the run reads its briefing,
/// where the person picks an output. A run still at Start waiting for its clear has
/// no step to reply to, and is left alone.
pub fn pause_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? {
        run.save(&tx, &pause(&run, run.state.clone(), why)?)?;
    }
    tx.commit().map_err(sql)
}

/// `state` for `run` once Claude will not reply to its running step (see
/// `pause_tab`); unchanged unless it runs a step Claude was to reply to (see
/// `answer_lost` for a manual prompt).
fn pause(run: &Stored, state: RunState, why: &str) -> Result<RunState, String> {
    if let Some(lost) = answer_lost(run, &state, why)? {
        return Ok(lost);
    }
    if state.status != Status::Running || state.clear.is_some() {
        return Ok(state);
    }
    Ok(if state.briefing { state.briefing_lost(why) } else { state.waiting_here(why, Some(Resume::Prompt)) })
}

/// Stops the active run of tab `tab_id`, if it has one, owing Claude nothing, as
/// the tab is gone. Stopping stays one UPDATE of the status rather than a
/// `RunState` save: it must work on a run this version cannot read (its steps, or
/// a resume or clear state from a newer version), which `RunState::load` refuses.
fn stop_in(conn: &Connection, tab_id: &str) -> Result<(), String> {
    conn.execute(
        &format!("UPDATE flow_runs SET status = 'stopped', updated_at = ?2 WHERE tab_id = ?1 AND status IN {ACTIVE}"),
        params![tab_id, now()],
    )
    .map(drop)
    .map_err(sql)
}

/// Stops the tab's active run, if it has one: the tab is closing.
pub fn stop_tab(conn: &Connection, tab_id: &TabId) -> Result<(), String> {
    stop_in(conn, tab_id.as_str())
}

/// At startup: stops the runs of tabs that are no longer open, and makes running
/// ones wait, as their Claude was stopped with misag and will not reply. A run this
/// version cannot read is logged and left for the person to stop (see `active`),
/// without holding up the others.
pub fn after_restart(conn: &mut Connection, open_tabs: &[TabId]) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    // One per tab (see `ACTIVE`).
    let runs: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(&format!("SELECT id, tab_id FROM flow_runs WHERE status IN {ACTIVE}")).map_err(sql)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?
    };
    for (id, tab) in runs {
        if !open_tabs.iter().any(|open| open.as_str() == tab) {
            stop_in(&tx, &tab)?;
            continue;
        }
        let restarted = active_run(&tx, "id = ?1", &id)
            .and_then(|run| run.map(|run| Ok::<_, String>((restarted(&tx, &run)?, run))).transpose());
        match restarted {
            Ok(Some((state, run))) => run.save(&tx, &state)?,
            Ok(None) => {}
            Err(e) => eprintln!("misag: run {id} was left as it was after the restart, as {e}"),
        }
    }
    tx.commit().map_err(sql)
}

/// `run`'s state once misag has restarted (see `after_restart`).
fn restarted(conn: &Connection, run: &Stored) -> Result<RunState, String> {
    let state = &run.state;
    let state = if state.status != Status::Running {
        state.clone()
    } else if state.clear.is_some() {
        // The app that would have sent the briefing after the clear is gone.
        state.clear_lost("Mischief of Agents was restarted before the conversation was cleared. Pick an output to go on without clearing, or stop the run and press ▶ again.")
    } else if let Some(lost) = answer_lost(run, state, "Mischief of Agents was restarted while Claude answered your prompt.")? {
        lost
    } else if state.briefing {
        state.briefing_lost("Mischief of Agents was restarted while Claude read the flow's briefing. Pick an output to go on; the first step briefs Claude again.")
    } else {
        // Claude comes back in its conversation, so it can be reminded.
        state.waiting_here(
            "Mischief of Agents was restarted while Claude worked on this step, so its reply was not seen. Pick an output to go on.",
            Some(Resume::Remind),
        )
    };
    refill(conn, run, state)
}

/// Stops run `run_id`; Claude is then told the flow is finished with the person's
/// next prompt (see `RunState::owes_closing`). A run whose steps cannot be read can
/// still be stopped, so this stays one UPDATE, as `stop_in` does.
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

/// Makes the runs that have timed out at Start wait for the person: one still
/// waiting for its clear `CLEAR_TIMEOUT_SECS` after it began (the app no longer
/// sees it through), as after a restart; and one whose briefing has had no reply
/// `BRIEFING_TIMEOUT_SECS` after it was sent. The app calls this before each look
/// at the runs (see `flow_runs`), so a run waits for you a few seconds after its
/// time is up. Takes the write lock only when a run it can read has timed out. A
/// run whose state this version cannot read is logged once and left to be stopped.
pub fn expire(conn: &mut Connection) -> Result<(), String> {
    // Look before taking the write lock.
    let timed_out = expired(conn)?;
    if !timed_out.into_iter().any(|(id, _, state)| readable(id, state).is_some()) {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    for (id, clear_expired, state) in expired(&tx)? {
        let Some(state) = readable(id, state) else { continue };
        let state = if clear_expired {
            // The 30 seconds are the app's own timeout, which the person sees, not the
            // longer `CLEAR_TIMEOUT_SECS`.
            state.clear_lost("Claude did not clear the conversation within 30 seconds. Pick an output to go on without clearing, or stop the run and press ▶ again.")
        } else {
            state.briefing_lost(&format!(
                "Claude did not reply to the flow's briefing within {} minutes (it may have been interrupted). Pick an output to go on; the first step briefs Claude again.",
                BRIEFING_TIMEOUT_SECS / 60
            ))
        };
        state.save(&tx, id)?;
    }
    tx.commit().map_err(sql)
}

/// A run that has timed out at Start: its id, whether its clear (rather than its
/// briefing) timed out, and its state, if this version can read it.
type Expired = (i64, bool, Result<RunState, String>);

/// The runs that have timed out at Start (see `expire`).
fn expired(conn: &Connection) -> Result<Vec<Expired>, String> {
    const CLEAR_EXPIRED: &str = "status = 'running' AND clear IS NOT NULL AND started_at <= ?1 - ?2";
    const BRIEFING_EXPIRED: &str = "status = 'running' AND briefing = 1 AND briefing_at <= ?1 - ?3";
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id, ({CLEAR_EXPIRED}) AS clear_expired, {} FROM flow_runs WHERE ({CLEAR_EXPIRED}) OR ({BRIEFING_EXPIRED})",
            RunState::COLUMNS
        ))
        .map_err(sql)?;
    stmt.query_map(params![now(), CLEAR_TIMEOUT_SECS, BRIEFING_TIMEOUT_SECS], |r| {
        Ok((r.get("id")?, r.get("clear_expired")?, RunState::from_row(r)?))
    })
    .map_err(sql)?
    .collect::<Result<_, _>>()
    .map_err(sql)
}

/// The state of timed-out run `id`, or None, logged once per process (`expire` runs
/// on every look at the runs), if this version cannot read it.
fn readable(id: i64, state: Result<RunState, String>) -> Option<RunState> {
    static LOGGED: Mutex<BTreeSet<i64>> = Mutex::new(BTreeSet::new());
    state
        .inspect_err(|e| {
            if LOGGED.lock().unwrap_or_else(PoisonError::into_inner).insert(id) {
                eprintln!("misag: run {id} has timed out at Start, but {e}");
            }
        })
        .ok()
}

/// Every active run, oldest first. Writes nothing: timeouts are `expire`'s, so
/// call that first when they must be applied.
pub fn active(conn: &Connection) -> Result<Vec<Run>, String> {
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
                    let resumable = continue_offered(status, node.kind, resume);
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
        proceed(conn, run, &step, output).map(|next| next.map(|n| n.prompt))
    }

    /// `resume` at the step the run is at.
    fn go_on(conn: &mut Connection, run: i64) -> Result<String, String> {
        let step = at(conn, run);
        resume(conn, run, &step).map(|n| n.prompt)
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
        conn.execute("UPDATE flow_runs SET briefing_at = briefing_at - ?1", [BRIEFING_TIMEOUT_SECS + 1]).unwrap();
        expire(&mut conn).unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.briefing, waiting.outputs.clone()), (Status::Attention, false, vec!["next".to_string()]));
        assert!(waiting.note.as_deref().unwrap().contains("briefing"), "{:?}", waiting.note);
        assert_eq!(pick(&mut conn, run, "next").unwrap(), Some(flows::first_prompt_text(&g, "F", &g.nodes[1])));
    }

    #[test]
    fn a_write_to_a_run_reading_its_briefing_does_not_restart_the_briefing_s_time() {
        let (_db, mut conn, flow) = setup("runs-briefing-timeout-touched", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        // The briefing went out long enough ago; something else wrote to the run just now.
        conn.execute("UPDATE flow_runs SET briefing_at = briefing_at - ?1, updated_at = ?2", params![BRIEFING_TIMEOUT_SECS + 1, now()])
            .unwrap();
        expire(&mut conn).unwrap();
        assert_eq!(only_run(&conn).status, Status::Attention);
    }

    #[test]
    fn listing_the_runs_writes_nothing_even_when_a_run_has_timed_out() {
        let (_db, mut conn, flow) = setup("runs-active-read-only", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        conn.execute("UPDATE flow_runs SET briefing_at = briefing_at - ?1", [BRIEFING_TIMEOUT_SECS + 1]).unwrap();
        let before = conn.total_changes();
        assert_eq!(only_run(&conn).status, Status::Running);
        assert_eq!(conn.total_changes(), before);
    }

    #[test]
    fn expiring_with_nothing_timed_out_does_not_wait_for_another_writer() {
        let path = db::tests::temp_db("runs-expire-no-lock");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        db.save_flow_graph(flow, &runnable()).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        start(&mut conn, flow, &tab("t1")).unwrap();
        // The hook, say, holds the write lock.
        let writer = db::connect_existing(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let begun = std::time::Instant::now();
        expire(&mut conn).unwrap();
        assert!(begun.elapsed() < std::time::Duration::from_secs(1), "{:?}", begun.elapsed());
        writer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(only_run(&conn).status, Status::Running);
    }

    #[test]
    fn an_expired_run_this_version_cannot_read_is_left_alone_and_takes_no_lock() {
        let path = db::tests::temp_db("runs-expire-unreadable");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        db.save_flow_graph(flow, &runnable()).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        let readable = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let odd = start(&mut conn, flow, &tab("t2")).unwrap().run_id;
        // As a newer version might store it; the CHECK constraints are dropped for the test.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
        conn.execute("UPDATE flow_runs SET resume = 'someday' WHERE id = ?1", [odd]).unwrap();
        conn.execute("UPDATE flow_runs SET briefing_at = briefing_at - ?1", [BRIEFING_TIMEOUT_SECS + 1]).unwrap();
        let row = |conn: &Connection| -> (String, Option<String>, bool, i64) {
            conn.query_row("SELECT status, note, briefing, updated_at FROM flow_runs WHERE id = ?1", [odd], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
        };
        let before = row(&conn);
        expire(&mut conn).unwrap();
        assert_eq!(state(&conn, readable).status, Status::Attention);
        assert_eq!(row(&conn), before);
        // With only the unreadable run timed out, the poll does not wait for the write lock.
        let writer = db::connect_existing(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let begun = std::time::Instant::now();
        expire(&mut conn).unwrap();
        assert!(begun.elapsed() < std::time::Duration::from_secs(1), "{:?}", begun.elapsed());
        writer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(row(&conn), before);
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
        for (i, (reply, why)) in [
            ("I changed some files.", "FLOW"),
            ("FLOW: maybe", "maybe"),
            ("Which database should I use?\nFLOW: attention", "asked for a person"),
        ]
        .into_iter()
        .enumerate()
        {
            let (_db, mut conn, flow) = setup(&format!("runs-attention-{i}"), &g);
            begin(&mut conn, flow, &tab("t1")).unwrap();
            assert_eq!(on_stop(&mut conn, &tab("t1"), Some(reply)).unwrap(), None);
            let run = only_run(&conn);
            assert_eq!(run.status, Status::Attention, "{reply:?}");
            assert!(run.note.as_deref().unwrap_or("").contains(why), "{:?} should mention {why:?}", run.note);
            assert!(run.resumable, "{reply:?}");
        }
        // The person answers Claude in the terminal; replies that do not end with one of the step's outputs
        // leave the wait as it was, and the first with one moves the run on.
        on_stop(&mut conn, &tab("t1"), Some("I changed some files.")).unwrap();
        let waiting = only_run(&conn);
        for reply in [Some("Which one?"), Some("FLOW: maybe"), Some("FLOW: attention"), None] {
            assert_eq!(on_stop(&mut conn, &tab("t1"), reply).unwrap(), None);
            assert_eq!(only_run(&conn), waiting, "{reply:?}");
        }
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Used SQLite.\nFLOW: fail")).unwrap(), prompt_of(&g, "p"));
        let run = only_run(&conn);
        assert_eq!((run.status, run.note), (Status::Running, None));
    }

    #[test]
    fn a_chat_turn_while_a_run_waits_leaves_its_note_and_continue_alone() {
        let g = runnable();
        let t = tab("t1");
        // After a clear, Continue sends the step's prompt again, as Claude lost it.
        let (_db, mut conn, flow) = setup("runs-chat-cleared", &g);
        begin(&mut conn, flow, &t).unwrap();
        clear_tab(&mut conn, &t, "The conversation was cleared.").unwrap();
        let waiting = only_run(&conn);
        assert_eq!(waiting.note.as_deref(), Some("The conversation was cleared."));
        assert_eq!(on_stop(&mut conn, &t, Some("Sure, here is the answer to your question.")).unwrap(), None);
        assert_eq!(only_run(&conn), waiting);
        let text = go_on(&mut conn, waiting.id).unwrap();
        assert!(text.contains(&flows::prompt_text(&g.nodes[1])), "{text}");
        // At the step limit, where only a pick goes on, not even a reply ending with one of its outputs moves it.
        let (_db, mut conn, flow) = setup("runs-chat-limit", &g);
        begin(&mut conn, flow, &t).unwrap();
        for _ in 0..MAX_RUN_STEPS {
            on_stop(&mut conn, &t, Some("FLOW: fail")).unwrap();
        }
        let waiting = only_run(&conn);
        assert!(!waiting.resumable);
        for reply in [Some("What now?"), Some("FLOW: fail"), Some("FLOW: pass"), None] {
            assert_eq!(on_stop(&mut conn, &t, reply).unwrap(), None);
            assert_eq!(only_run(&conn), waiting, "{reply:?}");
        }
        // Nor where Claude Code does not report replies.
        let (_db, mut conn, flow) = setup("runs-chat-unreported", &g);
        begin(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, None).unwrap();
        let waiting = only_run(&conn);
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: fail")).unwrap(), None);
        assert_eq!(only_run(&conn), waiting);
    }

    #[test]
    fn a_valid_flow_line_while_a_run_waits_moves_it_on() {
        let g = runnable();
        let t = tab("t1");
        // After a clear Claude lacks the run's rules, so the next step carries them.
        let (_db, mut conn, flow) = setup("runs-chat-moves", &g);
        begin(&mut conn, flow, &t).unwrap();
        clear_tab(&mut conn, &t, "The conversation was cleared.").unwrap();
        let next = on_stop(&mut conn, &t, Some("Done it again.\nFLOW: fail")).unwrap().unwrap();
        assert_eq!(next, flows::first_prompt_text(&g, "F", &g.nodes[1]));
        let run = only_run(&conn);
        assert_eq!((run.status, run.note, run.steps), (Status::Running, None, 2));
        // Going on still keeps to a step's run limit.
        let g = review_loop();
        let (_db, mut conn, flow) = setup("runs-chat-max", &g);
        begin(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, Some("FLOW: issues")).unwrap();
        on_stop(&mut conn, &t, Some("FLOW: done")).unwrap();
        on_stop(&mut conn, &t, Some("FLOW: issues")).unwrap();
        on_stop(&mut conn, &t, Some("Which fix?\nFLOW: attention")).unwrap();
        assert!(only_run(&conn).resumable);
        assert_eq!(on_stop(&mut conn, &t, Some("Fixed.\nFLOW: done")).unwrap(), None);
        let run = only_run(&conn);
        assert_eq!((run.status, run.step_id.as_str(), run.resumable), (Status::Attention, "fix", false));
        assert!(run.note.unwrap().contains("already run 2 times"));
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
    fn a_late_paste_record_for_an_ended_run_changes_nothing() {
        let (_db, mut conn, flow) = setup("runs-prefilled-ended", &manual_prompt_first("Fix it"));
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let token = state(&conn, run).prefill_token.unwrap();
        stop(&conn, run).unwrap();
        // Even one whose state this version cannot read (as a newer version might store it).
        conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
        conn.execute("UPDATE flow_runs SET resume = 'someday' WHERE id = ?1", [run]).unwrap();
        prefilled(&mut conn, run, "mp", token).unwrap();
        let prefill: bool = conn.query_row("SELECT prefill FROM flow_runs WHERE id = ?1", [run], |r| r.get(0)).unwrap();
        assert!(prefill);
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
    fn after_a_restart_a_run_that_cannot_be_read_does_not_hold_up_the_others() {
        let (_db, mut conn, flow) = setup("runs-restart-unreadable", &runnable());
        let odd = begin(&mut conn, flow, &tab("odd")).unwrap().run_id;
        begin(&mut conn, flow, &tab("open")).unwrap();
        begin(&mut conn, flow, &tab("closed")).unwrap();
        conn.execute("UPDATE flow_runs SET graph = 'broken' WHERE id = ?1", [odd]).unwrap();
        after_restart(&mut conn, &[tab("odd"), tab("open")]).unwrap();
        let summary: Vec<_> = active(&conn).unwrap().into_iter().map(|r| (r.tab_id, r.status)).collect();
        assert_eq!(summary, [("odd".to_string(), Status::Running), ("open".to_string(), Status::Attention)]);
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
        expire(&mut conn).unwrap();
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

    #[test]
    fn a_step_prompt_reaches_the_app_in_camel_case() {
        let next = StepPrompt { step_id: "p".into(), prompt: "do p".into(), briefs: true };
        assert_eq!(serde_json::to_value(&next).unwrap(), serde_json::json!({ "stepId": "p", "prompt": "do p", "briefs": true }));
    }

    #[test]
    fn a_pick_names_the_prompt_step_the_run_went_on_to() {
        let g = graph(
            vec![node("a", Kind::Prompt, &["done"]), node("b", Kind::Prompt, &["done"])],
            vec![edge("start", "next", "a"), edge("a", "done", "b")],
        );
        let (_db, mut conn, flow) = setup("runs-pick-step-id", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let next = proceed(&mut conn, run, "a", "done").unwrap().unwrap();
        assert_eq!(next, StepPrompt { step_id: "b".into(), prompt: prompt_of(&g, "b").unwrap(), briefs: false });
    }

    #[test]
    fn an_unsent_step_prompt_waits_for_the_person_and_continue_sends_it_again_with_the_rules() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-unsent", &g);
        let t = tab("t1");
        let run = begin(&mut conn, flow, &t).unwrap().run_id;
        // After a clear, the next prompt carries the run's rules.
        clear_tab(&mut conn, &t, "cleared").unwrap();
        let with_rules = flows::first_prompt_text(&g, "F", &g.nodes[1]);
        let next = proceed(&mut conn, run, "p", "fail").unwrap().unwrap();
        assert_eq!(next, StepPrompt { step_id: "p".into(), prompt: with_rules.clone(), briefs: true });
        // Claude left the tab before it was typed.
        assert!(unsent(&mut conn, run, &next.step_id, next.briefs, "Claude left.").unwrap());
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.note.as_deref(), waiting.step_id.as_str()), (Status::Attention, Some("Claude left."), "p"));
        assert!(waiting.resumable);
        // Claude does not have the rules: ending now owes it no closing note.
        assert!(!briefed(&conn, run));
        let again = resume(&mut conn, run, "p").unwrap();
        assert_eq!(again, StepPrompt { step_id: "p".into(), prompt: with_rules, briefs: true });
        assert_eq!(only_run(&conn).status, Status::Running);
    }

    #[test]
    fn an_unsent_prompt_without_the_rules_leaves_claude_briefed() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-unsent-briefed", &g);
        let t = tab("t1");
        let run = begin(&mut conn, flow, &t).unwrap().run_id;
        // A pick's prompt, Claude having the rules from the briefing.
        let next = proceed(&mut conn, run, "p", "fail").unwrap().unwrap();
        assert!(!next.briefs);
        assert!(unsent(&mut conn, run, &next.step_id, next.briefs, "Claude left.").unwrap());
        assert!(briefed(&conn, run), "Claude, resumed, still has the rules");
        let again = resume(&mut conn, run, "p").unwrap();
        assert_eq!((again.prompt, again.briefs), (prompt_of(&g, "p").unwrap(), false));
        // A reminder carries no rules either.
        on_stop(&mut conn, &t, Some("Hmm.")).unwrap();
        let reminder = resume(&mut conn, run, "p").unwrap();
        assert!(!reminder.briefs);
        assert!(unsent(&mut conn, run, "p", reminder.briefs, "Claude left.").unwrap());
        assert!(briefed(&conn, run));
        // So stopping the run still tells Claude the flow finished.
        stop(&conn, run).unwrap();
        assert_eq!(on_prompt(&mut conn, &t, "Thanks").unwrap(), finished());
    }

    #[test]
    fn unsent_leaves_a_run_that_moved_on_alone() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-unsent-moved-on", &g);
        let t = tab("t1");
        let run = begin(&mut conn, flow, &t).unwrap().run_id;
        // Another step than the one the prompt was for.
        assert!(!unsent(&mut conn, run, "start", true, "x").unwrap());
        assert_eq!((only_run(&conn).status, only_run(&conn).note), (Status::Running, None));
        assert!(briefed(&conn, run));
        // Not running any more: a Stop hook moved it already.
        on_stop(&mut conn, &t, Some("Hmm.")).unwrap();
        let before = only_run(&conn);
        assert!(!unsent(&mut conn, run, "p", true, "x").unwrap());
        assert_eq!(only_run(&conn).note, before.note);
        assert!(go_on(&mut conn, run).unwrap().starts_with("Go on with the flow step"), "Continue still reminds Claude");
        // Stopped: nothing to do.
        stop(&conn, run).unwrap();
        assert!(!unsent(&mut conn, run, "p", true, "x").unwrap());
        assert!(active(&conn).unwrap().is_empty());
    }

    fn briefed(conn: &Connection, run: i64) -> bool {
        conn.query_row("SELECT briefed FROM flow_runs WHERE id = ?1", [run], |r| r.get(0)).unwrap()
    }

    fn state(conn: &Connection, run: i64) -> RunState {
        RunState::load(conn, run).unwrap().unwrap()
    }

    #[test]
    fn a_run_s_state_is_saved_and_loaded_whole() {
        let (_db, mut conn, flow) = setup("runs-state-round-trip", &manual_first());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let set = RunState {
            node_id: "m".into(),
            status: Status::Attention,
            steps: 7,
            note: Some("why".into()),
            resume: Some(Resume::Prompt),
            clear: Some(Clear::Done),
            briefed: false,
            prefill: true,
            prefill_token: Some(42),
            briefing: true,
            briefing_at: Some(1234),
            closing: true,
        };
        set.save(&conn, run).unwrap();
        assert_eq!(state(&conn, run), set);
        let other = RunState {
            node_id: "p".into(),
            status: Status::Done,
            steps: 0,
            note: None,
            resume: Some(Resume::Remind),
            clear: Some(Clear::Pending),
            briefed: true,
            prefill: false,
            prefill_token: None,
            briefing: false,
            briefing_at: None,
            closing: false,
        };
        other.save(&conn, run).unwrap();
        assert_eq!(state(&conn, run), other);
        let unset = RunState { status: Status::Stopped, resume: None, clear: None, ..other };
        unset.save(&conn, run).unwrap();
        assert_eq!(state(&conn, run), unset);
        assert_eq!(RunState::load(&conn, run + 1).unwrap(), None);
        // Saving a run that is not there is an error naming it.
        let missing = set.save(&conn, run + 1).unwrap_err();
        assert!(missing.contains(&format!("run {}", run + 1)), "{missing}");
    }

    #[test]
    fn owes_closing_and_its_sql_agree() {
        let (_db, mut conn, flow) = setup("runs-owes-closing", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        for briefed in [false, true] {
            for clear in [None, Some(Clear::Pending), Some(Clear::Done)] {
                let s = RunState { briefed, clear, ..state(&conn, run) };
                s.save(&conn, run).unwrap();
                let owes: bool =
                    conn.query_row(&format!("SELECT {OWES_CLOSING} FROM flow_runs WHERE id = ?1"), [run], |r| r.get(0)).unwrap();
                assert_eq!(owes, s.owes_closing(), "briefed {briefed}, clear {clear:?}");
            }
        }
    }

    #[test]
    fn the_briefing_s_time_goes_once_the_briefing_is_over() {
        let (_db, mut conn, flow) = setup("runs-briefing-at", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert!(state(&conn, run).briefing_at.is_some());
        on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap();
        assert_eq!(state(&conn, run).briefing_at, None);
        // Nor does a briefing that was lost keep it.
        let (_db, mut conn, flow) = setup("runs-briefing-at-lost", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "x").unwrap();
        assert_eq!(state(&conn, run).briefing_at, None);
    }

    #[test]
    fn a_pause_during_the_briefing_offers_no_continue_as_after_a_restart() {
        // Continue cannot go on at Start (see `continue_offered`), so neither leaves one.
        let (_db, mut conn, flow) = setup("runs-briefing-pause-resume", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "The Stop hook failed.").unwrap();
        let paused = state(&conn, run);
        assert_eq!((paused.status, paused.resume, paused.briefing, paused.briefed), (Status::Attention, None, false, false));
        let (_db, mut conn, flow) = setup("runs-briefing-restart-resume", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        after_restart(&mut conn, &[tab("t1")]).unwrap();
        assert_eq!(state(&conn, run).resume, None);
    }

    #[test]
    fn a_transition_leaves_alone_what_it_does_not_change() {
        let g = manual_prompt_first("Fix it");
        let (_db, mut conn, flow) = setup("runs-state-untouched", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let before = state(&conn, run);
        assert!(before.prefill && before.prefill_token.is_some(), "{before:?}");
        // The paste stays owed, under the same token.
        prompt_unseen(&mut conn, &tab("t1"), "Your prompt could not be recorded.").unwrap();
        let after = state(&conn, run);
        assert_eq!(
            after,
            RunState { status: Status::Attention, note: Some("Your prompt could not be recorded.".into()), resume: None, ..before.clone() }
        );
        // A paste recorded under another token changes nothing.
        prefilled(&mut conn, run, "mp", before.prefill_token.unwrap() + 1).unwrap();
        assert_eq!(state(&conn, run), after);
        prefilled(&mut conn, run, "mp", before.prefill_token.unwrap()).unwrap();
        assert_eq!(state(&conn, run), RunState { prefill: false, ..after });
        // A pause leaves a run that waits for the person as it is.
        let (_db, mut conn, flow) = setup("runs-state-pause-waiting", &manual_first());
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let before = state(&conn, run);
        pause_tab(&mut conn, &tab("t1"), "x").unwrap();
        assert_eq!(state(&conn, run), before);
        // A clear only marks the clear done and Claude as lacking the rules.
        let mut g = manual_first();
        g.nodes[0].clear = true;
        let (_db, mut conn, flow) = setup("runs-state-clear", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let before = state(&conn, run);
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(state(&conn, run), RunState { clear: Some(Clear::Done), briefed: false, ..before });
    }
}
