//! Flow runs: a flow being worked through in a tab, one at a time per tab.
//!
//! A run starts at the flow's Start step and moves along outputs. A prompt step
//! sends its text to Claude; when Claude ends its turn, the Stop hook
//! (`misag --claude-hook`, a separate process) reads the `FLOW: <output>` line
//! Claude ended with and moves on, handing Claude the next prompt as hook context.
//! A pause waits for the person, who goes on by its one output (▶ in the Flows
//! box) or stops the run. A manual prompt has the app put Start's base info (if
//! Claude lacks it) and the step's draft into Claude's input without sending it
//! (`prefill_text`, `prefilled`); the person's own prompt, reported by the
//! UserPromptSubmit hook (`on_prompt`), has the step run, and Claude's answer
//! moves it on by its one output, with no `FLOW:` line. ▶ skips it. A reply the runner cannot follow waits too: the
//! person picks an output in the editor's run bar (and can do so for a running
//! step too, if Claude was interrupted) or has Claude go on with the step. A pick or Continue names the step it was made for, so one that
//! arrives after the run moved on is refused. An output with no next step ends the
//! run.
//!
//! A flow whose Start clears begins in the tab's new conversation: its run holds
//! the tab at Start while the app types `/clear`, the SessionStart hook reports the
//! clear (`clear_tab`), and the app then takes the first step (`cleared`). A run
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
use std::time::SystemTime;

/// A run that has taken this many steps waits for the person, in case it loops.
pub const MAX_RUN_STEPS: i64 = 50;

/// How long a run whose Start clears may stay at Start before it waits for the
/// person: the app's own 30 seconds (`CLEAR_TIMEOUT_MS` in src/main.ts, after which
/// it stops the run, saying why), and a little more, so its stop comes first.
pub const CLEAR_TIMEOUT_SECS: i64 = 35;

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
    /// Whether Claude can be asked to go on with the step (see `resume`).
    pub resumable: bool,
    /// Whether the app has yet to fill Claude's input for the manual prompt the run
    /// waits at (see `prefill_text`).
    pub prefill: bool,
    /// The kind of the step the run is at; None for a run that cannot be read.
    pub step_kind: Option<Kind>,
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
    /// Type `/clear`, then wait for `cleared` to give the first prompt.
    pub clear: bool,
    /// The first prompt to send, if the flow begins with a prompt step and does not clear.
    pub prompt: Option<String>,
}

/// How a run whose Start clears is getting on. Mirrored in src/main.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Cleared {
    /// Claude has not reported the clear yet.
    Pending,
    /// Claude has cleared and the run took its first step; send `prompt`, if any.
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
    /// Whether Claude has Start's base info: false from the start and after a clear.
    briefed: bool,
    /// Whether the app has yet to fill Claude's input for the manual prompt.
    prefill: bool,
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
            &format!("SELECT id, graph, node_id, status, steps, resume, clear, briefed, prefill FROM flow_runs WHERE {filter} AND status IN {ACTIVE}"),
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
                ))
            },
        )
        .optional()
        .map_err(sql)?;
    row.map(|(id, graph, node_id, status, steps, resume, clear, briefed, prefill)| {
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
        })
    })
    .transpose()
}

fn set(tx: &Transaction, run: i64, node_id: &str, status: Status, steps: i64, note: Option<&str>) -> Result<(), String> {
    tx.execute(
        "UPDATE flow_runs SET node_id = ?2, status = ?3, steps = ?4, note = ?5, resume = NULL, clear = NULL, prefill = 0, updated_at = ?6 WHERE id = ?1",
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

/// The prompt of `node` for the run, led by Start's base info if Claude lacks it,
/// which it then has.
fn prompt_for(tx: &Transaction, run: &Stored, node: &Node) -> Result<String, String> {
    if run.briefed {
        return Ok(flows::prompt_text(node));
    }
    tx.execute("UPDATE flow_runs SET briefed = 1 WHERE id = ?1", [run.id]).map_err(sql)?;
    Ok(flows::first_prompt_text(&run.graph, node))
}

/// Moves the run from `from` along `output` to the step it leads to, if any, with
/// the run's step count at `steps`; returns the prompt to send when that is a
/// prompt step. The run instead waits at `from` for the person when going on
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
    let Some(next) = next else {
        // The output leads nowhere: the flow is done.
        set(tx, run.id, &from.id, Status::Done, steps, None)?;
        return Ok(None);
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
        Kind::End | Kind::Start => set(tx, run.id, &next.id, Status::Done, steps, None).map(|()| None),
    }
}

/// Starts flow `flow_id` in tab `tab_id`: with the first prompt to send, if the
/// flow begins with a prompt step, or, if its Start clears, at Start until Claude
/// has cleared (see the module docs).
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
    let run = Stored { id: run_id, graph, node_id: start.id.clone(), status: Status::Running, steps: 0, resume: None, clear: None, briefed: false, prefill: false };
    let prompt = go(&tx, &run, &start, flows::START_OUTPUT, 0, By::Flow)?;
    tx.commit().map_err(sql)?;
    Ok(Started { run_id, clear, prompt })
}

/// How run `run_id`, which clears at its start, is getting on; once Claude has
/// cleared, takes its first step (once: later calls are `Gone`).
pub fn cleared(conn: &mut Connection, run_id: i64) -> Result<Cleared, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "id = ?1", &run_id)? else { return Ok(Cleared::Gone) };
    match run.clear {
        None => Ok(Cleared::Gone),
        Some(Clear::Pending) => Ok(Cleared::Pending),
        Some(Clear::Done) => {
            let start = run.node()?.clone();
            let prompt = go(&tx, &run, &start, flows::START_OUTPUT, 0, By::Flow)?;
            tx.commit().map_err(sql)?;
            Ok(Cleared::Ready { prompt })
        }
    }
}

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
        let why = "Claude Code did not report Claude's reply to Mischief of Agents; update Claude Code to run flows.";
        let prompt = wait_for_person(&tx, run.id, &node.id, run.steps, why, None)?;
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
pub fn on_prompt(conn: &mut Connection, tab_id: &TabId, prompt: &str) -> Result<(), String> {
    if prompt.trim_start().starts_with('/') {
        return Ok(());
    }
    // Every prompt in every tab comes here: look before taking the write lock.
    let waiting = conn
        .query_row(
            "SELECT 1 FROM flow_runs WHERE tab_id = ?1 AND status IN ('manual', 'attention')",
            [tab_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql)?;
    if waiting.is_none() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let Some(run) = active_run(&tx, "tab_id = ?1", &tab_id.as_str())? else { return Ok(()) };
    let node = run.node()?;
    if !awaits_prompt(&run, node) {
        return Ok(());
    }
    set(&tx, run.id, &node.id, Status::Running, run.steps, None)?;
    let info = flows::base_info(&run.graph);
    if !run.briefed && (info.is_empty() || prompt.contains(&info)) {
        tx.execute("UPDATE flow_runs SET briefed = 1 WHERE id = ?1", [run.id]).map_err(sql)?;
    }
    tx.commit().map_err(sql)
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
/// can take its first step (see `cleared`); a running step will get no reply, so
/// its run waits for the person, saying `why` (see `pause_tab`). Either way, the
/// run's next prompt carries Start's base info again.
pub fn clear_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    tx.execute(&format!("UPDATE flow_runs SET briefed = 0 WHERE tab_id = ?1 AND status IN {ACTIVE}"), [tab_id.as_str()])
        .map_err(sql)?;
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
        "UPDATE flow_runs SET status = 'attention', note = ?2, resume = 'prompt', updated_at = ?3 WHERE tab_id = ?1 AND status = 'running' AND clear IS NULL",
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
            // The app that would have taken the first step after the clear is gone.
            tx.execute(
                "UPDATE flow_runs SET status = 'attention', note = ?2, resume = NULL, clear = NULL, updated_at = ?3
                 WHERE tab_id = ?1 AND status = 'running' AND clear IS NOT NULL",
                params![tab, "Mischief of Agents was restarted before the conversation was cleared. Pick an output to go on without clearing, or stop the run and press ▶ again.", now()],
            )
            .map_err(sql)?;
            answer_lost(&tx, &tab, "Mischief of Agents was restarted while Claude answered your prompt.")?;
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

pub fn stop(conn: &Connection, run_id: i64) -> Result<(), String> {
    let changed = conn
        .execute(
            &format!("UPDATE flow_runs SET status = 'stopped', updated_at = ?2 WHERE id = ?1 AND status IN {ACTIVE}"),
            params![run_id, now()],
        )
        .map_err(sql)?;
    if changed == 1 { Ok(()) } else { Err(format!("there is no active run {run_id}")) }
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
    let mut stmt = conn
        .prepare(&format!(
            "SELECT r.id, r.flow_id, f.name, r.tab_id, r.graph, r.node_id, r.status, r.note, r.steps, r.resume, r.clear, r.prefill
             FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.status IN {ACTIVE} ORDER BY r.id"
        ))
        .map_err(sql)?;
    type Row = (i64, i64, String, String, String, String, String, Option<String>, i64, Option<String>, Option<String>, bool);
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
            ))
        })
        .map_err(sql)?
        .collect::<Result<Vec<Row>, _>>()
        .map_err(sql)?;
    rows.into_iter()
        .map(|(id, flow_id, flow_name, tab_id, graph, step_id, status, note, steps, resume, clear, prefill)| {
            // A run that cannot be read is still listed, saying why, so it can be stopped.
            let node = parse_graph(&graph).and_then(|graph| {
                let node = graph.nodes.into_iter().find(|n| n.id == step_id);
                node.ok_or_else(|| format!("the run is at a missing step {step_id:?}"))
            });
            let status = Status::parse(&status)?;
            let states = Resume::parse(resume).and_then(|resume| Ok((resume, Clear::parse(clear)?)));
            let (step, outputs, note, resumable, step_kind) = match node.and_then(|node| states.map(|states| (node, states))) {
                Ok((node, (resume, clear))) => {
                    // The same test as `resume`'s.
                    let resumable = status == Status::Attention && node.kind == Kind::Prompt && resume.is_some();
                    if clear.is_some() {
                        // Nothing to pick until the first step is taken (see `proceed`).
                        let note = Some("Clearing the conversation before the first step.".to_owned());
                        (node.name().to_owned(), Vec::new(), note, false, Some(node.kind))
                    } else {
                        (node.name().to_owned(), node.outputs, note, resumable, Some(node.kind))
                    }
                }
                Err(e) => (step_id.clone(), Vec::new(), Some(format!("This run cannot be read: {e}. Stop it.")), false, None),
            };
            Ok(Run { id, flow_id, flow_name, tab_id, step, outputs, step_id, status, note, steps, resumable, prefill, step_kind })
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

    fn prompt_of(graph: &Graph, id: &str) -> Option<String> {
        Some(flows::prompt_text(graph.nodes.iter().find(|n| n.id == id).unwrap()))
    }

    #[test]
    fn a_run_starts_with_the_first_prompt() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-start", &g);
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
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
    fn start_s_base_info_goes_with_the_first_prompt_only() {
        let mut g = runnable();
        g.nodes[0].text = "The app is in src/.".into();
        let (_db, mut conn, flow) = setup("runs-base-info", &g);
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(started.prompt, Some(flows::first_prompt_text(&g, &g.nodes[1])));
        assert!(started.prompt.unwrap().starts_with("The app is in src/.\n\ndo p"));
        // Looping back to the step, Claude already has it.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), prompt_of(&g, "p"));
        // After a clear, Claude has lost it with the step.
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(go_on(&mut conn, started.run_id).unwrap(), flows::first_prompt_text(&g, &g.nodes[1]));
    }

    #[test]
    fn start_s_base_info_waits_for_the_first_prompt_after_manual_steps() {
        let mut g = graph(
            vec![node("m", Kind::Pause, &["ok"]), node("m2", Kind::Pause, &["ok"]), node("p", Kind::Prompt, &["done"])],
            vec![edge("start", "next", "m"), edge("m", "ok", "m2"), edge("m2", "ok", "p")],
        );
        g.nodes[0].text = "The app is in src/.".into();
        let (_db, mut conn, flow) = setup("runs-base-info-manual", &g);
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(started.prompt, None);
        assert_eq!(pick(&mut conn, started.run_id, "ok").unwrap(), None);
        assert_eq!(pick(&mut conn, started.run_id, "ok").unwrap(), Some(flows::first_prompt_text(&g, &g.nodes[3])));
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
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        let prompt = pick(&mut conn, started.run_id, "fail").unwrap().unwrap();
        assert!(prompt.starts_with("The app is in src/.\n\ndo p"), "{prompt}");
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
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), Some(flows::first_prompt_text(&g, &g.nodes[2])));
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        // Back at the manual step, Claude still has it.
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), prompt_of(&g, "p"));
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual, "a clear does not hold up a manual step");
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), Some(flows::first_prompt_text(&g, &g.nodes[2])));
    }

    #[test]
    fn continue_after_a_failed_hook_does_not_send_base_info_again() {
        // The conversation still has it.
        let g = briefing(runnable(), "The app is in src/.");
        let (_db, mut conn, flow) = setup("runs-base-info-hook", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        pause_tab(&mut conn, &tab("t1"), "The Stop hook failed.").unwrap();
        assert_eq!(Some(go_on(&mut conn, run).unwrap()), prompt_of(&g, "p"));
    }

    #[test]
    fn a_run_that_clears_sends_base_info_with_its_first_prompt() {
        let mut g = briefing(runnable(), "The app is in src/.");
        g.nodes[0].clear = true;
        let (_db, mut conn, flow) = setup("runs-base-info-clears", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        clear_tab(&mut conn, &tab("t1"), "cleared").unwrap();
        assert_eq!(cleared(&mut conn, run).unwrap(), Cleared::Ready { prompt: Some(flows::first_prompt_text(&g, &g.nodes[1])) });
    }

    #[test]
    fn blank_base_info_sends_the_prompt_as_it_is() {
        let g = briefing(runnable(), " \n ");
        let (_db, mut conn, flow) = setup("runs-base-info-blank", &g);
        assert_eq!(start(&mut conn, flow, &tab("t1")).unwrap().prompt, prompt_of(&g, "p"));
    }

    #[test]
    fn a_flow_that_is_not_ready_does_not_start() {
        let (_db, mut conn, flow) = setup("runs-not-ready", &Graph::default());
        let err = start(&mut conn, flow, &tab("t1")).unwrap_err();
        assert!(err.contains("not ready") && err.contains("Start is not connected"), "{err}");
        assert!(active(&conn).unwrap().is_empty());
        assert!(start(&mut conn, flow + 100, &tab("t1")).unwrap_err().contains("no flow"));
    }

    #[test]
    fn a_tab_runs_one_flow_at_a_time() {
        let (_db, mut conn, flow) = setup("runs-one", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        let err = start(&mut conn, flow, &tab("t1")).unwrap_err();
        assert!(err.contains("already running"), "{err}");
        start(&mut conn, flow, &tab("t2")).unwrap();
        let first = active(&conn).unwrap()[0].id;
        stop(&conn, first).unwrap();
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert!(stop(&conn, 999).unwrap_err().contains("no active run"));
    }

    #[test]
    fn claude_s_flow_line_picks_the_next_step() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-follow", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        // fail loops back to the same prompt.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Broken.\nFLOW: fail")).unwrap(), prompt_of(&g, "p"));
        assert_eq!(only_run(&conn).steps, 2);
        // pass leads to End, which finishes the run.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("All good.\nFLOW: pass")).unwrap(), None);
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
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_reply_that_cannot_be_followed_needs_attention_until_one_can() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-attention", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
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
        assert!(start(&mut conn, flow, &tab("t1")).unwrap_err().contains("more than one output"));
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_manual_step_waits_for_the_person() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-manual", &g);
        assert_eq!(start(&mut conn, flow, &tab("t1")).unwrap().prompt, None);
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
    const INFO_AND_DRAFT: &str = "The app is in src/.\n\nFix the bug in ";

    #[test]
    fn a_manual_prompt_fills_claude_s_input_and_goes_on_once_claude_answers_the_person() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt", &g);
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(started.prompt, None, "nothing is sent");
        let run = only_run(&conn);
        assert_eq!((run.status, run.step_id.as_str(), run.prefill), (Status::Manual, "mp", true));
        assert!(owed(&mut conn, run.id, "p").is_none(), "a fill for another step gets nothing");
        // Reading the text does not use it up: a paste that fails is tried again.
        assert_eq!(owed(&mut conn, run.id, "mp").as_deref(), Some(INFO_AND_DRAFT));
        assert_eq!(paste(&mut conn, run.id).as_deref(), Some(INFO_AND_DRAFT));
        assert!(!only_run(&conn).prefill);
        assert_eq!(owed(&mut conn, run.id, "mp"), None, "it is pasted once");
        // A turn the person did not start (a background task waking Claude) is not their prompt.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Task done.")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Manual);
        on_prompt(&mut conn, &tab("t1"), "The app is in src/.\n\nFix the bug in main.rs").unwrap();
        assert_eq!(only_run(&conn).status, Status::Running, "Claude is answering the person");
        // Claude's reply needs no FLOW line, and the person's prompt carried the base info.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Fixed it.")).unwrap(), prompt_of(&g, "p"));
        assert_eq!(only_run(&conn).step_id, "p");
    }

    #[test]
    fn base_info_the_person_did_not_send_goes_with_the_next_prompt() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-no-info", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        paste(&mut conn, run);
        // The person deleted the base info before sending.
        on_prompt(&mut conn, &tab("t1"), "Fix the bug in main.rs").unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Fixed it.")).unwrap(), Some(flows::first_prompt_text(&g, &g.nodes[2])));
    }

    #[test]
    fn a_manual_prompt_with_nothing_to_fill_waits_for_the_person_s_prompt() {
        let mut g = manual_prompt_first("");
        g.nodes[0].text.clear();
        let (_db, mut conn, flow) = setup("runs-manual-prompt-empty", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
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
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        // Claude stops; the app fills the input once it sees the run there.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        assert_eq!(paste(&mut conn, run).as_deref(), Some("Now ask about "), "Claude has the base info");
        on_prompt(&mut conn, &tab("t1"), "Now ask about it").unwrap();
        // The flow ends after the person's prompt is answered.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Answer.")).unwrap(), None);
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
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
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
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
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
        start(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), "hi").unwrap();
        on_prompt(&mut conn, &tab("t2"), "hi").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual);
    }

    #[test]
    fn a_paste_recorded_late_does_not_cancel_a_refill() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-late", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
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
        start(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), "  /model opus").unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual);
    }

    #[test]
    fn a_manual_prompt_whose_answer_was_lost_waits_for_the_prompt_again() {
        let g = manual_prompt_first("Fix the bug in ");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-lost", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
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
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Fixed.")).unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn a_restart_while_claude_answers_a_manual_prompt_waits_for_the_prompt_again() {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-restart", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
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
        start(&mut conn, flow, &tab("t1")).unwrap();
        prompt_unseen(&mut conn, &tab("t1"), "Your prompt could not be recorded.").unwrap();
        let waiting = only_run(&conn);
        assert_eq!((waiting.status, waiting.note.as_deref()), (Status::Attention, Some("Your prompt could not be recorded.")));
        // A run at any other step is left as it is.
        let (_db2, mut conn2, flow2) = setup("runs-pause-unseen", &manual_first());
        start(&mut conn2, flow2, &tab("t1")).unwrap();
        prompt_unseen(&mut conn2, &tab("t1"), "x").unwrap();
        assert_eq!(only_run(&conn2).status, Status::Manual);
    }

    #[test]
    fn a_run_says_what_kind_of_step_it_is_at() {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-step-kind", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(only_run(&conn).step_kind, Some(Kind::ManualPrompt));
    }

    #[test]
    fn the_person_can_skip_a_manual_prompt_and_base_info_still_goes() {
        let g = manual_prompt_first("draft");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-skip", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        // Pasted, but never sent: Claude still lacks the base info.
        paste(&mut conn, run);
        assert_eq!(pick(&mut conn, run, "sent").unwrap(), Some(flows::first_prompt_text(&g, &g.nodes[2])));
        assert!(!only_run(&conn).prefill);
    }

    #[test]
    fn a_run_stored_with_the_old_kind_name_goes_on_from_its_pause() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-old-manual", &g);
        let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
        let stored: String = conn.query_row("SELECT graph FROM flow_runs", [], |r| r.get(0)).unwrap();
        assert!(stored.contains(r#""kind":"manual""#), "older misags read it: {stored}");
        assert_eq!(pick(&mut conn, run, "ok").unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn the_person_can_move_a_running_run_on_by_hand() {
        // Claude was interrupted, crashed or never ran the hook: no Stop will come.
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-take-over", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn);
        assert_eq!(pick(&mut conn, run.id, "fail").unwrap(), prompt_of(&g, "p"));
        assert_eq!(pick(&mut conn, run.id, "pass").unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_prompt_can_lead_to_a_manual_step() {
        let g = graph(
            vec![node("p", Kind::Prompt, &["done"]), node("m", Kind::Pause, &["ok"])],
            vec![edge("start", "next", "p"), edge("p", "done", "m")],
        );
        let (_db, mut conn, flow) = setup("runs-to-manual", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap(), None);
        assert_eq!((only_run(&conn).status, only_run(&conn).step_id), (Status::Manual, "m".to_string()));
    }

    #[test]
    fn a_claude_code_that_does_not_report_its_reply_is_named_as_the_cause() {
        let (_db, mut conn, flow) = setup("runs-no-reply", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), None).unwrap(), None);
        let run = only_run(&conn);
        assert_eq!(run.status, Status::Attention);
        assert!(run.note.unwrap().contains("update Claude Code"));
    }

    #[test]
    fn clearing_the_conversation_pauses_a_running_run() {
        let (_db, mut conn, flow) = setup("runs-clear", &manual_first());
        start(&mut conn, flow, &tab("t2")).unwrap();
        let (_db2, mut conn2, flow2) = setup("runs-clear-2", &runnable());
        start(&mut conn2, flow2, &tab("t1")).unwrap();
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
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        // The run holds the tab before anything is typed, so a second start is refused.
        assert_eq!((started.clear, started.prompt), (true, None));
        assert!(start(&mut conn, flow, &tab("t1")).unwrap_err().contains("already running"));
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
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Ready { prompt: prompt_of(&g, "p") });
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
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
        stop(&conn, started.run_id).unwrap();
        assert_eq!(cleared(&mut conn, started.run_id).unwrap(), Cleared::Gone);
    }

    #[test]
    fn a_restart_while_clearing_waits_for_the_person() {
        let (_db, mut conn, flow) = setup("runs-clear-restart", &clearing());
        let started = start(&mut conn, flow, &tab("t1")).unwrap();
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
        start(&mut conn, flow, &tab("open")).unwrap();
        start(&mut conn, manual, &tab("waiting")).unwrap();
        start(&mut conn, flow, &tab("closed")).unwrap();
        after_restart(&mut conn, &[tab("open"), tab("waiting")]).unwrap();
        let runs = active(&conn).unwrap();
        let summary: Vec<_> = runs.iter().map(|r| (r.tab_id.as_str(), r.status)).collect();
        assert_eq!(summary, [("open", Status::Attention), ("waiting", Status::Manual)]);
        assert!(runs[0].note.as_deref().unwrap().contains("restarted"));
    }

    #[test]
    fn closing_a_tab_stops_its_run() {
        let (_db, mut conn, flow) = setup("runs-close", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        stop_tab(&conn, &tab("t1")).unwrap();
        assert!(active(&conn).unwrap().is_empty());
        stop_tab(&conn, &tab("t1")).unwrap();
    }

    #[test]
    fn a_run_whose_steps_cannot_be_read_is_still_listed() {
        let (_db, mut conn, flow) = setup("runs-bad-row", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
        start(&mut conn, flow, &tab("t2")).unwrap();
        conn.execute("UPDATE flow_runs SET graph = 'broken' WHERE tab_id = 't1'", []).unwrap();
        let runs = active(&conn).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].note.as_deref().unwrap().contains("cannot be read"));
        assert!(runs[0].outputs.is_empty());
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
        assert!(MAX_RUN_STEPS < i64::from(crate::claude::STOP_CAP));
    }

    #[test]
    fn the_person_can_pick_the_output_of_a_step_that_needs_attention() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-pick", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        on_stop(&mut conn, &tab("t1"), Some("Hmm.")).unwrap();
        let run = only_run(&conn);
        assert_eq!(pick(&mut conn, run.id, "fail").unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn a_step_that_needs_attention_can_be_resumed_by_asking_claude_to_go_on() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-resume", &g);
        let t = tab("t1");
        start(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, Some("Which database?\nFLOW: attention")).unwrap();
        let run = only_run(&conn);
        assert!(run.resumable);
        let prompt = go_on(&mut conn, run.id).unwrap();
        assert!(prompt.contains("\"P\"") && prompt.contains("`FLOW: pass` or `FLOW: fail`"), "{prompt}");
        let resumed = only_run(&conn);
        assert!(!resumed.resumable);
        assert_eq!((resumed.status, resumed.note, resumed.step_id.as_str(), resumed.steps), (Status::Running, None, "p", run.steps));
        // Claude's reply moves it on as usual.
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: pass")).unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
        // Only a prompt step that needs attention is resumed.
        let (_db, mut conn, flow) = setup("runs-resume-manual", &manual_first());
        start(&mut conn, flow, &t).unwrap();
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
            start(&mut conn, flow, &t).unwrap();
            on_stop(&mut conn, &t, Some(reply)).unwrap();
            let run = only_run(&conn);
            assert!(run.resumable, "{reply:?}");
            assert!(go_on(&mut conn, run.id).unwrap().starts_with("Go on with the flow step"));
        }
        // The conversation was cleared: Claude gets the step's prompt again.
        let (_db, mut conn, flow) = setup("runs-resume-cleared", &g);
        start(&mut conn, flow, &t).unwrap();
        pause_tab(&mut conn, &t, "The conversation was cleared.").unwrap();
        let run = only_run(&conn);
        assert!(run.resumable);
        assert_eq!(Some(go_on(&mut conn, run.id).unwrap()), prompt_of(&g, "p"));
        // Claude Code that does not report replies would only wait again.
        let (_db, mut conn, flow) = setup("runs-resume-unreported", &g);
        start(&mut conn, flow, &t).unwrap();
        on_stop(&mut conn, &t, None).unwrap();
        assert!(!only_run(&conn).resumable);
        // A step limit: going on would only redo the step and wait again.
        let (_db, mut conn, flow) = setup("runs-resume-limit", &g);
        start(&mut conn, flow, &t).unwrap();
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
        start(&mut conn, flow, &t).unwrap();
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
        start(&mut conn, flow, &t).unwrap();
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
        start(&mut conn, flow, &t).unwrap();
        after_restart(&mut conn, std::slice::from_ref(&t)).unwrap();
        let run = only_run(&conn);
        assert!(run.resumable);
        assert!(go_on(&mut conn, run.id).unwrap().starts_with("Go on with the flow step"));
    }

    #[test]
    fn a_run_that_keeps_going_waits_after_the_step_limit() {
        let (_db, mut conn, flow) = setup("runs-limit", &runnable());
        start(&mut conn, flow, &tab("t1")).unwrap();
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
        assert_eq!(start(&mut conn, flow, &t).unwrap().prompt, prompt_of(&g, "review"));
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
        assert_eq!(on_stop(&mut conn, &t, Some("FLOW: clean")).unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_step_that_may_run_once_says_so() {
        let mut g = review_loop();
        g.nodes[1].max_runs = Some(1);
        let (_db, mut conn, flow) = setup("runs-max-once", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
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
        start(&mut conn, flow, &tab("t1")).unwrap();
        assert!(on_stop(&mut conn, &tab("t1"), Some("FLOW: again")).unwrap().is_some());
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: again")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Attention);
    }

    #[test]
    fn a_run_follows_the_flow_as_it_was_when_started() {
        let g = runnable();
        let (db, mut conn, flow) = setup("runs-snapshot", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        let mut edited = g.clone();
        edited.nodes[1].text = "something else".into();
        db.save_flow_graph(flow, &edited).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), prompt_of(&g, "p"));
    }

    #[test]
    fn other_tabs_and_finished_runs_are_left_alone() {
        let (_db, mut conn, flow) = setup("runs-other", &runnable());
        assert_eq!(on_stop(&mut conn, &tab("t9"), Some("FLOW: pass")).unwrap(), None);
        start(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn).id;
        stop(&conn, run).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), None);
        assert!(pick(&mut conn, run, "fail").unwrap_err().contains("no active run"));
    }

    #[test]
    fn a_flow_that_starts_at_its_end_finishes_at_once() {
        let g = graph(vec![node("e", Kind::End, &[])], vec![edge("start", "next", "e")]);
        let (_db, mut conn, flow) = setup("runs-empty", &g);
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
        assert!(start(&mut conn, flow.id, &tab("t")).unwrap_err().contains("confirm"));
        db.confirm_flow(flow.id).unwrap();
        assert!(!db.list_flows().unwrap()[0].unconfirmed);
        assert!(start(&mut conn, flow.id, &tab("t")).is_ok());
    }

    #[test]
    fn a_pick_or_continue_for_a_step_the_run_has_left_is_refused() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-stale-pick", &g);
        let run = start(&mut conn, flow, &tab("stale")).unwrap();
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
        let run = start(&mut conn, flow, &tab("odd")).unwrap();
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
        let pending = start(&mut conn, flow, &tab("pending")).unwrap();
        let cleared = start(&mut conn, flow, &tab("cleared")).unwrap();
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
