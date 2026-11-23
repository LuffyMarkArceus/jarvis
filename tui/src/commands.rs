//! Every `/` command, once: `/help` and the `/` menu are both built from
//! this table (Milestone 6.7a).

pub struct CommandInfo {
    pub name: &'static str,
    pub args: &'static str,
    pub about: &'static str,
}

const fn c(name: &'static str, args: &'static str, about: &'static str) -> CommandInfo {
    CommandInfo { name, args, about }
}

pub const COMMANDS: &[CommandInfo] = &[
    c("/new", "", "start a new conversation"),
    c("/sessions", "", "list conversations and switch"),
    c("/switch", "<id>", "switch to a conversation"),
    c("/delete", "<id>", "delete a conversation"),
    c("/prune", "", "remove empty conversations"),
    c("/session", "", "show this window's conversation id"),
    c(
        "/model",
        "[name] [msg]",
        "pick a model; switch by name, then send msg",
    ),
    c("/remote", "", "answer with the remote model"),
    c("/local", "", "answer with the local model"),
    c(
        "/mode",
        "[autonomy]",
        "show the mode, or plan / interactive / autopilot",
    ),
    c(
        "/approval",
        "<level>",
        "manual / assisted / allow-all: what needs a yes",
    ),
    c("/sandbox", "on|off", "where commands run"),
    c(
        "/approve",
        "[all|none|n]",
        "list or run what Autopilot left for you",
    ),
    c("/go", "[auto]", "carry out the plan above"),
    c(
        "/changes",
        "",
        "what the last turn changed, with r to revert (Ctrl+D)",
    ),
    c(
        "/steer",
        "<message>",
        "add to the running turn; the model sees it next step",
    ),
    c("/queue", "", "what is waiting to be sent after this turn"),
    c("/resume", "", "send a held queue"),
    c(
        "/undo",
        "[list|id]",
        "undo Jarvis's last file change (asks first)",
    ),
    c(
        "/trust",
        "[clear]",
        "what this conversation trusts; forget it",
    ),
    c("/recall", "<query>", "search memories across conversations"),
    c("/timing", "", "where the last turn's time went"),
    c("/bad", "[note]", "flag the last answer for review"),
    c("/good", "[note]", "mark the last answer as good"),
    c("/status", "", "daemon information"),
    c("/tools", "", "the tools Jarvis can use"),
    c("/clear", "", "clear the screen"),
    c("/help", "", "keys and commands"),
    c("/quit", "", "leave"),
];

/// The commands for what was typed (`/mo`): those starting with it first,
/// then those containing it.
pub fn matching(typed: &str) -> Vec<&'static CommandInfo> {
    let needle = typed.trim_start_matches('/');
    let prefix = COMMANDS.iter().filter(|c| c.name[1..].starts_with(needle));
    let inside = COMMANDS
        .iter()
        .filter(|c| !c.name[1..].starts_with(needle) && c.name[1..].contains(needle));
    prefix.chain(inside).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{parse_command, Command};

    #[test]
    fn every_listed_command_parses() {
        for info in COMMANDS {
            let parsed = parse_command(info.name);
            assert!(
                parsed.is_some() && !matches!(parsed, Some(Command::Unknown(_))),
                "{} is listed but does not parse",
                info.name
            );
        }
    }

    #[test]
    fn every_command_is_listed() {
        // Review of 6.7a: /resume, /queue and /steer parsed but were missing
        // from /help and the menu.
        for name in [
            "/help",
            "/new",
            "/sessions",
            "/switch",
            "/delete",
            "/prune",
            "/recall",
            "/status",
            "/remote",
            "/local",
            "/model",
            "/timing",
            "/bad",
            "/good",
            "/undo",
            "/trust",
            "/tools",
            "/session",
            "/clear",
            "/quit",
            "/mode",
            "/approval",
            "/sandbox",
            "/approve",
            "/go",
            "/resume",
            "/queue",
            "/steer",
        ] {
            assert!(
                COMMANDS.iter().any(|c| c.name == name),
                "{name} is not listed"
            );
        }
    }

    #[test]
    fn matching_puts_prefix_first() {
        let names: Vec<&str> = matching("/mo").iter().map(|c| c.name).collect();
        assert_eq!(&names[..2], &["/model", "/mode"]);
        let names: Vec<&str> = matching("/ap").iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/approval", "/approve"]);
        let contains: Vec<&str> = matching("/ss").iter().map(|c| c.name).collect();
        assert_eq!(
            contains,
            vec!["/sessions", "/session"],
            "contains, not prefix"
        );
        assert_eq!(matching("/").len(), COMMANDS.len());
    }
}
