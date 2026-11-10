//! Every model request goes through here: the egress check, redaction, the call.

use super::*;

/// `https://openrouter.ai/api/v1` -> `openrouter.ai`.
pub(super) fn host_of(base_url: &str) -> String {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| base_url.to_string())
}

/// The parts of a model request that do not depend on the tier.
pub(super) struct Request<'a> {
    pub(super) system: &'a str,
    pub(super) messages: &'a [ChatMessage],
    pub(super) tools: Option<&'a [Value]>,
}

/// Where requests with these settings go. A config that cannot be read was
/// already rejected at startup (`provider::from_config`); if one gets here
/// anyway, it is treated as remote with an undeclared policy, which asks
/// rather than sends.
pub(super) fn destination_of(model: &ModelConfig) -> Destination {
    egress::destination(model).unwrap_or(Destination {
        locality: Locality::Remote,
        data_policy: DataPolicy::Unknown,
    })
}

/// What came back from [`Orchestrator::ask_model`].
pub(super) enum ModelReply {
    Turn(ChatTurn),
    /// The request was not sent; this explains why, as the answer.
    Withheld(String),
}

pub(super) fn describe_concern(concern: &Concern) -> String {
    match (concern.origin, &concern.path) {
        (Origin::File, Some(path)) => format!("the contents of {}", path),
        (Origin::File, None) => "file contents".into(),
        (Origin::Shell, _) => "command output".into(),
        (Origin::System, _) => "information about this machine".into(),
        (Origin::Other, _) => "earlier or unrecognised content of unknown origin".into(),
        (origin, _) => format!("{:?} content", origin).to_lowercase(),
    }
}

impl Orchestrator {
    /// Every model request goes through here: the egress check, redaction,
    /// the call, and putting personal data back into what comes back.
    ///
    /// A local tier gets the request unchanged. A remote one gets it only if
    /// the policy allows everything in it or the user approves, and always
    /// with secrets and personal data replaced by placeholders. When it is
    /// not sent, the user is asked whether the local model should answer.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn ask_model(
        &self,
        system: &str,
        system_label: &Label,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder,
    ) -> Result<ModelReply> {
        rec.saw_request(system, system_label, messages);
        let (cfg, client) = self.tier();
        let (cfg, client) = (&cfg, &client);
        let request = Request {
            system,
            messages,
            tools,
        };
        if !destination_of(cfg).is_remote() {
            return self
                .send(cfg, client, request, false, cancel, sink, rec)
                .await;
        }

        let mut labels = vec![system_label.clone()];
        labels.extend(egress::label(messages));
        let refused = match egress::check(&labels, &self.egress_policy()) {
            Verdict::Send => None,
            Verdict::Never(concerns) => Some(self.withheld(cfg, client, &concerns, true)),
            Verdict::Ask(concerns) => {
                let new: Vec<Concern> = {
                    let approved = rec.approved.lock().unwrap();
                    concerns
                        .into_iter()
                        .filter(|c| !approved.contains(c))
                        .collect()
                };
                if new.is_empty() || self.approve_sending(cfg, client, &new, confirm).await {
                    rec.approved.lock().unwrap().extend(new);
                    None
                } else {
                    Some(self.withheld(cfg, client, &new, false))
                }
            }
        };

        let Some(why) = refused else {
            return self
                .send(cfg, client, request, true, cancel, sink, rec)
                .await;
        };
        // The owner's rule: when nothing could be sent, ask each time whether
        // the local model should answer instead. Nobody to ask means no.
        if let (Some((local_cfg, local)), Some(confirm)) = (self.local_fallback(), confirm) {
            let (local_cfg, local) = (&local_cfg, &local);
            let args = json!({ "model": local_cfg.default_model, "reason": why });
            if confirm.confirm(EGRESS_LOCAL_TOOL, &args).await {
                return self
                    .send(local_cfg, local, request, false, cancel, sink, rec)
                    .await;
            }
        }
        rec.withheld
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(ModelReply::Withheld(why))
    }

    /// Ask whether `concerns` may be sent: once, or (for files) always from
    /// their folders, which writes the allow file.
    pub(super) async fn approve_sending(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        concerns: &[Concern],
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> bool {
        let Some(confirm) = confirm else {
            return false;
        };
        let folders = egress::folders_to_allow(concerns);
        let mut choices = vec![Choice {
            key: 'y',
            label: "send once".into(),
        }];
        if !folders.is_empty() {
            choices.push(Choice {
                key: 'a',
                label: format!("always allow {}", folders.join(", ")),
            });
        }
        let args = json!({
            "provider": client.name(),
            "url": cfg.base_url,
            "would_send": concerns.iter().map(describe_concern).collect::<Vec<_>>(),
        });
        match confirm.choose(EGRESS_TOOL, &args, &choices).await {
            Some('a') => {
                for folder in &folders {
                    if let Err(e) = egress::allow_folder(&self.allow_file(), folder) {
                        warn!("could not save allowed folder {}: {}", folder, e);
                    }
                }
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Send one request to one tier, redacting and restoring when `remote`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        request: Request<'_>,
        remote: bool,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder,
    ) -> Result<ModelReply> {
        let redacted = remote.then(|| {
            let mut redactor = rec.redactor.lock().unwrap();
            let system = redactor.redact(request.system);
            let messages: Vec<ChatMessage> = request
                .messages
                .iter()
                .map(|m| {
                    let mut m = m.clone();
                    m.content = redactor.redact(&m.content);
                    m
                })
                .collect();
            (system, messages, redactor.count() > 0)
        });
        let (system, messages) = match &redacted {
            Some((s, m, _)) => (s.as_str(), m.as_slice()),
            None => (request.system, request.messages),
        };
        let chat = ChatRequest {
            model: &cfg.default_model,
            system: Some(system),
            messages,
            tools: request.tools,
            options: Some(self.model_options(cfg)),
        };
        // A placeholder can be split across streamed chunks and shown before
        // it is restored; a redacted remote answer is shown whole instead.
        let stream_to = sink.filter(|_| !redacted.as_ref().is_some_and(|r| r.2));
        let mut turn = match stream_to {
            Some(sink) => client.chat_turn_streaming(chat, cancel, sink).await?,
            None => client.chat_turn(chat, cancel).await?,
        };
        // For a remote turn the footer names where the data went (the host),
        // not the wire format: "openai" for an OpenRouter model misleads.
        let provider = if remote {
            host_of(&cfg.base_url)
        } else {
            client.name().to_string()
        };
        rec.used(cfg, &provider, remote);
        if redacted.is_some() {
            let redactor = rec.redactor.lock().unwrap();
            turn.message.content = redactor.restore(&turn.message.content);
            for call in turn.message.tool_calls.iter_mut().flatten() {
                let args = redactor.restore(&call.function.arguments.to_string());
                if let Ok(restored) = serde_json::from_str(&args) {
                    call.function.arguments = restored;
                }
            }
        }
        Ok(ModelReply::Turn(turn))
    }

    /// The answer for a request that was not sent.
    pub(super) fn withheld(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        concerns: &[Concern],
        never: bool,
    ) -> String {
        let items: Vec<String> = concerns.iter().map(describe_concern).collect();
        let why = if never {
            "which is never sent to a remote provider"
        } else {
            "which was not approved for sending"
        };
        format!(
            "Nothing was sent to {} ({}). This request would have included {}, {}.\n\n\
             Ask without that content, use /local, or allow a folder with `jarvis egress allow <folder>`.",
            client.name(),
            cfg.base_url,
            items.join(", "),
            why
        )
    }
}
