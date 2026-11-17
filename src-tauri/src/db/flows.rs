//! The flows: their names, order, archiving and steps. Their runs are `runs`'s.

use super::{now_secs, sql, Db};
use crate::flows::{self, Graph};
use crate::runs;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;

/// A flow as the sidebar lists it. Mirrored by `Flow` in src/flows.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FlowSummary {
    pub id: i64,
    pub name: String,
    pub archived: bool,
    /// Whether a run begins by clearing the conversation (`flows::clears`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
    /// Added by a program in a tab and not yet confirmed by the person, so it does
    /// not run (see `import_unconfirmed_flow`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unconfirmed: bool,
}

/// The longest flow name, in characters, as long as a step title.
const MAX_FLOW_NAME: usize = 200;

/// A flow's name as stored: trimmed, not empty, at most `MAX_FLOW_NAME` characters,
/// and plain text, as it is printed in terminals (`misag flow list`) and read by Claude.
fn flow_name(name: &str) -> Result<&str, String> {
    match name.trim() {
        "" => Err("a flow needs a name".into()),
        name if name.chars().count() > MAX_FLOW_NAME => Err(format!("a flow name can be at most {MAX_FLOW_NAME} characters")),
        name => match name.chars().find(|&c| c.is_control() || flows::hidden(c)) {
            Some(c) => Err(format!("a flow name cannot hold control or hidden characters (U+{:04X})", c as u32)),
            None => Ok(name),
        },
    }
}

fn graph_json(graph: &Graph) -> Result<String, String> {
    serde_json::to_string(graph).map_err(|e| format!("the steps could not be stored ({e})"))
}

/// Fails unless a statement changed exactly the flow `id`.
fn one_flow(id: i64, changed: usize) -> Result<(), String> {
    if changed == 1 { Ok(()) } else { Err(format!("there is no flow {id}")) }
}

impl Db {
    /// Every flow in order, archived ones included.
    pub fn list_flows(&self) -> Result<Vec<FlowSummary>, String> {
        let conn = self.conn();
        // Read in SQL, so listing does not parse every flow's steps; it must agree
        // with `flows::clears` (a test checks), and a graph that is not JSON does not clear.
        let mut stmt = conn
            .prepare(
                "SELECT id, name, archived, CASE WHEN json_valid(graph) THEN EXISTS (
                    SELECT 1 FROM json_each(graph, '$.nodes')
                    WHERE json_extract(value, '$.kind') = 'start' AND json_extract(value, '$.clear') = 1
                 ) ELSE 0 END, unconfirmed
                 FROM flows ORDER BY position, id",
            )
            .map_err(sql)?;
        stmt.query_map([], |r| {
            Ok(FlowSummary { id: r.get(0)?, name: r.get(1)?, archived: r.get(2)?, clear: r.get(3)?, unconfirmed: r.get(4)? })
        })
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)
    }

    /// Adds a flow after the others, with only its Start step.
    pub fn create_flow(&self, name: &str) -> Result<FlowSummary, String> {
        self.add_flow(name, &Graph::default(), false)
    }

    /// Adds a flow after the others with these steps, if they pass `flows::check`
    /// (see `flow_file` for where they come from).
    pub fn import_flow(&self, name: &str, graph: &Graph) -> Result<FlowSummary, String> {
        flows::check(graph)?;
        self.add_flow(name, &flows::stored(graph.clone()), false)
    }

    /// Like `import_flow`, for a flow a program in a tab added (Claude, say, which a
    /// file or page it read may have steered): it runs only once the person has
    /// confirmed it (`confirm_flow`), as ▶ would otherwise send its steps unseen.
    pub fn import_unconfirmed_flow(&self, name: &str, graph: &Graph) -> Result<FlowSummary, String> {
        flows::check(graph)?;
        self.add_flow(name, &flows::stored(graph.clone()), true)
    }

    /// The person has seen the flow and lets it run.
    pub fn confirm_flow(&self, id: i64) -> Result<(), String> {
        let changed = self.conn().execute("UPDATE flows SET unconfirmed = 0 WHERE id = ?1", [id]).map_err(sql)?;
        one_flow(id, changed)
    }

    fn add_flow(&self, name: &str, graph: &Graph, unconfirmed: bool) -> Result<FlowSummary, String> {
        let name = flow_name(name)?;
        let clear = flows::clears(graph);
        let graph = graph_json(graph)?;
        let conn = self.conn();
        let now = now_secs();
        conn.execute(
            "INSERT INTO flows (name, position, graph, created_at, updated_at, unconfirmed)
             VALUES (?1, (SELECT COALESCE(MAX(position), -1) + 1 FROM flows), ?3, ?2, ?2, ?4)",
            params![name, now, graph, unconfirmed],
        )
        .map_err(sql)?;
        Ok(FlowSummary { id: conn.last_insert_rowid(), name: name.into(), archived: false, clear, unconfirmed })
    }

    pub fn rename_flow(&self, id: i64, name: &str) -> Result<(), String> {
        let name = flow_name(name)?;
        let changed = self
            .conn()
            .execute("UPDATE flows SET name = ?2, updated_at = ?3 WHERE id = ?1", params![id, name, now_secs()])
            .map_err(sql)?;
        one_flow(id, changed)
    }

    /// Archived flows keep their place, so unarchiving puts them back where they were.
    pub fn set_flow_archived(&self, id: i64, archived: bool) -> Result<(), String> {
        let changed = self
            .conn()
            .execute("UPDATE flows SET archived = ?2, updated_at = ?3 WHERE id = ?1", params![id, archived, now_secs()])
            .map_err(sql)?;
        one_flow(id, changed)
    }

    /// Puts the flows in the order of `ids`, which must list every flow exactly once.
    pub fn reorder_flows(&self, ids: &[i64]) -> Result<(), String> {
        let mut conn = self.conn();
        // IMMEDIATE, so the hook cannot write between the check and the update.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        let mut stored: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM flows").map_err(sql)?;
            stmt.query_map([], |r| r.get(0)).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?
        };
        let mut given = ids.to_vec();
        stored.sort_unstable();
        given.sort_unstable();
        if given != stored {
            return Err("the new order must list every flow exactly once; the flows may have changed, so reload them".into());
        }
        for (position, id) in ids.iter().enumerate() {
            tx.execute("UPDATE flows SET position = ?2 WHERE id = ?1", params![id, position as i64]).map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    /// The flow and its steps; one stored without any steps (the column's default)
    /// loads as a new flow.
    pub fn load_flow(&self, id: i64) -> Result<(FlowSummary, Graph), String> {
        let (name, archived, graph, unconfirmed): (String, bool, String, bool) = self
            .conn()
            .query_row("SELECT name, archived, graph, unconfirmed FROM flows WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()
            .map_err(sql)?
            .ok_or_else(|| format!("there is no flow {id}"))?;
        let graph: Graph = serde_json::from_str(&graph)
            .map_err(|e| format!("the steps of \"{name}\" cannot be read ({e})"))?;
        let graph = if graph.nodes.is_empty() && graph.edges.is_empty() { Graph::default() } else { graph };
        Ok((FlowSummary { id, name, archived, clear: flows::clears(&graph), unconfirmed }, graph))
    }

    /// Stores the flow's steps if they pass `flows::check`.
    pub fn save_flow_graph(&self, id: i64, graph: &Graph) -> Result<(), String> {
        flows::check(graph)?;
        let graph = graph_json(&flows::stored(graph.clone()))?;
        let changed = self
            .conn()
            .execute("UPDATE flows SET graph = ?2, updated_at = ?3 WHERE id = ?1", params![id, graph, now_secs()])
            .map_err(sql)?;
        one_flow(id, changed)
    }

    /// Deletes a flow and its finished runs; refused while a tab is running it.
    pub fn delete_flow(&self, id: i64) -> Result<(), String> {
        let changed = self
            .conn()
            .execute(
                &format!(
                    "DELETE FROM flows WHERE id = ?1 AND NOT EXISTS (
                         SELECT 1 FROM flow_runs WHERE flow_id = ?1 AND status IN {})",
                    runs::ACTIVE
                ),
                [id],
            )
            .map_err(sql)?;
        if changed == 0 && self.conn().query_row("SELECT 1 FROM flows WHERE id = ?1", [id], |_| Ok(())).optional().map_err(sql)?.is_some() {
            return Err("the flow is running in a tab; stop that run first".into());
        }
        one_flow(id, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::db::connect_existing;

    #[test]
    fn an_imported_flow_is_added_after_the_others_with_its_steps() {
        let db = Db::open(temp_db("import-flow")).unwrap();
        db.create_flow("First").unwrap();
        let graph = crate::flows::tests::runnable();
        let flow = db.import_flow("  Second ", &graph).unwrap();
        assert_eq!((flow.name.as_str(), flow.archived), ("Second", false));
        assert_eq!(flow_names(&db), names(&["First", "Second"]));
        assert_eq!(db.load_flow(flow.id).unwrap().1, graph);
    }

    #[test]
    fn a_flow_that_cannot_be_stored_is_not_imported() {
        let db = Db::open(temp_db("import-bad-flow")).unwrap();
        let mut graph = crate::flows::tests::runnable();
        graph.nodes[1].kind = crate::flows::Kind::Start;
        graph.nodes[1].outputs = vec!["next".into()];
        assert_eq!(db.import_flow("Two starts", &graph).unwrap_err(), "a flow has exactly one Start step");
        assert_eq!(db.import_flow(" ", &crate::flows::tests::runnable()).unwrap_err(), "a flow needs a name");
        // A name is printed in terminals (misag flow list) and shown to Claude.
        for (name, code) in [("red\u{1b}[31m", "001B"), ("a\u{200B}b", "200B"), ("x\u{202E}y", "202E")] {
            assert_eq!(
                db.import_flow(name, &crate::flows::tests::runnable()).unwrap_err(),
                format!("a flow name cannot hold control or hidden characters (U+{code})")
            );
            assert!(db.create_flow(name).is_err());
        }
        assert!(db.list_flows().unwrap().is_empty());
    }

    #[test]
    fn flows_are_created_in_order_with_trimmed_names() {
        let db = Db::open(temp_db("flows-create")).unwrap();
        assert!(db.list_flows().unwrap().is_empty());
        let deploy = db.create_flow("  Deploy ").unwrap();
        assert_eq!((deploy.name.as_str(), deploy.archived), ("Deploy", false));
        db.create_flow("Review").unwrap();
        assert_eq!(flow_names(&db), names(&["Deploy", "Review"]));
        assert_ne!(db.list_flows().unwrap()[0].id, db.list_flows().unwrap()[1].id);
    }

    #[test]
    fn a_flow_needs_a_name() {
        let db = Db::open(temp_db("flows-name")).unwrap();
        let err = db.create_flow("   ").unwrap_err();
        assert!(err.contains("name"), "{err}");
        assert!(db.list_flows().unwrap().is_empty());
        let id = db.create_flow("Deploy").unwrap().id;
        assert!(db.rename_flow(id, " ").is_err());
        db.rename_flow(id, " Ship it ").unwrap();
        assert_eq!(flow_names(&db), names(&["Ship it"]));
        let err = db.rename_flow(id + 100, "Gone").unwrap_err();
        assert!(err.contains("no flow"), "{err}");
        assert!(db.rename_flow(id, &"x".repeat(MAX_FLOW_NAME)).is_ok());
        let err = db.rename_flow(id, &"x".repeat(MAX_FLOW_NAME + 1)).unwrap_err();
        assert!(err.contains("at most"), "{err}");
        assert!(db.create_flow(&"x".repeat(MAX_FLOW_NAME + 1)).is_err());
    }

    #[test]
    fn archiving_keeps_a_flows_place() {
        let db = Db::open(temp_db("flows-archive")).unwrap();
        let ids: Vec<i64> = ["a", "b", "c"].iter().map(|n| db.create_flow(n).unwrap().id).collect();
        db.set_flow_archived(ids[1], true).unwrap();
        let archived = vec![("a".into(), false), ("b".into(), true), ("c".into(), false)];
        assert_eq!(flow_names(&db), archived);
        db.set_flow_archived(ids[1], false).unwrap();
        assert_eq!(flow_names(&db), names(&["a", "b", "c"]));
        assert!(db.set_flow_archived(ids[2] + 100, true).unwrap_err().contains("no flow"));
    }

    #[test]
    fn flows_are_reordered_only_by_a_complete_order() {
        let db = Db::open(temp_db("flows-reorder")).unwrap();
        let ids: Vec<i64> = ["a", "b", "c"].iter().map(|n| db.create_flow(n).unwrap().id).collect();
        db.reorder_flows(&[ids[2], ids[0], ids[1]]).unwrap();
        assert_eq!(flow_names(&db), names(&["c", "a", "b"]));
        // Missing, repeated and unknown ids leave the order alone.
        for bad in [vec![ids[0], ids[1]], vec![ids[0], ids[0], ids[1]], vec![ids[0], ids[1], ids[2], 999]] {
            assert!(db.reorder_flows(&bad).is_err(), "{bad:?}");
            assert_eq!(flow_names(&db), names(&["c", "a", "b"]));
        }
        // A new flow still goes last.
        db.create_flow("d").unwrap();
        assert_eq!(flow_names(&db), names(&["c", "a", "b", "d"]));
    }

    #[test]
    fn a_deleted_flow_is_gone() {
        let db = Db::open(temp_db("flows-delete")).unwrap();
        let a = db.create_flow("a").unwrap().id;
        db.create_flow("b").unwrap();
        db.delete_flow(a).unwrap();
        assert_eq!(flow_names(&db), names(&["b"]));
        assert!(db.delete_flow(a).unwrap_err().contains("no flow"));
    }

    #[test]
    fn the_schema_refuses_a_blank_flow_name() {
        let db = Db::open(temp_db("flows-blank")).unwrap();
        let blank = db.conn().execute(
            "INSERT INTO flows (name, position, created_at, updated_at) VALUES ('  ', 0, 0, 0)",
            [],
        );
        assert!(blank.is_err());
    }

    #[test]
    fn a_new_flow_starts_with_its_start_step_and_its_graph_round_trips() {
        let db = Db::open(temp_db("flows-graph")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let (flow, graph) = db.load_flow(id).unwrap();
        assert_eq!(flow.name, "Deploy");
        assert_eq!(graph, Graph::default());
        let runnable = crate::flows::tests::runnable();
        db.save_flow_graph(id, &runnable).unwrap();
        assert_eq!(db.load_flow(id).unwrap().1, runnable);
        assert!(db.load_flow(id + 100).unwrap_err().contains("no flow"));
        assert!(db.save_flow_graph(id + 100, &runnable).unwrap_err().contains("no flow"));
    }

    #[test]
    fn a_graph_is_stored_as_the_oldest_format_that_holds_it() {
        let db = Db::open(temp_db("flows-version")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes[1].max_runs = Some(2);
        db.save_flow_graph(id, &g).unwrap();
        let (_, loaded) = db.load_flow(id).unwrap();
        assert_eq!((loaded.version, loaded.nodes[1].max_runs), (2, Some(2)));
    }

    #[test]
    fn the_list_says_which_flows_start_with_clear() {
        let db = Db::open(temp_db("flows-clear")).unwrap();
        let plain = db.create_flow("Deploy").unwrap().id;
        let clears = db.create_flow("Review").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes[0].clear = true;
        db.save_flow_graph(clears, &g).unwrap();
        let listed: Vec<_> = db.list_flows().unwrap().into_iter().map(|f| (f.id, f.clear)).collect();
        assert_eq!(listed, vec![(plain, false), (clears, true)]);
    }

    #[test]
    fn the_list_reads_clear_as_flows_clears_does() {
        let db = Db::open(temp_db("flows-clear-parity")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let listed = || db.list_flows().unwrap()[0].clear;
        let mut g = crate::flows::tests::runnable();
        for clear in [false, true] {
            g.nodes[0].clear = clear;
            db.save_flow_graph(id, &g).unwrap();
            assert_eq!(listed(), crate::flows::clears(&g));
        }
        // Rows check would refuse, which only a hand edit or an older bug could store.
        let raw = |json: &str| db.conn().execute("UPDATE flows SET graph = ?1 WHERE id = ?2", params![json, id]).unwrap();
        raw(r#"{"nodes":[{"id":"p","kind":"prompt","clear":true,"x":0,"y":0}],"edges":[]}"#);
        assert!(!listed(), "only Start clears");
        raw(r#"{"nodes":[{"id":"start","kind":"start","clear":"yes","x":0,"y":0}],"edges":[]}"#);
        assert!(!listed());
        raw("not json");
        assert!(!listed());
    }

    #[test]
    fn a_graph_that_fails_its_check_is_not_saved() {
        let db = Db::open(temp_db("flows-bad-graph")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let mut bad = crate::flows::tests::runnable();
        bad.edges[0].to = "ghost".into();
        assert!(db.save_flow_graph(id, &bad).unwrap_err().contains("ghost"));
        assert_eq!(db.load_flow(id).unwrap().1, Graph::default());
    }

    #[test]
    fn a_flow_stored_without_steps_loads_as_a_new_one() {
        let db = Db::open(temp_db("flows-empty-graph")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        db.conn().execute("UPDATE flows SET graph = '{\"nodes\":[],\"edges\":[]}'", []).unwrap();
        assert_eq!(db.load_flow(id).unwrap().1, Graph::default());
        db.conn().execute("UPDATE flows SET graph = 'not json'", []).unwrap();
        assert!(db.load_flow(id).unwrap_err().contains("cannot be read"));
    }

    #[test]
    fn a_running_flow_is_not_deleted_and_a_finished_one_takes_its_runs_along() {
        let path = temp_db("flows-delete-runs");
        let db = Db::open(path.clone()).unwrap();
        let mut runs = connect_existing(&path).unwrap();
        let runnable = crate::flows::tests::runnable();
        let ids: Vec<i64> = ["busy", "idle"].iter().map(|n| db.create_flow(n).unwrap().id).collect();
        for (id, tab) in ids.iter().zip(["t1", "t2"]) {
            db.save_flow_graph(*id, &runnable).unwrap();
            crate::runs::start(&mut runs, *id, &tid(tab)).unwrap();
            // Claude's reply to the briefing takes the first step.
            crate::runs::on_stop(&mut runs, &tid(tab), Some("Ready")).unwrap();
        }
        let idle_run = crate::runs::active(&runs).unwrap()[1].id;
        crate::runs::stop(&runs, idle_run).unwrap();
        for status in ["running", "manual", "attention"] {
            // A run needs attention saying why (see the CHECKs on flow_runs).
            runs.execute("UPDATE flow_runs SET status = ?1, note = iif(?1 = 'attention', 'why', NULL) WHERE tab_id = 't1'", [status])
                .unwrap();
            let err = db.delete_flow(ids[0]).unwrap_err();
            assert!(err.contains("running"), "{status}: {err}");
        }
        db.delete_flow(ids[1]).unwrap();
        assert!(db.delete_flow(ids[1]).unwrap_err().contains("no flow"));
        let left: i64 = runs.query_row("SELECT COUNT(*) FROM flow_runs", [], |r| r.get(0)).unwrap();
        let events: i64 = runs.query_row("SELECT COUNT(*) FROM flow_run_events", [], |r| r.get(0)).unwrap();
        assert_eq!((left, events), (1, 1), "only the busy flow's run and its first step remain");
    }
}
