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
//! A prompt step that pauses (`Node::pause`) waits for the person once Claude has
//! answered, with the output Claude picked suggested: ▶ in the Flows box goes on by
//! it, or the person picks another, or stops the run. One with no prompt only
//! waits. A draft step has the app put the step's draft (led by Start's base info
//! after a clear) into Claude's input without sending it (`prefill_text`,
//! `prefilled`); the person's own prompt, reported by the UserPromptSubmit hook
//! (`on_prompt`), has the step run, and Claude's answer moves it on: by its one
//! output, with no `FLOW:` line, or by the `FLOW:` line Claude was asked for along
//! with the prompt when it has several. ▶ skips it. A reply the
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
    /// Waiting for the person: at a pause, to go on (▶, by the output Claude
    /// suggested, or the step's one output) or stop the run; at a draft step, also
    /// to send their prompt, which makes the step `Running` until Claude answers.
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
    /// Why the run waits, when it needs attention; at a pause, the step's note.
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
    /// Whether the app has yet to fill Claude's input for the draft step the run
    /// waits at (see `prefill_text`).
    pub prefill: bool,
    /// The kind of the step the run is at; None for a run that cannot be read.
    pub step_kind: Option<Kind>,
    /// Whether the run waits at Start for Claude's reply to its briefing.
    pub briefing: bool,
    /// The output Claude picked at a step that pauses, which ▶ goes on by.
    pub suggested: Option<String>,
    /// Whether the run waits for the person's own prompt at a draft step.
    pub awaits_prompt: bool,
}

/// How Continue has Claude go on with a step that needs attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resume {
    /// Claude still has the step in mind (it asked for a person, or its reply could
    /// not be followed): remind it to go on and how to end.
    Remind,
    /// Claude no longer has the step in mind (the conversation was cleared, the
    /// step's prompt could not be typed, see `unsent`, or a prompt the app typed for
    /// a run moved on by hand was lost, see `Phase::Typed`): send the step's prompt
    /// again.
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

/// Statuses in which a run holds its tab, as SQL: written out, since the
/// `one_active_run_per_tab` index in migration 7 lists the same ones. A test checks
/// both against `Status::ACTIVE`.
pub(crate) const ACTIVE: &str = "('running', 'manual', 'attention')";

impl Status {
    /// The statuses in which a run holds its tab, which the module's `ACTIVE`
    /// writes out as SQL; a test checks they agree.
    #[cfg(test)]
    const ACTIVE: [Status; 3] = [Status::Running, Status::Manual, Status::Attention];

    /// Every status, for tests to go through.
    #[cfg(test)]
    const ALL: [Status; 5] = [Status::Running, Status::Manual, Status::Attention, Status::Done, Status::Stopped];

    /// The statuses in which Claude's turn ending may move a run on (see `on_stop`).
    const LISTENING: [Status; 2] = [Status::Running, Status::Attention];

    /// The statuses in which the person's prompt may change a run (see `on_prompt`),
    /// besides a running one whose typed prompt is pending (`Phase::Typed`).
    const PROMPTED: [Status; 2] = [Status::Manual, Status::Attention];

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

/// `statuses` as an SQL list, such as `('running', 'manual')`.
fn sql_list(statuses: &[Status]) -> String {
    let quoted: Vec<String> = statuses.iter().map(|s| format!("'{}'", s.as_str())).collect();
    format!("({})", quoted.join(", "))
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
/// numbers hold exactly (2^53 - 1), as the app hands it back; never `NO_TOKEN`.
fn new_token() -> i64 {
    const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
    i64::try_from(uuid::Uuid::new_v4().as_u64_pair().0 % (MAX_SAFE_INTEGER - 1) + 1).unwrap_or(1)
}

/// The token naming a paste owed with none stored (`prefill_token` NULL, as a
/// database edited by hand may leave it), so that paste too can be recorded.
const NO_TOKEN: i64 = 0;

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
    /// person's prompt at a draft step.
    Working,
    /// Running a prompt step whose prompt the app typed into the tab after the person
    /// moved a running step on, entered from `Working` or from `Typed` itself (a
    /// repeat pick; see `transition::proceed`). Claude may still be answering the
    /// step before, so a turn that ends before Claude Code reports this step's own
    /// prompt submitted (see `on_prompt` and `transition::prompt_sent`) is not its
    /// reply; once it does, the run is `Working`. `turn_ended`: a turn has ended since
    /// the prompt was typed, or since another prompt was submitted, with none
    /// submitted after it (status running, typed 1 without, 2 with): one more turn
    /// ending without the step's prompt means the prompt was not sent (folded into
    /// the turn before, dropped by Esc, or edited away), and the run waits for the
    /// person (see `transition::on_stop`).
    ///
    /// This rests on what Claude Code 2.1.326 was seen to do: a prompt typed during
    /// a turn is held, and its UserPromptSubmit hook runs after that turn's Stop
    /// hook. Moving on by hand into a step that clears or briefs Claude first (a
    /// late reply landing on those) is out of scope: only prompt steps are `Typed`.
    Typed { turn_ended: bool },
    /// Waiting for the person (status manual): at a pause to go on, at a draft step
    /// for their prompt; `prefill`, whether the app has yet to fill Claude's input for
    /// that prompt (prefill 1; see `prefill_text`); `suggested`, the output Claude
    /// picked at a step that pauses once done (suggested), which ▶ goes on by.
    Waiting { prefill: bool, suggested: Option<String> },
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
            Phase::Clearing { .. } | Phase::Briefing { .. } | Phase::Working | Phase::Typed { .. } => Status::Running,
            Phase::Waiting { .. } => Status::Manual,
            Phase::NeedsPerson { .. } => Status::Attention,
            Phase::Ended { stopped: true, .. } => Status::Stopped,
            Phase::Ended { stopped: false, .. } => Status::Done,
        }
    }

    /// Whether the app has yet to fill Claude's input for the draft step the run
    /// waits at (see `prefill_text`).
    fn prefill(&self) -> bool {
        matches!(self, Phase::Waiting { prefill: true, .. } | Phase::NeedsPerson { prefill: true, .. })
    }

    /// The same phase, owing a paste or not; a phase that cannot owe one is as it is.
    fn with_prefill(self, owed: bool) -> Self {
        match self {
            Phase::Waiting { suggested, .. } => Phase::Waiting { prefill: owed, suggested },
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
    /// 0, or 1 or 2 for `Phase::Typed` (see there).
    typed: i64,
    suggested: Option<String>,
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
            typed: row.get("typed")?,
            suggested: row.get("suggested")?,
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
    const COLUMNS: &str =
        "node_id, status, steps, note, resume, clear, briefed, prefill, prefill_token, briefing, briefing_at, closing, typed, suggested";

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
            Status::Running | Status::Attention if c.suggested.is_some() => return cannot("a suggested output"),
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
                let typed = match c.typed {
                    0 => None,
                    1 | 2 => Some(Phase::Typed { turn_ended: c.typed == 2 }),
                    other => return Err(format!("a run has the typed state {other}, which this version does not know")),
                };
                match (clear, c.briefing, typed) {
                    (Some(_), true, _) => return cannot("both a clear and a briefing"),
                    (Some(_), false, Some(_)) => return cannot("both a clear and a typed prompt"),
                    (None, true, Some(_)) => return cannot("both a briefing and a typed prompt"),
                    (Some(clear), false, None) => Phase::Clearing { done: clear == Clear::Done },
                    (None, true, None) => match c.briefing_at {
                        Some(sent_at) => Phase::Briefing { sent_at },
                        None => return cannot("a briefing with no time it was sent"),
                    },
                    (None, false, Some(typed)) => typed,
                    (None, false, None) => Phase::Working,
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
                if c.typed != 0 {
                    return cannot("a typed prompt");
                }
                if status == Status::Manual {
                    if c.note.is_some() {
                        return cannot("a note");
                    }
                    if resume.is_some() {
                        return cannot("a way to continue");
                    }
                    // A suggestion means Claude has answered: no prompt of the person's is owed.
                    if c.prefill && c.suggested.is_some() {
                        return cannot("both a paste owed and a suggested output");
                    }
                    Phase::Waiting { prefill: c.prefill, suggested: c.suggested }
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
            typed: 0,
            suggested: None,
        };
        match &self.phase {
            Phase::Clearing { done } => c.clear = Some(if *done { Clear::Done } else { Clear::Pending }.as_str().to_owned()),
            Phase::Briefing { sent_at } => (c.briefing, c.briefing_at) = (true, Some(*sent_at)),
            Phase::Working => {}
            Phase::Typed { turn_ended } => c.typed = 1 + i64::from(*turn_ended),
            Phase::Waiting { prefill, suggested } => (c.prefill, c.suggested) = (*prefill, suggested.clone()),
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
                 briefing_at = :briefing_at, closing = :closing, typed = :typed, suggested = :suggested, updated_at = :updated_at
                 WHERE id = :id",
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
                    ":typed": c.typed,
                    ":suggested": c.suggested,
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
        Phase::NeedsPerson { resume, .. } if matches!(kind, Kind::Prompt | Kind::Title) => *resume,
        _ => None,
    }
}

/// Whether the run waits for the person at a draft step (`node`, where it is): for
/// their prompt, or, once Claude's answer to it was lost or could not be followed,
/// for it again or an output. Not once Claude has answered a step that pauses,
/// which waits for the person to go on.
fn awaits_prompt(state: &RunState, node: &Node) -> bool {
    node.drafts() && matches!(state.phase, Phase::Waiting { suggested: None, .. } | Phase::NeedsPerson { .. })
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
    // A newer misag may have stored fields this one would ignore and run differently.
    flows::check(&graph)?;
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
/// that waits at Start, a pause or a draft step always is. A draft step with one
/// output the person sent takes it, whatever the reply (and waits there with it
/// suggested if the step pauses).
pub fn on_stop(conn: &mut Connection, tab_id: &TabId, reply: Option<&str>) -> Result<Option<String>, String> {
    // Every turn of every tab ends here: most tabs run no flow, so look before
    // taking the write lock.
    let listening = conn
        .query_row(
            &format!("SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND status IN {}", sql_list(&Status::LISTENING)),
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
    // Leaving a tab title step on Claude's reply hands its title to the app.
    let named = change.left.as_ref().and_then(|(id, _)| node_of(&run.graph, id).ok()).filter(|n| n.kind == Kind::Title);
    if let Some(title) = named.and(reply).and_then(flows::parse_title) {
        name_tab(&tx, tab_id, &title)?;
    }
    let next = run.apply(&tx, change)?;
    tx.commit().map_err(sql)?;
    Ok(texts(next))
}

/// A tab title Claude wrote at a tab title step, for the app to make the tab's name.
/// Mirrored by `TabTitle` in src/runController.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TabTitle {
    pub tab_id: String,
    pub title: String,
}

/// Has the app name tab `tab_id` `title` (see `take_tab_titles`); a newer title
/// replaces one not yet taken.
fn name_tab(tx: &Transaction, tab_id: &TabId, title: &str) -> Result<(), String> {
    tx.execute(
        "INSERT INTO tab_titles (tab_id, title) VALUES (?1, ?2) ON CONFLICT (tab_id) DO UPDATE SET title = excluded.title",
        params![tab_id.as_str(), title],
    )
    .map(drop)
    .map_err(sql)
}

/// The tab titles Claude wrote since the last call (see `name_tab`), each handed
/// over once: the app polls this with the runs and names the tabs.
pub fn take_tab_titles(conn: &mut Connection) -> Result<Vec<TabTitle>, String> {
    let mut stmt = conn.prepare_cached("DELETE FROM tab_titles RETURNING tab_id, title").map_err(sql)?;
    let titles = stmt.query_map([], |r| Ok(TabTitle { tab_id: r.get(0)?, title: r.get(1)? })).map_err(sql)?;
    titles.collect::<Result<_, _>>().map_err(sql)
}

/// The person picked `output` for `step_id`, the step they saw the run at; returns
/// the next prompt to send, if any. Usually the run waits (manual or needs
/// attention), but a running step can be moved on too, for when Claude was
/// interrupted and no reply will come; as Claude may yet be answering it, a prompt
/// step reached that way counts a reply only once Claude Code reports its prompt
/// submitted (see `Phase::Typed`). Refused if the run has moved on from
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

/// What the app puts into Claude's input for a draft step (see `prefill_text`).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Prefill {
    pub text: String,
    /// Names this paste to `prefilled`.
    pub token: i64,
}

/// The text to put into Claude's input for the draft step run `run_id` waits at,
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
    Ok(Some(Prefill { text, token: state.prefill_token.unwrap_or(NO_TOKEN) }).filter(|p| !p.text.is_empty()))
}

/// The app put the draft step's text named by `token` into Claude's input (see
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
/// a run waiting at a draft step runs it, so Claude's answer moves it on (see
/// `on_stop`); with several outputs, Claude is told to end with one of them (see
/// `flows::draft_rule`). Claude has Start's base info if the prompt carried it. A run moved
/// on by hand (`Phase::Typed`) reacts to the step's own prompt, which the app
/// typed: Claude's answer to it is the step's reply (see `transition::prompt_sent`).
/// A command Claude Code handles itself is not a prompt to Claude (see
/// `local_command`), and prompts at any other step change nothing.
/// Returns what to add to Claude's context with the prompt: that a flow which ran
/// in the tab is finished, owed since it ended (see `RunState::owes_closing`), and
/// a draft step's rule.
pub fn on_prompt(conn: &mut Connection, tab_id: &TabId, prompt: &str) -> Result<Option<String>, String> {
    if local_command(prompt) {
        return Ok(None);
    }
    // Every prompt in every tab comes here: look before taking the write lock. The
    // phases listed are those `transition::prompt_sent` changes: Waiting and
    // NeedsPerson (manual, attention) and Typed (running, typed 1 or 2).
    let waiting = conn
        .query_row(
            &format!(
                "SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND (status IN {} OR (status = ?2 AND typed <> 0))
                 UNION ALL SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND closing = 1 LIMIT 1",
                sql_list(&Status::PROMPTED)
            ),
            params![tab_id.as_str(), Status::Running.as_str()],
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
    let mut rule = None;
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())?
        && let Some(state) = transition::prompt_sent(&run.cx(NOTHING_TAKEN), &run.state, prompt)?
    {
        let node = node_of(&run.graph, &state.node_id)?;
        if state.phase == Phase::Working && awaits_prompt(&run.state, node) {
            rule = flows::draft_rule(node);
        }
        state.save(&tx, run.id)?;
    }
    tx.commit().map_err(sql)?;
    let context: Vec<String> = closing.into_iter().chain(rule).collect();
    Ok((!context.is_empty()).then(|| context.join("\n\n")))
}

/// Commands Claude Code runs itself, whose output is not an answer to the person's
/// prompt: sending one is not their prompt at a draft step (no answer would move
/// the step on), nor does it carry a finished flow's note. Other text that starts
/// with a slash is a prompt: a skill, a custom or MCP command, a built-in sent to
/// Claude as a prompt (/review, /init, /doctor), or a file path.
///
/// Matches the first word only, aliases included, as listed for Claude Code 2.1.326
/// (code.claude.com/docs/en/commands). A command missing here counts as a prompt:
/// the step shows Running, and the person can still pick an output. Update it when
/// Claude Code adds commands.
const LOCAL_COMMANDS: &[&str] = &[
    "add-dir", "advisor", "agents", "android", "app", "autocompact", "bashes", "branch", "btw", "bug", "cd", "checkpoint",
    "chrome", "clear", "color", "compact", "config", "context", "continue", "copy", "cost", "desktop", "design-login",
    "diff", "effort", "exit", "export", "fast", "feedback", "focus", "fork", "goal", "heapdump", "help", "hooks", "ide",
    "import", "insights", "install-github-app", "install-slack-app", "ios", "keybindings", "list-agents", "login",
    "logout", "mcp", "memory", "mobile", "model", "new", "output-style", "passes", "permissions", "plan", "plugin",
    "powerup", "privacy-settings", "quit", "radio", "rate-limit-options", "recap", "release-notes", "reload-plugins",
    "reload-skills", "remote-control", "remote-env", "rename", "reset", "restart", "resume", "rewind", "sandbox",
    "scroll-speed", "setup-bedrock", "setup-vertex", "skill-doctor", "skills", "stats", "status", "stickers", "stop",
    "subtask", "tasks", "teleport", "terminal-setup", "theme", "todos", "tp", "tui", "undo", "upgrade", "usage",
    "usage-credits", "vim", "voice", "web-setup", "workflows",
];

/// Whether `prompt` is one of `LOCAL_COMMANDS`, with or without arguments.
fn local_command(prompt: &str) -> bool {
    let Some(rest) = prompt.trim_start().strip_prefix('/') else { return false };
    let name = rest.split_whitespace().next().unwrap_or_default();
    LOCAL_COMMANDS.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// The UserPromptSubmit hook could not record a prompt in tab `tab_id`, failing
/// with `error`: a run waiting at a draft step, or for the prompt the app typed
/// (`Phase::Typed`), would never see Claude's answer, so it waits for the person,
/// saying why (see `transition::prompt_unseen`), rather than for a prompt already
/// sent. Its paste, if still owed, stays owed.
fn prompt_unseen(conn: &mut Connection, tab_id: &TabId, error: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? else { return Ok(()) };
    let Some(state) = transition::prompt_unseen(&run.cx(NOTHING_TAKEN), &run.state, error)? else { return Ok(()) };
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
/// `pause_tab`); after the UserPromptSubmit hook, so does a draft step waiting
/// for the person's prompt, or a run waiting for the prompt the app typed (see
/// `prompt_unseen`). Best effort: returns `error`, with why the run could not be
/// marked if it could not.
pub fn hook_failed(conn: &mut Connection, tab_id: &TabId, hook: Hook, error: String) -> String {
    let marked = match hook {
        Hook::Stop => pause_tab(conn, tab_id, &format!("The Stop hook could not move the run on ({error}). Pick an output to go on.")),
        Hook::Prompt => prompt_unseen(conn, tab_id, &error),
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

/// The Stop hook failed in tab `tab_id`: makes the tab's running run wait for the
/// person, saying why, as Claude's reply to its step was not seen (see
/// `transition::stop_failed`), so Continue sends the step's prompt again, except
/// while the run reads its briefing, where the person picks an output, and while it
/// runs a draft step, where the person sends their prompt again or picks one
/// (see `transition::answer_lost`). A run still at Start waiting for its clear has
/// no step to reply to, and one waiting for the prompt the app typed lost no reply
/// of its step: both are left alone.
fn pause_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    if let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? {
        run.save(&tx, &transition::stop_failed(&run.cx(NOTHING_TAKEN), run.state.clone(), why)?)?;
    }
    tx.commit().map_err(sql)
}

/// Stops active run `run_id` (see `stop`), in the caller's transaction; false if
/// there is no such run. A run this version can read is saved stopped
/// (`RunState::stopped`). One it cannot read (a resume or clear state from a newer
/// version, which `RunState::from_columns` refuses) must still stop, so it is one
/// raw UPDATE of the status, which works out what it owes Claude from the row
/// (`OWES_CLOSING`) as a best guess, drops a typed prompt's wait (`typed`, which
/// `on_prompt` looks for) and a suggested output, and leaves the rest as it was.
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
                &format!("UPDATE flow_runs SET status = ?4, closing = ?3 AND ({OWES_CLOSING}), typed = 0, suggested = NULL, updated_at = ?2 WHERE id = ?1 AND status IN {ACTIVE}"),
                params![run_id, now(), tab_open, Status::Stopped.as_str()],
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
pub fn stop_tab(conn: &mut Connection, tab_id: &TabId) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
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
pub fn stop(conn: &mut Connection, run_id: i64) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
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
        .prepare_cached(&format!("SELECT id, started_at, {} FROM flow_runs WHERE status = ?1", RunState::COLUMNS))
        .map_err(sql)?;
    let runs = stmt
        .query_map([Status::Running.as_str()], |r| Ok((r.get::<_, i64>("id")?, r.get::<_, i64>("started_at")?, RunState::from_row(r))))
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

/// An active run's row, as `active` reads it: what cannot be read is kept as its
/// error, so the run is still listed.
struct ActiveRow {
    id: i64,
    flow_id: i64,
    flow_name: String,
    tab_id: String,
    graph: Result<String, String>,
    step_id: String,
    status: String,
    state: Result<RunState, String>,
}

/// What `active` shows of the step a run is at.
struct Shown {
    step: String,
    outputs: Vec<String>,
    note: Option<String>,
    resumable: bool,
    step_kind: Option<Kind>,
    prefill: bool,
    briefing: bool,
    steps: i64,
    suggested: Option<String>,
    awaits_prompt: bool,
}

impl Shown {
    /// What is shown of a run at `node` in `state`.
    fn of(node: Node, state: RunState) -> Self {
        let resumable = continue_offered(&state.phase, node.kind).is_some();
        let awaits_prompt = awaits_prompt(&state, &node);
        let mut shown = Shown {
            step: node.name().to_owned(),
            outputs: Vec::new(),
            note: None,
            resumable: false,
            step_kind: Some(node.kind),
            prefill: state.phase.prefill(),
            briefing: false,
            steps: state.steps,
            suggested: None,
            awaits_prompt,
        };
        match state.phase {
            // Nothing to pick until the first step is taken (see `proceed`).
            Phase::Clearing { .. } => shown.note = Some("Clearing the conversation before the first step.".to_owned()),
            Phase::Briefing { .. } => shown.briefing = true,
            Phase::NeedsPerson { why, .. } => (shown.outputs, shown.note, shown.resumable) = (node.outputs, Some(why), resumable),
            // A pause says what to do there; a draft step waiting for the prompt has nothing to say yet.
            Phase::Waiting { suggested, .. } => {
                let note = (node.pauses() && !awaits_prompt && !node.note.trim().is_empty()).then(|| node.note.trim().to_owned());
                (shown.outputs, shown.note, shown.suggested) = (node.outputs, note, suggested);
            }
            _ => (shown.outputs, shown.resumable) = (node.outputs, resumable),
        }
        shown
    }

    /// What is shown of a run at `step_id` that cannot be read, saying why.
    fn unreadable(step_id: &str, why: &str) -> Self {
        Shown {
            step: step_id.to_owned(),
            outputs: Vec::new(),
            note: Some(format!("This run cannot be read: {why}. Stop it.")),
            resumable: false,
            step_kind: None,
            prefill: false,
            briefing: false,
            steps: 0,
            suggested: None,
            awaits_prompt: false,
        }
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
    let rows = stmt
        .query_map([], |r| {
            Ok(ActiveRow {
                id: r.get("id")?,
                flow_id: r.get("flow_id")?,
                flow_name: r.get("flow_name")?,
                tab_id: r.get("tab_id")?,
                graph: r.get("graph").map_err(|e| format!("the run's steps cannot be read ({e})")),
                // Of the wrong type, it is only shown blank: the state's error says why.
                step_id: r.get("node_id").unwrap_or_default(),
                status: r.get("status")?,
                state: RunState::from_row(r),
            })
        })
        .map_err(sql)?
        .collect::<Result<Vec<ActiveRow>, _>>()
        .map_err(sql)?;
    rows.into_iter()
        .map(|ActiveRow { id, flow_id, flow_name, tab_id, graph, step_id, status, state }| {
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
            let Shown { step, outputs, note, resumable, step_kind, prefill, briefing, steps, suggested, awaits_prompt } =
                match node.and_then(|node| state.map(|state| (node, state))) {
                    Ok((node, state)) => Shown::of(node, state),
                    Err(e) => Shown::unreadable(&step_id, &e),
                };
            Ok(Run {
                id,
                flow_id,
                flow_name,
                tab_id,
                step,
                outputs,
                step_id,
                status,
                note,
                steps,
                done,
                total,
                resumable,
                prefill,
                step_kind,
                briefing,
                suggested,
                awaits_prompt,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
