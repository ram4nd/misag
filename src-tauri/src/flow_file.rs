//! Flow files: a flow as JSON, to export, import, or have Claude write (see
//! `cli::FLOW_HELP`, shown by `misag flow help`). The steps are a `flows::Graph`;
//! positions may be left out and are then laid out from Start. Unknown fields are
//! refused, so a misspelt one (`maxruns`) is not silently dropped.

use crate::flows::{self, Graph, START_OUTPUT};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};

/// The newest file format this misag reads.
pub const FORMAT: u32 = 1;

/// Where laid-out steps go: the first one's position, and the room each takes.
const ORIGIN: (f64, f64) = (40.0, 40.0);
const COLUMN: f64 = 260.0;
const ROW: f64 = 180.0;

/// The fields each part of a file may have, as `write` writes them.
const FILE_FIELDS: &[&str] = &["misagFlow", "name", "graph"];
const GRAPH_FIELDS: &[&str] = &["version", "nodes", "edges"];
const STEP_FIELDS: &[&str] = &["id", "kind", "title", "text", "outputs", "maxRuns", "x", "y"];
const EDGE_FIELDS: &[&str] = &["from", "output", "to"];

/// The flow in `text` as its name and steps, checked like an edit (see `flows::check`).
pub fn parse(text: &str) -> Result<(String, Graph), String> {
    let mut file: Value = serde_json::from_str(text).map_err(|e| format!("not a flow file: {e}"))?;
    match file.get("misagFlow").and_then(Value::as_u64) {
        None | Some(0) => return Err(format!("not a flow file: it has no \"misagFlow\": {FORMAT}")),
        Some(n) if n > u64::from(FORMAT) => {
            return Err(format!(
                "this flow file was written by a newer Mischief of Agents (format {n}, this one knows {FORMAT})"
            ));
        }
        Some(_) => {}
    }
    let name = match file.get("name").and_then(Value::as_str).map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => return Err("the flow has no \"name\"".into()),
    };
    known_fields(&file, FILE_FIELDS, "the flow")?;
    let Some(graph) = file.get_mut("graph").and_then(Value::as_object_mut) else {
        return Err("the flow has no \"graph\" with its steps".into());
    };
    check_fields(graph)?;
    complete(graph);
    let graph: Graph = serde_json::from_value(Value::Object(graph.clone())).map_err(|e| format!("the steps cannot be read: {e}"))?;
    if graph.nodes.is_empty() {
        return Err("the flow has no steps: it needs at least a Start step".into());
    }
    flows::check(&graph)?;
    Ok((name, flows::stored(graph)))
}

/// Fails on a field `value` (an object, else nothing to check) should not have.
fn known_fields(value: &Value, known: &[&str], what: &str) -> Result<(), String> {
    let unknown = value.as_object().into_iter().flat_map(|o| o.keys()).find(|k| !known.contains(&k.as_str()));
    match unknown {
        Some(field) => Err(format!("{what} has an unknown field \"{field}\" (see misag flow help)")),
        None => Ok(()),
    }
}

/// Fails on an unknown field in the graph, its steps or its connections, and on a
/// step with half a position.
fn check_fields(graph: &Map<String, Value>) -> Result<(), String> {
    let id = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("?").to_owned();
    known_fields(&Value::Object(graph.clone()), GRAPH_FIELDS, "the graph")?;
    let list = |key: &str| graph.get(key).and_then(Value::as_array).into_iter().flatten();
    for step in list("nodes") {
        known_fields(step, STEP_FIELDS, &format!("step \"{}\"", id(step, "id")))?;
        match (step.get("x"), step.get("y")) {
            (Some(_), None) => return Err(format!("step \"{}\" has an \"x\" but no \"y\": give both or neither", id(step, "id"))),
            (None, Some(_)) => return Err(format!("step \"{}\" has a \"y\" but no \"x\": give both or neither", id(step, "id"))),
            _ => {}
        }
    }
    for edge in list("edges") {
        known_fields(edge, EDGE_FIELDS, &format!("a connection from \"{}\"", id(edge, "from")))?;
    }
    Ok(())
}

/// The flow as a file, pretty-printed.
pub fn write(name: &str, graph: &Graph) -> String {
    let file = json!({ "misagFlow": FORMAT, "name": name, "graph": graph });
    serde_json::to_string_pretty(&file).expect("a graph is always JSON")
}

/// Fills in what a hand-written flow may leave out: Start's one output, and the
/// positions of steps without one: a column per number of steps from Start (the
/// steps it does not reach in one more column), stacked in the order listed.
fn complete(graph: &mut Map<String, Value>) {
    let Some(nodes) = graph.get("nodes").and_then(Value::as_array) else { return };
    let id = |n: &Value| n.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let edges: Vec<(String, String)> = graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e.get("from")?.as_str()?.to_owned(), e.get("to")?.as_str()?.to_owned())))
        .collect();
    // The fewest steps from Start to each step.
    let mut depth = HashMap::new();
    let mut queue: VecDeque<String> =
        nodes.iter().filter(|n| n.get("kind").and_then(Value::as_str) == Some("start")).map(id).collect();
    for start in &queue {
        depth.insert(start.clone(), 0usize);
    }
    while let Some(from) = queue.pop_front() {
        let next = depth[&from] + 1;
        for (_, to) in edges.iter().filter(|(f, _)| *f == from) {
            if !depth.contains_key(to) {
                depth.insert(to.clone(), next);
                queue.push_back(to.clone());
            }
        }
    }
    let unreached = depth.values().max().map_or(0, |d| d + 1);
    let mut rows: HashMap<usize, usize> = HashMap::new();
    let Some(nodes) = graph.get_mut("nodes").and_then(Value::as_array_mut) else { return };
    for node in nodes.iter_mut() {
        let column = depth.get(&id(node)).copied().unwrap_or(unreached);
        let Some(node) = node.as_object_mut() else { continue };
        if node.get("kind").and_then(Value::as_str) == Some("start") && !node.contains_key("outputs") {
            node.insert("outputs".into(), json!([START_OUTPUT]));
        }
        // `check_fields` has made sure a step has both or neither.
        if node.contains_key("x") {
            continue;
        }
        let row = rows.entry(column).or_default();
        node.insert("x".into(), json!(ORIGIN.0 + COLUMN * column as f64));
        node.insert("y".into(), json!(ORIGIN.1 + ROW * *row as f64));
        *row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::tests::runnable;
    use crate::flows::{self, Kind};

    const MINIMAL: &str = r#"{
        "misagFlow": 1,
        "name": "  Fix it  ",
        "graph": {
            "nodes": [
                {"id": "start", "kind": "start"},
                {"id": "fix", "kind": "prompt", "title": "Fix", "text": "Fix the tests.", "outputs": ["done", "stuck"]},
                {"id": "check", "kind": "manual", "title": "Check", "outputs": ["ok"]},
                {"id": "loose", "kind": "manual", "outputs": ["ok"]}
            ],
            "edges": [
                {"from": "start", "output": "next", "to": "fix"},
                {"from": "fix", "output": "done", "to": "check"}
            ]
        }
    }"#;

    #[test]
    fn a_written_flow_reads_back_the_same() {
        let graph = flows::stored(runnable());
        let (name, read) = parse(&write("Deploy", &graph)).unwrap();
        assert_eq!((name.as_str(), read), ("Deploy", graph));
    }

    #[test]
    fn a_written_flow_says_its_format() {
        let json: serde_json::Value = serde_json::from_str(&write("Deploy", &runnable())).unwrap();
        assert_eq!(json["misagFlow"], FORMAT);
        assert_eq!(json["name"], "Deploy");
        assert!(json["graph"]["nodes"].is_array());
    }

    #[test]
    fn a_hand_written_flow_gets_start_s_output_and_positions() {
        let (name, graph) = parse(MINIMAL).unwrap();
        assert_eq!(name, "Fix it");
        let at = |id: &str| graph.nodes.iter().find(|n| n.id == id).map(|n| (n.x, n.y)).unwrap();
        assert_eq!(graph.nodes[0].kind, Kind::Start);
        assert_eq!(graph.nodes[0].outputs, ["next"]);
        // A column per step from Start; a step Start does not reach goes after them.
        assert_eq!(at("start"), (40.0, 40.0));
        assert_eq!(at("fix"), (300.0, 40.0));
        assert_eq!(at("check"), (560.0, 40.0));
        assert_eq!(at("loose"), (820.0, 40.0));
    }

    #[test]
    fn steps_in_the_same_column_are_stacked() {
        let text = r#"{"misagFlow": 1, "name": "Branch", "graph": {
            "nodes": [{"id": "s", "kind": "start"}, {"id": "a", "kind": "manual", "outputs": ["x", "y"]},
                      {"id": "b", "kind": "manual", "outputs": ["ok"]}, {"id": "c", "kind": "manual", "outputs": ["ok"]}],
            "edges": [{"from": "s", "output": "next", "to": "a"}, {"from": "a", "output": "x", "to": "b"},
                      {"from": "a", "output": "y", "to": "c"}]}}"#;
        let (_, graph) = parse(text).unwrap();
        let at = |id: &str| graph.nodes.iter().find(|n| n.id == id).map(|n| (n.x, n.y)).unwrap();
        assert_eq!((at("b"), at("c")), ((560.0, 40.0), (560.0, 220.0)));
    }

    #[test]
    fn given_positions_are_kept() {
        let text = MINIMAL.replace(r#"{"id": "start", "kind": "start"}"#, r#"{"id": "start", "kind": "start", "x": 5, "y": 6}"#);
        let (_, graph) = parse(&text).unwrap();
        assert_eq!((graph.nodes[0].x, graph.nodes[0].y), (5.0, 6.0));
        // Half a position is a mistake, not a hint.
        let text = MINIMAL.replace(r#"{"id": "start", "kind": "start"}"#, r#"{"id": "start", "kind": "start", "x": 5}"#);
        assert_eq!(parse(&text).unwrap_err(), "step \"start\" has an \"x\" but no \"y\": give both or neither");
    }

    #[test]
    fn what_is_not_a_flow_file_says_why() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("not json").starts_with("not a flow file: "), "{}", err("not json"));
        assert_eq!(err(r#"{"name": "x", "graph": {"nodes": [], "edges": []}}"#), "not a flow file: it has no \"misagFlow\": 1");
        assert!(err(r#"{"misagFlow": 2, "name": "x", "graph": {"nodes": [], "edges": []}}"#).contains("newer"));
        assert_eq!(err(r#"{"misagFlow": 1, "graph": {"nodes": [], "edges": []}}"#), "the flow has no \"name\"");
        assert_eq!(err(r#"{"misagFlow": 1, "name": " ", "graph": {"nodes": [], "edges": []}}"#), "the flow has no \"name\"");
        assert_eq!(err(r#"{"misagFlow": 1, "name": "x"}"#), "the flow has no \"graph\" with its steps");
        let unknown = err(r#"{"misagFlow": 1, "name": "x", "graph": {"nodes": [{"id": "s", "kind": "loop"}], "edges": []}}"#);
        assert!(unknown.starts_with("the steps cannot be read: "), "{unknown}");
    }

    #[test]
    fn unknown_fields_are_refused_so_a_typo_is_not_lost() {
        let see = " (see misag flow help)";
        let typo = MINIMAL.replace(r#""title": "Fix","#, r#""title": "Fix", "maxruns": 3,"#);
        assert_eq!(parse(&typo).unwrap_err(), format!("step \"fix\" has an unknown field \"maxruns\"{see}"));
        let top = MINIMAL.replace(r#""name""#, r#""nme": 1, "name""#);
        assert_eq!(parse(&top).unwrap_err(), format!("the flow has an unknown field \"nme\"{see}"));
        let graph = MINIMAL.replace(r#""edges""#, r#""steps": [], "edges""#);
        assert_eq!(parse(&graph).unwrap_err(), format!("the graph has an unknown field \"steps\"{see}"));
        let edge = MINIMAL.replace(r#""to": "check"}"#, r#""to": "check", "label": "x"}"#);
        assert_eq!(parse(&edge).unwrap_err(), format!("a connection from \"fix\" has an unknown field \"label\"{see}"));
        // What a written flow holds is known.
        assert!(parse(&write("Deploy", &flows::stored(runnable()))).is_ok());
    }

    #[test]
    fn only_format_1_is_a_flow_file() {
        let zero = MINIMAL.replace(r#""misagFlow": 1"#, r#""misagFlow": 0"#);
        assert_eq!(parse(&zero).unwrap_err(), "not a flow file: it has no \"misagFlow\": 1");
    }

    #[test]
    fn a_flow_that_could_not_be_stored_is_refused() {
        let two_starts = MINIMAL.replace(r#""kind": "manual", "outputs": ["ok"]}"#, r#""kind": "start"}"#);
        assert_eq!(parse(&two_starts).unwrap_err(), "a flow has exactly one Start step");
        let empty = r#"{"misagFlow": 1, "name": "x", "graph": {"nodes": [], "edges": []}}"#;
        assert_eq!(parse(empty).unwrap_err(), "the flow has no steps: it needs at least a Start step");
    }
}
