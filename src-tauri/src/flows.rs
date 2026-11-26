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
/// The most Start's base info can be, in characters: after a clear it goes with a
/// prompt (see `first_prompt_text`), which can have `MAX_TEXT` of its own.
pub const MAX_INFO: usize = 4_000;
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
/// conversation at Start (`Node::clear`), version 4 Start's base info (its `text`),
/// version 5 manual prompts (a `manual_prompt` kind then), version 6 tab title
/// steps (`Kind::Title`), version 7 the prompt step's `pause`, `draft` and `note`,
/// which took over the pause and manual prompt kinds (see `StoredNode`).
pub const GRAPH_VERSION: u32 = 7;
/// The most a tab title Claude writes can be, in characters (see `parse_title`).
pub const MAX_TAB_TITLE: usize = 60;

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
    /// Sends `text` to Claude; or, with `Node::draft`, puts it into Claude's input
    /// for the person to finish and send; or, a pause with no text, only waits for
    /// the person (see `Node::only_waits`). With `Node::pause`, the run waits for the
    /// person once Claude has answered, Claude's output suggested.
    Prompt,
    /// Has Claude name the run's tab after what the person asked it: Claude replies
    /// with a `TITLE: <title>` line (see `parse_title`), which the app makes the tab's
    /// name, and the run goes on by the step's one output. `text`, optional, guides
    /// the title.
    Title,
    End,
}

/// Read through `StoredNode`, which turns the step kinds older versions saved into
/// prompt steps.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", try_from = "StoredNode")]
pub struct Node {
    pub id: String,
    pub kind: Kind,
    #[serde(default)]
    pub title: String,
    /// A prompt step's prompt, or with `draft` its draft, put into Claude's input
    /// for the person to finish; empty at a pause that only waits. For Start:
    /// optional base info (where things are and the like), sent in the run's
    /// briefing (see `briefing_text`). For a tab title step, optional guidance.
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
    /// For a prompt step: the run waits for the person when the step is done, with
    /// the output Claude picked suggested, for the person to go on by (▶) or pick
    /// another. Without text (and no `draft`), the step only waits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pause: bool,
    /// For a prompt step: `text` is a draft the app puts into Claude's input without
    /// sending it; the person finishes and sends the prompt, and Claude's answer
    /// ends the step. With `pause` too, the run waits for the person once more after
    /// Claude has answered, as at any step that pauses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
    /// For a prompt step that pauses (and only one, see `check_node`): what the person
    /// is to do there, shown while the run waits; never sent to Claude.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub x: f64,
    pub y: f64,
}

/// A step as stored, by this version or an older one: older versions had kinds of
/// their own for a pause ("manual", or "pause") and a manual prompt, which load as
/// prompt steps that pause, or that are drafts.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNode {
    id: String,
    kind: StoredKind,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    max_runs: Option<u32>,
    #[serde(default)]
    clear: bool,
    #[serde(default)]
    pause: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    note: String,
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum StoredKind {
    Start,
    Prompt,
    /// A pause, before version 7: its text said what the person does.
    #[serde(alias = "pause")]
    Manual,
    /// A manual prompt, before version 7: its text was the draft.
    #[serde(rename = "manual_prompt")]
    ManualPrompt,
    Title,
    End,
}

impl TryFrom<StoredNode> for Node {
    type Error = String;

    /// The step `n` holds. An older kind never had the fields that replaced it, so
    /// one with them (a file edited by hand) is refused rather than read as some
    /// other step.
    fn try_from(n: StoredNode) -> Result<Self, String> {
        let older = matches!(n.kind, StoredKind::Manual | StoredKind::ManualPrompt);
        if older && (n.pause || n.draft || !n.note.is_empty()) {
            return Err(format!(
                "step {:?} has an older step kind (\"manual\", \"pause\" or \"manual_prompt\") with \"pause\", \"draft\" or \"note\"; \
                 make it a \"prompt\" step with those instead",
                n.id
            ));
        }
        let (kind, text, pause, draft, note) = match n.kind {
            StoredKind::Start => (Kind::Start, n.text, n.pause, n.draft, n.note),
            StoredKind::Prompt => (Kind::Prompt, n.text, n.pause, n.draft, n.note),
            StoredKind::Manual => (Kind::Prompt, String::new(), true, false, n.text),
            StoredKind::ManualPrompt => (Kind::Prompt, n.text, false, true, String::new()),
            StoredKind::Title => (Kind::Title, n.text, n.pause, n.draft, n.note),
            StoredKind::End => (Kind::End, n.text, n.pause, n.draft, n.note),
        };
        Ok(Node { id: n.id, kind, title: n.title, text, outputs: n.outputs, max_runs: n.max_runs, clear: n.clear, pause, draft, note, x: n.x, y: n.y })
    }
}

/// What a prompt step has the run do when it gets there (see `Node::mode`).
/// Mirrored by `stepMode` in src/stepMode.ts, which the editor draws steps by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Sends its prompt to Claude (and, with `pause`, waits for the person once
    /// Claude has answered).
    Send,
    /// Puts its draft into Claude's input for the person to finish and send.
    Draft,
    /// Only waits for the person: it pauses, with no prompt for Claude.
    Wait,
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
            pause: false,
            draft: false,
            note: String::new(),
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
            ("", Kind::Prompt) if self.only_waits() => "Pause",
            ("", Kind::Prompt) if self.draft => "Manual prompt",
            ("", Kind::Prompt) => "Prompt step",
            ("", Kind::Title) => "Tab title",
            ("", Kind::End) => "End",
            (title, _) => title,
        }
    }

    /// What a prompt step has the run do; None for another kind of step. The one
    /// place a step's flags and text are read for it: a pause whose prompt is
    /// cleared only waits from then on, as the editor's canvas shows.
    pub fn mode(&self) -> Option<Mode> {
        match self {
            Node { kind: Kind::Prompt, draft: true, .. } => Some(Mode::Draft),
            Node { kind: Kind::Prompt, pause: true, text, .. } if text.trim().is_empty() => Some(Mode::Wait),
            Node { kind: Kind::Prompt, .. } => Some(Mode::Send),
            _ => None,
        }
    }

    /// Whether the step waits for the person's own prompt, its text a draft for it.
    pub fn drafts(&self) -> bool {
        self.mode() == Some(Mode::Draft)
    }

    /// Whether the step only waits for the person: a pause with no prompt for Claude.
    pub fn only_waits(&self) -> bool {
        self.mode() == Some(Mode::Wait)
    }

    /// Whether taking the step sends Claude a prompt of the flow's.
    pub fn sends(&self) -> bool {
        self.kind == Kind::Title || self.mode() == Some(Mode::Send)
    }

    /// Whether the run waits for the person once the step is done (see `pause`).
    pub fn pauses(&self) -> bool {
        self.kind == Kind::Prompt && self.pause
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
        Kind::Prompt | Kind::Title => {}
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
    if node.title.chars().count() > MAX_TITLE || node.text.chars().count() > MAX_TEXT || node.note.chars().count() > MAX_TEXT {
        return Err(format!("\"{}\" has a title, text or note that is too long", node.name()));
    }
    if let Some(c) = node.title.chars().chain(node.text.chars()).chain(node.note.chars()).find(|&c| hidden(c)) {
        return Err(format!(
            "\"{}\" has a hidden character (U+{:04X}) that Claude would read but the editor does not show",
            node.name(),
            c as u32
        ));
    }
    if node.kind == Kind::Start && node.text.chars().count() > MAX_INFO {
        return Err(format!("Start's base info can be at most {MAX_INFO} characters"));
    }
    if !node.x.is_finite() || !node.y.is_finite() {
        return Err(format!("\"{}\" has no valid position", node.name()));
    }
    match (node.max_runs, node.kind) {
        (None, _) => {}
        // Naming the tab again and again needs no limit.
        (Some(_), Kind::Start | Kind::Title | Kind::End) => {
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
    if node.kind != Kind::Prompt && (node.pause || node.draft || !node.note.is_empty()) {
        return Err(format!("\"{}\" cannot pause, be a draft or have a note: only prompt steps can", node.name()));
    }
    // It would never be shown, yet would keep older misags from reading the flow.
    if !node.pause && !node.note.is_empty() {
        return Err(format!("\"{}\" has a note but does not pause: a note only shows at a pause", node.name()));
    }
    check_outputs(node)
}

/// `graph` as stored: in the oldest format that holds it, so a misag that would
/// ignore a newer field refuses the graph rather than run it differently.
pub fn stored(mut graph: Graph) -> Graph {
    // The format each step needs, by the newest field it uses.
    // A note goes with a pause (see `check_node`).
    let needs = |n: &Node| match n {
        Node { pause: true, .. } | Node { draft: true, .. } => 7,
        Node { kind: Kind::Title, .. } => 6,
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

/// The tab title Claude ends its reply with at a tab title step: the last non-empty
/// line (closing code fences aside), `TITLE: <title>` in any case, possibly in
/// backticks, bold or quotes, its whitespace collapsed, characters that do not show
/// or that act in a terminal dropped, and cut to `MAX_TAB_TITLE` characters. None
/// without one.
pub fn parse_title(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).rfind(|line| !line.is_empty() && !line.starts_with("```"))?;
    let (label, value) = line.split_once(':')?;
    if !label.trim_matches(|c| c == '`' || c == '*').trim().eq_ignore_ascii_case("title") {
        return None;
    }
    let value = typeable(value.to_owned());
    let value = value.trim_matches(|c: char| c.is_whitespace() || matches!(c, '`' | '*' | '"' | '\'' | '“' | '”'));
    let title: String = value.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TAB_TITLE).collect();
    let title = title.trim_end().to_owned();
    (!title.is_empty()).then_some(title)
}

/// What is sent to Claude for a prompt step: the step's text and how to end the
/// reply so the runner knows which output. A tab title step asks for the title
/// instead (see `title_request`).
/// Control characters (other than newlines and tabs) are dropped: the prompt may be
/// typed into a terminal, where they would act as keys or end the paste early. So
/// are hidden ones, which `check` refuses, in case a flow stored before holds some.
pub fn prompt_text(node: &Node) -> String {
    if node.kind == Kind::Title {
        return typeable(title_request(node));
    }
    typeable(format!("{}\n\n{}", node.text.trim_end(), reply_rule(node)))
}

/// What a tab title step asks of Claude: a title for the tab from what the person
/// asked, guided by the step's text if it has any, as the reply's one line.
fn title_request(node: &Node) -> String {
    let guide = node.text.trim();
    let guide = if guide.is_empty() { String::new() } else { format!("\n\n{guide}") };
    format!(
        "Give this tab a short title, 2 to 6 words, that says what I asked you to do in this conversation.{guide}\n\n{}",
        title_rule()
    )
}

fn title_rule() -> String {
    format!(
        "Do nothing else for this step. Reply with one line that is exactly `TITLE: <the title>`, and no FLOW line. \
         If you need a person first, end your reply with `FLOW: {ATTENTION}` instead and say why."
    )
}

/// What a run tells Claude before its first step: the run's rules, what Claude
/// should know before the flow (Start's base info, if any), and to wait for the
/// first step, so Claude does not start on the whole job. Claude loses it with a
/// clear, so `first_prompt_text` gives it again.
pub fn briefing_text(graph: &Graph, flow: &str) -> String {
    typeable(format!("{}{}\n\nDo not act on this message. Reply only with: Ready", rules(flow), background(graph)))
}

/// The first prompt after the run's conversation was cleared (whether the run took
/// the step, or the person picked an output): the run's rules and Start's base
/// info, which Claude lost with it, then `node`'s prompt.
pub fn first_prompt_text(graph: &Graph, flow: &str, node: &Node) -> String {
    typeable(format!("{}{}\n\nThe current step:\n\n{}", rules(flow), background(graph), prompt_text(node)))
}

/// Whether Claude's reply to a run's briefing only says it is ready, as asked, so
/// it did not start on the job or ask something: the reply's letters spell "ready",
/// whatever the case, punctuation or markdown around them.
pub fn says_ready(reply: &str) -> bool {
    let letters: String = reply.chars().filter(|c| c.is_alphanumeric()).collect();
    letters.eq_ignore_ascii_case("ready")
}

/// What a run tells Claude once it is done, as context with the person's next
/// prompt (see `runs::on_prompt`).
pub fn closing_text(flow: &str) -> String {
    typeable(format!(
        "The flow \"{flow}\" is finished. You no longer need to end your replies with a FLOW line; carry on as usual."
    ))
}

fn rules(flow: &str) -> String {
    format!(
        "Mischief of Agents runs the flow \"{flow}\" in this conversation. Its steps come to you one at a time, each as a message:\n\
         - Do only what the current step asks. Do not start on later work until its step arrives.\n\
         - Each step names its outputs. When you have finished the step, end your reply with a last line that is exactly `FLOW: <output>`, the output that fits.\n\
         - If you need a person to decide or do something first, end your reply with `FLOW: {ATTENTION}` instead and say why.\n\
         - Some steps are the person's own: they will write to you themselves.\n\
         - A step that asks for a tab title ends with a `TITLE:` line instead of a FLOW line."
    )
}

/// Start's base info, as the briefing gives it; empty without any.
fn background(graph: &Graph) -> String {
    let info = base_info(graph);
    if info.is_empty() { String::new() } else { format!("\n\nBackground for the flow:\n\n{info}") }
}

/// What a draft step puts into Claude's input: Start's base info (trimmed) when
/// `with_info`, then the step's draft (trimmed), either of which may be empty.
pub fn prefill_text(graph: &Graph, node: &Node, with_info: bool) -> String {
    let info = if with_info { base_info(graph) } else { String::new() };
    // A trailing space stays, so what the person types does not run into the draft.
    let draft = typeable(node.text.trim_start().trim_end_matches(['\n', '\r']).to_owned());
    [info, draft].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// Start's base info (trimmed), as it is typed to Claude; empty without any.
pub fn base_info(graph: &Graph) -> String {
    typeable(graph.nodes.iter().find(|n| n.kind == Kind::Start).map_or("", |n| n.text.trim()).to_owned())
}

/// What is sent to Claude to go on with a prompt step that needs attention, once
/// the person has seen to it.
pub fn resume_text(node: &Node) -> String {
    let rule = if node.kind == Kind::Title { title_rule() } else { reply_rule(node) };
    typeable(format!("Go on with the flow step \"{}\". {rule}", node.name()))
}

/// What Claude is told along with the person's prompt at a draft step with several
/// outputs, so it ends its answer with one (see `runs::on_prompt`); None for a draft
/// step with one, which goes on by it whatever Claude answers.
pub fn draft_rule(node: &Node) -> Option<String> {
    (node.drafts() && node.outputs.len() > 1)
        .then(|| typeable(format!("This prompt is the flow step \"{}\". {}", node.name(), reply_rule(node))))
}

fn reply_rule(node: &Node) -> String {
    let outputs: Vec<String> = node.outputs.iter().map(|o| format!("`FLOW: {o}`")).collect();
    format!(
        "Do only this step; the next one comes after your reply. \
         When you have finished, end your reply with a last line that is exactly {}, whichever fits. \
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

/// The ids of `start` and every step it leads to, directly or not.
pub fn reachable<'g>(graph: &'g Graph, start: &'g Node) -> HashSet<&'g str> {
    let mut reached = HashSet::from([start.id.as_str()]);
    let mut queue = vec![start.id.as_str()];
    while let Some(id) = queue.pop() {
        for edge in graph.edges.iter().filter(|e| e.from == id) {
            if reached.insert(edge.to.as_str()) {
                queue.push(edge.to.as_str());
            }
        }
    }
    reached
}

/// What keeps a stored graph from running, in the order the steps are listed.
pub fn problems(graph: &Graph) -> Vec<String> {
    let Some(start) = graph.nodes.iter().find(|n| n.kind == Kind::Start) else {
        return vec!["The flow needs a Start step.".into()];
    };
    let reached = reachable(graph, start);
    let mut problems = Vec::new();
    // Steps Start cannot reach never run, so they are left alone.
    for node in graph.nodes.iter().filter(|n| reached.contains(n.id.as_str())) {
        let name = node.name();
        // A draft is optional, as the person writes the prompt; a pause may only wait.
        if node.kind == Kind::Prompt && !node.draft && !node.pause && node.text.trim().is_empty() {
            problems.push(format!("\"{name}\" has no prompt."));
        }
        if node.kind == Kind::Title && node.outputs.len() > 1 {
            problems.push(format!(
                "\"{name}\" has more than one output; a tab title step goes on one way once Claude names the tab, so give it a single output."
            ));
        }
        // Start must lead somewhere; other outputs with no next step end the flow there.
        if node.kind == Kind::Start && !graph.edges.iter().any(|e| e.from == node.id) {
            problems.push("Start is not connected to a step.".into());
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
            pause: false,
            draft: false,
            note: String::new(),
            x: 0.0,
            y: 0.0,
        }
    }

    /// A prompt step that only waits for the person (a pause).
    pub(crate) fn pause(id: &str, outputs: &[&str]) -> Node {
        Node { text: String::new(), pause: true, ..node(id, Kind::Prompt, outputs) }
    }

    /// A prompt step whose text is a draft for the person's own prompt (a manual prompt).
    pub(crate) fn draft(id: &str, outputs: &[&str]) -> Node {
        Node { text: String::new(), draft: true, ..node(id, Kind::Prompt, outputs) }
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
        let newer = serde_json::json!({ "version": GRAPH_VERSION + 1, "nodes": [], "edges": [] });
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
            if kind != Kind::Prompt {
                n.text.clear();
            }
            g
        };
        for max in [1, 3, 100] {
            check(&with_limit("p", Kind::Prompt, max)).unwrap();
        }
        rejected(&with_limit("p", Kind::Title, 2), "cannot have a run limit");
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
    fn a_graph_with_a_tab_title_step_is_a_newer_format() {
        let mut g = runnable();
        g.nodes.push(node("t", Kind::Title, &["done"]));
        assert_eq!(stored(g.clone()).version, 6);
        check(&stored(g)).unwrap();
        let read: Node = serde_json::from_value(serde_json::json!({ "id": "t", "kind": "title", "x": 0, "y": 0 })).unwrap();
        assert_eq!((read.kind, read.name()), (Kind::Title, "Tab title"));
    }

    #[test]
    fn a_tab_title_step_has_one_output_no_run_limit_and_needs_no_text() {
        let mut g = runnable();
        g.nodes.push(node("t", Kind::Title, &["done"]));
        g.edges[1].to = "t".into();
        assert!(problems(&g).is_empty(), "{:?}", problems(&g));
        g.nodes[3].outputs.push("again".into());
        assert!(problems(&g).iter().any(|p| p.contains("single output")), "{:?}", problems(&g));
        let mut g = runnable();
        let mut t = node("t", Kind::Title, &["done"]);
        t.max_runs = Some(2);
        g.nodes.push(t);
        rejected(&g, "cannot have a run limit");
    }

    #[test]
    fn a_tab_title_step_asks_claude_for_a_title_line_from_the_person_s_request() {
        let mut t = node("t", Kind::Title, &["done"]);
        let plain = prompt_text(&t);
        assert!(plain.contains("what I asked") && plain.contains("TITLE:"), "{plain}");
        assert!(!plain.contains("FLOW: done"), "{plain}");
        t.text = "Use the ticket number.".into();
        assert!(prompt_text(&t).contains("Use the ticket number."));
        assert!(resume_text(&t).contains("TITLE:"));
    }

    #[test]
    fn a_title_line_is_read_from_the_end_of_the_reply() {
        assert_eq!(parse_title("Sure.\n\nTITLE: Fix login bug"), Some("Fix login bug".into()));
        assert_eq!(parse_title("**Title:** `Fix login bug`\n```"), Some("Fix login bug".into()));
        assert_eq!(parse_title("title: \"Refactor  the\tparser\""), Some("Refactor the parser".into()));
        assert_eq!(parse_title(&format!("TITLE: {}", "a".repeat(100))).unwrap().chars().count(), MAX_TAB_TITLE);
        assert_eq!(parse_title("TITLE: Fix\u{200B}\u{1b}[2J it"), Some("Fix[2J it".into()));
        for none in ["Fix login bug", "TITLE:", "TITLE: \"\"", "TITLE: x\nmore", "FLOW: done"] {
            assert_eq!(parse_title(none), None, "{none:?}");
        }
    }

    #[test]
    fn a_graph_whose_prompts_pause_or_draft_is_a_newer_format() {
        for step in [pause("m", &["ok"]), draft("m", &["sent"]), Node { note: "Look".into(), ..pause("m", &["ok"]) }] {
            let mut g = runnable();
            g.nodes.push(step);
            assert_eq!(stored(g.clone()).version, 7);
            check(&stored(g)).unwrap();
        }
        assert_eq!(serde_json::to_value(pause("m", &["ok"])).unwrap()["pause"], true);
        let plain = serde_json::to_value(node("p", Kind::Prompt, &["ok"])).unwrap();
        for field in ["pause", "draft", "note"] {
            assert!(plain.get(field).is_none(), "{field} is left out when unused");
        }
    }

    #[test]
    fn only_prompt_steps_pause_draft_or_have_a_note() {
        for kind in [Kind::Start, Kind::Title, Kind::End] {
            let sets: [fn(&mut Node); 3] = [|n| n.pause = true, |n| n.draft = true, |n| n.note = "x".into()];
            for set in sets {
                let mut g = runnable();
                g.nodes.push(node("x", kind, if kind == Kind::Title { &["done"] } else { &[] }));
                if kind == Kind::Start {
                    g.nodes.retain(|n| n.id != "start");
                    g.edges.retain(|e| e.from != "start");
                    g.nodes.last_mut().unwrap().outputs = vec!["next".into()];
                }
                set(g.nodes.last_mut().unwrap());
                rejected(&g, "only prompt steps can");
            }
        }
    }

    #[test]
    fn older_pause_and_manual_prompt_kinds_load_as_prompt_steps() {
        let old: Node = serde_json::from_str(r#"{"id": "m", "kind": "manual", "text": "Try it", "outputs": ["ok"], "x": 0, "y": 0}"#).unwrap();
        assert_eq!((old.kind, old.pause, old.draft, old.text.as_str(), old.note.as_str()), (Kind::Prompt, true, false, "", "Try it"));
        assert!(old.only_waits() && !old.sends());
        let named: Node = serde_json::from_str(r#"{"id": "m", "kind": "pause", "outputs": ["ok"], "x": 0, "y": 0}"#).unwrap();
        assert_eq!((named.kind, named.pause, named.name()), (Kind::Prompt, true, "Pause"));
        let mp: Node = serde_json::from_str(r#"{"id": "m", "kind": "manual_prompt", "text": "Fix ", "outputs": ["sent"], "x": 0, "y": 0}"#).unwrap();
        assert_eq!((mp.kind, mp.pause, mp.draft, mp.text.as_str()), (Kind::Prompt, false, true, "Fix "));
        assert!(mp.drafts() && !mp.sends());
        assert_eq!(mp.name(), "Manual prompt");
        // Saved as a prompt step from then on.
        let saved = serde_json::to_value(&old).unwrap();
        assert_eq!((saved["kind"].as_str(), saved["pause"].as_bool(), saved["note"].as_str()), (Some("prompt"), Some(true), Some("Try it")));
        assert!(saved.get("text").is_some_and(|t| t == ""));
    }

    #[test]
    fn a_prompt_that_pauses_still_sends_its_text() {
        let p = Node { pause: true, ..node("p", Kind::Prompt, &["ok"]) };
        assert!(p.sends() && !p.only_waits() && p.pauses());
        assert!(!node("p", Kind::Prompt, &["ok"]).pauses());
        // A pause needs no prompt, nor a draft; a plain prompt step does.
        let mut g = runnable();
        g.nodes[1].text.clear();
        assert_eq!(problems(&g), vec!["\"P\" has no prompt."]);
        g.nodes[1].pause = true;
        assert!(problems(&g).is_empty());
        g.nodes[1].pause = false;
        g.nodes[1].draft = true;
        assert!(problems(&g).is_empty());
    }

    #[test]
    fn a_draft_step_with_several_outputs_tells_claude_to_pick_one() {
        let one = draft("m", &["sent"]);
        assert_eq!(draft_rule(&one), None);
        let two = draft("m", &["ok", "redo"]);
        let rule = draft_rule(&two).unwrap();
        assert!(rule.contains("\"M\"") && rule.contains("`FLOW: ok` or `FLOW: redo`"), "{rule}");
        assert_eq!(draft_rule(&node("p", Kind::Prompt, &["a", "b"])), None);
    }

    #[test]
    fn a_note_goes_with_a_pause() {
        let mut g = runnable();
        g.nodes[1].note = "Check the diff.".into();
        rejected(&g, "a note only shows at a pause");
        g.nodes[1].pause = true;
        check(&g).unwrap();
    }

    #[test]
    fn an_older_kind_with_a_newer_field_is_refused_not_dropped() {
        for json in [
            r#"{"id": "m", "kind": "manual", "draft": true, "outputs": ["ok"], "x": 0, "y": 0}"#,
            r#"{"id": "m", "kind": "pause", "note": "x", "outputs": ["ok"], "x": 0, "y": 0}"#,
            r#"{"id": "m", "kind": "manual_prompt", "pause": true, "outputs": ["ok"], "x": 0, "y": 0}"#,
        ] {
            let e = serde_json::from_str::<Node>(json).unwrap_err().to_string();
            assert!(e.contains("older step kind"), "{json}: {e}");
        }
    }

    #[test]
    fn a_step_s_mode_is_worked_out_in_one_place() {
        assert_eq!(node("p", Kind::Prompt, &["ok"]).mode(), Some(Mode::Send));
        assert_eq!(Node { pause: true, ..node("p", Kind::Prompt, &["ok"]) }.mode(), Some(Mode::Send));
        assert_eq!(pause("m", &["ok"]).mode(), Some(Mode::Wait));
        assert_eq!(draft("m", &["ok"]).mode(), Some(Mode::Draft));
        assert_eq!(Node { pause: true, ..draft("m", &["ok"]) }.mode(), Some(Mode::Draft));
        assert_eq!(node("t", Kind::Title, &["ok"]).mode(), None);
    }

    #[test]
    fn a_note_is_checked_like_the_text() {
        let mut g = runnable();
        g.nodes[1].pause = true;
        g.nodes[1].note = "x".repeat(20_001);
        rejected(&g, "too long");
        g.nodes[1].note = "look\u{200B}".into();
        rejected(&g, "hidden character");
    }

    #[test]
    fn base_info_is_short_enough_to_leave_room_for_the_prompt() {
        let mut g = runnable();
        g.nodes[0].text = "x".repeat(MAX_INFO);
        check(&g).unwrap();
        g.nodes[0].text.push('x');
        rejected(&g, "base info");
    }

    #[test]
    fn a_run_s_briefing_sets_out_the_rules_and_the_background() {
        let mut g = runnable();
        let text = briefing_text(&g, "Deploy");
        assert!(text.starts_with("Mischief of Agents runs the flow \"Deploy\" in this conversation."), "{text}");
        for rule in ["one at a time", "Do only what the current step asks", "`FLOW: <output>`", "`FLOW: attention`", "the person's own"] {
            assert!(text.contains(rule), "{rule}: {text}");
        }
        assert!(text.ends_with("Do not act on this message. Reply only with: Ready"), "{text}");
        assert!(!text.contains("Background"), "no base info, no background");
        g.nodes[0].text = "The app is in src/.\u{1b}\n\n".into();
        let text = briefing_text(&g, "Deploy");
        assert!(text.contains("Background for the flow:\n\nThe app is in src/.\n\nDo not act"), "{text}");
    }

    #[test]
    fn after_a_clear_the_step_s_prompt_briefs_claude_again() {
        let mut g = runnable();
        g.nodes[0].text = "The app is in src/.".into();
        let text = first_prompt_text(&g, "Deploy", &g.nodes[1]);
        assert!(text.starts_with("Mischief of Agents runs the flow \"Deploy\""), "{text}");
        assert!(text.contains("Background for the flow:\n\nThe app is in src/."), "{text}");
        assert!(text.ends_with(&format!("The current step:\n\n{}", prompt_text(&g.nodes[1]))), "{text}");
        assert!(!text.contains("Reply only with"), "{text}");
    }

    #[test]
    fn each_step_asks_for_that_step_only() {
        let g = runnable();
        assert!(prompt_text(&g.nodes[1]).contains("Do only this step"));
    }

    #[test]
    fn the_end_of_a_run_lets_claude_go_back_to_normal() {
        let text = closing_text("Deploy");
        assert!(text.contains("\"Deploy\" is finished"), "{text}");
        assert!(text.contains("no longer need to end your replies with a FLOW line"), "{text}");
        // It comes with the person's next prompt, which Claude answers.
        assert!(!text.contains("Reply only"), "{text}");
    }

    #[test]
    fn only_a_reply_of_ready_counts_as_ready() {
        for reply in ["Ready", "Ready.", "**Ready.**", " ready!\n", "`Ready`"] {
            assert!(says_ready(reply), "{reply:?}");
        }
        for reply in ["Not ready, which repo?", "Already on it.", "I'm ready", "Ready\nFLOW: attention", "Ready? Starting on the tests now.", ""] {
            assert!(!says_ready(reply), "{reply:?}");
        }
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
    fn pauses_and_drafts_may_go_on_several_ways() {
        for step in [pause("m", &["ok", "redo"]), draft("m", &["ok", "redo"])] {
            let mut g = runnable();
            g.nodes.push(step);
            g.edges.push(edge("p", "pass", "m"));
            g.edges.retain(|e| e.to != "e");
            g.nodes.retain(|n| n.id != "e");
            check(&g).unwrap();
            assert!(problems(&g).is_empty(), "{:?}", problems(&g));
        }
    }

    #[test]
    fn a_manual_prompt_s_prefill_leads_with_base_info_when_owed() {
        let mut g = runnable();
        g.nodes[0].text = " The app is in src/. ".into();
        let mut m = draft("m", &["ok"]);
        m.text = "Fix the \u{1b}bug ".into();
        // A trailing space stays, so what the person types does not run into the draft.
        assert_eq!(prefill_text(&g, &m, true), "The app is in src/.\n\nFix the bug ");
        assert_eq!(prefill_text(&g, &m, false), "Fix the bug ");
        m.text = "\n  Fix it\n\n".into();
        assert_eq!(prefill_text(&g, &m, false), "Fix it", "blank lines around it go");
        m.text.clear();
        assert_eq!(prefill_text(&g, &m, false), "");
    }

    #[test]
    fn steps_start_cannot_reach_are_ignored() {
        let mut g = runnable();
        g.nodes.push(node("x", Kind::End, &[]));
        g.nodes.push(Node { text: String::new(), ..node("q", Kind::Prompt, &["done"]) });
        g.nodes.push(Node { title: "M".into(), ..node("m", Kind::Title, &["ok", "redo"]) });
        g.edges.push(edge("q", "done", "m"));
        assert!(problems(&g).is_empty());
    }

    #[test]
    fn problems_name_the_steps_in_order() {
        let mut g = runnable();
        g.nodes[1].text = "  ".into();
        // An output with no next step ends the flow there, which is fine.
        g.edges.retain(|e| e.output != "fail");
        g.nodes.push(Node { title: String::new(), ..pause("m", &["done"]) });
        assert_eq!(problems(&g), vec!["\"P\" has no prompt."]);
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
