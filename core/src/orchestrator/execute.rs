//! Running a tool call: the web check, the permission and untrusted-content gates, undo.

use super::*;
use crate::conversation::Deferred;
use crate::modes::{decide, Approval, Call, Change, Decision, Mode};

/// The real arguments, when a model nested them inside an echo of the tool
/// schema: `{"parameters": {...}}` or `{"properties": {...}, "type": "object"}`.
/// Arguments that already have every required key are left alone.
///
/// A nested object is used only if every required key holds a value. When the
/// model echoes the schema itself, `properties.path` is `{"type": "string"}`;
/// passing that on would turn "no arguments" into a wrong argument.
pub(super) fn unwrap_echoed_schema(args: Value, required: &[String]) -> Value {
    if required.is_empty() {
        return args;
    }
    if required.iter().all(|k| args.get(k).is_some()) {
        return args;
    }
    let is_value = |v: &Value| !(v.is_object() && v.get("type").is_some());
    for wrapper in ["parameters", "properties"] {
        if let Some(nested) = args.get(wrapper).and_then(|p| p.as_object()) {
            if required.iter().all(|k| nested.get(k).is_some_and(is_value)) {
                return Value::Object(nested.clone());
            }
        }
    }
    args
}

/// What a tool call is doing, for the progress line: short, and never the
/// whole arguments (a written file's content is not a status).
pub(super) fn describe_step(tool: &str, args: &Value) -> String {
    let file = || {
        args["path"]
            .as_str()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string())
            })
            .unwrap_or_else(|| "a file".to_string())
    };
    match tool {
        "filesystem.write" => format!("writing {}", file()),
        "filesystem.edit" => format!("editing {}", file()),
        "filesystem.delete" => format!("deleting {}", file()),
        "filesystem.read" => format!("reading {}", file()),
        "filesystem.list_dir" | "filesystem.search" | "filesystem.stat" => {
            "looking through files".to_string()
        }
        "shell.run" => {
            let command: String = args["command"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            format!("running: {}", command)
        }
        "system.info" | "process.list" => "checking the system".to_string(),
        t if t.starts_with("mcp.search.") => "searching the web".to_string(),
        t if t.starts_with("mcp.fetch.") => match args["url"].as_str() {
            Some(url) => format!("reading {}", host_of(url)),
            None => "reading a web page".to_string(),
        },
        other => format!("using {}", other),
    }
}

/// Tool arguments sometimes arrive as a JSON-encoded string.
pub(super) fn normalize_arguments(args: Value) -> Value {
    match &args {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        _ => args,
    }
}

impl Orchestrator {
    /// Execute a native tool call through the permission gate and registry.
    pub(super) async fn execute_native_call(
        &self,
        call: &ModelToolCall,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder<'_>,
    ) -> ToolOutcome {
        let name = call.function.name.clone();
        let args = normalize_arguments(call.function.arguments.clone());
        self.execute_offered(name, args, true, plan, confirm, rec)
            .await
    }

    /// Execute a tool call parsed out of the model's text.
    pub(super) async fn execute_text_call(
        &self,
        call: &parser::ToolCall,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder<'_>,
    ) -> ToolOutcome {
        let args = normalize_arguments(call.args.clone());
        self.execute_offered(call.name.clone(), args, true, plan, confirm, rec)
            .await
    }

    /// Run a model-requested call only if this turn's plan offered it.
    ///
    /// A hallucinated tool name must not become an execution path around the
    /// planner. This is a correctness guard, not a permission check: the
    /// permission system still runs for every call that gets this far.
    ///
    /// `by_model` says who wrote the arguments, which matters for what a
    /// web call may carry (see [`Self::check_web_call`]).
    pub(super) async fn execute_offered(
        &self,
        name: String,
        args: Value,
        by_model: bool,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder<'_>,
    ) -> ToolOutcome {
        if !plan.allows(&name) {
            let available = if plan.tools.is_empty() {
                "none".to_string()
            } else {
                plan.names().join(", ")
            };
            let outcome = ToolOutcome {
                name,
                args,
                status: OutcomeStatus::Failed,
                output: format!(
                    "That tool is not available for this request. Available tools: {}.",
                    available
                ),
                result: None,
                pattern: None,
            };
            rec.tool(&outcome, "not_offered");
            return outcome;
        }
        let args = match self
            .check_web_call(&name, args, by_model, confirm, rec)
            .await
        {
            Ok(args) => args,
            Err((args, why)) => {
                let outcome = ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Withheld,
                    output: why,
                    result: None,
                    pattern: None,
                };
                rec.tool(&outcome, "withheld");
                return outcome;
            }
        };
        // Only a model can be steered by what it read. The owner's own
        // command, routed without one, never comes through here.
        let untrusted = if by_model && taint::is_action(&name) {
            let seen = taint::untrusted_sources(&rec.label(), &self.egress_policy());
            self.trust.untrusted(&rec.conversation.id, seen)
        } else {
            Vec::new()
        };
        self.execute_call(name, args, confirm, &untrusted, rec)
            .await
    }

    /// A web call sends its arguments off this machine on any tier: the
    /// query to the search service, the URL to its site. This is the one
    /// place they are checked, because it is the one place they leave.
    ///
    /// * A search query is redacted, like anything of the user's that
    ///   leaves: it still works with `<EMAIL_1>` in it. A remote model's
    ///   calls had their placeholders restored on the way back, so without
    ///   this the real value would go to the search service.
    /// * An address cannot be redacted and still work, so one carrying a
    ///   secret or personal data is asked about instead.
    /// * Arguments the model wrote can carry what it read this turn - a
    ///   file, command output - so they are judged by the turn's labels,
    ///   like a model request. The user's own words sent as the first
    ///   search are not: they were the query by definition.
    ///
    /// Returns the arguments to send, or why nothing was sent.
    pub(super) async fn check_web_call(
        &self,
        name: &str,
        mut args: Value,
        by_model: bool,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder<'_>,
    ) -> std::result::Result<Value, (Value, String)> {
        if egress::tool_origin(name) != Origin::Web {
            return Ok(args);
        }
        if let Some(query) = args.get("query").and_then(Value::as_str) {
            let redacted = rec.redactor.lock().unwrap().redact(query);
            args["query"] = Value::String(redacted);
        }
        let url = args.get("url").and_then(Value::as_str).map(String::from);
        let service = match &url {
            Some(url) => host_of(url),
            None => "the web search service".to_string(),
        };

        let mut reasons: Vec<String> = Vec::new();
        if url
            .as_deref()
            .is_some_and(|u| Redactor::new().redact(u) != u)
        {
            reasons.push("a secret or personal data in the address".into());
        }
        let mut new: Vec<Concern> = Vec::new();
        if by_model {
            match egress::check(&[rec.label()], &self.egress_policy()) {
                Verdict::Send => {}
                Verdict::Never(concerns) => {
                    let items: Vec<String> = concerns.iter().map(describe_concern).collect();
                    return Err((
                        args,
                        format!(
                            "the model wrote this after reading {}, which never leaves this machine",
                            items.join(", ")
                        ),
                    ));
                }
                Verdict::Ask(concerns) => {
                    let approved = rec.web_approved.lock().unwrap();
                    new = concerns
                        .into_iter()
                        .filter(|c| !approved.contains(c))
                        .collect();
                }
            }
        }
        if !new.is_empty() {
            let items: Vec<String> = new.iter().map(describe_concern).collect();
            reasons.push(format!(
                "the model wrote it after reading {}",
                items.join(", ")
            ));
        }
        // Data leaving through an address: untrusted text could have asked
        // for it, the turn holds something private, and the model built the
        // address rather than copying it.
        let label = rec.label();
        let policy = self.egress_policy();
        let exfil = by_model
            && url.as_deref().is_some_and(|u| {
                let shown = rec.shown_urls.lock().unwrap();
                taint::exfiltration_risk(&label, &policy, u, &shown)
            });
        if exfil {
            reasons.push(format!(
                "the model built this address after reading untrusted content; {} would receive it",
                service
            ));
        }
        if reasons.is_empty() {
            return Ok(args);
        }

        let sending = url.unwrap_or_else(|| args["query"].as_str().unwrap_or_default().to_string());
        let mut ask = json!({ "service": service, "sending": sending, "because": reasons });
        if exfil {
            let built_after: Vec<String> = taint::untrusted_sources(&label, &policy)
                .into_iter()
                .map(|s| s.name)
                .collect();
            ask["built_after"] = json!(built_after);
            ask["also_read"] = json!(taint::private_sources(&label));
            ask["host"] = json!(service);
        }
        let Some(confirm) = confirm else {
            return Err((args, format!("{}; nobody was asked", reasons.join("; "))));
        };
        // One yes, and no "trust": agreeing to a page is not agreeing to
        // send data out, so this asks every time.
        let fetch_once = [Choice {
            key: 'y',
            label: if exfil { "fetch once" } else { "send once" }.into(),
        }];
        if confirm
            .choose(EGRESS_WEB_TOOL, &ask, &fetch_once)
            .await
            .is_some()
        {
            rec.web_approved.lock().unwrap().extend(new);
            Ok(args)
        } else {
            Err((args, format!("{}; not approved", reasons.join("; "))))
        }
    }

    /// Run a single tool call and record it in the turn report.
    pub(super) async fn execute_call(
        &self,
        name: String,
        args: Value,
        confirm: Option<&dyn ConfirmationHandler>,
        untrusted: &[Source],
        rec: &TurnRecorder<'_>,
    ) -> ToolOutcome {
        rec.step(describe_step(&name, &args));
        let started = std::time::Instant::now();
        let outcome = self
            .gate_and_run(
                rec.conversation,
                name,
                args,
                confirm,
                untrusted,
                rec.settings().mode,
            )
            .await;
        if outcome.output.contains(crate::modes::PLAN_REFUSAL) {
            rec.plan_refused
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        rec.tool_time(&outcome.name, started.elapsed());
        rec.tool(&outcome, status_label(&outcome.status));
        outcome
    }

    /// Whether this call runs outside the sandbox: asked for, or there is
    /// no sandbox to run it in.
    pub(super) fn runs_outside(&self, name: &str, args: &Value) -> bool {
        name == "shell.run"
            && (args["outside_sandbox"].as_bool().unwrap_or(false)
                || !crate::sandbox::ready(&self.config))
    }

    /// Run a single tool call through the permission gate and the registry.
    ///
    /// `untrusted` is what the model read that the owner has not vouched
    /// for. When it is not empty, an action asks even where the permission
    /// says allow, and the prompt names it; deny still wins.
    pub(super) async fn gate_and_run(
        &self,
        conv: &Conversation,
        name: String,
        args: Value,
        confirm: Option<&dyn ConfirmationHandler>,
        untrusted: &[Source],
        mode: Mode,
    ) -> ToolOutcome {
        let mut args = self.sanitize_args(&name, args);
        // A call missing what it needs goes back to the model, which can
        // fix it, instead of to the owner, who would be asked to approve
        // nothing (a 1B sends its schema instead of a command).
        let missing = self.missing_args(&name, &args);
        if !missing.is_empty() {
            return ToolOutcome {
                output: format!(
                    "Tool {} was not run: it needs {} and the call did not give {}. Call it again with the actual values.",
                    name,
                    missing.join(", "),
                    if missing.len() == 1 { "it" } else { "them" }
                ),
                name,
                args,
                status: OutcomeStatus::Failed,
                result: None,
                pattern: None,
            };
        }
        let verdict = self.permissions.check_permission(&name, &args);
        // What the call itself asked for, before the mode adds to it: a
        // deferred call keeps only this, so /approve decides by the mode then.
        let as_asked = args.clone();
        let outside = self.runs_outside(&name, &args) || (name == "shell.run" && !mode.sandbox);
        if outside {
            if let Some(map) = args.as_object_mut() {
                map.insert("outside_sandbox".into(), json!(true));
            }
        }
        let call = Call {
            rule: verdict.action,
            change: self.change_of(&name, &args, outside),
            untrusted: !untrusted.is_empty(),
            allow_outside: self.config.sandbox.allow_outside,
            sandbox_ready: crate::sandbox::ready(&self.config),
        };
        let auto = verdict.action == PermissionAction::Ask;
        match decide(&mode, &call) {
            Decision::Run => {
                let how = match (auto, mode.approval) {
                    (false, _) => "allowed",
                    (true, Approval::AllowAll) => "auto_approved_allow_all",
                    (true, _) => "auto_approved_undoable",
                };
                self.run_tool(conv, name, args, how, verdict.pattern).await
            }
            Decision::RunReadOnly => {
                if let Some(map) = args.as_object_mut() {
                    map.insert("_read_only".into(), json!(true));
                }
                let how = if auto {
                    "auto_approved_read_only"
                } else {
                    "allowed"
                };
                self.run_tool(conv, name, args, how, verdict.pattern).await
            }
            Decision::Refuse(why) => {
                let (status, audit, msg) = if verdict.action == PermissionAction::Deny {
                    let msg = match &verdict.pattern {
                        Some(p) => format!(
                            "Tool {} denied by pattern \"{}\". Args: {}",
                            name,
                            p,
                            compact_args(&args)
                        ),
                        None => format!(
                            "Tool {} denied by policy. Args: {}",
                            name,
                            compact_args(&args)
                        ),
                    };
                    (OutcomeStatus::Denied, "denied", msg)
                } else if outside && !untrusted.is_empty() {
                    let names: Vec<&str> = untrusted.iter().map(|s| s.name.as_str()).collect();
                    let msg = format!(
                        "Tool {} was not run: the model asked to run outside the sandbox after reading content you have not marked as trusted ({}). If you want it, run it yourself.",
                        name,
                        names.join(", ")
                    );
                    (OutcomeStatus::Untrusted, "outside_sandbox_untrusted", msg)
                } else if outside && !self.config.sandbox.allow_outside {
                    let msg = format!(
                        "Tool {} was not run: running a command outside the sandbox is switched off ([sandbox] allow_outside). Args: {}",
                        name,
                        compact_args(&args)
                    );
                    (OutcomeStatus::Denied, "outside_sandbox_off", msg)
                } else {
                    let msg = format!(
                        "Tool {} was not run: {} Args: {}",
                        name,
                        why,
                        compact_args(&args)
                    );
                    (OutcomeStatus::Denied, "refused_plan", msg)
                };
                let _ = self
                    .permissions
                    .log_audit(Some(&conv.id), &name, &args, audit, Some(&msg));
                ToolOutcome {
                    name,
                    args,
                    status,
                    output: msg,
                    result: None,
                    pattern: verdict.pattern,
                }
            }
            Decision::Defer(why) => {
                let msg = format!(
                    "Not done yet: {} needs the owner's yes, so it waits for /approve (Autopilot). Carry on with the rest of the task. Args: {}",
                    name,
                    compact_args(&args)
                );
                let _ = self.permissions.log_audit(
                    Some(&conv.id),
                    &name,
                    &args,
                    "deferred",
                    Some(&msg),
                );
                conv.defer(Deferred {
                    name: name.clone(),
                    args: as_asked,
                    why,
                    untrusted: untrusted.to_vec(),
                });
                ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Deferred,
                    output: msg,
                    result: None,
                    pattern: verdict.pattern,
                }
            }
            Decision::Ask if !untrusted.is_empty() => {
                self.ask_after_untrusted(conv, name, args, confirm, untrusted, verdict.pattern)
                    .await
            }
            Decision::Ask => match confirm {
                Some(handler) => {
                    if handler.confirm(&name, &args).await {
                        self.run_tool(conv, name, args, "confirmed", verdict.pattern)
                            .await
                    } else {
                        let msg = format!(
                            "Tool {} was declined by the user. Args: {}",
                            name,
                            compact_args(&args)
                        );
                        let _ = self.permissions.log_audit(
                            Some(&conv.id),
                            &name,
                            &args,
                            "declined_by_user",
                            Some(&msg),
                        );
                        ToolOutcome {
                            name,
                            args,
                            status: OutcomeStatus::Declined,
                            output: msg,
                            result: None,
                            pattern: verdict.pattern,
                        }
                    }
                }
                None => {
                    let msg = format!(
                        "Tool {} requires confirmation and was auto-denied in non-interactive mode. Args: {}",
                        name,
                        compact_args(&args)
                    );
                    let _ = self.permissions.log_audit(
                        Some(&conv.id),
                        &name,
                        &args,
                        "ask_auto_denied",
                        Some(&msg),
                    );
                    ToolOutcome {
                        name,
                        args,
                        status: OutcomeStatus::NeedsConfirmation,
                        output: msg,
                        result: None,
                        pattern: verdict.pattern,
                    }
                }
            },
        }
    }

    /// What a call would change, for the mode's decision.
    pub(super) fn change_of(&self, name: &str, args: &Value, outside: bool) -> Change {
        match name {
            "shell.run" if outside => Change::Outside,
            "shell.run" => Change::Shell {
                writes: args["writes"].as_bool().unwrap_or(false),
            },
            "filesystem.write" | "filesystem.edit" | "filesystem.delete" => {
                let path = args
                    .get("path")
                    .and_then(Value::as_str)
                    .map(std::path::Path::new);
                match (&self.undo, path) {
                    (Some(store), Some(path)) if store.can_keep(path) => Change::Undoable,
                    _ => Change::NotUndoable,
                }
            }
            n if taint::is_action(n) => Change::NotUndoable,
            _ => Change::None,
        }
    }

    /// The prompt for an action the model asked for after reading
    /// untrusted content: what it read, and whether it can be undone.
    pub(super) async fn ask_after_untrusted(
        &self,
        conv: &Conversation,
        name: String,
        args: Value,
        confirm: Option<&dyn ConfirmationHandler>,
        untrusted: &[Source],
        pattern: Option<String>,
    ) -> ToolOutcome {
        let names: Vec<&str> = untrusted.iter().map(|s| s.name.as_str()).collect();
        let why = format!("after reading: {}", names.join(", "));
        let Some(handler) = confirm else {
            let msg = format!(
                "Tool {} was not run: the model asked for it after reading content you have not marked as trusted ({}), and nobody could be asked.",
                name,
                names.join(", ")
            );
            let _ = self.permissions.log_audit(
                Some(&conv.id),
                &name,
                &args,
                "untrusted_not_asked",
                Some(&why),
            );
            return ToolOutcome {
                name,
                args,
                status: OutcomeStatus::Untrusted,
                output: msg,
                result: None,
                pattern,
            };
        };

        let mut shown = args.clone();
        if let Some(map) = shown.as_object_mut() {
            map.insert(
                "_untrusted".into(),
                untrusted
                    .iter()
                    .map(|s| json!({ "name": s.name, "kind": s.describe() }))
                    .collect(),
            );
            let undo = self.undo_note(&name, &args);
            map.insert("_undo".into(), json!(undo));
        }
        let choices = [
            Choice {
                key: 'y',
                label: "run once".into(),
            },
            Choice {
                key: 't',
                label: "trust these for this conversation (new pages and files will still ask)"
                    .into(),
            },
        ];
        match handler.choose(&name, &shown, &choices).await {
            Some(key) => {
                if key == 't' {
                    self.trust.trust(&conv.id, untrusted);
                }
                let _ = self.permissions.log_audit(
                    Some(&conv.id),
                    &name,
                    &args,
                    "confirmed_after_untrusted",
                    Some(&why),
                );
                // The row above says why it asked; this one what it did.
                self.run_tool(conv, name, args, "confirmed", pattern).await
            }
            None => {
                let msg = format!(
                    "Tool {} was declined by the user ({}). Args: {}",
                    name,
                    why,
                    compact_args(&args)
                );
                let _ = self.permissions.log_audit(
                    Some(&conv.id),
                    &name,
                    &args,
                    "declined_after_untrusted",
                    Some(&why),
                );
                ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Declined,
                    output: msg,
                    result: None,
                    pattern,
                }
            }
        }
    }

    /// Whether undo will be able to reverse this call: `kept`, `too_large`
    /// (no copy over the size limit), `unavailable` (no undo folder), or
    /// `never` (commands and other tools).
    pub(super) fn undo_note(&self, name: &str, args: &Value) -> &'static str {
        if !matches!(
            name,
            "filesystem.write" | "filesystem.edit" | "filesystem.delete"
        ) {
            return "never";
        }
        let Some(store) = &self.undo else {
            return "unavailable";
        };
        match args.get("path").and_then(Value::as_str) {
            Some(path) if store.would_keep(std::path::Path::new(path)) => "kept",
            Some(_) => "too_large",
            None => "kept",
        }
    }

    /// Execute an approved tool and record the outcome.
    ///
    /// `decision` is what lands in the audit log, so a call a human approved
    /// is distinguishable after the fact from one the policy allowed
    /// outright.
    pub(super) async fn run_tool(
        &self,
        conv: &Conversation,
        name: String,
        args: Value,
        decision: &str,
        pattern: Option<String>,
    ) -> ToolOutcome {
        // Check if it's an MCP tool (namespaced as mcp.server.tool)
        if name.starts_with("mcp.") {
            let parts: Vec<&str> = name.split('.').collect();
            if parts.len() >= 3 {
                let server = parts[1].to_string();
                let tool_name = parts[2..].join(".");
                match self.mcp.call_tool(&server, &tool_name, args.clone()).await {
                    Ok(res) => {
                        let _ = self.permissions.log_audit(
                            Some(&conv.id),
                            &name,
                            &args,
                            decision,
                            Some(&res),
                        );
                        let res_val = serde_json::json!({ "result": res });
                        ToolOutcome {
                            name,
                            args,
                            status: OutcomeStatus::Succeeded,
                            output: res.clone(),
                            result: Some(res_val),
                            pattern,
                        }
                    }
                    Err(e) => {
                        let msg = format!("Tool {} failed: {}", name, e);
                        let _ = self.permissions.log_audit(
                            Some(&conv.id),
                            &name,
                            &args,
                            &format!("{}_failed", decision),
                            Some(&msg),
                        );
                        ToolOutcome {
                            name,
                            args,
                            status: OutcomeStatus::Failed,
                            output: msg,
                            result: None,
                            pattern,
                        }
                    }
                }
            } else {
                let msg = format!("Invalid MCP tool name: {}", name);
                ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Failed,
                    output: msg,
                    result: None,
                    pattern,
                }
            }
        } else {
            // A copy before Jarvis changes a file, so `jarvis undo` can put
            // it back. Every write goes through here, typed or the model's.
            let pending = match (&self.undo, args.get("path").and_then(Value::as_str)) {
                (Some(store), Some(path))
                    if matches!(
                        name.as_str(),
                        "filesystem.write" | "filesystem.edit" | "filesystem.delete"
                    ) =>
                {
                    match store.before_change(std::path::Path::new(path), &name, &conv.id) {
                        // Approved only because it could be undone, but the
                        // file grew past the limit since: no copy, no write.
                        Ok(pending) if decision == "auto_approved_undoable" && !pending.kept() => {
                            store.discard(pending);
                            let msg = format!(
                                "Tool {} was not run: the file is now too large to keep an undo copy, so it was not written automatically. Ask again to be asked.",
                                name
                            );
                            let _ = self.permissions.log_audit(
                                Some(&conv.id),
                                &name,
                                &args,
                                "no_undo_copy",
                                Some(&msg),
                            );
                            return ToolOutcome {
                                name,
                                args,
                                status: OutcomeStatus::Failed,
                                output: msg,
                                result: None,
                                pattern,
                            };
                        }
                        Ok(pending) => Some(pending),
                        // Approved only because it could be undone: with no
                        // copy, it is not made (6.6).
                        Err(e) if decision == "auto_approved_undoable" => {
                            let msg = format!(
                                "Tool {} was not run: no undo copy could be kept ({}), so it was not written automatically. Ask again to be asked.",
                                name, e
                            );
                            let _ = self.permissions.log_audit(
                                Some(&conv.id),
                                &name,
                                &args,
                                "no_undo_copy",
                                Some(&msg),
                            );
                            return ToolOutcome {
                                name,
                                args,
                                status: OutcomeStatus::Failed,
                                output: msg,
                                result: None,
                                pattern,
                            };
                        }
                        Err(e) => {
                            warn!("no undo copy of {}: {}", path, e);
                            None
                        }
                    }
                }
                _ => None,
            };
            let executed = self.tools.execute(&name, args.clone()).await;
            if let (Some(store), Some(pending)) = (&self.undo, pending) {
                if executed.is_ok() {
                    if let Err(e) = store.after_change(pending) {
                        warn!("undo record failed: {}", e);
                    }
                } else {
                    store.discard(pending);
                }
            }
            match executed {
                Ok(res) => {
                    let compact = compact_output(&name, &res);
                    let _ = self.permissions.log_audit(
                        Some(&conv.id),
                        &name,
                        &args,
                        decision,
                        Some(&compact),
                    );
                    ToolOutcome {
                        name,
                        args,
                        status: OutcomeStatus::Succeeded,
                        output: compact,
                        result: Some(res),
                        pattern,
                    }
                }
                Err(e) => {
                    let msg = format!("Tool {} failed: {}", name, e);
                    let _ = self.permissions.log_audit(
                        Some(&conv.id),
                        &name,
                        &args,
                        &format!("{}_failed", decision),
                        Some(&msg),
                    );
                    ToolOutcome {
                        name,
                        args,
                        status: OutcomeStatus::Failed,
                        output: msg,
                        result: None,
                        pattern,
                    }
                }
            }
        }
    }

    /// Required arguments that are absent, null, or a piece of schema
    /// (`{"type": "string"}`) where a value should be.
    pub(super) fn missing_args(&self, tool: &str, args: &Value) -> Vec<String> {
        if tool.starts_with("mcp.") {
            return Vec::new();
        }
        self.tools
            .required_args(tool)
            .into_iter()
            .filter(|key| match args.get(key) {
                None | Some(Value::Null) => true,
                Some(v) => v.is_object() && v.get("type").is_some(),
            })
            .collect()
    }

    /// Some models echo the tool schema back as the arguments object, e.g.
    /// `{"type": "function", "function": "list_dir", "parameters": {...}}`.
    /// Unwrap that so the real arguments reach the tool.
    pub(super) fn sanitize_args(&self, tool: &str, args: Value) -> Value {
        let required = if tool.starts_with("mcp.") {
            self.mcp
                .list_tools()
                .into_iter()
                .find(|t| t.full_name == tool)
                .and_then(|t| t.schema.get("required").and_then(|r| r.as_array()).cloned())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        } else {
            self.tools.required_args(tool)
        };
        let mut args = unwrap_echoed_schema(args, &required);
        // `~/notes.md` from a model means the owner's home, as it does on a
        // deterministic route. Expanded here, once, so the permission check,
        // the egress label, undo and the tool all see the same file; left
        // as it was, the tool wrote `./~/notes.md`.
        if tool.starts_with("filesystem.") {
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                let expanded = router::expand_tilde(path);
                let resolved = match &self.config.tools.work_dir {
                    Some(dir) if !std::path::Path::new(&expanded).is_absolute() => {
                        dir.join(&expanded).display().to_string()
                    }
                    _ => expanded,
                };
                args["path"] = Value::String(resolved);
            }
        }
        args
    }
}
