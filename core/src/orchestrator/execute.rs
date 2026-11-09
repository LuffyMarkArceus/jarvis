//! Running a tool call: the web check, the permission and untrusted-content gates, undo.

use super::*;

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
        rec: &TurnRecorder,
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
        rec: &TurnRecorder,
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
        rec: &TurnRecorder,
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
            self.trust.untrusted(&self.session_id, seen)
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
        rec: &TurnRecorder,
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
        rec: &TurnRecorder,
    ) -> ToolOutcome {
        let outcome = self.gate_and_run(name, args, confirm, untrusted).await;
        rec.tool(&outcome, status_label(&outcome.status));
        outcome
    }

    /// Run a single tool call through the permission gate and the registry.
    ///
    /// `untrusted` is what the model read that the owner has not vouched
    /// for. When it is not empty, an action asks even where the permission
    /// says allow, and the prompt names it; deny still wins.
    pub(super) async fn gate_and_run(
        &self,
        name: String,
        args: Value,
        confirm: Option<&dyn ConfirmationHandler>,
        untrusted: &[Source],
    ) -> ToolOutcome {
        let args = self.sanitize_args(&name, args);
        let verdict = self.permissions.check_permission(&name, &args);

        if !untrusted.is_empty() && verdict.action != PermissionAction::Deny {
            return self
                .ask_after_untrusted(name, args, confirm, untrusted, verdict.pattern)
                .await;
        }

        match verdict.action {
            PermissionAction::Deny => {
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
                let _ = self.permissions.log_audit(
                    Some(&self.session_id),
                    &name,
                    &args,
                    "denied",
                    Some(&msg),
                );
                ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Denied,
                    output: msg,
                    result: None,
                    pattern: verdict.pattern,
                }
            }
            PermissionAction::Ask => match confirm {
                Some(handler) => {
                    if handler.confirm(&name, &args).await {
                        self.run_tool(name, args, "confirmed", verdict.pattern)
                            .await
                    } else {
                        let msg = format!(
                            "Tool {} was declined by the user. Args: {}",
                            name,
                            compact_args(&args)
                        );
                        let _ = self.permissions.log_audit(
                            Some(&self.session_id),
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
                        Some(&self.session_id),
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
            PermissionAction::Allow => self.run_tool(name, args, "allowed", verdict.pattern).await,
        }
    }

    /// The prompt for an action the model asked for after reading
    /// untrusted content: what it read, and whether it can be undone.
    pub(super) async fn ask_after_untrusted(
        &self,
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
                Some(&self.session_id),
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
                    self.trust.trust(&self.session_id, untrusted);
                }
                let _ = self.permissions.log_audit(
                    Some(&self.session_id),
                    &name,
                    &args,
                    "confirmed_after_untrusted",
                    Some(&why),
                );
                // The row above says why it asked; this one what it did.
                self.run_tool(name, args, "confirmed", pattern).await
            }
            None => {
                let msg = format!(
                    "Tool {} was declined by the user ({}). Args: {}",
                    name,
                    why,
                    compact_args(&args)
                );
                let _ = self.permissions.log_audit(
                    Some(&self.session_id),
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
                            Some(&self.session_id),
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
                            Some(&self.session_id),
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
                    match store.before_change(std::path::Path::new(path), &name, &self.session_id) {
                        Ok(pending) => Some(pending),
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
                        Some(&self.session_id),
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
                        Some(&self.session_id),
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
