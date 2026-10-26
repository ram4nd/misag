//! Flows: graphs of steps that send prompts to Claude or wait for a person. A
//! flow's graph is stored as JSON in `flows.graph` and edited on its editor tab.
//!
//! `check` is what every stored graph satisfies; a graph being edited may still
//! be incomplete, and `problems` says what keeps it from running.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const MAX_STEPS: usize = 200;
pub const MAX_OUTPUTS: usize = 8;
const MAX_TITLE: usize = 200;
const MAX_TEXT: usize = 20_000;
const MAX_ID: usize = 40;
const MAX_OUTPUT: usize = 24;
/// The most a step's `max_runs` can be.
const MAX_RUNS: u32 = 100;
/// The output a Start step leads on by.
pub const START_OUTPUT: &str = "next";
/// Reserved for the flow runner, which reads it as "needs a person" in place of
/// an output, so no step may use it as one.
pub const ATTENTION: &str = "attention";

/// The newest graph format this misag reads; `check` refuses newer ones, which it
/// could misread. A graph is stored as the oldest format that holds it (see
/// `stored`), so older misags keep reading the flows they can run correctly.
/// Version 2 added step run limits (`Node::max_runs`), version 3 clearing the
/// conversation at Start (`Node::clear`), version 4 Start's base info (its `text`).
pub const GRAPH_VERSION: u32 = 4;

/// Mirrored by `Graph` in src/flowEditor.ts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Graph {
    /// Graphs stored before versions were added are version 1.
    #[serde(default = "first_version")]
    pub version: u32,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

fn first_version() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Start,
    /// Sends `text` to Claude.
    Prompt,
    /// Waits for a person, who picks one of the outputs.
    Manual,
    End,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    pub kind: Kind,
    #[serde(default)]
    pub title: String,
    /// A prompt step's prompt, or what the person does at a manual step. For Start:
    /// optional base info (where things are and the like), sent ahead of the run's
    /// first prompt.
    #[serde(default)]
    pub text: String,
    /// The ways the step can end, each leading to at most one next step.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// For a prompt step: how many times the flow may take a run into it, such as a
    /// review that loops back to a fix. Once reached, the run waits for the person
    /// at the step before, who can still send it on. None: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u32>,
    /// For the Start step: the run begins by typing `/clear`, so Claude starts the
    /// flow in a new conversation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
    pub x: f64,
    pub y: f64,
}

/// From `from`'s output `output` to the step `to`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub from: String,
    pub output: String,
    pub to: String,
}

impl Default for Graph {
    /// A new flow: only its Start step.
    fn default() -> Self {
        let start = Node {
            id: "start".into(),
            kind: Kind::Start,
            title: "Start".into(),
            text: String::new(),
            outputs: vec![START_OUTPUT.into()],
            max_runs: None,
            clear: false,
            x: 40.0,
            y: 40.0,
        };
        Graph { version: GRAPH_VERSION, nodes: vec![start], edges: Vec::new() }
    }
}

impl Node {
    /// The step as the user knows it: its title, or what kind of step it is.
    pub fn name(&self) -> &str {
        match (self.title.trim(), self.kind) {
            ("", Kind::Start) => "Start",
            ("", Kind::Prompt) => "Prompt step",
            ("", Kind::Manual) => "Manual step",
            ("", Kind::End) => "End",
            (title, _) => title,
        }
    }
}

/// Characters that take no room on screen yet reach Claude: zero-width and bidi
/// controls, tags, variation selectors and the like. A shared flow could hide
/// instructions in them. The joiner and emoji presentation selectors that make emoji,
/// which people do see, are allowed.
pub fn hidden(c: char) -> bool {
    matches!(c,
        '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}' | '\u{200C}' | '\u{200E}' | '\u{200F}'
        | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0D}'
        | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}' | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
}

fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// An output is a short lowercase word, so Claude can end its reply with it.
fn plain_output(output: &str) -> bool {
    let mut bytes = output.bytes();
    output.len() <= MAX_OUTPUT
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn check_outputs(node: &Node) -> Result<(), String> {
    let name = node.name();
    match node.kind {
        Kind::Start if node.outputs != [START_OUTPUT] => {
            return Err(format!("Start must have the one output \"{START_OUTPUT}\""));
        }
        Kind::End if !node.outputs.is_empty() => return Err(format!("End step \"{name}\" cannot have outputs")),
        Kind::Start | Kind::End => return Ok(()),
        Kind::Prompt | Kind::Manual => {}
    }
    if node.outputs.is_empty() || node.outputs.len() > MAX_OUTPUTS {
        return Err(format!("\"{name}\" needs between 1 and at most {MAX_OUTPUTS} outputs"));
    }
    let mut seen = HashSet::new();
    for output in &node.outputs {
        if !plain_output(output) || output == ATTENTION {
            return Err(format!(
                "\"{name}\" has the output {output:?}; outputs are short lowercase words (letters, digits, - and _), and \"{ATTENTION}\" is reserved"
            ));
        }
        if !seen.insert(output.as_str()) {
            return Err(format!("\"{name}\" has the output \"{output}\" twice"));
        }
    }
    Ok(())
}

fn check_node(node: &Node) -> Result<(), String> {
    if !plain_id(&node.id) {
        return Err(format!("bad step id {:?}: a step id is 1 to {MAX_ID} letters, digits, - or _", node.id));
    }
    if node.title.chars().count() > MAX_TITLE || node.text.chars().count() > MAX_TEXT {
        return Err(format!("\"{}\" has a title or text that is too long", node.name()));
    }
    if let Some(c) = node.title.chars().chain(node.text.chars()).find(|&c| hidden(c)) {
        return Err(format!(
            "\"{}\" has a hidden character (U+{:04X}) that Claude would read but the editor does not show",
            node.name(),
            c as u32
        ));
    }
    if !node.x.is_finite() || !node.y.is_finite() {
        return Err(format!("\"{}\" has no valid position", node.name()));
    }
    match (node.max_runs, node.kind) {
        (None, _) => {}
        // Leaving a manual step is always the person's pick, which no limit holds back.
        (Some(_), Kind::Start | Kind::Manual | Kind::End) => {
            return Err(format!("\"{}\" cannot have a run limit: only prompt steps can", node.name()));
        }
        (Some(n), _) if !(1..=MAX_RUNS).contains(&n) => {
            return Err(format!("\"{}\" can run 1 to {MAX_RUNS} times, not {n}", node.name()));
        }
        (Some(_), _) => {}
    }
    if node.clear && node.kind != Kind::Start {
        return Err(format!("\"{}\" cannot clear the conversation: only Start can", node.name()));
    }
    check_outputs(node)
}

/// `graph` as stored: in the oldest format that holds it, so a misag that would
/// ignore a newer field refuses the graph rather than run it differently.
pub fn stored(mut graph: Graph) -> Graph {
    // The format each step needs, by the newest field it uses.
    let needs = |n: &Node| match n {
        Node { kind: Kind::Start, text, .. } if !text.trim().is_empty() => 4,
        Node { kind: Kind::Start, clear: true, .. } => 3,
        Node { max_runs: Some(_), .. } => 2,
        _ => 1,
    };
    graph.version = graph.nodes.iter().map(needs).max().unwrap_or(1);
    graph
}

/// Whether a run of `graph` begins by clearing the conversation.
pub fn clears(graph: &Graph) -> bool {
    graph.nodes.iter().any(|n| n.kind == Kind::Start && n.clear)
}

/// Ok if `graph` is well formed enough to store (see the module docs), else why not.
pub fn check(graph: &Graph) -> Result<(), String> {
    if graph.version > GRAPH_VERSION {
        return Err(format!(
            "this flow was saved by a newer Mischief of Agents (format {}, this one knows {GRAPH_VERSION})",
            graph.version
        ));
    }
    if graph.nodes.len() > MAX_STEPS {
        return Err(format!("a flow has at most {MAX_STEPS} steps"));
    }
    let mut nodes = HashMap::new();
    for node in &graph.nodes {
        check_node(node)?;
        if nodes.insert(node.id.as_str(), node).is_some() {
            return Err(format!("there is more than one step with the id {:?}", node.id));
        }
    }
    // Only a flow stored before it had steps has none; it loads as a new flow.
    let starts = graph.nodes.iter().filter(|n| n.kind == Kind::Start).count();
    if !graph.nodes.is_empty() && starts != 1 {
        return Err("a flow has exactly one Start step".into());
    }
    let mut used = HashSet::new();
    for edge in &graph.edges {
        let from = nodes.get(edge.from.as_str()).ok_or_else(|| format!("a connection comes from the missing step {:?}", edge.from))?;
        let to = nodes.get(edge.to.as_str()).ok_or_else(|| format!("a connection goes to the missing step {:?}", edge.to))?;
        if from.kind == Kind::End {
            return Err(format!("End step \"{}\" cannot lead anywhere", from.name()));
        }
        if to.kind == Kind::Start {
            return Err("no step can lead back to Start".into());
        }
        if !from.outputs.contains(&edge.output) {
            return Err(format!("\"{}\" has no output {:?}", from.name(), edge.output));
        }
        if !used.insert((edge.from.as_str(), edge.output.as_str())) {
            return Err(format!("\"{}\" has more than one next step for \"{}\"", from.name(), edge.output));
        }
    }
    Ok(())
}

/// The marker Claude ends its reply with, lowercased: the last non-empty line
/// (closing code fences aside), `FLOW: <word>` in any case, possibly in backticks
/// or bold. The value must be a single word. None without one.
pub fn parse_marker(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).rfind(|line| !line.is_empty() && !line.starts_with("```"))?;
    let line = line.trim_matches(|c| c == '`' || c == '*').trim();
    let (label, value) = line.split_once(':')?;
    let value = value.trim().to_lowercase();
    let word = !value.is_empty() && !value.contains(char::is_whitespace);
    (label.trim().eq_ignore_ascii_case("flow") && word).then_some(value)
}

/// What is sent to Claude for a prompt step: the step's text and how to end the
/// reply so the runner knows which output.
/// Control characters (other than newlines and tabs) are dropped: the prompt may be
/// typed into a terminal, where they would act as keys or end the paste early. So
/// are hidden ones, which `check` refuses, in case a flow stored before holds some.
pub fn prompt_text(node: &Node) -> String {
    typeable(format!("{}\n\n{}", node.text.trim_end(), reply_rule(node)))
}

/// `prompt_text` for the first prompt Claude gets in a run, or after the run's
/// conversation was cleared: led by Start's base info, if it has any.
pub fn first_prompt_text(graph: &Graph, node: &Node) -> String {
    let info = graph.nodes.iter().find(|n| n.kind == Kind::Start).map_or("", |n| n.text.trim());
    if info.is_empty() {
        return prompt_text(node);
    }
    typeable(format!("{info}\n\n{}", prompt_text(node)))
}

/// What is sent to Claude to go on with a prompt step that needs attention, once
/// the person has seen to it.
pub fn resume_text(node: &Node) -> String {
    typeable(format!("Go on with the flow step \"{}\". {}", node.name(), reply_rule(node)))
}

fn reply_rule(node: &Node) -> String {
    let outputs: Vec<String> = node.outputs.iter().map(|o| format!("`FLOW: {o}`")).collect();
    format!(
        "When you have finished, end your reply with a last line that is exactly {}, whichever fits. \
         If you need a person to decide or do something first, end it with `FLOW: {ATTENTION}` instead and say why.",
        outputs.join(" or ")
    )
}

fn typeable(mut text: String) -> String {
    text.retain(|c| (!c.is_control() || c == '\n' || c == '\t') && !hidden(c));
    text
}

/// The step `from`'s `output` leads to; None ends the flow there.
pub fn next_step<'g>(graph: &'g Graph, from: &str, output: &str) -> Option<&'g Node> {
    let edge = graph.edges.iter().find(|e| e.from == from && e.output == output)?;
    graph.nodes.iter().find(|n| n.id == edge.to)
}

/// What keeps a stored graph from running, in the order the steps are listed.
pub fn problems(graph: &Graph) -> Vec<String> {
    let Some(start) = graph.nodes.iter().find(|n| n.kind == Kind::Start) else {
        return vec!["The flow needs a Start step.".into()];
    };
    // Every step Start leads to, directly or not.
    let mut reached = HashSet::from([start.id.as_str()]);
    let mut queue = vec![start.id.as_str()];
    while let Some(id) = queue.pop() {
        for edge in graph.edges.iter().filter(|e| e.from == id) {
            if reached.insert(edge.to.as_str()) {
                queue.push(edge.to.as_str());
            }
        }
    }
    let mut problems = Vec::new();
    for node in &graph.nodes {
        let name = node.name();
        if node.kind == Kind::Prompt && node.text.trim().is_empty() {
            problems.push(format!("\"{name}\" has no prompt."));
        }
        // Other outputs with no next step end the flow there.
        if node.kind == Kind::Start && !graph.edges.iter().any(|e| e.from == node.id) {
            problems.push("Start is not connected to a step.".into());
        }
        if !reached.contains(node.id.as_str()) {
            problems.push(format!("\"{name}\" cannot be reached from Start."));
        }
    }
    problems
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn node(id: &str, kind: Kind, outputs: &[&str]) -> Node {
        Node {
            id: id.into(),
            kind,
            title: id.to_uppercase(),
            text: if kind == Kind::Prompt { format!("do {id}") } else { String::new() },
            outputs: outputs.iter().map(|o| o.to_string()).collect(),
            max_runs: None,
            clear: false,
            x: 0.0,
            y: 0.0,
        }
    }

    pub(crate) fn edge(from: &str, output: &str, to: &str) -> Edge {
        Edge { from: from.into(), output: output.into(), to: to.into() }
    }

    /// Start → a prompt that passes to an end, or fails back to itself.
    pub(crate) fn runnable() -> Graph {
        Graph {
            nodes: vec![
                node("start", Kind::Start, &["next"]),
                node("p", Kind::Prompt, &["pass", "fail"]),
                node("e", Kind::End, &[]),
            ],
            edges: vec![edge("start", "next", "p"), edge("p", "pass", "e"), edge("p", "fail", "p")],
            version: 1,
        }
    }

    fn rejected(graph: &Graph, why: &str) {
        let err = check(graph).expect_err(why);
        assert!(err.contains(why), "{err:?} should mention {why:?}");
    }

    #[test]
    fn a_new_flow_is_a_lone_start() {
        let graph = Graph::default();
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!((graph.nodes[0].kind, graph.nodes[0].outputs.clone()), (Kind::Start, vec!["next".to_string()]));
        assert!(graph.edges.is_empty());
        check(&graph).unwrap();
        assert_eq!(problems(&graph), vec!["Start is not connected to a step."]);
    }

    #[test]
    fn a_complete_flow_has_no_problems() {
        check(&runnable()).unwrap();
        assert!(problems(&runnable()).is_empty());
    }

    #[test]
    fn the_json_shape_matches_the_frontend() {
        let json = serde_json::to_value(runnable()).unwrap();
        assert_eq!(json["nodes"][1]["kind"], "prompt");
        assert_eq!(json["nodes"][1]["outputs"], serde_json::json!(["pass", "fail"]));
        assert_eq!(json["edges"][0], serde_json::json!({ "from": "start", "output": "next", "to": "p" }));
        assert_eq!(json["version"], 1);
        // Title, text and outputs may be left out.
        let parsed: Graph = serde_json::from_value(serde_json::json!({
            "nodes": [{ "id": "e", "kind": "end", "x": 1, "y": 2 }], "edges": []
        }))
        .unwrap();
        assert_eq!(parsed.nodes[0].title, "");
        assert_eq!(parsed.nodes[0].max_runs, None);
        assert!(json["nodes"][1].get("maxRuns").is_none(), "no limit is left out");
        let mut limited = runnable();
        limited.nodes[1].max_runs = Some(3);
        assert_eq!(serde_json::to_value(limited).unwrap()["nodes"][1]["maxRuns"], 3);
        let read: Node = serde_json::from_value(serde_json::json!({ "id": "p", "kind": "prompt", "maxRuns": 3, "x": 0, "y": 0 })).unwrap();
        assert_eq!(read.max_runs, Some(3));
        assert!(json["nodes"][0].get("clear").is_none(), "not clearing is left out");
        let read: Node = serde_json::from_value(serde_json::json!({ "id": "start", "kind": "start", "clear": true, "x": 0, "y": 0 })).unwrap();
        assert!(read.clear);
        assert_eq!(parsed.version, 1, "graphs stored before versions are version 1");
        let newer = serde_json::json!({ "version": 5, "nodes": [], "edges": [] });
        assert!(check(&serde_json::from_value(newer).unwrap()).unwrap_err().contains("newer Mischief of Agents"));
    }

    #[test]
    fn limits_count_characters_not_bytes() {
        let mut g = runnable();
        g.nodes[1].title = "ä".repeat(200);
        check(&g).unwrap();
        let mut g = Graph::default();
        g.nodes.extend((1..200).map(|i| node(&format!("n{i}"), Kind::End, &[])));
        check(&g).unwrap();
    }

    #[test]
    fn step_ids_are_unique_and_plain() {
        let mut g = runnable();
        g.nodes[2].id = "p".into();
        rejected(&g, "more than one step with the id");
        for bad in ["", "a b", "x\n", &"n".repeat(41)] {
            let mut g = runnable();
            g.nodes[2].id = bad.into();
            g.edges[1].to = bad.into();
            rejected(&g, "step id");
        }
    }

    #[test]
    fn there_is_at_most_one_start_and_it_has_one_output() {
        let mut g = runnable();
        g.nodes.push(node("s2", Kind::Start, &["next"]));
        rejected(&g, "exactly one Start");
        let mut g = runnable();
        g.nodes.remove(0);
        g.edges.remove(0);
        rejected(&g, "exactly one Start");
        check(&Graph { version: 1, nodes: vec![], edges: vec![] }).unwrap();
        let mut g = runnable();
        g.nodes[0].outputs = vec!["go".into()];
        g.edges[0].output = "go".into();
        rejected(&g, "Start");
        let mut g = runnable();
        g.nodes[2].outputs = vec!["again".into()];
        rejected(&g, "End");
    }

    #[test]
    fn outputs_are_short_lowercase_words() {
        for bad in [vec![], vec!["Pass"], vec!["a b"], vec![""], vec!["ok", "ok"], vec!["attention"], vec!["x".repeat(25).as_str()]] {
            let mut g = runnable();
            g.nodes[1].outputs = bad.iter().map(|o| o.to_string()).collect();
            g.edges.retain(|e| e.from != "p");
            assert!(check(&g).is_err(), "{bad:?}");
        }
        let mut g = runnable();
        g.nodes[1].outputs = (0..9).map(|i| format!("o{i}")).collect();
        g.edges.retain(|e| e.from != "p");
        rejected(&g, "at most 8");
        let mut g = runnable();
        g.nodes[1].outputs = vec!["done".into(), "needs-fix".into(), "retry_2".into()];
        g.edges.retain(|e| e.from != "p");
        check(&g).unwrap();
    }

    #[test]
    fn edges_join_existing_outputs_and_steps() {
        let mut g = runnable();
        g.edges.push(edge("ghost", "next", "p"));
        rejected(&g, "ghost");
        let mut g = runnable();
        g.edges[1].to = "ghost".into();
        rejected(&g, "ghost");
        let mut g = runnable();
        g.edges[1].output = "maybe".into();
        rejected(&g, "maybe");
        let mut g = runnable();
        g.edges.push(edge("p", "pass", "p"));
        rejected(&g, "more than one next step");
        let mut g = runnable();
        g.edges.push(edge("e", "next", "p"));
        rejected(&g, "End");
        let mut g = runnable();
        g.edges[2].to = "start".into();
        rejected(&g, "Start");
    }

    #[test]
    fn a_run_limit_is_a_sensible_count_on_a_prompt_step() {
        let with_limit = |id: &str, kind: Kind, max: u32| {
            let mut g = runnable();
            let n = g.nodes.iter_mut().find(|n| n.id == id).unwrap();
            n.kind = kind;
            n.max_runs = Some(max);
            if kind == Kind::Manual {
                n.text.clear();
            }
            g
        };
        for max in [1, 3, 100] {
            check(&with_limit("p", Kind::Prompt, max)).unwrap();
        }
        // Leaving a manual step is always the person's pick, which no limit holds back.
        rejected(&with_limit("p", Kind::Manual, 2), "cannot have a run limit");
        rejected(&with_limit("e", Kind::End, 2), "cannot have a run limit");
        rejected(&with_limit("start", Kind::Start, 2), "cannot have a run limit");
        for bad in [0, 101] {
            rejected(&with_limit("p", Kind::Prompt, bad), "1 to 100 times");
        }
    }

    #[test]
    fn a_graph_with_run_limits_is_a_newer_format() {
        // An older misag would ignore the limits, so it must refuse such a graph.
        let mut g = runnable();
        assert_eq!(stored(g.clone()).version, 1);
        g.nodes[1].max_runs = Some(2);
        assert_eq!(stored(g.clone()).version, 2);
        check(&stored(g)).unwrap();
    }

    #[test]
    fn only_start_can_clear_the_conversation() {
        let mut g = runnable();
        g.nodes[0].clear = true;
        check(&g).unwrap();
        let mut g = runnable();
        g.nodes[1].clear = true;
        rejected(&g, "only Start can");
    }

    #[test]
    fn a_graph_that_clears_is_a_newer_format() {
        // An older misag would ignore `clear` and run in the old conversation.
        let mut g = runnable();
        g.nodes[0].clear = true;
        assert_eq!(stored(g.clone()).version, 3);
        g.nodes[1].max_runs = Some(2);
        assert_eq!(stored(g.clone()).version, 3);
        check(&stored(g)).unwrap();
        assert!(!clears(&runnable()));
        let mut g = runnable();
        g.nodes[0].clear = true;
        assert!(clears(&g));
    }

    #[test]
    fn a_graph_whose_start_has_base_info_is_a_newer_format() {
        // An older misag would ignore Start's text and run without it.
        let mut g = runnable();
        g.nodes[0].text = "  \n".into();
        assert_eq!(stored(g.clone()).version, 1, "blank text is no base info");
        g.nodes[0].text = "The app is in src/.".into();
        assert_eq!(stored(g.clone()).version, 4);
        check(&stored(g)).unwrap();
    }

    #[test]
    fn the_first_prompt_carries_start_s_base_info() {
        let mut g = runnable();
        assert_eq!(first_prompt_text(&g, &g.nodes[1]), prompt_text(&g.nodes[1]));
        g.nodes[0].text = "The app is in src/.\u{1b}\n\n".into();
        let text = first_prompt_text(&g, &g.nodes[1]);
        assert_eq!(text, format!("The app is in src/.\n\n{}", prompt_text(&g.nodes[1])));
    }

    #[test]
    fn sizes_and_positions_are_bounded() {
        let mut g = runnable();
        g.nodes[1].text = "x".repeat(20_001);
        rejected(&g, "too long");
        let mut g = runnable();
        g.nodes[1].title = "x".repeat(201);
        rejected(&g, "too long");
        let mut g = runnable();
        g.nodes[1].x = f64::NAN;
        rejected(&g, "position");
        let mut g = Graph::default();
        g.nodes.extend((0..200).map(|i| node(&format!("n{i}"), Kind::End, &[])));
        rejected(&g, "at most 200");
    }

    #[test]
    fn problems_name_the_steps_in_order() {
        let mut g = runnable();
        g.nodes[1].text = "  ".into();
        // An output with no next step ends the flow there, which is fine.
        g.edges.retain(|e| e.output != "fail");
        g.nodes.push(Node { title: String::new(), ..node("m", Kind::Manual, &["done"]) });
        assert_eq!(
            problems(&g),
            vec!["\"P\" has no prompt.", "\"Manual step\" cannot be reached from Start."]
        );
        let mut g = runnable();
        g.nodes.remove(0);
        g.edges.remove(0);
        assert_eq!(problems(&g)[0], "The flow needs a Start step.");
    }

    #[test]
    fn the_flow_line_is_the_last_line_of_the_reply() {
        for (reply, marker) in [
            ("Tests pass.\n\nFLOW: pass", Some("pass")),
            ("FLOW: Pass  \n\n", Some("pass")),
            ("flow:fail", Some("fail")),
            ("Done.\n`FLOW: needs-fix`", Some("needs-fix")),
            ("Done.\n**FLOW: attention**", Some("attention")),
            ("FLOW: pass\nBut one more thing.", None),
            ("FLOW:", None),
            ("FLOW: two words", None),
            ("", None),
            ("No marker here.", None),
            ("Done.\r\nFLOW: pass\r\n", Some("pass")),
            ("```\nFLOW: pass\n```", Some("pass")),
            ("Note: see FLOW: pass", None),
        ] {
            assert_eq!(parse_marker(reply).as_deref(), marker, "{reply:?}");
        }
    }

    #[test]
    fn a_prompt_asks_for_a_flow_line_naming_its_outputs() {
        let g = runnable();
        let text = prompt_text(&g.nodes[1]);
        assert!(text.starts_with("do p"), "{text}");
        for part in ["FLOW: pass", "FLOW: fail", "FLOW: attention", "last line"] {
            assert!(text.contains(part), "{part:?} in {text}");
        }
        // Control characters could end the paste the prompt is typed as, or act as keys.
        let mut odd = g.nodes[1].clone();
        odd.text = "a\u{1b}[201~b\tc\u{7}d\ne".into();
        assert!(prompt_text(&odd).starts_with("a[201~b\tcd\ne"));
        // Characters Claude reads but no one sees, in a flow stored before they were refused.
        odd.text = "a\u{200B}b\u{E0041}\u{202E}c \u{2764}\u{FE0F}".into();
        assert!(prompt_text(&odd).starts_with("abc \u{2764}\u{FE0F}\n"));
    }

    #[test]
    fn the_reminder_to_go_on_names_the_step_and_its_outputs_typeably() {
        let mut g = runnable();
        g.nodes[1].title = "Fix\u{1b}[201~\u{202E} it".into();
        let text = resume_text(&g.nodes[1]);
        assert!(text.starts_with("Go on with the flow step \"Fix[201~ it\". "), "{text}");
        assert!(text.contains("`FLOW: pass` or `FLOW: fail`") && text.contains("`FLOW: attention`"), "{text}");
    }

    #[test]
    fn hidden_characters_are_refused_but_emoji_are_kept() {
        let mut g = runnable();
        for hidden in ['\u{200B}', '\u{202E}', '\u{2066}', '\u{FEFF}', '\u{E0041}', '\u{E0100}', '\u{AD}'] {
            g.nodes[1].text = format!("do p{hidden} and more");
            let err = check(&g).unwrap_err();
            assert_eq!(
                err,
                format!("\"P\" has a hidden character (U+{:04X}) that Claude would read but the editor does not show", hidden as u32)
            );
        }
        g.nodes[1].text = "do p".into();
        g.nodes[1].title = "P\u{200B}".into();
        assert!(check(&g).unwrap_err().contains("U+200B"));
        // Joiners and emoji presentation make emoji, which people do see.
        g.nodes[1].title = "\u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F}".into();
        g.nodes[1].text = "\u{2705} done".into();
        assert_eq!(check(&g), Ok(()));
    }

    #[test]
    fn an_output_leads_to_its_step_or_ends_the_flow() {
        let g = runnable();
        assert_eq!(next_step(&g, "p", "pass").map(|n| n.id.as_str()), Some("e"));
        assert_eq!(next_step(&g, "p", "fail").map(|n| n.id.as_str()), Some("p"));
        assert_eq!(next_step(&g, "p", "skip"), None);
        assert_eq!(next_step(&g, "start", START_OUTPUT).map(|n| n.id.as_str()), Some("p"));
    }
}
