//! Golden routing set.
//!
//! Router behaviour used to be verified by ad-hoc `jarvis ask` sessions and
//! by reading the diff. That is how "the model dumped raw HTML" and "the
//! answer was hallucinated" reached users: nothing asserted the whole table
//! at once.
//!
//! Each row is an input and the route it must produce. When a row fails, the
//! failure message shows the input, the expected route and the actual route,
//! so a regression is diagnosable without running the daemon.

use jarvis_core::orchestrator::route_label;
use jarvis_core::router::route;

#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Plain conversation, no tools.
    Direct,
    /// The model picks from the tools the planner offers.
    Model,
    /// One deterministic tool call.
    Tool(&'static str),
}

impl std::fmt::Display for Expect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expect::Direct => write!(f, "Direct"),
            Expect::Model => write!(f, "Model"),
            Expect::Tool(name) => write!(f, "Tool({})", name),
        }
    }
}

const GOLDEN: &[(&str, Expect)] = &[
    // ---- conversation: no tool should be involved -------------------------
    ("hello there", Expect::Direct),
    ("who are you?", Expect::Direct),
    ("tell me a joke", Expect::Direct),
    ("what is 2+2", Expect::Direct),
    ("thanks, that helped", Expect::Direct),
    ("write a poem about the sea", Expect::Direct),
    ("explain ownership in rust", Expect::Direct),
    // ---- filesystem ------------------------------------------------------
    ("list contents of /tmp", Expect::Tool("filesystem.list_dir")),
    ("list the files", Expect::Tool("filesystem.list_dir")),
    ("read /etc/hostname", Expect::Tool("filesystem.read")),
    (
        "what is inside /etc/hostname",
        Expect::Tool("filesystem.read"),
    ),
    ("/list /tmp", Expect::Tool("filesystem.list_dir")),
    ("/read /etc/hostname", Expect::Tool("filesystem.read")),
    (
        "find files containing TODO in /mnt/storage",
        Expect::Tool("filesystem.search"),
    ),
    // ---- shell / system --------------------------------------------------
    ("run echo test", Expect::Tool("shell.run")),
    ("/run ls -la", Expect::Tool("shell.run")),
    ("what OS am I running on?", Expect::Tool("system.info")),
    // ---- web / external facts -------------------------------------------
    ("what is the latest news about rust", Expect::Model),
    ("fetch https://example.com", Expect::Tool("mcp.fetch.fetch")),
    (
        "What are the millennium prize problems and which have been solved?",
        Expect::Model,
    ),
    // Recency and live status need a source, whatever the subject is. These
    // rows exist because the rule used to be a list of topic names; adding a
    // new subject should not require a router edit.
    ("who won the 2026 world cup", Expect::Model),
    ("which films won awards in 2025", Expect::Model),
    ("has the riemann hypothesis been solved", Expect::Model),
    ("what is the current version of postgres", Expect::Model),
    // A stable historical fact with no recency marker does not need one.
    ("what are the millennium prize problems", Expect::Direct),
    ("when did the second world war end", Expect::Direct),
    // ---- ambiguous machine actions --------------------------------------
    ("create a file called notes.md", Expect::Model),
    // A path does not make a change into a read. These listed the directory
    // and presented the listing as the answer; found by the eval suite.
    (
        "create a file called notes.md in /tmp containing hello",
        Expect::Model,
    ),
    ("write hello to /tmp/a.txt", Expect::Model),
    ("delete /tmp/old.log", Expect::Model),
    ("rename /tmp/a.txt to /tmp/b.txt", Expect::Model),
    ("check the git status of this repo", Expect::Model),
];

#[test]
fn the_golden_set_holds() {
    let mut failures = Vec::new();
    for (input, expected) in GOLDEN {
        let got = route_label(&route(input));
        if got != expected.to_string() {
            failures.push(format!(
                "  input:    {:?}\n  expected: {}\n  actual:   {}",
                input, expected, got
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} routing case(s) changed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The router and the planner must agree about what "needs the web" means.
/// When they disagree, the router sends a request to the model and the
/// planner withholds the only tool that could answer it.
#[test]
fn web_intent_is_shared_not_duplicated() {
    let web = [
        "what is the latest news about rust",
        "look up the weather in london",
        "fetch https://example.com",
        "What are the millennium prize problems and which have been solved?",
    ];
    for input in web {
        assert!(
            jarvis_core::router::needs_web(input) || jarvis_core::router::needs_source(input),
            "{:?} should be recognised as needing an external source",
            input
        );
    }

    let not_web = [
        "hello there",
        "what is 2+2",
        "list contents of /tmp",
        "read /etc/hostname",
    ];
    for input in not_web {
        assert!(
            !jarvis_core::router::needs_web(input) && !jarvis_core::router::needs_source(input),
            "{:?} should not be routed to the web",
            input
        );
    }
}
