//! Flow runs: a flow being worked through in a tab, one at a time per tab.
//!
//! A run starts at the flow's Start step and moves along outputs. It first briefs
//! Claude (the run's rules and Start's base info) and waits at Start for Claude's
//! reply, "Ready", which takes the first step (see `on_stop`); a flow that leads
//! straight to its end skips that. A briefing with no reply within
//! `BRIEFING_TIMEOUT_SECS` (Claude was interrupted, or crashed) waits for the
//! person (see `expire`). A prompt step sends its text to Claude; when Claude ends
//! its turn, the Stop hook (`misag --claude-hook`, a separate process) reads the
//! `FLOW: <output>` line Claude ended with and moves on, handing Claude the next
//! prompt as hook context.
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
use rusqlite::{named_params, params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

/// A run that has taken this many steps waits for the person, in case it loops.
pub const MAX_RUN_STEPS: i64 = 50;

/// How long a run whose Start clears may stay at Start before it waits for the
/// person: the app's own 30 seconds (`CLEAR_TIMEOUT_MS` in src/runController.ts,
/// after which it stops the run, saying why), and a little more, so its stop comes
/// first.
pub const CLEAR_TIMEOUT_SECS: i64 = 35;

/// How long a run waits at Start for Claude's reply to its briefing, which asks
/// only for "Ready", before it waits for the person: Claude was interrupted (which
/// runs no Stop hook) or failed. `transition::timed_out`'s note gives it in minutes.
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

/// A run just started. Mirrored by `Started` in src/runController.ts.
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

/// How a run whose Start clears is getting on. Mirrored by `Cleared` in src/runController.ts.
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

/// A new token naming a paste (see `prefilled`), kept within what JavaScript
/// numbers hold exactly (2^53 - 1), as the app hands it back.
fn new_token() -> i64 {
    const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
    i64::try_from(uuid::Uuid::new_v4().as_u64_pair().0 % MAX_SAFE_INTEGER).unwrap_or(0)
}

/// Where a run is with its step, and what only that place has. Each phase has one
/// status (see `Phase::status`); `RunState::from_columns` and `RunState::columns`
/// map it to and from `flow_runs`' columns. The CHECKs on `flow_runs` (in `db::schema`'s
/// MIGRATIONS) spell out the same rules, and SQLite cannot change a table's CHECKs:
/// changing the phases or how they are stored needs a new migration that rebuilds
/// the table with new CHECKs.
#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Running at Start, whose step clears: the app types `/clear` (status running,
    /// clear "pending"), and once Claude reports it (`done`; clear "done") sends the
    /// briefing (see `cleared`).
    Clearing { done: bool },
    /// Running at Start, waiting for Claude's reply to the briefing sent at `sent_at`
    /// (see `expire`; status running, briefing 1, briefing_at).
    Briefing { sent_at: i64 },
    /// Running a step (status running): Claude works on a prompt step, or answers the
    /// person's prompt at a manual prompt.
    Working,
    /// Waiting for the person (status manual): at a pause to go on, at a manual prompt
    /// for their prompt; `prefill`, whether the app has yet to fill Claude's input for
    /// that prompt (prefill 1; see `prefill_text`).
    Waiting { prefill: bool },
    /// Needing a person (status attention), saying `why` (note), with how Continue
    /// goes on (`resume`; None, it cannot help), and `prefill` as for `Waiting`.
    NeedsPerson { why: String, resume: Option<Resume>, prefill: bool },
    /// Done, or `stopped` (status done or stopped), with whether Claude has yet to be
    /// told its flow is finished (closing 1; see `RunState::owes_closing`).
    Ended { stopped: bool, owes_closing: bool },
}

impl Phase {
    fn status(&self) -> Status {
        match self {
            Phase::Clearing { .. } | Phase::Briefing { .. } | Phase::Working => Status::Running,
            Phase::Waiting { .. } => Status::Manual,
            Phase::NeedsPerson { .. } => Status::Attention,
            Phase::Ended { stopped: true, .. } => Status::Stopped,
            Phase::Ended { stopped: false, .. } => Status::Done,
        }
    }

    /// Whether the app has yet to fill Claude's input for the manual prompt the run
    /// waits at (see `prefill_text`).
    fn prefill(&self) -> bool {
        matches!(self, Phase::Waiting { prefill: true } | Phase::NeedsPerson { prefill: true, .. })
    }

    /// The same phase, owing a paste or not; a phase that cannot owe one is as it is.
    fn with_prefill(self, owed: bool) -> Self {
        match self {
            Phase::Waiting { .. } => Phase::Waiting { prefill: owed },
            Phase::NeedsPerson { why, resume, .. } => Phase::NeedsPerson { why, resume, prefill: owed },
            other => other,
        }
    }
}

/// A run's state as `flow_runs` stores it, column by column (see `RunState::COLUMNS`).
#[derive(Clone, Debug, PartialEq)]
struct Columns {
    node_id: String,
    status: String,
    steps: i64,
    note: Option<String>,
    resume: Option<String>,
    clear: Option<String>,
    briefed: bool,
    prefill: bool,
    prefill_token: Option<i64>,
    briefing: bool,
    briefing_at: Option<i64>,
    closing: bool,
}

impl Columns {
    /// `row`'s `RunState::COLUMNS`, read by name.
    fn read(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Columns {
            node_id: row.get("node_id")?,
            status: row.get("status")?,
            steps: row.get("steps")?,
            note: row.get("note")?,
            resume: row.get("resume")?,
            clear: row.get("clear")?,
            briefed: row.get("briefed")?,
            prefill: row.get("prefill")?,
            prefill_token: row.get("prefill_token")?,
            briefing: row.get("briefing")?,
            briefing_at: row.get("briefing_at")?,
            closing: row.get("closing")?,
        })
    }
}

/// What changes about a run as it goes: every column of `flow_runs` but its `id`,
/// the ones fixed when it starts (its flow, tab, steps' copy and start time), and
/// `updated_at`, which `RunState::save` stamps. Every state change loads it, works
/// out the whole new state (see `transition`) and saves it in one write
/// (`RunState::save`), in the transaction that read it, so no write depends on
/// another's order. The raw UPDATEs that remain: stopping a run this version cannot
/// read (in `stop` and in `stop_in`), dropping the note owed for an ended run
/// (`drop_closing`, and `on_prompt`'s `closing` UPDATE ... RETURNING), and stopping
/// the runs of tabs a raterm import leaves behind (in `db::raterm_import`). Every other state
/// change is a load and a save. The CHECKs on `flow_runs` (in `db::schema`'s MIGRATIONS)
/// hold an active row to the combinations `from_columns` reads, for every writer.
#[derive(Clone, Debug, PartialEq)]
struct RunState {
    node_id: String,
    steps: i64,
    /// Whether Claude has the run's rules and Start's base info (see `first_turn` and
    /// `RunState::prompt`): false until the briefing is sent, and after a clear.
    briefed: bool,
    /// Names the last paste owed (see `prefilled`). It outlives the paste, so a
    /// record of an earlier one that arrives late cannot cancel one owed since.
    prefill_token: Option<i64>,
    phase: Phase,
}

impl RunState {
    /// The columns `from_row` reads.
    const COLUMNS: &str = "node_id, status, steps, note, resume, clear, briefed, prefill, prefill_token, briefing, briefing_at, closing";

    /// The state in `row`'s `COLUMNS`; an error for a state this version cannot read
    /// (see `from_columns`), including a column of the wrong type, so one such run
    /// does not fail a query over the others.
    fn from_row(row: &rusqlite::Row) -> Result<Self, String> {
        Self::from_columns(Columns::read(row).map_err(|e| format!("the run's state cannot be read ({e})"))?)
    }

    /// The state the columns `c` store. An error for a value this version does not
    /// know (a run from a newer version is not driven on a guess) and for columns
    /// the status leaves no room for, naming them; except what versions before this
    /// one left behind, which is dropped: everything but `closing` on an ended run
    /// (a raw stop, as for an unreadable run, leaves the rest as it was), and a
    /// `briefing_at` the briefing has outlived, which the CHECKs now refuse (the
    /// migration that added them cleared it), so only a writer that bypasses them
    /// (such as a database edited by hand) can leave one. Changing what this reads
    /// needs a new migration rebuilding `flow_runs` with matching CHECKs (see `Phase`).
    fn from_columns(c: Columns) -> Result<Self, String> {
        let status = Status::parse(&c.status)?;
        let cannot = |what: &str| Err(format!("the run is {} with {what}, which cannot be", status.as_str()));
        // Resume and clear are read only where they can be, so an ended run's are not.
        let phase = match status {
            Status::Done | Status::Stopped => Phase::Ended { stopped: status == Status::Stopped, owes_closing: c.closing },
            _ if c.closing => return cannot("a finished flow's note owed"),
            Status::Running => {
                let (resume, clear) = (Resume::parse(c.resume)?, Clear::parse(c.clear)?);
                if c.note.is_some() {
                    return cannot("a note");
                }
                if resume.is_some() {
                    return cannot("a way to continue");
                }
                if c.prefill {
                    return cannot("a paste owed");
                }
                match (clear, c.briefing) {
                    (Some(_), true) => return cannot("both a clear and a briefing"),
                    (Some(clear), false) => Phase::Clearing { done: clear == Clear::Done },
                    (None, true) => match c.briefing_at {
                        Some(sent_at) => Phase::Briefing { sent_at },
                        None => return cannot("a briefing with no time it was sent"),
                    },
                    (None, false) => Phase::Working,
                }
            }
            Status::Manual | Status::Attention => {
                let (resume, clear) = (Resume::parse(c.resume)?, Clear::parse(c.clear)?);
                if clear.is_some() {
                    return cannot("a clear");
                }
                if c.briefing {
                    return cannot("a briefing");
                }
                if status == Status::Manual {
                    if c.note.is_some() {
                        return cannot("a note");
                    }
                    if resume.is_some() {
                        return cannot("a way to continue");
                    }
                    Phase::Waiting { prefill: c.prefill }
                } else {
                    match c.note {
                        Some(why) => Phase::NeedsPerson { why, resume, prefill: c.prefill },
                        None => return cannot("no note saying why"),
                    }
                }
            }
        };
        Ok(RunState { node_id: c.node_id, steps: c.steps, briefed: c.briefed, prefill_token: c.prefill_token, phase })
    }

    /// The columns that store this state.
    fn columns(&self) -> Columns {
        let mut c = Columns {
            node_id: self.node_id.clone(),
            status: self.phase.status().as_str().to_owned(),
            steps: self.steps,
            note: None,
            resume: None,
            clear: None,
            briefed: self.briefed,
            prefill: false,
            prefill_token: self.prefill_token,
            briefing: false,
            briefing_at: None,
            closing: false,
        };
        match &self.phase {
            Phase::Clearing { done } => c.clear = Some(if *done { Clear::Done } else { Clear::Pending }.as_str().to_owned()),
            Phase::Briefing { sent_at } => (c.briefing, c.briefing_at) = (true, Some(*sent_at)),
            Phase::Working => {}
            Phase::Waiting { prefill } => c.prefill = *prefill,
            Phase::NeedsPerson { why, resume, prefill } => {
                (c.note, c.resume, c.prefill) = (Some(why.clone()), resume.map(|r| r.as_str().to_owned()), *prefill);
            }
            Phase::Ended { owes_closing, .. } => c.closing = *owes_closing,
        }
        c
    }

    /// A new run of a flow whose Start is `start`: at Start, clearing first if
    /// `clears`, otherwise about to take its first turn (see `first_turn`). Claude
    /// lacks the run's rules until then.
    fn initial(start: &Node, clears: bool) -> Self {
        RunState {
            node_id: start.id.clone(),
            steps: 0,
            briefed: false,
            prefill_token: None,
            phase: if clears { Phase::Clearing { done: false } } else { Phase::Working },
        }
    }

    /// The state of run `run_id`, whatever its status; None for no such run.
    #[cfg(test)]
    fn load(conn: &Connection, run_id: i64) -> Result<Option<Self>, String> {
        Self::load_where(conn, "id = ?1", run_id)?.transpose()
    }

    /// The state of run `run_id` if it is active; None otherwise. The inner error is
    /// a state this version cannot read.
    fn load_active(conn: &Connection, run_id: i64) -> Result<Option<Result<Self, String>>, String> {
        Self::load_where(conn, &format!("id = ?1 AND status IN {ACTIVE}"), run_id)
    }

    fn load_where(conn: &Connection, filter: &str, run_id: i64) -> Result<Option<Result<Self, String>>, String> {
        conn.query_row(&format!("SELECT {} FROM flow_runs WHERE {filter}", Self::COLUMNS), [run_id], |r| Ok(Self::from_row(r)))
            .optional()
            .map_err(sql)
    }

    /// Makes this run `run_id`'s state, in one write that also stamps `updated_at`.
    /// An error if there is no run `run_id`.
    fn save(&self, conn: &Connection, run_id: i64) -> Result<(), String> {
        let c = self.columns();
        let changed = conn
            .execute(
                "UPDATE flow_runs SET node_id = :node_id, status = :status, steps = :steps, note = :note, resume = :resume,
                 clear = :clear, briefed = :briefed, prefill = :prefill, prefill_token = :prefill_token, briefing = :briefing,
                 briefing_at = :briefing_at, closing = :closing, updated_at = :updated_at WHERE id = :id",
                named_params! {
                    ":id": run_id,
                    ":node_id": c.node_id,
                    ":status": c.status,
                    ":steps": c.steps,
                    ":note": c.note,
                    ":resume": c.resume,
                    ":clear": c.clear,
                    ":briefed": c.briefed,
                    ":prefill": c.prefill,
                    ":prefill_token": c.prefill_token,
                    ":briefing": c.briefing,
                    ":briefing_at": c.briefing_at,
                    ":closing": c.closing,
                    ":updated_at": now(),
                },
            )
            .map_err(sql)?;
        if changed != 1 {
            return Err(format!("run {run_id} could not be saved: there is no such run"));
        }
        Ok(())
    }

    /// The run at `node_id` in `phase` with `steps`. What Claude has (`briefed`) and
    /// the last paste's token carry over.
    fn moved(&self, node_id: &str, phase: Phase, steps: i64) -> Self {
        RunState { node_id: node_id.to_owned(), steps, phase, ..self.clone() }
    }

    /// The run waiting at `node_id` for the person, saying why, and how Continue goes
    /// on from there (None: it cannot help, and the person picks an output).
    fn waiting(&self, node_id: &str, steps: i64, why: &str, resume: Option<Resume>) -> Self {
        self.moved(node_id, Phase::NeedsPerson { why: why.to_owned(), resume, prefill: false }, steps)
    }

    /// The run waiting where it is for the person, saying why, and how Continue goes on
    /// from there (see `waiting`); a paste it owes stays owed.
    fn waiting_here(&self, why: &str, resume: Option<Resume>) -> Self {
        let phase = Phase::NeedsPerson { why: why.to_owned(), resume, prefill: self.phase.prefill() };
        RunState { phase, ..self.clone() }
    }

    /// The run, at Start reading its briefing, waiting there for the person, saying
    /// why: Claude may not have read the briefing, so the first step carries it again,
    /// and Continue has no step prompt to send (see `continue_offered`).
    fn briefing_lost(&self, why: &str) -> Self {
        RunState { briefed: false, ..self.waiting_here(why, None) }
    }

    /// The run, still at Start waiting for its clear, waiting there for the person
    /// to go on without it, saying why.
    fn clear_lost(&self, why: &str) -> Self {
        self.waiting_here(why, None)
    }

    /// The run done at `node_id`; Claude, if it has the run's rules, is told it no
    /// longer needs to end its replies with a FLOW line, with the person's next
    /// prompt (see `RunState::owes_closing`).
    fn finished(&self, node_id: &str, steps: i64) -> Self {
        self.moved(node_id, Phase::Ended { stopped: false, owes_closing: self.owes_closing() }, steps)
    }

    /// The run stopped where it is (see `finished`).
    fn stopped(&self) -> Self {
        RunState { phase: Phase::Ended { stopped: true, owes_closing: self.owes_closing() }, ..self.clone() }
    }

    /// Whether the run, ending, owes Claude the note that its flow is finished: Claude
    /// has the run's rules (`briefed`; not after a clear, or while the run clears at
    /// its start). Nothing is typed and no turn is spent: stopping must not send into
    /// a turn or a permission prompt. `OWES_CLOSING` says the same in SQL, for a run
    /// this version cannot read.
    fn owes_closing(&self) -> bool {
        self.briefed && !matches!(self.phase, Phase::Clearing { .. })
    }

    /// The run owing the app a paste named by `token` (see `prefilled`).
    fn with_prefill(self, token: i64) -> Self {
        RunState { prefill_token: Some(token), phase: self.phase.with_prefill(true), ..self }
    }

    /// Has Claude get `node`'s prompt, led by the run's rules and Start's base info if
    /// it lacks them (after a clear), which it then has.
    fn prompt(&mut self, node: &Node) -> Say {
        let briefs = !self.briefed;
        self.briefed = true;
        Say::Prompt { node_id: node.id.clone(), briefs }
    }
}

/// What a change to a run has the app or hook send Claude. Its text is made at the
/// edge (see `render`), which looks up the flow's name.
#[derive(Clone, Debug, PartialEq)]
enum Say {
    /// The run's briefing: its rules and Start's base info.
    Briefing,
    /// Step `node_id`'s prompt, led by the run's rules and Start's base info if `briefs`.
    Prompt { node_id: String, briefs: bool },
    /// The reminder to go on with step `node_id` (see `Resume::Remind`).
    Remind { node_id: String },
}

/// A change to a run: its new state, the step it left and the output it left by, to
/// record (see `left`), and what to send Claude.
#[derive(Clone, Debug, PartialEq)]
struct Change {
    state: RunState,
    left: Option<(String, String)>,
    say: Option<Say>,
}

impl Change {
    /// A change of state alone.
    fn to(state: RunState) -> Self {
        Change { state, left: None, say: None }
    }
}

/// How often a run has left each step so far (`flow_run_events`), for `max_runs`.
#[derive(Default)]
struct Taken(BTreeMap<String, i64>);

/// For a run that has left no step: one at Start, which it leaves only once.
const NOTHING_TAKEN: &Taken = &Taken(BTreeMap::new());

/// How often run `run_id` has left each step.
fn taken(conn: &Connection, run_id: i64) -> Result<Taken, String> {
    let mut stmt = conn.prepare_cached("SELECT node_id, COUNT(*) FROM flow_run_events WHERE run_id = ?1 GROUP BY node_id").map_err(sql)?;
    let counts = stmt.query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?;
    Ok(Taken(counts))
}

/// Who moves a run on.
#[derive(Clone, Copy, Debug)]
enum By {
    /// The flow itself: Claude's reply, or the start.
    Flow,
    /// The person, who is not held back by a step's `max_runs`. (Their picks also
    /// restart the count toward `MAX_RUN_STEPS`, through the `steps` they pass.)
    Person,
}

/// Why a run waits when Claude Code does not report Claude's replies.
const NO_REPLY: &str = "Claude Code did not report Claude's reply to Mischief of Agents; update Claude Code to run flows.";

/// The step `id` of `graph`.
fn node_of<'g>(graph: &'g Graph, id: &str) -> Result<&'g Node, String> {
    graph.nodes.iter().find(|n| n.id == id).ok_or_else(|| format!("the run is at a missing step {id:?}"))
}

/// Whether Continue is offered for a run in `phase` at a step of `kind`, and how it
/// goes on (see `resume`): only at a prompt step that needs attention, where Claude
/// going on can help. A reply that ends with one of the step's outputs moves such a
/// run on too (see `on_stop`).
fn continue_offered(phase: &Phase, kind: Kind) -> Option<Resume> {
    match phase {
        Phase::NeedsPerson { resume, .. } if kind == Kind::Prompt => *resume,
        _ => None,
    }
}

/// Whether the run waits for the person at a manual prompt (`node`, where it is):
/// for their prompt, or, once Claude's answer to it was lost, for it again or an
/// output.
fn awaits_prompt(state: &RunState, node: &Node) -> bool {
    node.kind == Kind::ManualPrompt && matches!(state.phase, Phase::Waiting { .. } | Phase::NeedsPerson { .. })
}

mod transition;

use transition::Cx;

/// An active run as stored.
struct Stored {
    id: i64,
    graph: Graph,
    state: RunState,
}

impl Stored {
    /// What its changes are worked out from (see `transition::Cx`), with a fresh
    /// paste token, and `taken` for how often it took each step.
    fn cx<'a>(&'a self, taken: &'a Taken) -> Cx<'a> {
        Cx { graph: &self.graph, taken, token: new_token() }
    }

    /// Saves `state` as the run's; skips the write (and its `updated_at` stamp) when
    /// `state` is what the run already has.
    fn save(&self, conn: &Connection, state: &RunState) -> Result<(), String> {
        if *state == self.state {
            return Ok(());
        }
        state.save(conn, self.id)
    }

    /// Makes `change` the run's: records the step it left, saves its new state, and
    /// returns what to send Claude.
    fn apply(&self, tx: &Transaction, change: Change) -> Result<Option<StepPrompt>, String> {
        if let Some((node_id, output)) = &change.left {
            left(tx, self.id, node_id, output)?;
        }
        change.state.save(tx, self.id)?;
        change.say.map(|say| render(tx, self.id, &self.graph, say)).transpose()
    }
}

/// Why an active run could not be loaded.
#[derive(Debug)]
enum LoadError {
    /// The database failed (busy, corrupt, ...): the run itself may be fine.
    Sql(String),
    /// The run cannot be read by this version: its state or steps (see
    /// `RunState::from_columns`), or a step it names that its steps lack.
    Unreadable(String),
}

impl From<LoadError> for String {
    fn from(e: LoadError) -> Self {
        match e {
            LoadError::Sql(e) | LoadError::Unreadable(e) => e,
        }
    }
}

/// The active run matching `filter` (`tab_id = ?1` or `id = ?1`).
fn active_run(conn: &Connection, filter: &str, key: &dyn rusqlite::ToSql) -> Result<Option<Stored>, LoadError> {
    let row = conn
        .query_row(
            &format!("SELECT id, graph, {} FROM flow_runs WHERE {filter} AND status IN {ACTIVE}", RunState::COLUMNS),
            [key],
            |r| {
                let graph = r.get::<_, String>("graph").map_err(|e| format!("the run's steps cannot be read ({e})"));
                Ok((r.get::<_, i64>("id")?, graph, RunState::from_row(r)))
            },
        )
        .optional()
        .map_err(|e| LoadError::Sql(sql(e)))?;
    let Some((id, graph, state)) = row else { return Ok(None) };
    let stored = graph.and_then(|graph| Ok(Stored { id, graph: parse_graph(&graph)?, state: state? }));
    stored.map(Some).map_err(LoadError::Unreadable)
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

/// `RunState::owes_closing` in SQL, for `stop_run` on a run this version cannot read,
/// which it works out from the row itself.
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

/// The text of `say` for run `run_id`, whose steps are `graph`.
fn render(conn: &Connection, run_id: i64, graph: &Graph, say: Say) -> Result<StepPrompt, String> {
    Ok(match say {
        Say::Briefing => {
            let start = graph.nodes.iter().find(|n| n.kind == Kind::Start).ok_or("the flow has no Start step")?;
            StepPrompt { step_id: start.id.clone(), prompt: flows::briefing_text(graph, &flow_name(conn, run_id)?), briefs: true }
        }
        Say::Prompt { node_id, briefs } => {
            let node = node_of(graph, &node_id)?;
            let prompt = if briefs { flows::first_prompt_text(graph, &flow_name(conn, run_id)?, node) } else { flows::prompt_text(node) };
            StepPrompt { step_id: node_id, prompt, briefs }
        }
        Say::Remind { node_id } => StepPrompt { prompt: flows::resume_text(node_of(graph, &node_id)?), step_id: node_id, briefs: false },
    })
}

/// The text of the prompt a change returned, for where no step id goes along.
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
    let initial = RunState::initial(&start, clear);
    // The row's run state is saved whole below; this takes the tab (the one-run index).
    let inserted = tx.execute(
        "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, started_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            flow_id,
            tab_id.as_str(),
            serde_json::to_string(&graph).map_err(|e| e.to_string())?,
            initial.node_id,
            initial.phase.status().as_str(),
            now
        ],
    );
    match inserted {
        // The one-run index is the only UNIQUE constraint an insert can break; any
        // other refusal (a CHECK, say) is reported as itself.
        Err(rusqlite::Error::SqliteFailure(f, _)) if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => {
            return Err("this tab is already running a flow; stop that run first".into());
        }
        other => other.map_err(sql)?,
    };
    let run = Stored { id: tx.last_insert_rowid(), graph, state: initial };
    let prompt = if clear {
        run.state.save(&tx, run.id)?;
        None
    } else {
        let change = transition::first_turn(&run.cx(NOTHING_TAKEN), &run.state, &start, now);
        texts(run.apply(&tx, change)?)
    };
    tx.commit().map_err(sql)?;
    Ok(Started { run_id: run.id, clear, prompt })
}

/// How run `run_id`, which clears at its start, is getting on; once Claude has
/// cleared, gives the briefing (once: later calls are `Gone`).
pub fn cleared(conn: &mut Connection, run_id: i64) -> Result<Cleared, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "id = ?1", &run_id)? else { return Ok(Cleared::Gone) };
    match run.state.phase {
        Phase::Clearing { done: false } => Ok(Cleared::Pending),
        Phase::Clearing { done: true } => {
            let change = transition::first_turn(&run.cx(NOTHING_TAKEN), &run.state, node_of(&run.graph, &run.state.node_id)?, now());
            let prompt = texts(run.apply(&tx, change)?);
            tx.commit().map_err(sql)?;
            Ok(Cleared::Ready { prompt })
        }
        _ => Ok(Cleared::Gone),
    }
}

/// Claude in tab `tab_id` ended its turn with `reply` (None when Claude Code did
/// not report it): moves the tab's run on. Returns the next prompt to send, if
/// any. A tab with no run is left alone. A run that waits for the person at a
/// prompt step is left alone too, unless Claude's reply ends with one of the
/// step's outputs and Continue is offered there (see `continue_offered`); one
/// that waits at Start, a pause or a manual prompt always is. A manual prompt the
/// person sent goes on by its one output, whatever the reply.
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
    let taken = taken(&tx, run.id)?;
    let Some(change) = transition::on_stop(&run.cx(&taken), &run.state, reply)? else { return Ok(None) };
    let next = run.apply(&tx, change)?;
    tx.commit().map_err(sql)?;
    Ok(texts(next))
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
    // The person is not held back by `max_runs`, so how often each step ran does not count.
    let change = transition::proceed(&run.cx(NOTHING_TAKEN), &run.state, step_id, output)?;
    let next = run.apply(&tx, change)?;
    tx.commit().map_err(sql)?;
    Ok(next)
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
    if state.node_id != step_id || !state.phase.prefill() {
        return Ok(None);
    }
    let node = node_of(&run.graph, &state.node_id)?;
    if !awaits_prompt(state, node) {
        return Ok(None);
    }
    let text = flows::prefill_text(&run.graph, node, !state.briefed);
    Ok(Some(Prefill { text, token: state.prefill_token.unwrap_or(0) }).filter(|p| !p.text.is_empty()))
}

/// The app put the manual prompt's text named by `token` into Claude's input (see
/// `prefill_text`). A paste owed since (after a clear, say) stays owed. Claude has
/// the base info only once the person sends it (see `on_prompt`).
pub fn prefilled(conn: &mut Connection, run_id: i64, step_id: &str, token: i64) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(state) = RunState::load_active(&tx, run_id)?.transpose()? else { return Ok(()) };
    let Some(state) = transition::prefilled(&state, step_id, token) else { return Ok(()) };
    state.save(&tx, run_id)?;
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
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())?
        && let Some(state) = transition::prompt_sent(&run.cx(NOTHING_TAKEN), &run.state)?
    {
        state.save(&tx, run.id)?;
    }
    tx.commit().map_err(sql)?;
    Ok(closing)
}

/// The UserPromptSubmit hook could not record the person's prompt in tab `tab_id`:
/// a run waiting at a manual prompt would never see Claude's answer, so it waits
/// for the person, saying `why`, rather than for a prompt already sent. Its paste,
/// if still owed, stays owed.
fn prompt_unseen(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? else { return Ok(()) };
    let Some(state) = transition::prompt_unseen(&run.cx(NOTHING_TAKEN), &run.state, why)? else { return Ok(()) };
    state.save(&tx, run.id)?;
    tx.commit().map_err(sql)
}

/// A hook of misag's that reports to the tab's run (see `hook_failed`).
#[derive(Clone, Copy, Debug)]
pub enum Hook {
    /// The Stop hook, which moves the run on (see `on_stop`).
    Stop,
    /// The UserPromptSubmit hook, which tells it of the person's prompt (see `on_prompt`).
    Prompt,
}

/// `hook` in tab `tab_id` failed with `error`, so the tab's run would wait for what
/// it will never see: after the Stop hook, a running step waits for the person (see
/// `pause_tab`); after the UserPromptSubmit hook, so does a manual prompt waiting
/// for the person's prompt (see `prompt_unseen`). Best effort: returns `error`,
/// with why the run could not be marked if it could not.
pub fn hook_failed(conn: &mut Connection, tab_id: &TabId, hook: Hook, error: String) -> String {
    let marked = match hook {
        Hook::Stop => pause_tab(conn, tab_id, &format!("The Stop hook could not move the run on ({error}). Pick an output to go on.")),
        Hook::Prompt => prompt_unseen(
            conn,
            tab_id,
            &format!("misag could not record your prompt ({error}), so Claude's answer will not move the run on. Pick an output to go on."),
        ),
    };
    match marked {
        Ok(()) => error,
        Err(also) => format!("{error}; and could not mark the run: {also}"),
    }
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
    let change = transition::resume(&run.cx(NOTHING_TAKEN), &run.state, step_id)?;
    // Logged with why it waited, which going on clears.
    let why = match &run.state.phase {
        Phase::NeedsPerson { why, .. } => Some(why),
        _ => None,
    };
    tx.execute(
        "INSERT INTO flow_run_resumes (run_id, node_id, note, at) VALUES (?1, ?2, ?3, ?4)",
        params![run.id, run.state.node_id, why, now()],
    )
    .map_err(sql)?;
    let next = run.apply(&tx, change)?.ok_or("going on sends Claude nothing")?;
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
    let Some(state) = transition::unsent(&run.state, step_id, briefs, why) else { return Ok(false) };
    state.save(&tx, run.id)?;
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
        run.save(&tx, &transition::cleared_tab(&run.cx(NOTHING_TAKEN), &run.state, why)?)?;
    }
    tx.commit().map_err(sql)
}

/// Makes the tab's running run wait for the person, saying why: Claude will not
/// reply to its step (the conversation was cleared, or the hook failed), so
/// Continue sends the step's prompt again, except while the run reads its briefing,
/// where the person picks an output, and while it runs a manual prompt, where the
/// person sends their prompt again or picks one (see `transition::answer_lost`). A
/// run still at Start waiting for its clear has no step to reply to, and is left
/// alone.
fn pause_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? {
        run.save(&tx, &transition::pause(&run.cx(NOTHING_TAKEN), run.state.clone(), why)?)?;
    }
    tx.commit().map_err(sql)
}

/// Stops active run `run_id` (see `stop`), in the caller's transaction; false if
/// there is no such run. A run this version can read is saved stopped
/// (`RunState::stopped`). One it cannot read (a resume or clear state from a newer
/// version, which `RunState::from_columns` refuses) must still stop, so it is one
/// raw UPDATE of the status, which works out what it owes Claude from the row
/// (`OWES_CLOSING`) as a best guess, and leaves the rest as it was.
fn stop_run(conn: &Connection, run_id: i64, tab_open: bool) -> Result<bool, String> {
    match RunState::load_active(conn, run_id)? {
        None => Ok(false),
        Some(Ok(state)) => {
            let mut stopped = state.stopped();
            // A tab that is gone sends no next prompt to carry the note.
            if !tab_open {
                stopped.phase = Phase::Ended { stopped: true, owes_closing: false };
            }
            stopped.save(conn, run_id).map(|()| true)
        }
        Some(Err(why)) => {
            eprintln!("misag: run {run_id} is stopped by its row alone, as {why}");
            conn.execute(
                &format!("UPDATE flow_runs SET status = 'stopped', closing = ?3 AND ({OWES_CLOSING}), updated_at = ?2 WHERE id = ?1 AND status IN {ACTIVE}"),
                params![run_id, now(), tab_open],
            )
            .map_err(sql)?;
            Ok(true)
        }
    }
}

/// Stops the active run of tab `tab_id`, which is gone, if it has one: as `stop` does,
/// but owing Claude no closing note, as no prompt of that tab will carry it; in the
/// caller's transaction.
fn stop_in(conn: &Connection, tab_id: &str) -> Result<(), String> {
    let run: Option<i64> = conn
        .query_row(&format!("SELECT id FROM flow_runs WHERE tab_id = ?1 AND status IN {ACTIVE}"), [tab_id], |r| r.get(0))
        .optional()
        .map_err(sql)?;
    run.map_or(Ok(()), |run| stop_run(conn, run, false).map(drop))
}

/// Stops the tab's active run, if it has one: the tab is closing.
pub fn stop_tab(conn: &Connection, tab_id: &TabId) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(sql)?;
    stop_in(&tx, tab_id.as_str())?;
    tx.commit().map_err(sql)
}

/// At startup: stops the runs of tabs that are no longer open, and makes running
/// ones wait, as their Claude was stopped with misag and will not reply. A run this
/// version cannot read is logged and left for the person to stop (see `active`),
/// without holding up the others; a database error fails the whole restart.
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
        let left = |e: String| eprintln!("misag: run {id} was left as it was after the restart, as {e}");
        let run = match active_run(&tx, "id = ?1", &id) {
            Ok(Some(run)) => run,
            Ok(None) => continue,
            Err(LoadError::Unreadable(e)) => {
                left(e);
                continue;
            }
            Err(LoadError::Sql(e)) => return Err(e),
        };
        // An error here is a step its steps lack: the run cannot be read either.
        match transition::restarted(&run.cx(NOTHING_TAKEN), &run.state) {
            Ok(state) => run.save(&tx, &state)?,
            Err(e) => left(e),
        }
    }
    tx.commit().map_err(sql)
}

/// Stops run `run_id`; Claude is then told the flow is finished with the person's
/// next prompt (see `RunState::owes_closing`). A run this version cannot read can
/// still be stopped, by one UPDATE whose `closing` is then a best guess from the
/// row (see `stop_run`); why it could not be read is logged.
pub fn stop(conn: &Connection, run_id: i64) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(sql)?;
    if !stop_run(&tx, run_id, true)? {
        return Err(format!("there is no active run {run_id}"));
    }
    tx.commit().map_err(sql)
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
/// `BRIEFING_TIMEOUT_SECS` after it was sent (see `transition::timed_out`). The app
/// calls this before each look at the runs (see `commands::flow_runs`), so a run waits for
/// you a few seconds after its time is up. Takes the write lock only when a run it
/// can read has timed out. A running run whose state this version cannot read is
/// logged once and left to be stopped.
pub fn expire(conn: &mut Connection) -> Result<(), String> {
    let now = now();
    // Look before taking the write lock.
    if timed_out(conn, now)?.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    for (id, state) in timed_out(&tx, now)? {
        state.save(&tx, id)?;
    }
    tx.commit().map_err(sql)
}

/// The runs that have timed out at Start at `now`, with the state each goes to
/// (see `expire`). Only a running run can.
fn timed_out(conn: &Connection, now: i64) -> Result<Vec<(i64, RunState)>, String> {
    let mut stmt = conn
        .prepare_cached(&format!("SELECT id, started_at, {} FROM flow_runs WHERE status = 'running'", RunState::COLUMNS))
        .map_err(sql)?;
    let runs = stmt
        .query_map([], |r| Ok((r.get::<_, i64>("id")?, r.get::<_, i64>("started_at")?, RunState::from_row(r))))
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(runs
        .into_iter()
        .filter_map(|(id, started_at, state)| readable(id, state).and_then(|s| transition::timed_out(&s, started_at, now)).map(|s| (id, s)))
        .collect())
}

/// The state of running run `id`, or None, logged once (see `log_once`), if this
/// version cannot read it.
fn readable(id: i64, state: Result<RunState, String>) -> Option<RunState> {
    state.inspect_err(|e| log_once("unreadable", id, || format!("run {id} cannot be read, so its timeouts are skipped: {e}"))).ok()
}

/// Logs `message` about run `id` once per process for each `what`: the runs are
/// looked at on every poll (`expire`, `active`), which would repeat it.
fn log_once(what: &'static str, id: i64, message: impl FnOnce() -> String) {
    static LOGGED: Mutex<BTreeSet<(&str, i64)>> = Mutex::new(BTreeSet::new());
    if LOGGED.lock().unwrap_or_else(PoisonError::into_inner).insert((what, id)) {
        eprintln!("misag: {}", message());
    }
}

/// Every active run, oldest first. Writes nothing: timeouts are `expire`'s, so
/// call that first when they must be applied.
pub fn active(conn: &Connection) -> Result<Vec<Run>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT r.id AS id, r.flow_id AS flow_id, f.name AS flow_name, r.tab_id AS tab_id, r.graph AS graph, {}
             FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.status IN {ACTIVE} ORDER BY r.id",
            RunState::COLUMNS
        ))
        .map_err(sql)?;
    type Row = (i64, i64, String, String, Result<String, String>, String, String, Result<RunState, String>);
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get("id")?,
                r.get("flow_id")?,
                r.get("flow_name")?,
                r.get("tab_id")?,
                r.get("graph").map_err(|e| format!("the run's steps cannot be read ({e})")),
                // Of the wrong type, it is only shown blank: the state's error says why.
                r.get("node_id").unwrap_or_default(),
                r.get("status")?,
                RunState::from_row(r),
            ))
        })
        .map_err(sql)?
        .collect::<Result<Vec<Row>, _>>()
        .map_err(sql)?;
    rows.into_iter()
        .map(|(id, flow_id, flow_name, tab_id, graph, step_id, status, state)| {
            // A run that cannot be read is still listed, saying why, so it can be stopped.
            let graph = graph.and_then(|graph| parse_graph(&graph));
            // Only shown, so a failure to count leaves the run listed, and stoppable.
            let (done, total) = match &graph {
                Ok(graph) => progress(conn, id, graph).unwrap_or_else(|e| {
                    log_once("progress", id, || format!("could not count the steps run {id} has done: {e}"));
                    (0, 0)
                }),
                Err(_) => (0, 0),
            };
            let node = graph.and_then(|graph| {
                let node = graph.nodes.into_iter().find(|n| n.id == step_id);
                node.ok_or_else(|| format!("the run is at a missing step {step_id:?}"))
            });
            // Only active statuses are selected, so this one is known.
            let status = Status::parse(&status)?;
            let (step, outputs, note, resumable, step_kind, prefill, briefing, steps) = match node.and_then(|node| state.map(|state| (node, state))) {
                Ok((node, state)) => {
                    let resumable = continue_offered(&state.phase, node.kind).is_some();
                    let (prefill, steps) = (state.phase.prefill(), state.steps);
                    match state.phase {
                        // Nothing to pick until the first step is taken (see `proceed`).
                        Phase::Clearing { .. } => {
                            let note = Some("Clearing the conversation before the first step.".to_owned());
                            (node.name().to_owned(), Vec::new(), note, false, Some(node.kind), prefill, false, steps)
                        }
                        Phase::Briefing { .. } => (node.name().to_owned(), Vec::new(), None, false, Some(node.kind), prefill, true, steps),
                        Phase::NeedsPerson { why, .. } => (node.name().to_owned(), node.outputs, Some(why), resumable, Some(node.kind), prefill, false, steps),
                        _ => (node.name().to_owned(), node.outputs, None, resumable, Some(node.kind), prefill, false, steps),
                    }
                }
                Err(e) => (step_id.clone(), Vec::new(), Some(format!("This run cannot be read: {e}. Stop it.")), false, None, false, false, 0),
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
        let path = db::test_util::temp_db(name);
        let db = Db::open(path.clone()).unwrap();
        let id = db.create_flow("F").unwrap().id;
        db.save_flow_graph(id, graph).unwrap();
        (db, db::connect_existing(&path).unwrap(), id)
    }

    pub(super) fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        let mut all = vec![node("start", Kind::Start, &["next"])];
        all.extend(nodes);
        Graph { version: 1, nodes: all, edges }
    }

    /// Start → a pause → a prompt.
    pub(super) fn manual_first() -> Graph {
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
        let path = db::test_util::temp_db("runs-expire-no-lock");
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
        let path = db::test_util::temp_db("runs-expire-unreadable");
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
        assert_eq!(state(&conn, readable).phase.status(), Status::Attention);
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
    pub(super) fn manual_prompt_first(draft: &str) -> Graph {
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
        conn.execute("UPDATE flow_runs SET resume = 'someday', prefill = 1 WHERE id = ?1", [run]).unwrap();
        let row = |conn: &Connection| (column::<bool>(conn, run, "prefill"), column::<i64>(conn, run, "updated_at"));
        let before = row(&conn);
        prefilled(&mut conn, run, "mp", token).unwrap();
        assert_eq!(row(&conn), before);
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
        let path = db::test_util::temp_db("runs-restart");
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
    pub(super) fn review_loop() -> Graph {
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
        let path = db::test_util::temp_db("runs-unconfirmed");
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


    /// Run `run`'s column `name`, as stored.
    fn column<T: rusqlite::types::FromSql>(conn: &Connection, run: i64, name: &str) -> T {
        conn.query_row(&format!("SELECT {name} FROM flow_runs WHERE id = ?1"), [run], |r| r.get(0)).unwrap()
    }

    /// A run at `node_id` in `phase`, having taken 3 steps, whose Claude has the rules.
    pub(super) fn at_step(node_id: &str, phase: Phase) -> RunState {
        RunState { node_id: node_id.into(), steps: 3, briefed: true, prefill_token: None, phase }
    }

    pub(super) fn needs(why: &str, resume: Option<Resume>, prefill: bool) -> Phase {
        Phase::NeedsPerson { why: why.into(), resume, prefill }
    }

    /// Every phase, each in a way that shows its data.
    fn phases() -> Vec<Phase> {
        vec![
            Phase::Clearing { done: false },
            Phase::Clearing { done: true },
            Phase::Briefing { sent_at: 1234 },
            Phase::Working,
            Phase::Waiting { prefill: false },
            Phase::Waiting { prefill: true },
            needs("why", Some(Resume::Prompt), true),
            needs("why", Some(Resume::Remind), false),
            needs("why", None, false),
            Phase::Ended { stopped: true, owes_closing: true },
            Phase::Ended { stopped: false, owes_closing: false },
        ]
    }

    /// Each save also passes flow_runs' CHECKs (see `db::schema`'s MIGRATIONS), which hold every phase.
    #[test]
    fn a_run_s_state_is_saved_and_loaded_whole() {
        let (_db, mut conn, flow) = setup("runs-state-round-trip", &manual_first());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        for (i, phase) in phases().into_iter().enumerate() {
            let set = RunState { node_id: "m".into(), steps: 7, briefed: i % 2 == 0, prefill_token: (i % 3 == 0).then_some(42), phase };
            set.save(&conn, run).unwrap();
            assert_eq!(state(&conn, run), set);
        }
        assert_eq!(RunState::load(&conn, run + 1).unwrap(), None);
        // Saving a run that is not there is an error naming it.
        let missing = state(&conn, run).save(&conn, run + 1).unwrap_err();
        assert!(missing.contains(&format!("run {}", run + 1)), "{missing}");
    }

    /// Columns as a running run with nothing else to it stores them.
    fn working_columns() -> Columns {
        at_step("p", Phase::Working).columns()
    }

    #[test]
    fn columns_the_status_leaves_no_room_for_are_refused_and_named() {
        let running = working_columns();
        let manual = Columns { status: "manual".into(), ..working_columns() };
        let attention = Columns { status: "attention".into(), note: Some("why".into()), ..working_columns() };
        let cases = [
            (Columns { note: Some("stale".into()), ..running.clone() }, "running with a note"),
            (Columns { resume: Some("remind".into()), ..running.clone() }, "running with a way to continue"),
            (Columns { prefill: true, ..running.clone() }, "running with a paste owed"),
            (Columns { clear: Some("pending".into()), briefing: true, briefing_at: Some(1), ..running.clone() }, "both a clear and a briefing"),
            (Columns { briefing: true, ..running.clone() }, "a briefing with no time"),
            (Columns { closing: true, ..running }, "running with a finished flow's note owed"),
            (Columns { note: Some("why".into()), ..manual.clone() }, "manual with a note"),
            (Columns { resume: Some("prompt".into()), ..manual.clone() }, "manual with a way to continue"),
            (Columns { clear: Some("done".into()), ..manual }, "manual with a clear"),
            (Columns { note: None, ..attention.clone() }, "attention with no note"),
            (Columns { briefing: true, briefing_at: Some(1), ..attention.clone() }, "attention with a briefing"),
            (Columns { clear: Some("pending".into()), ..attention }, "attention with a clear"),
        ];
        for (columns, named) in cases {
            let e = RunState::from_columns(columns.clone()).unwrap_err();
            assert!(e.contains(named), "{columns:?}: {e}");
        }
    }

    #[test]
    fn what_older_versions_left_behind_is_dropped_on_load() {
        // A stop leaves the other columns as they were (and always has), so an ended
        // run is read by its status and `closing` alone.
        let stopped = Columns {
            status: "stopped".into(),
            note: Some("why".into()),
            resume: Some("prompt".into()),
            clear: Some("pending".into()),
            briefing: true,
            briefing_at: Some(5),
            prefill: true,
            closing: true,
            ..working_columns()
        };
        let state = RunState::from_columns(stopped).unwrap();
        assert_eq!(state.phase, Phase::Ended { stopped: true, owes_closing: true });
        // A briefing's time, which versions before this one kept after the briefing.
        let stale = Columns { briefing_at: Some(5), ..working_columns() };
        let state = RunState::from_columns(stale).unwrap();
        assert_eq!(state.phase, Phase::Working);
        assert_eq!(state.columns(), working_columns());
        // Nor are an ended run's resume and clear read, even ones this version does not know.
        let odd = Columns { status: "done".into(), resume: Some("someday".into()), clear: Some("later".into()), ..working_columns() };
        assert_eq!(RunState::from_columns(odd).unwrap().phase, Phase::Ended { stopped: false, owes_closing: false });
        // A value this version does not know is still an error on an active run.
        let odd = Columns { resume: Some("someday".into()), ..working_columns() };
        assert!(RunState::from_columns(odd).unwrap_err().contains("someday"));
    }

    #[test]
    fn owes_closing_and_its_sql_agree() {
        let (_db, mut conn, flow) = setup("runs-owes-closing", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        for briefed in [false, true] {
            for phase in phases().into_iter().filter(|p| !matches!(p, Phase::Ended { .. })) {
                let s = RunState { briefed, ..at_step("p", phase) };
                s.save(&conn, run).unwrap();
                let owes: bool =
                    conn.query_row(&format!("SELECT {OWES_CLOSING} FROM flow_runs WHERE id = ?1"), [run], |r| r.get(0)).unwrap();
                assert_eq!(owes, s.owes_closing(), "{s:?}");
            }
        }
    }

    #[test]
    fn a_run_this_version_cannot_read_can_still_be_stopped_and_owes_what_its_row_says() {
        let (_db, mut conn, flow) = setup("runs-stop-unreadable", &runnable());
        conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
        for (tab_id, odd, owes) in [("t1", "clear = 'later'", false), ("t2", "resume = 'someday'", true)] {
            let run = begin(&mut conn, flow, &tab(tab_id)).unwrap().run_id;
            conn.execute(&format!("UPDATE flow_runs SET {odd} WHERE id = ?1"), [run]).unwrap();
            stop(&conn, run).unwrap();
            assert_eq!((column::<String>(&conn, run, "status"), column::<bool>(&conn, run, "closing")), ("stopped".into(), owes), "{odd}");
        }
        assert!(stop(&conn, 999).unwrap_err().contains("no active run 999"));
    }

    /// The stored columns a stop clears or sets: status, note, resume, prefill and closing.
    fn stop_columns(conn: &Connection, run: i64) -> (String, Option<String>, Option<String>, bool, bool) {
        conn.query_row("SELECT status, note, resume, prefill, closing FROM flow_runs WHERE id = ?1", [run], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
    }

    #[test]
    fn every_way_of_stopping_a_readable_run_leaves_the_same_row() {
        for how in ["stop", "stop_tab", "after_restart"] {
            let (_db, mut conn, flow) = setup(&format!("runs-stop-shape-{how}"), &runnable());
            let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
            let needing = RunState { prefill_token: Some(5), ..at_step("p", needs("why", Some(Resume::Prompt), true)) };
            needing.save(&conn, run).unwrap();
            match how {
                "stop" => stop(&conn, run).unwrap(),
                "stop_tab" => stop_tab(&conn, &tab("t1")).unwrap(),
                _ => after_restart(&mut conn, &[]).unwrap(),
            }
            // Claude has the run's rules, so ■ owes it the note that the flow is finished;
            // a tab that is gone sends no prompt to carry it.
            let owes = how == "stop";
            assert_eq!(stop_columns(&conn, run), ("stopped".into(), None, None, false, owes), "{how}");
            let want = RunState { phase: Phase::Ended { stopped: true, owes_closing: owes }, ..needing.stopped() };
            assert_eq!(state(&conn, run), want, "{how}");
        }
    }

    #[test]
    fn a_run_whose_columns_have_the_wrong_type_does_not_hold_up_the_others() {
        let (_db, mut conn, flow) = setup("runs-bad-type", &runnable());
        let odd = start(&mut conn, flow, &tab("odd")).unwrap().run_id;
        let fine = start(&mut conn, flow, &tab("fine")).unwrap().run_id;
        conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
        conn.execute("UPDATE flow_runs SET steps = 'many', briefed = 'yes' WHERE id = ?1", [odd]).unwrap();
        conn.execute("UPDATE flow_runs SET briefing_at = briefing_at - ?1", [BRIEFING_TIMEOUT_SECS + 1]).unwrap();
        expire(&mut conn).unwrap();
        assert_eq!(state(&conn, fine).phase.status(), Status::Attention);
        let runs = active(&conn).unwrap();
        let summary: Vec<_> = runs.iter().map(|r| (r.id, r.status)).collect();
        assert_eq!(summary, [(odd, Status::Running), (fine, Status::Attention)]);
        assert!(runs[0].note.as_deref().unwrap().contains("cannot be read"), "{:?}", runs[0].note);
        after_restart(&mut conn, &[tab("odd"), tab("fine")]).unwrap();
        assert_eq!(column::<String>(&conn, odd, "status"), "running");
        // It can still be stopped.
        stop(&conn, odd).unwrap();
        assert_eq!(column::<String>(&conn, odd, "status"), "stopped");
    }

    #[test]
    fn after_a_restart_a_database_error_is_returned_not_skipped() {
        let (_db, mut conn, flow) = setup("runs-restart-sql-error", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        // A column the run's state is read from is gone (not one a CHECK names, which
        // SQLite will not drop): the database, not the run, is at fault.
        conn.execute_batch("ALTER TABLE flow_runs DROP COLUMN prefill_token").unwrap();
        let e = after_restart(&mut conn, &[tab("t1")]).unwrap_err();
        assert!(e.contains("prefill_token"), "{e}");
    }

    #[test]
    fn loading_a_run_tells_a_database_error_from_a_run_this_version_cannot_read() {
        let (_db, mut conn, flow) = setup("runs-load-error", &runnable());
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        conn.execute("UPDATE flow_runs SET graph = 'broken' WHERE id = ?1", [run]).unwrap();
        assert!(matches!(active_run(&conn, "id = ?1", &run), Err(LoadError::Unreadable(_))));
        assert!(matches!(active_run(&conn, "no_such_column = ?1", &run), Err(LoadError::Sql(_))));
    }

    #[test]
    fn rows_older_versions_wrote_load_and_save_back_unchanged() {
        let attention = Columns { status: "attention".into(), note: Some("why".into()), ..working_columns() };
        for c in [
            Columns { resume: Some("prompt".into()), ..attention.clone() },
            Columns { status: "manual".into(), prefill: true, prefill_token: None, ..working_columns() },
            Columns { prefill: true, ..attention },
        ] {
            let state = RunState::from_columns(c.clone()).unwrap();
            assert_eq!(state.columns(), c);
        }
    }

    #[test]
    fn the_briefing_s_time_goes_once_the_briefing_is_over() {
        let (_db, mut conn, flow) = setup("runs-briefing-at", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert!(column::<Option<i64>>(&conn, run, "briefing_at").is_some());
        on_stop(&mut conn, &tab("t1"), Some("Ready")).unwrap();
        assert_eq!(column::<Option<i64>>(&conn, run, "briefing_at"), None);
        // Nor does a briefing that was lost keep it.
        let (_db, mut conn, flow) = setup("runs-briefing-at-lost", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "x").unwrap();
        assert_eq!(column::<Option<i64>>(&conn, run, "briefing_at"), None);
    }

    #[test]
    fn a_pause_during_the_briefing_offers_no_continue_as_after_a_restart() {
        // Continue cannot go on at Start (see `continue_offered`), so neither leaves one.
        let (_db, mut conn, flow) = setup("runs-briefing-pause-resume", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "The Stop hook failed.").unwrap();
        let paused = state(&conn, run);
        assert_eq!((paused.phase, paused.briefed), (needs("The Stop hook failed.", None, false), false));
        let (_db, mut conn, flow) = setup("runs-briefing-restart-resume", &runnable());
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        after_restart(&mut conn, &[tab("t1")]).unwrap();
        assert!(matches!(state(&conn, run).phase, Phase::NeedsPerson { resume: None, .. }));
    }

    #[test]
    fn a_failed_hook_marks_the_run_through_runs_and_returns_its_error() {
        let (_db, mut conn, flow) = setup("runs-hook-failed", &runnable());
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(hook_failed(&mut conn, &tab("t1"), Hook::Stop, "boom".into()), "boom");
        let run = only_run(&conn);
        assert_eq!((run.status, run.resumable), (Status::Attention, true));
        assert!(run.note.unwrap().contains("The Stop hook could not move the run on (boom)"));
        // A prompt the hook could not record changes nothing at a prompt step.
        let (_db, mut conn, flow) = setup("runs-hook-failed-prompt", &manual_prompt_first("Fix it"));
        begin(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(hook_failed(&mut conn, &tab("t1"), Hook::Prompt, "bad".into()), "bad");
        let run = only_run(&conn);
        assert_eq!((run.status, run.prefill), (Status::Attention, true), "the paste stays owed");
        assert!(run.note.unwrap().contains("could not record your prompt (bad)"));
    }

    #[test]
    fn a_transition_leaves_alone_what_it_does_not_change() {
        let g = manual_prompt_first("Fix it");
        let (_db, mut conn, flow) = setup("runs-state-untouched", &g);
        let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let before = state(&conn, run);
        let token = before.prefill_token.unwrap();
        assert_eq!(before.phase, Phase::Waiting { prefill: true });
        // The paste stays owed, under the same token.
        prompt_unseen(&mut conn, &tab("t1"), "Your prompt could not be recorded.").unwrap();
        let after = state(&conn, run);
        assert_eq!(after, RunState { phase: needs("Your prompt could not be recorded.", None, true), ..before.clone() });
        // A paste recorded under another token changes nothing.
        prefilled(&mut conn, run, "mp", token + 1).unwrap();
        assert_eq!(state(&conn, run), after);
        prefilled(&mut conn, run, "mp", token).unwrap();
        assert_eq!(state(&conn, run), RunState { phase: needs("Your prompt could not be recorded.", None, false), ..after });
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
        assert_eq!(before, RunState::initial(&g.nodes[0], true));
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(state(&conn, run), RunState { phase: Phase::Clearing { done: true }, briefed: false, ..before });
    }

    #[test]
    fn the_checks_on_flow_runs_accept_an_active_row_exactly_when_from_columns_reads_it() {
        let (_db, mut conn, flow) = setup("runs-checks-agree", &runnable());
        conn.pragma_update(None, "ignore_check_constraints", false).unwrap();
        let (mut stored_any, mut refused_any) = (false, false);
        // Every active status, with each column the CHECKs name set or not.
        for i in 0..3 * 2 * 2 * 3 * 2 * 2 * 2 * 2 {
            let mut left = i;
            let mut pick = |n: usize| {
                let picked = left % n;
                left /= n;
                picked
            };
            let c = Columns {
                status: ["running", "manual", "attention"][pick(3)].into(),
                note: (pick(2) == 1).then(|| "why".to_string()),
                resume: (pick(2) == 1).then(|| "remind".to_string()),
                clear: [None, Some("pending"), Some("done")][pick(3)].map(String::from),
                briefing: pick(2) == 1,
                briefing_at: (pick(2) == 1).then_some(5),
                prefill: pick(2) == 1,
                closing: pick(2) == 1,
                ..working_columns()
            };
            let tx = conn.transaction().unwrap();
            let inserted = tx.execute(
                "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, steps, note, started_at, updated_at, resume, clear,
                                        briefed, prefill, prefill_token, briefing, briefing_at, closing)
                 VALUES (?1, 't1', '{}', ?2, ?3, ?4, ?5, 0, 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    flow, c.node_id, c.status, c.steps, c.note, c.resume, c.clear, c.briefed, c.prefill, c.prefill_token, c.briefing,
                    c.briefing_at, c.closing
                ],
            );
            drop(tx);
            let stored = match inserted {
                Ok(_) => true,
                Err(e) => {
                    assert!(e.to_string().contains("CHECK constraint failed"), "{c:?}: {e}");
                    false
                }
            };
            (stored_any, refused_any) = (stored_any || stored, refused_any || !stored);
            // from_columns also reads a briefing's time the briefing has outlived,
            // which only writers that bypass the CHECKs can leave.
            let stale_briefing_at = !c.briefing && c.briefing_at.is_some();
            assert_eq!(stored, RunState::from_columns(c.clone()).is_ok() && !stale_briefing_at, "{c:?}");
        }
        assert!(stored_any && refused_any);
    }

    #[test]
    fn start_reports_a_refused_insert_as_itself_not_as_a_busy_tab() {
        let (_db, mut conn, flow) = setup("runs-start-refused", &runnable());
        conn.execute_batch("CREATE TEMP TRIGGER refuse BEFORE INSERT ON flow_runs BEGIN SELECT RAISE(ABORT, 'refused here'); END")
            .unwrap();
        let e = start(&mut conn, flow, &tab("t1")).unwrap_err();
        assert!(e.contains("refused here") && !e.contains("already running"), "{e}");
    }

    #[test]
    fn paste_tokens_stay_within_what_javascript_holds_exactly() {
        for _ in 0..1000 {
            let token = new_token();
            assert!((0..(1 << 53) - 1).contains(&token), "{token}");
        }
    }
}
