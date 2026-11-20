//! Modes (Milestone 6.6): who drives (autonomy), which actions need a yes
//! (approval), and where commands run (the sandbox), per conversation.
//!
//! `decide` is the whole policy, and it is pure: the protections first -
//! deny, nothing outside the sandbox after untrusted content or when the
//! owner switched outside runs off, Plan changes nothing - and only then
//! the approval mode reshaping the owner's rule. No mode can reach a
//! protection, because the protections are decided before the mode is.

use crate::config::ModesConfig;
use crate::permissions::PermissionAction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    /// Look and propose; change nothing.
    Plan,
    /// Check in along the way.
    #[default]
    Interactive,
    /// Carry the task to the end without a prompt.
    Autopilot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// The owner's rules as written.
    #[default]
    Manual,
    /// Run what can be undone, or only reads in a read-only sandbox.
    Assisted,
    /// Run whatever the rules would ask about.
    AllowAll,
}

/// A conversation's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    pub autonomy: Autonomy,
    pub approval: Approval,
    /// Commands run in the sandbox.
    pub sandbox: bool,
}

impl Default for Mode {
    fn default() -> Self {
        Mode {
            autonomy: Autonomy::Interactive,
            approval: Approval::Manual,
            sandbox: true,
        }
    }
}

impl Mode {
    pub fn from_config(config: &ModesConfig) -> Mode {
        Mode {
            autonomy: config.autonomy,
            approval: config.approval,
            sandbox: config.sandbox,
        }
    }

    /// "Interactive · Manual · Sandbox on", for the header and `jarvis mode`.
    pub fn label(&self) -> String {
        let autonomy = match self.autonomy {
            Autonomy::Plan => "Plan",
            Autonomy::Interactive => "Interactive",
            Autonomy::Autopilot => "Autopilot",
        };
        let approval = match self.approval {
            Approval::Manual => "Manual",
            Approval::Assisted => "Assisted",
            Approval::AllowAll => "Allow all",
        };
        let sandbox = if self.sandbox { "on" } else { "off" };
        format!("{autonomy} · {approval} · Sandbox {sandbox}")
    }

    /// This mode, no wider than `ceiling` (a Phase 7 job's): approval and
    /// the sandbox are capped; autonomy is not, since an unattended job is
    /// Autopilot whatever it is capped to.
    pub fn within(&self, ceiling: &Mode) -> Mode {
        Mode {
            autonomy: self.autonomy,
            approval: self.approval.min(ceiling.approval),
            sandbox: self.sandbox || ceiling.sandbox,
        }
    }
}

/// What a call would do, as far as the policy cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Looks only: reads, search, process and system info, the web.
    None,
    /// Changes a file, with an undo copy that will survive the run.
    Undoable,
    /// Changes a file with no lasting undo copy.
    NotUndoable,
    /// A command in the sandbox; `writes` is whether it says it changes files.
    Shell { writes: bool },
    /// Runs outside the sandbox: a command, or an app launch.
    Outside,
}

/// One call, as the policy sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    /// The owner's rule for it.
    pub rule: PermissionAction,
    pub change: Change,
    /// The turn has read content the owner has not vouched for.
    pub untrusted: bool,
    /// `[sandbox] allow_outside`.
    pub allow_outside: bool,
    /// The sandbox can run here (bubblewrap works).
    pub sandbox_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run,
    /// Run in the sandbox with every writable folder read-only.
    RunReadOnly,
    Ask,
    /// Not now: kept for `/approve` (Autopilot does not prompt).
    Defer(String),
    Refuse(String),
}

pub const PLAN_REFUSAL: &str = "Plan mode changes nothing; /go to carry out the plan.";

/// The decision for `call` under `mode`.
pub fn decide(mode: &Mode, call: &Call) -> Decision {
    // 1-3: the protections, before any mode.
    if call.rule == PermissionAction::Deny {
        return Decision::Refuse("denied by your permission rules".into());
    }
    if call.change == Change::Outside && call.untrusted {
        return Decision::Refuse(
            "nothing runs outside the sandbox after content you have not marked as trusted".into(),
        );
    }
    if call.change == Change::Outside && !call.allow_outside {
        return Decision::Refuse(
            "running outside the sandbox is switched off ([sandbox] allow_outside)".into(),
        );
    }
    let read_only = call.sandbox_ready && mode.sandbox;
    let looks = Change::Shell { writes: false };
    // 4: Plan changes nothing; its shell only looks, read-only.
    if mode.autonomy == Autonomy::Plan {
        return match call.change {
            Change::None => verdict(mode, call.rule),
            c if c == looks && read_only => Decision::RunReadOnly,
            _ => Decision::Refuse(PLAN_REFUSAL.into()),
        };
    }
    // Outside the sandbox asks even where the rule allows (as before 6.6);
    // only Allow all runs it unasked.
    let rule = if call.change == Change::Outside && mode.approval != Approval::AllowAll {
        PermissionAction::Ask
    } else {
        call.rule
    };
    let call = &Call { rule, ..*call };
    // 6: after untrusted content, an action asks whatever the approval.
    if call.untrusted && call.change != Change::None {
        return verdict(mode, PermissionAction::Ask);
    }
    // 5: the approval mode reshapes the rule's "ask".
    if call.rule == PermissionAction::Ask {
        match mode.approval {
            Approval::Manual => {}
            Approval::Assisted => match call.change {
                Change::None | Change::Undoable => return Decision::Run,
                c if c == looks && read_only => return Decision::RunReadOnly,
                _ => {}
            },
            Approval::AllowAll => return Decision::Run,
        }
    }
    verdict(mode, call.rule)
}

/// A rule's verdict as it stands; 7: Autopilot does not ask, it defers.
fn verdict(mode: &Mode, rule: PermissionAction) -> Decision {
    match rule {
        PermissionAction::Allow => Decision::Run,
        PermissionAction::Deny => Decision::Refuse("denied by your permission rules".into()),
        PermissionAction::Ask if mode.autonomy == Autonomy::Autopilot => {
            Decision::Defer("needs your yes".into())
        }
        PermissionAction::Ask => Decision::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionAction::{Allow, Ask, Deny};

    const CHANGES: [Change; 6] = [
        Change::None,
        Change::Undoable,
        Change::NotUndoable,
        Change::Shell { writes: false },
        Change::Shell { writes: true },
        Change::Outside,
    ];

    fn modes() -> Vec<Mode> {
        let mut all = Vec::new();
        for autonomy in [Autonomy::Plan, Autonomy::Interactive, Autonomy::Autopilot] {
            for approval in [Approval::Manual, Approval::Assisted, Approval::AllowAll] {
                for sandbox in [true, false] {
                    all.push(Mode {
                        autonomy,
                        approval,
                        sandbox,
                    });
                }
            }
        }
        all
    }

    fn call(rule: PermissionAction, change: Change) -> Call {
        Call {
            rule,
            change,
            untrusted: false,
            allow_outside: true,
            sandbox_ready: true,
        }
    }

    fn mode(autonomy: Autonomy, approval: Approval) -> Mode {
        Mode {
            autonomy,
            approval,
            sandbox: true,
        }
    }

    fn is_refuse(d: &Decision) -> bool {
        matches!(d, Decision::Refuse(_))
    }

    #[test]
    fn protections_hold_in_every_mode() {
        for m in modes() {
            for change in CHANGES {
                for untrusted in [false, true] {
                    let c = Call {
                        untrusted,
                        ..call(Deny, change)
                    };
                    assert!(is_refuse(&decide(&m, &c)), "deny: {m:?} {c:?}");
                }
                for rule in [Allow, Ask] {
                    let outside_untrusted = Call {
                        untrusted: true,
                        ..call(rule, Change::Outside)
                    };
                    assert!(
                        is_refuse(&decide(&m, &outside_untrusted)),
                        "outside after untrusted: {m:?} {rule:?}"
                    );
                    let switched_off = Call {
                        allow_outside: false,
                        ..call(rule, Change::Outside)
                    };
                    assert!(
                        is_refuse(&decide(&m, &switched_off)),
                        "allow_outside off: {m:?}"
                    );
                    let page_write = Call {
                        untrusted: true,
                        ..call(rule, Change::Undoable)
                    };
                    let d = decide(&m, &page_write);
                    assert!(
                        !matches!(d, Decision::Run | Decision::RunReadOnly),
                        "an action after untrusted content ran unasked: {m:?} {rule:?} {d:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn plan_refuses_every_change_and_runs_reads() {
        for approval in [Approval::Manual, Approval::Assisted, Approval::AllowAll] {
            let m = mode(Autonomy::Plan, approval);
            for change in [
                Change::Undoable,
                Change::NotUndoable,
                Change::Shell { writes: true },
                Change::Outside,
            ] {
                match decide(&m, &call(Allow, change)) {
                    Decision::Refuse(why) => assert_eq!(why, PLAN_REFUSAL),
                    d => panic!("plan ran a change: {approval:?} {change:?} {d:?}"),
                }
            }
            assert_eq!(decide(&m, &call(Allow, Change::None)), Decision::Run);
            assert_eq!(
                decide(&m, &call(Ask, Change::Shell { writes: false })),
                Decision::RunReadOnly
            );
            assert_eq!(
                decide(&m, &call(Allow, Change::Shell { writes: false })),
                Decision::RunReadOnly,
                "plan's shell is read-only even where the rule allows"
            );
        }
    }

    #[test]
    fn assisted_runs_what_can_be_undone_or_only_reads() {
        let m = mode(Autonomy::Interactive, Approval::Assisted);
        assert_eq!(decide(&m, &call(Ask, Change::Undoable)), Decision::Run);
        assert_eq!(decide(&m, &call(Ask, Change::NotUndoable)), Decision::Ask);
        assert_eq!(
            decide(&m, &call(Ask, Change::Shell { writes: false })),
            Decision::RunReadOnly
        );
        assert_eq!(
            decide(&m, &call(Ask, Change::Shell { writes: true })),
            Decision::Ask
        );
        assert_eq!(decide(&m, &call(Ask, Change::Outside)), Decision::Ask);
        let no_bwrap = Call {
            sandbox_ready: false,
            ..call(Ask, Change::Shell { writes: false })
        };
        assert_eq!(decide(&m, &no_bwrap), Decision::Ask);
        let sandbox_off = Mode {
            sandbox: false,
            ..m
        };
        assert_eq!(
            decide(&sandbox_off, &call(Ask, Change::Shell { writes: false })),
            Decision::Ask,
            "with the sandbox off there is no read-only run"
        );
    }

    #[test]
    fn allow_all_runs_asks_but_not_after_untrusted() {
        let m = mode(Autonomy::Interactive, Approval::AllowAll);
        for change in CHANGES {
            assert_eq!(decide(&m, &call(Ask, change)), Decision::Run, "{change:?}");
            let untrusted = Call {
                untrusted: true,
                ..call(Ask, change)
            };
            if change != Change::Outside && change != Change::None {
                assert_eq!(decide(&m, &untrusted), Decision::Ask, "{change:?}");
            }
        }
    }

    #[test]
    fn manual_keeps_rules_as_written() {
        let m = Mode::default();
        for change in CHANGES {
            assert_eq!(decide(&m, &call(Ask, change)), Decision::Ask, "{change:?}");
            if change != Change::Outside {
                assert_eq!(
                    decide(&m, &call(Allow, change)),
                    Decision::Run,
                    "{change:?}"
                );
            }
        }
    }

    #[test]
    fn outside_the_sandbox_asks_unless_allow_all() {
        // As before 6.6: running outside asks even where the rule allows.
        for approval in [Approval::Manual, Approval::Assisted] {
            let m = mode(Autonomy::Interactive, approval);
            assert_eq!(
                decide(&m, &call(Allow, Change::Outside)),
                Decision::Ask,
                "{approval:?}"
            );
        }
        let all = mode(Autonomy::Interactive, Approval::AllowAll);
        assert_eq!(decide(&all, &call(Allow, Change::Outside)), Decision::Run);
        let autopilot = mode(Autonomy::Autopilot, Approval::Assisted);
        assert!(matches!(
            decide(&autopilot, &call(Allow, Change::Outside)),
            Decision::Defer(_)
        ));
    }

    #[test]
    fn autopilot_defers_what_would_ask() {
        let manual = mode(Autonomy::Autopilot, Approval::Manual);
        assert!(matches!(
            decide(&manual, &call(Ask, Change::Undoable)),
            Decision::Defer(_)
        ));
        let all = mode(Autonomy::Autopilot, Approval::AllowAll);
        assert_eq!(decide(&all, &call(Ask, Change::Outside)), Decision::Run);
        let page = Call {
            untrusted: true,
            ..call(Allow, Change::Undoable)
        };
        assert!(matches!(decide(&all, &page), Decision::Defer(_)));
    }

    #[test]
    fn within_caps_approval_and_sandbox_not_autonomy() {
        let wide = Mode {
            autonomy: Autonomy::Autopilot,
            approval: Approval::AllowAll,
            sandbox: false,
        };
        let ceiling = Mode {
            autonomy: Autonomy::Interactive,
            approval: Approval::Assisted,
            sandbox: true,
        };
        let capped = wide.within(&ceiling);
        assert_eq!(capped.approval, Approval::Assisted);
        assert!(capped.sandbox);
        assert_eq!(capped.autonomy, Autonomy::Autopilot);
        let narrow = Mode::default();
        assert_eq!(narrow.within(&ceiling), narrow);
    }

    #[test]
    fn the_label_names_all_three() {
        assert_eq!(Mode::default().label(), "Interactive · Manual · Sandbox on");
        let m = Mode {
            autonomy: Autonomy::Autopilot,
            approval: Approval::AllowAll,
            sandbox: false,
        };
        assert_eq!(m.label(), "Autopilot · Allow all · Sandbox off");
    }
}
