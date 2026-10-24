//! Flow runs: a flow being worked through in a tab, one at a time per tab.
//!
//! A run starts at the flow's Start step and moves along outputs. A prompt step
//! sends its text to Claude; when Claude ends its turn, the Stop hook
//! (`raterm --claude-hook`, a separate process) reads the `FLOW: <output>` line
//! Claude ended with and moves on, handing Claude the next prompt as hook context.
//! A manual step, or a reply the runner cannot follow, waits for the person, who
//! picks an output in the sidebar (and can do so for a running step too, if Claude
//! was interrupted). An output with no next step ends the run.
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

/// Its active statuses are mirrored by `RunStatus` in src/flows.ts.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Claude is working on a prompt step.
    Running,
    /// At a manual step, waiting for the person to pick an output.
    Manual,
    /// The run needs a person (its note says why): Claude's reply could not be
    /// followed, it asked for one, or the run hit `MAX_RUN_STEPS`. The person picks
    /// an output, or Claude's next reply with a valid `FLOW:` line moves the run on.
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
            other => return Err(format!("a run has the status {other:?}, which this raterm does not know")),
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
}

impl Stored {
    fn node(&self) -> Result<&Node, String> {
        let id = &self.node_id;
        self.graph.nodes.iter().find(|n| &n.id == id).ok_or_else(|| format!("the run is at a missing step {id:?}"))
    }
}

/// The active run matching `filter` (`tab_id = ?1` or `id = ?1`).
fn active_run(tx: &Transaction, filter: &str, key: &dyn rusqlite::ToSql) -> Result<Option<Stored>, String> {
    let row = tx
        .query_row(
            &format!("SELECT id, graph, node_id, status, steps FROM flow_runs WHERE {filter} AND status IN {ACTIVE}"),
            [key],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(sql)?;
    row.map(|(id, graph, node_id, status, steps)| {
        Ok(Stored { id, graph: parse_graph(&graph)?, node_id, status: Status::parse(&status)?, steps })
    })
    .transpose()
}

fn set(tx: &Transaction, run: i64, node_id: &str, status: Status, steps: i64, note: Option<&str>) -> Result<(), String> {
    tx.execute(
        "UPDATE flow_runs SET node_id = ?2, status = ?3, steps = ?4, note = ?5, updated_at = ?6 WHERE id = ?1",
        params![run, node_id, status.as_str(), steps, note, now()],
    )
    .map(drop)
    .map_err(sql)
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

/// Moves the run from `from` along `output` to the step it leads to, if any, with
/// the run's step count at `steps`; returns the prompt to send when that is a
/// prompt step. At `MAX_RUN_STEPS` the run instead waits at `from` for the person.
fn go(tx: &Transaction, run: &Stored, from: &Node, output: &str, steps: i64, note: Option<&str>) -> Result<Option<String>, String> {
    let next = flows::next_step(&run.graph, &from.id, output);
    if next.is_some_and(|n| n.kind == Kind::Prompt) && steps >= MAX_RUN_STEPS {
        let why = format!("The run has taken {MAX_RUN_STEPS} steps without you; pick an output to go on.");
        return set(tx, run.id, &from.id, Status::Attention, steps, Some(&why)).map(|()| None);
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
            Ok(Some(flows::prompt_text(next, note)))
        }
        Kind::Manual => set(tx, run.id, &next.id, Status::Manual, steps, None).map(|()| None),
        Kind::End | Kind::Start => set(tx, run.id, &next.id, Status::Done, steps, None).map(|()| None),
    }
}

/// Starts flow `flow_id` in tab `tab_id`; returns the first prompt to send, if
/// the flow begins with a prompt step.
pub fn start(conn: &mut Connection, flow_id: i64, tab_id: &TabId) -> Result<Option<String>, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let graph: String = tx
        .query_row("SELECT graph FROM flows WHERE id = ?1", [flow_id], |r| r.get(0))
        .optional()
        .map_err(sql)?
        .ok_or_else(|| format!("there is no flow {flow_id}"))?;
    let graph = parse_graph(&graph)?;
    let problems = flows::problems(&graph);
    if !problems.is_empty() {
        return Err(format!("the flow is not ready to run: {}", problems.join(" ")));
    }
    let start = graph.nodes.iter().find(|n| n.kind == Kind::Start).ok_or("the flow has no Start step")?.clone();
    let now = now();
    let inserted = tx.execute(
        "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, steps, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'running', 0, ?5, ?5)",
        params![flow_id, tab_id.as_str(), serde_json::to_string(&graph).map_err(|e| e.to_string())?, start.id, now],
    );
    match inserted {
        // The flow exists and the values are checked, so the constraint is the one-run index.
        Err(e) if e.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) => {
            return Err("this tab is already running a flow; stop that run first".into());
        }
        other => other.map_err(sql)?,
    };
    let run = Stored { id: tx.last_insert_rowid(), graph, node_id: start.id.clone(), status: Status::Running, steps: 0 };
    let prompt = go(&tx, &run, &start, flows::START_OUTPUT, 0, None)?;
    tx.commit().map_err(sql)?;
    Ok(prompt)
}

/// Claude in tab `tab_id` ended its turn with `reply` (None when Claude Code did
/// not report it): moves the tab's run on. Returns the next prompt to send, if
/// any. A tab whose run is not working on a prompt (none, or one at a manual step)
/// is left alone.
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
    if run.status == Status::Manual {
        return Ok(None);
    }
    let node = run.node()?.clone();
    let wait = |why: String| set(&tx, run.id, &node.id, Status::Attention, run.steps, Some(&why)).map(|()| None);
    let Some(reply) = reply else {
        let prompt = wait("Claude Code did not report Claude's reply to raterm; update Claude Code to run flows.".into())?;
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
        Some(m) => go(&tx, &run, &node, &m, run.steps, None),
    }?;
    tx.commit().map_err(sql)?;
    Ok(prompt)
}

/// The person picked `output` for the run's current step, with an optional note
/// for Claude; returns the next prompt to send, if any. Usually the run waits
/// (manual or needs attention), but a running step can be moved on too, for when
/// Claude was interrupted and no reply will come. The person stepping in restarts
/// the count toward `MAX_RUN_STEPS`.
pub fn proceed(conn: &mut Connection, run_id: i64, output: &str, note: &str) -> Result<Option<String>, String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let run = active_run(&tx, "id = ?1", &run_id)?.ok_or_else(|| format!("there is no active run {run_id}"))?;
    let node = run.node()?.clone();
    if !node.outputs.iter().any(|o| o == output) {
        return Err(format!("\"{}\" has no output {output:?}", node.name()));
    }
    let prompt = go(&tx, &run, &node, output, 0, Some(note))?;
    tx.commit().map_err(sql)?;
    Ok(prompt)
}

/// Makes the tab's running run wait for the person, saying why: Claude will not
/// reply to its step (the conversation was cleared, or the hook failed).
pub fn pause_tab(conn: &mut Connection, tab_id: &TabId, why: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE flow_runs SET status = 'attention', note = ?2, updated_at = ?3 WHERE tab_id = ?1 AND status = 'running'",
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
/// ones wait, as their Claude was stopped with raterm and will not reply.
pub fn after_restart(conn: &mut Connection, open_tabs: &[TabId]) -> Result<(), String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let tabs: Vec<String> = {
        let mut stmt = tx.prepare(&format!("SELECT DISTINCT tab_id FROM flow_runs WHERE status IN {ACTIVE}")).map_err(sql)?;
        let tabs = stmt.query_map([], |r| r.get(0)).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?;
        tabs
    };
    for tab in tabs {
        if open_tabs.iter().any(|open| open.as_str() == tab) {
            tx.execute(
                "UPDATE flow_runs SET status = 'attention', note = ?2, updated_at = ?3 WHERE tab_id = ?1 AND status = 'running'",
                params![tab, "raterm was restarted while Claude worked on this step, so its reply was not seen. Pick an output to go on.", now()],
            )
            .map_err(sql)?;
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

/// Every active run, oldest first.
pub fn active(conn: &Connection) -> Result<Vec<Run>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT r.id, r.flow_id, f.name, r.tab_id, r.graph, r.node_id, r.status, r.note, r.steps
             FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.status IN {ACTIVE} ORDER BY r.id"
        ))
        .map_err(sql)?;
    type Row = (i64, i64, String, String, String, String, String, Option<String>, i64);
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))
        .map_err(sql)?
        .collect::<Result<Vec<Row>, _>>()
        .map_err(sql)?;
    rows.into_iter()
        .map(|(id, flow_id, flow_name, tab_id, graph, step_id, status, note, steps)| {
            // A run that cannot be read is still listed, saying why, so it can be stopped.
            let node = parse_graph(&graph).and_then(|graph| {
                let node = graph.nodes.into_iter().find(|n| n.id == step_id);
                node.ok_or_else(|| format!("the run is at a missing step {step_id:?}"))
            });
            let (step, outputs, note) = match node {
                Ok(node) => (node.name().to_owned(), node.outputs, note),
                Err(e) => (step_id.clone(), Vec::new(), Some(format!("This run cannot be read: {e}. Stop it."))),
            };
            Ok(Run { id, flow_id, flow_name, tab_id, step, outputs, step_id, status: Status::parse(&status)?, note, steps })
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

    /// Start → a manual step that is ok (→ a prompt) or redone.
    fn manual_first() -> Graph {
        graph(
            vec![node("m", Kind::Manual, &["ok", "redo"]), node("p", Kind::Prompt, &["done"])],
            vec![edge("start", "next", "m"), edge("m", "ok", "p"), edge("m", "redo", "m")],
        )
    }

    fn only_run(conn: &Connection) -> Run {
        let runs = active(conn).unwrap();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }

    fn prompt_of(graph: &Graph, id: &str, note: Option<&str>) -> Option<String> {
        Some(flows::prompt_text(graph.nodes.iter().find(|n| n.id == id).unwrap(), note))
    }

    #[test]
    fn a_run_starts_with_the_first_prompt() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-start", &g);
        assert_eq!(start(&mut conn, flow, &tab("t1")).unwrap(), prompt_of(&g, "p", None));
        let run = only_run(&conn);
        assert_eq!(
            (run.flow_name.as_str(), run.tab_id.as_str(), run.step.as_str(), run.status, run.steps),
            ("F", "t1", "P", Status::Running, 1)
        );
        assert_eq!(run.outputs, ["pass", "fail"]);
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
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Broken.\nFLOW: fail")).unwrap(), prompt_of(&g, "p", None));
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
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("Used SQLite.\nFLOW: fail")).unwrap(), prompt_of(&g, "p", None));
        let run = only_run(&conn);
        assert_eq!((run.status, run.note), (Status::Running, None));
    }

    #[test]
    fn a_manual_step_waits_for_the_person() {
        let g = manual_first();
        let (_db, mut conn, flow) = setup("runs-manual", &g);
        assert_eq!(start(&mut conn, flow, &tab("t1")).unwrap(), None);
        let run = only_run(&conn);
        assert_eq!((run.status, run.outputs.clone()), (Status::Manual, vec!["ok".to_string(), "redo".to_string()]));
        // Claude ending a turn (the person chatting) does not move a manual step.
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: ok")).unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Manual);
        assert!(proceed(&mut conn, run.id, "maybe", "").unwrap_err().contains("maybe"));
        assert_eq!(proceed(&mut conn, run.id, "redo", "").unwrap(), None);
        assert_eq!(only_run(&conn).status, Status::Manual);
        assert_eq!(proceed(&mut conn, run.id, "ok", "Looks good").unwrap(), prompt_of(&g, "p", Some("Looks good")));
        assert_eq!(only_run(&conn).status, Status::Running);
    }

    #[test]
    fn the_person_can_move_a_running_run_on_by_hand() {
        // Claude was interrupted, crashed or never ran the hook: no Stop will come.
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-take-over", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn);
        assert_eq!(proceed(&mut conn, run.id, "fail", "").unwrap(), prompt_of(&g, "p", None));
        assert_eq!(proceed(&mut conn, run.id, "pass", "").unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_prompt_can_lead_to_a_manual_step() {
        let g = graph(
            vec![node("p", Kind::Prompt, &["done"]), node("m", Kind::Manual, &["ok"])],
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
    fn raterm_s_step_limit_stays_below_claude_code_s() {
        assert!(MAX_RUN_STEPS < i64::from(crate::claude::STOP_CAP));
    }

    #[test]
    fn the_person_can_pick_the_output_of_a_step_that_needs_attention() {
        let g = runnable();
        let (_db, mut conn, flow) = setup("runs-pick", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        on_stop(&mut conn, &tab("t1"), Some("Hmm.")).unwrap();
        let run = only_run(&conn);
        assert_eq!(proceed(&mut conn, run.id, "fail", "Try the other fix").unwrap(), prompt_of(&g, "p", Some("Try the other fix")));
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
        assert!(proceed(&mut conn, run.id, "fail", "").unwrap().is_some());
        assert_eq!(only_run(&conn).steps, 1);
    }

    #[test]
    fn a_run_follows_the_flow_as_it_was_when_started() {
        let g = runnable();
        let (db, mut conn, flow) = setup("runs-snapshot", &g);
        start(&mut conn, flow, &tab("t1")).unwrap();
        let mut edited = g.clone();
        edited.nodes[1].text = "something else".into();
        db.save_flow_graph(flow, &edited).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), prompt_of(&g, "p", None));
    }

    #[test]
    fn other_tabs_and_finished_runs_are_left_alone() {
        let (_db, mut conn, flow) = setup("runs-other", &runnable());
        assert_eq!(on_stop(&mut conn, &tab("t9"), Some("FLOW: pass")).unwrap(), None);
        start(&mut conn, flow, &tab("t1")).unwrap();
        let run = only_run(&conn).id;
        stop(&conn, run).unwrap();
        assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: fail")).unwrap(), None);
        assert!(proceed(&mut conn, run, "fail", "").unwrap_err().contains("no active run"));
    }

    #[test]
    fn a_flow_that_starts_at_its_end_finishes_at_once() {
        let g = graph(vec![node("e", Kind::End, &[])], vec![edge("start", "next", "e")]);
        let (_db, mut conn, flow) = setup("runs-empty", &g);
        assert_eq!(start(&mut conn, flow, &tab("t1")).unwrap(), None);
        assert!(active(&conn).unwrap().is_empty());
    }
}
