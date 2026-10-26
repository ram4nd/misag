//! `misag flow ...`: flows from the command line, for a person or for Claude, which
//! learns the file format from `misag flow help`. Works on the app's database
//! (`MISAG_DB` in misag's tabs), which the app re-reads, so it shows the change.

use crate::{claude, db, flow_file, flows};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The first argument that runs this instead of the app.
pub const FLOW_COMMAND: &str = "flow";
/// The app's identifiers, which name its data directory: a release's (tauri.conf.json)
/// and a dev build's (tauri.dev.conf.json, which `pnpm app:dev` uses).
const RELEASE_ID: &str = "io.github.ram4nd.misag";
const DEV_ID: &str = "io.github.ram4nd.misag.dev";
/// Those this build's app may run under. A debug build can be started with the dev
/// config (`pnpm app:dev`) or without it (`pnpm tauri dev`, which keeps the release
/// identifier); `in_build` keeps both apart from an installed misag's data.
const APP_IDS: &[&str] = if cfg!(debug_assertions) { &[DEV_ID, RELEASE_ID] } else { &[RELEASE_ID] };

pub const APP_HELP: &str = "\
Mischief of Agents (misag): tabs of Claude Code and terminals, and flows of prompts.

Usage:
  misag              Start the app
  misag flow ...     Import, export and list flows (see misag flow help)
  misag --help       Show this help
";

const FLOW_HELP: &str = r#"misag flow: import, export and list Mischief of Agents flows.

Usage:
  misag flow list                       List the flows: id, name, and (archived)
  misag flow export <id|name> [FILE] [--force]
                                        Write a flow as JSON to FILE, or to stdout;
                                        an existing FILE is replaced only with --force
  misag flow import <FILE|-> [--name NAME]
                                        Add a flow from a JSON file (- reads stdin),
                                        after the others; --name overrides its name
  misag flow help                       Show this help

  --db PATH   The database to use. By default $MISAG_DB (set in misag's tabs,
              where `misag` is the misag that runs them), else misag.db in the
              app's data directory: $XDG_DATA_HOME/io.github.ram4nd.misag, by
              default ~/.local/share/io.github.ram4nd.misag.

The app shows an added flow within seconds. A flow that is not ready to run yet
is still added; import lists what to fix, and its editor in the app shows it too.

A flow file:

{
  "misagFlow": 1,
  "name": "Fix and review",
  "graph": {
    "nodes": [
      {"id": "start", "kind": "start"},
      {"id": "fix", "kind": "prompt", "title": "Fix",
       "text": "Fix the failing tests.", "outputs": ["done"]},
      {"id": "review", "kind": "prompt", "title": "Review", "maxRuns": 3,
       "text": "Review the change for bugs.", "outputs": ["ok", "changes"]},
      {"id": "try", "kind": "manual", "title": "Try it",
       "text": "Try the change in the app.", "outputs": ["works", "broken"]}
    ],
    "edges": [
      {"from": "start", "output": "next", "to": "fix"},
      {"from": "fix", "output": "done", "to": "review"},
      {"from": "review", "output": "changes", "to": "fix"},
      {"from": "review", "output": "ok", "to": "try"},
      {"from": "try", "output": "broken", "to": "fix"}
    ]
  }
}

A run starts at Start and follows one edge out of each step it finishes.

Steps ("nodes"):
  id        1 to 40 letters, digits, - or _, unique in the flow; edges use it.
  kind      "start": where a run begins. Exactly one; its one output is "next".
            "prompt": sends "text" to Claude in the run's tab. misag adds an
              instruction to end the reply with "FLOW: <output>", which picks
              the output, so the text need not mention the outputs.
            "manual": waits for the person, who picks an output in the app;
              "text" says what to do.
            "end": from flows made by older versions; it ends the run and has
              no outputs. Not needed: an output that leads nowhere ends it.
  title     The step's name on the canvas (up to 200 characters).
  text      The prompt, or what the person does (up to 20000 characters).
            Optional on Start: base info, such as where things are, sent
            ahead of the run's first prompt (and again after a /clear).
  outputs   How the step can end: 1 to 8 short lowercase words (a-z, 0-9, -
            and _, starting with a letter or digit, up to 24 characters).
            "attention" is reserved: Claude ends with it to ask for the person.
  maxRuns   Optional, prompt steps only: how many times a run may take the
            step (1 to 100), for loops such as review and fix; then the run
            waits for the person.
  clear     Optional, Start only: true begins each run by typing /clear, so
            Claude starts the flow in a new conversation.
  x, y      Optional position on the canvas: give both or neither. Steps
            without one are laid out in columns from Start.

Edges lead from a step's output to the next step: at most one per output, and
none back to Start. An output with no edge ends the run there. A flow has at
most 200 steps. Other fields are refused, so a misspelt one is not lost, and so
are characters that do not show, such as zero-width spaces and bidi controls.
"#;

/// Runs `misag flow <args>`, returning the exit code: 0 done, 1 failed, 2 a wrong
/// command line.
pub fn run(args: Vec<OsString>) -> i32 {
    let mut stdout = std::io::stdout().lock();
    match command(args, &mut std::io::stdin().lock(), &mut stdout) {
        Ok(()) => 0,
        Err(Failure::Usage(why)) => {
            eprintln!("misag: {why}; see misag flow help");
            2
        }
        Err(Failure::Failed(why)) => {
            eprintln!("misag: {why}");
            1
        }
    }
}

enum Failure {
    Usage(String),
    Failed(String),
}

impl From<String> for Failure {
    fn from(why: String) -> Self {
        Failure::Failed(why)
    }
}

/// The command line after `misag flow`: its words (command, flow, file), and options.
#[derive(Default)]
struct Line {
    words: Vec<OsString>,
    db: Option<PathBuf>,
    name: Option<String>,
    force: bool,
    help: bool,
}

fn parse(args: Vec<OsString>) -> Result<Line, Failure> {
    let mut line = Line::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => line.help = true,
            Some("--force") => line.force = true,
            Some(option @ ("--db" | "--name")) => {
                let value = args.next().ok_or_else(|| Failure::Usage(format!("{option} needs a value")))?;
                let taken = if option == "--db" { line.db.is_some() } else { line.name.is_some() };
                if taken {
                    return Err(Failure::Usage(format!("{option} is given twice")));
                }
                if option == "--db" {
                    line.db = Some(value.into());
                } else {
                    line.name = Some(value.into_string().map_err(|_| Failure::Usage("--name is not valid UTF-8".into()))?);
                }
            }
            _ => line.words.push(arg),
        }
    }
    Ok(line)
}

fn command(args: Vec<OsString>, stdin: &mut impl Read, out: &mut impl Write) -> Result<(), Failure> {
    let line = parse(args)?;
    let written = |r: std::io::Result<()>| r.map_err(|e| Failure::Failed(format!("cannot write the output: {e}")));
    let word = |i: usize| -> Result<Option<&str>, Failure> {
        line.words.get(i).map(|w| w.to_str().ok_or_else(|| Failure::Usage(format!("{w:?} is not valid UTF-8")))).transpose()
    };
    let command = word(0)?;
    if line.help || matches!(command, None | Some("help")) {
        return written(out.write_all(FLOW_HELP.as_bytes()));
    }
    let command = command.unwrap_or_default();
    if line.name.is_some() && command != "import" {
        return Err(Failure::Usage("--name is only for import".into()));
    }
    if line.force && !(command == "export" && line.words.len() == 3) {
        return Err(Failure::Usage("--force is only for export to a FILE".into()));
    }
    match (command, line.words.len()) {
        ("list", 1) => {
            for flow in open(line.db)?.list_flows()? {
                let archived = if flow.archived { "\t(archived)" } else { "" };
                let unconfirmed = if flow.unconfirmed { "\t(not yet confirmed)" } else { "" };
                written(writeln!(out, "{}\t{}{archived}{unconfirmed}", flow.id, shown(&flow.name)))?;
            }
            Ok(())
        }
        ("export", 2 | 3) => {
            let which = word(1)?.unwrap_or_default();
            let db = open(line.db)?;
            let (flow, graph) = db.load_flow(find(&db, which)?)?;
            let file = flow_file::write(&flow.name, &graph) + "\n";
            let Some(path) = line.words.get(2).map(Path::new) else { return written(out.write_all(file.as_bytes())) };
            if path.exists() && !line.force {
                return Err(Failure::Failed(format!("{} already exists; add --force to replace it", path.display())));
            }
            std::fs::write(path, file).map_err(|e| Failure::Failed(format!("cannot write {}: {e}", path.display())))
        }
        ("import", 2) => {
            let from = &line.words[1];
            let mut text = String::new();
            if from == "-" {
                stdin.read_to_string(&mut text).map_err(|e| format!("cannot read the flow from stdin: {e}"))?;
            } else {
                let from = Path::new(from);
                text = std::fs::read_to_string(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
            }
            let (file_name, graph) = flow_file::parse(&text)?;
            let db = open(line.db)?;
            let name = line.name.as_deref().unwrap_or(&file_name);
            // From one of misag's tabs, a program (such as Claude) may be adding it.
            let in_tab = std::env::var_os(claude::TAB_ENV).is_some_and(|t| !t.is_empty());
            let flow = if in_tab { db.import_unconfirmed_flow(name, &graph)? } else { db.import_flow(name, &graph)? };
            let mut report = format!("Added \"{}\" as flow {}.\n", shown(&flow.name), flow.id);
            if flow.unconfirmed {
                report.push_str("It was added from a misag tab, so it runs only once you confirm it in the app (its ▶ asks).\n");
            }
            let problems = flows::problems(&graph);
            if !problems.is_empty() {
                report.push_str("Before it can run:\n");
                for problem in problems {
                    report.push_str(&format!("- {}\n", shown(&problem)));
                }
            }
            // Stored already: a failure now must not read as one to retry, which would add it twice.
            out.write_all(report.as_bytes()).map_err(|e| {
                Failure::Failed(format!("added \"{}\" as flow {}, but could not print that: {e}", shown(&flow.name), flow.id))
            })
        }
        ("list" | "export" | "import", _) => Err(Failure::Usage(format!("wrong arguments for misag flow {command}"))),
        _ => Err(Failure::Usage(format!("misag flow has no command {command:?}"))),
    }
}

/// `text` safe to print to a terminal: control and hidden characters (which a name
/// stored before they were refused may hold) shown as U+FFFD.
fn shown(text: &str) -> String {
    text.chars().map(|c| if c.is_control() || flows::hidden(c) { '\u{FFFD}' } else { c }).collect()
}

/// The app's database: `path`, else `MISAG_DB`, else the one in the app's data
/// directory. It must exist, at this binary's schema: a new one would hold no flows
/// the app uses, and only the app migrates (this may be a newer misag than the one
/// running, just after an upgrade).
fn open(path: Option<PathBuf>) -> Result<db::Db, String> {
    let path = match path.or_else(|| std::env::var_os(claude::DB_ENV).filter(|p| !p.is_empty()).map(PathBuf::from)) {
        Some(path) => path,
        None => data_dir_db()?,
    };
    if !path.exists() {
        return Err(format!(
            "there is no Mischief of Agents database at {}; start the app once, or give it with --db",
            path.display()
        ));
    }
    db::Db::open_existing(path).map_err(|e| {
        format!("cannot use the database ({e}); use the misag that runs the app, or restart the app after an upgrade")
    })
}

/// The directory Tauri puts app data directories in (its `app_data_dir` is this
/// joined with the identifier), as the `dirs` crate it uses finds it.
fn data_home() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support"))
    } else if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share")))
    }
}

/// Where the app keeps its database: in its data directory, under whichever of
/// `APP_IDS` the app runs as (see `pick_db`).
fn data_dir_db() -> Result<PathBuf, String> {
    let home = data_home().ok_or("cannot find the app's data directory (HOME is not set); give the database with --db")?;
    pick_db(APP_IDS.iter().map(|id| crate::in_build(home.join(id)).join(db::FILE)).collect())
}

/// The one of `candidates` that exists, else the first (which `open` then says is
/// missing); more than one is an error, as the app may run as either.
fn pick_db(candidates: Vec<PathBuf>) -> Result<PathBuf, String> {
    let found: Vec<&PathBuf> = candidates.iter().filter(|p| p.exists()).collect();
    match found[..] {
        [one] => Ok(one.clone()),
        [] => candidates.into_iter().next().ok_or_else(|| "this build has no app identifier".to_owned()),
        _ => Err(format!(
            "there is a database for each way this build can run ({}); give the one to use with --db",
            found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The flow `which` names: its id, else its name, which must be one flow's.
fn find(db: &db::Db, which: &str) -> Result<i64, String> {
    let flows = db.list_flows()?;
    if let Ok(id) = which.parse::<i64>()
        && flows.iter().any(|f| f.id == id)
    {
        return Ok(id);
    }
    let named: Vec<_> = flows.iter().filter(|f| f.name == which.trim()).collect();
    match named[..] {
        [flow] => Ok(flow.id),
        [] => Err(format!("there is no flow \"{}\"; see misag flow list", shown(which))),
        _ => Err(format!("more than one flow is named \"{}\"; give its id (see misag flow list)", shown(which))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_directory_is_named_by_the_app_s_identifiers() {
        let release: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let dev: serde_json::Value = serde_json::from_str(include_str!("../tauri.dev.conf.json")).unwrap();
        assert_eq!((&release["identifier"], &dev["identifier"]), (&RELEASE_ID.into(), &DEV_ID.into()));
        assert_eq!(APP_IDS.contains(&DEV_ID), cfg!(debug_assertions));
    }

    #[test]
    fn the_database_is_the_one_that_exists_under_either_identifier() {
        let dir = crate::test_support::temp_dir("cli-pick-db");
        let (dev, release) = (dir.join("dev.db"), dir.join("release.db"));
        let both = || vec![dev.clone(), release.clone()];
        assert_eq!(pick_db(both()).unwrap(), dev, "neither: the first, which open says is missing");
        std::fs::write(&release, "").unwrap();
        assert_eq!(pick_db(both()).unwrap(), release);
        std::fs::write(&dev, "").unwrap();
        assert!(pick_db(both()).unwrap_err().contains("--db"));
    }

    #[test]
    fn the_help_s_example_is_a_flow_that_imports_ready_to_run() {
        let start = FLOW_HELP.find("{\n  \"misagFlow\"").unwrap();
        let end = start + FLOW_HELP[start..].find("\n}\n").unwrap() + 2;
        let (name, graph) = flow_file::parse(&FLOW_HELP[start..end]).unwrap();
        assert_eq!(name, "Fix and review");
        assert_eq!(flows::problems(&graph), Vec::<String>::new());
    }
}
