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
fn a_slash_prompt_that_reaches_claude_carries_a_finished_flow_s_note() {
    let (_db, mut conn, flow) = setup("runs-stop-closing-slash", &runnable());
    let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
    stop(&mut conn, run).unwrap();
    assert_eq!(on_prompt(&mut conn, &tab("t1"), "/review the diff").unwrap(), finished());
}

#[test]
fn a_stopped_briefed_run_tells_claude_with_the_person_s_next_prompt() {
    let (_db, mut conn, flow) = setup("runs-stop-closing", &runnable());
    let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
    stop(&mut conn, run).unwrap();
    assert!(stop(&mut conn, run).unwrap_err().contains("no active run"));
    // A command Claude Code runs itself is not a message to Claude, so it does not carry it.
    assert_eq!(on_prompt(&mut conn, &tab("t1"), "/model").unwrap(), None);
    assert_eq!(on_prompt(&mut conn, &tab("t2"), "Hi").unwrap(), None, "another tab");
    assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), finished());
    assert_eq!(on_prompt(&mut conn, &tab("t1"), "And now?").unwrap(), None, "once");
    // Claude got the briefing even if it has yet to reply.
    let (_db, mut conn, flow) = setup("runs-stop-briefing", &runnable());
    let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
    stop(&mut conn, run).unwrap();
    assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), finished());
    // A run still clearing never briefed Claude.
    let mut g = runnable();
    g.nodes[0].clear = true;
    let (_db, mut conn, flow) = setup("runs-stop-clearing", &g);
    let run = start(&mut conn, flow, &tab("t1")).unwrap().run_id;
    stop(&mut conn, run).unwrap();
    assert_eq!(on_prompt(&mut conn, &tab("t1"), "Thanks").unwrap(), None);
}

#[test]
fn a_new_run_or_a_clear_drops_the_finished_flow_s_note() {
    let (_db, mut conn, flow) = setup("runs-closing-dropped", &runnable());
    let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
    stop(&mut conn, run).unwrap();
    // The new run's briefing supersedes it.
    let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
    stop(&mut conn, run).unwrap();
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
fn a_flow_saved_by_a_newer_version_does_not_start() {
    let (_db, mut conn, flow) = setup("runs-newer-graph", &runnable());
    // A newer misag stored it (the app's own save would refuse it).
    let mut newer = serde_json::to_value(runnable()).unwrap();
    newer["version"] = (flows::GRAPH_VERSION + 1).into();
    conn.execute("UPDATE flows SET graph = ?1 WHERE id = ?2", params![newer.to_string(), flow]).unwrap();
    let err = start(&mut conn, flow, &tab("t1")).unwrap_err();
    assert!(err.contains("newer"), "{err}");
    assert!(active(&conn).unwrap().is_empty());
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
    stop(&mut conn, first).unwrap();
    begin(&mut conn, flow, &tab("t1")).unwrap();
    assert!(stop(&mut conn, 999).unwrap_err().contains("no active run"));
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
fn a_paste_owed_with_no_token_is_pasted_once() {
    // As a database edited by hand, or an older version, may leave it.
    let g = manual_prompt_first("Fix the bug in ");
    let (_db, mut conn, flow) = setup("runs-manual-prompt-no-token", &g);
    let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
    conn.execute("UPDATE flow_runs SET prefill_token = NULL WHERE id = ?1", [run]).unwrap();
    assert_eq!(paste(&mut conn, run).as_deref(), Some(DRAFT));
    assert_eq!(owed(&mut conn, run, "mp"), None, "it is pasted once");
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
    stop(&mut conn, run).unwrap();
    // Even one whose state this version cannot read (as a newer version might store it).
    conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
    conn.execute("UPDATE flow_runs SET resume = 'someday', prefill = 1 WHERE id = ?1", [run]).unwrap();
    let row = |conn: &Connection| (column::<bool>(conn, run, "prefill"), column::<i64>(conn, run, "updated_at"));
    let before = row(&conn);
    prefilled(&mut conn, run, "mp", token).unwrap();
    assert_eq!(row(&conn), before);
}

#[test]
fn a_local_command_is_not_the_person_s_prompt() {
    let g = manual_prompt_first("");
    let (_db, mut conn, flow) = setup("runs-manual-prompt-slash", &g);
    begin(&mut conn, flow, &tab("t1")).unwrap();
    on_prompt(&mut conn, &tab("t1"), "  /model opus").unwrap();
    assert_eq!(only_run(&conn).status, Status::Manual);
}

#[test]
fn a_prompt_that_starts_with_a_slash_but_reaches_claude_runs_the_manual_prompt() {
    // A command Claude Code sends Claude as a prompt (a skill, a custom command,
    // or its own /review), and a path, are the person's prompt.
    for prompt in ["/review", "/my-skill fix the bug", "/home/me/app/main.rs fails to build", "/mcp__srv__ask hi"] {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-slash-prompt", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), prompt).unwrap();
        assert_eq!(only_run(&conn).status, Status::Running, "{prompt}");
    }
    // Commands Claude Code handles itself are not.
    for command in ["/clear", "/compact keep the plan", "/model", "/COST", "/exit"] {
        let g = manual_prompt_first("");
        let (_db, mut conn, flow) = setup("runs-manual-prompt-slash-local", &g);
        begin(&mut conn, flow, &tab("t1")).unwrap();
        on_prompt(&mut conn, &tab("t1"), command).unwrap();
        assert_eq!(only_run(&conn).status, Status::Manual, "{command}");
    }
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
    prompt_unseen(&mut conn, &tab("t1"), "bad").unwrap();
    let waiting = only_run(&conn);
    assert_eq!(waiting.status, Status::Attention);
    assert!(waiting.note.as_deref().unwrap().starts_with("misag could not record your prompt (bad)"), "{:?}", waiting.note);
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
fn a_late_reply_to_a_step_moved_on_by_hand_does_not_count_for_the_next() {
    let g = runnable();
    let (_db, mut conn, flow) = setup("runs-take-over-late", &g);
    begin(&mut conn, flow, &tab("t1")).unwrap();
    let run = only_run(&conn);
    // The person moves on while Claude is still answering "p"; the app types the
    // next prompt, which Claude Code holds until that turn ends.
    assert_eq!(pick(&mut conn, run.id, "fail").unwrap(), prompt_of(&g, "p"));
    assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: pass")).unwrap(), None, "the late reply");
    let still = only_run(&conn);
    assert_eq!((still.status, still.step_id.as_str(), still.steps), (Status::Running, "p", 1));
    // Claude Code reports the typed prompt; the turn after it is the step's reply.
    on_prompt(&mut conn, &tab("t1"), &prompt_of(&g, "p").unwrap()).unwrap();
    assert_eq!(on_stop(&mut conn, &tab("t1"), Some("FLOW: pass")).unwrap(), None);
    assert!(active(&conn).unwrap().is_empty(), "the reply ended the run");
    // A pick while Claude waits for the person sends a prompt with no turn in the way.
    let g = manual_first();
    let (_db, mut conn, flow) = setup("runs-take-over-idle", &g);
    let run = begin(&mut conn, flow, &tab("t1")).unwrap().run_id;
    pick(&mut conn, run, "ok").unwrap();
    on_stop(&mut conn, &tab("t1"), Some("FLOW: done")).unwrap();
    assert!(active(&conn).unwrap().is_empty());
}

#[test]
fn only_the_step_s_own_prompt_counts_for_a_run_moved_on_by_hand() {
    let g = runnable();
    let (_db, mut conn, flow) = setup("runs-typed-own-prompt", &g);
    let t = tab("t1");
    begin(&mut conn, flow, &t).unwrap();
    let run = only_run(&conn);
    let typed = pick(&mut conn, run.id, "fail").unwrap().unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: fail")).unwrap(), None, "the late reply");
    // A message the person queued before the pick goes first: its answer is not the step's reply.
    on_prompt(&mut conn, &t, "Also check the logs.").unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: pass")).unwrap(), None);
    assert_eq!(only_run(&conn).status, Status::Running);
    // The step's prompt, however Claude Code spaces it, is.
    on_prompt(&mut conn, &t, &format!("{}\n", typed.replace('\n', "\r\n"))).unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: pass")).unwrap(), None);
    assert!(active(&conn).unwrap().is_empty(), "the reply ended the run");
}

#[test]
fn a_second_pick_while_the_first_prompt_waits_counts_only_the_second_prompt() {
    let g = review_loop();
    let (_db, mut conn, flow) = setup("runs-typed-two-picks", &g);
    let t = tab("t1");
    begin(&mut conn, flow, &t).unwrap();
    let run = only_run(&conn).id;
    assert_eq!(pick(&mut conn, run, "issues").unwrap(), prompt_of(&g, "fix"));
    assert_eq!(pick(&mut conn, run, "done").unwrap(), prompt_of(&g, "review"));
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: clean")).unwrap(), None, "the late reply to the first review");
    // Claude Code sends the first pick's prompt; its answer is not the review's.
    on_prompt(&mut conn, &t, &prompt_of(&g, "fix").unwrap()).unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: done")).unwrap(), None);
    let still = only_run(&conn);
    assert_eq!((still.status, still.step_id.as_str()), (Status::Running, "review"));
    on_prompt(&mut conn, &t, &prompt_of(&g, "review").unwrap()).unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: clean")).unwrap(), None);
    assert!(active(&conn).unwrap().is_empty(), "the review's reply ended the run");
}

#[test]
fn a_typed_prompt_never_seen_sent_waits_for_the_person_and_continue_sends_it_again() {
    let g = runnable();
    let (_db, mut conn, flow) = setup("runs-typed-lost", &g);
    let t = tab("t1");
    begin(&mut conn, flow, &t).unwrap();
    let run = only_run(&conn).id;
    pick(&mut conn, run, "fail").unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: fail")).unwrap(), None, "the late reply");
    assert_eq!(only_run(&conn).status, Status::Running);
    // Another turn ends with no prompt of the step's sent: it was folded into the
    // turn before, dropped by Esc, or edited away.
    assert_eq!(on_stop(&mut conn, &t, Some("Done.")).unwrap(), None);
    let waiting = only_run(&conn);
    assert_eq!((waiting.status, waiting.resumable), (Status::Attention, true));
    assert!(waiting.note.as_deref().unwrap().contains("not seen being sent"), "{:?}", waiting.note);
    assert_eq!(go_on(&mut conn, run).unwrap(), prompt_of(&g, "p").unwrap());
}

#[test]
fn a_failed_stop_hook_while_a_typed_prompt_waits_keeps_waiting_for_it() {
    let g = runnable();
    let (_db, mut conn, flow) = setup("runs-typed-stop-failed", &g);
    let t = tab("t1");
    begin(&mut conn, flow, &t).unwrap();
    let run = only_run(&conn).id;
    let typed = pick(&mut conn, run, "fail").unwrap().unwrap();
    // That Stop was the late reply to the step before: Continue would type the prompt twice.
    hook_failed(&mut conn, &t, Hook::Stop, "boom".into());
    assert_eq!(only_run(&conn).status, Status::Running);
    on_prompt(&mut conn, &t, &typed).unwrap();
    assert_eq!(on_stop(&mut conn, &t, Some("FLOW: pass")).unwrap(), None);
    assert!(active(&conn).unwrap().is_empty(), "the reply ended the run");
}

#[test]
fn a_stopped_run_moved_on_by_hand_takes_no_lock_for_a_prompt() {
    let g = runnable();
    let path = db::test_util::temp_db("runs-typed-stopped");
    let db = Db::open(path.clone()).unwrap();
    let flow = db.create_flow("F").unwrap().id;
    db.save_flow_graph(flow, &g).unwrap();
    let mut conn = db::connect_existing(&path).unwrap();
    for (t, unreadable) in [("t1", false), ("t2", true)] {
        let t = tab(t);
        let run = begin(&mut conn, flow, &t).unwrap().run_id;
        pick(&mut conn, run, "fail").unwrap();
        assert_eq!(column::<i64>(&conn, run, "typed"), 1);
        if unreadable {
            // As a newer version might store it, so the stop is a raw UPDATE.
            conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
            conn.execute("UPDATE flow_runs SET resume = 'someday' WHERE id = ?1", [run]).unwrap();
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF").unwrap();
        }
        // A closed tab owes no finished flow's note, which a prompt would carry.
        stop_tab(&mut conn, &t).unwrap();
        assert_eq!(column::<i64>(&conn, run, "typed"), 0, "unreadable: {unreadable}");
        // The hook, say, holds the write lock.
        let writer = db::connect_existing(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let begun = std::time::Instant::now();
        assert_eq!(on_prompt(&mut conn, &t, &prompt_of(&g, "p").unwrap()).unwrap(), None);
        assert!(begun.elapsed() < std::time::Duration::from_secs(1), "{:?}", begun.elapsed());
        writer.execute_batch("ROLLBACK").unwrap();
    }
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
    stop(&mut conn, started.run_id).unwrap();
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
    stop_tab(&mut conn, &tab("t1")).unwrap();
    assert!(active(&conn).unwrap().is_empty());
    stop_tab(&mut conn, &tab("t1")).unwrap();
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
    stop(&mut conn, runs[0].id).unwrap();
    assert_eq!(active(&conn).unwrap().len(), 1);
}

#[test]
fn the_one_run_index_covers_the_active_statuses() {
    let (_db, conn, _) = setup("runs-index", &runnable());
    let index: String =
        conn.query_row("SELECT sql FROM sqlite_master WHERE name = 'one_active_run_per_tab'", [], |r| r.get(0)).unwrap();
    assert!(index.ends_with(&format!("WHERE status IN {ACTIVE}")), "{index}");
    assert_eq!(ACTIVE, sql_list(&Status::ACTIVE));
    // Every active status is one a run can be read in, and only those.
    for status in [Status::Running, Status::Manual, Status::Attention, Status::Done, Status::Stopped] {
        let active = !matches!(status, Status::Done | Status::Stopped);
        assert_eq!(Status::ACTIVE.contains(&status), active, "{status:?}");
    }
    assert!(Status::LISTENING.iter().chain(&Status::PROMPTED).all(|s| Status::ACTIVE.contains(s)));
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
    stop(&mut conn, id).unwrap();
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
    stop(&mut conn, run).unwrap();
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
    stop(&mut conn, run).unwrap();
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
    stop(&mut conn, run).unwrap();
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
        Phase::Typed { turn_ended: false },
        Phase::Typed { turn_ended: true },
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
        (Columns { clear: Some("done".into()), typed: 1, ..running.clone() }, "both a clear and a typed prompt"),
        (Columns { briefing: true, briefing_at: Some(1), typed: 2, ..running.clone() }, "both a briefing and a typed prompt"),
        (Columns { typed: 3, ..running.clone() }, "typed state 3"),
        (Columns { closing: true, ..running }, "running with a finished flow's note owed"),
        (Columns { note: Some("why".into()), ..manual.clone() }, "manual with a note"),
        (Columns { resume: Some("prompt".into()), ..manual.clone() }, "manual with a way to continue"),
        (Columns { typed: 1, ..manual.clone() }, "manual with a typed prompt"),
        (Columns { clear: Some("done".into()), ..manual }, "manual with a clear"),
        (Columns { note: None, ..attention.clone() }, "attention with no note"),
        (Columns { briefing: true, briefing_at: Some(1), ..attention.clone() }, "attention with a briefing"),
        (Columns { typed: 2, ..attention.clone() }, "attention with a typed prompt"),
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
        typed: 7,
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
        stop(&mut conn, run).unwrap();
        assert_eq!((column::<String>(&conn, run, "status"), column::<bool>(&conn, run, "closing")), ("stopped".into(), owes), "{odd}");
    }
    assert!(stop(&mut conn, 999).unwrap_err().contains("no active run 999"));
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
            "stop" => stop(&mut conn, run).unwrap(),
            "stop_tab" => stop_tab(&mut conn, &tab("t1")).unwrap(),
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
    stop(&mut conn, odd).unwrap();
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
    prompt_unseen(&mut conn, &tab("t1"), "bad").unwrap();
    let after = state(&conn, run);
    let Phase::NeedsPerson { why, .. } = &after.phase else { panic!("{after:?}") };
    let why = why.clone();
    assert_eq!(after, RunState { phase: needs(&why, None, true), ..before.clone() });
    // A paste recorded under another token changes nothing.
    prefilled(&mut conn, run, "mp", token + 1).unwrap();
    assert_eq!(state(&conn, run), after);
    prefilled(&mut conn, run, "mp", token).unwrap();
    assert_eq!(state(&conn, run), RunState { phase: needs(&why, None, false), ..after });
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
    for i in 0..3 * 2 * 2 * 3 * 2 * 2 * 2 * 2 * 4 {
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
            typed: [0, 1, 2, 3][pick(4)],
            ..working_columns()
        };
        let tx = conn.transaction().unwrap();
        let inserted = tx.execute(
            "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, steps, note, started_at, updated_at, resume, clear,
                                    briefed, prefill, prefill_token, briefing, briefing_at, closing, typed)
             VALUES (?1, 't1', '{}', ?2, ?3, ?4, ?5, 0, 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                flow, c.node_id, c.status, c.steps, c.note, c.resume, c.clear, c.briefed, c.prefill, c.prefill_token, c.briefing,
                c.briefing_at, c.closing, c.typed
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
        assert!((1..(1 << 53) - 1).contains(&token), "{token}");
    }
}
