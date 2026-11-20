//! Every model request goes through here: the egress check, redaction, the call.

use super::*;
use crate::lane::{Admit, LanePermit};

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

/// A model's name without its vendor or ":free": "nvidia/nemotron-3-ultra:free"
/// is "nemotron-3-ultra".
pub(super) fn short_model(name: &str) -> &str {
    let name = name.rsplit('/').next().unwrap_or(name);
    name.strip_suffix(":free").unwrap_or(name)
}

/// The system prompt of a turn's requests, with the version a spill to the
/// remote tier sends: recall the policy would block left out, as a remote
/// conversation's prompt would have it (6.5b).
pub(super) struct SystemPrompt {
    pub(super) text: String,
    pub(super) label: Label,
    pub(super) spill: Option<(String, Label)>,
}

/// A local call's way forward.
enum Local {
    Slot(LanePermit),
    Spilled(ModelReply),
}

/// What came back from [`Orchestrator::ask_model`].
pub(super) enum ModelReply {
    /// Boxed: a turn is much larger than a refusal's text.
    Turn(Box<ChatTurn>),
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
        system: &SystemPrompt,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder<'_>,
    ) -> Result<ModelReply> {
        rec.saw_request(&system.text, &system.label, messages);
        let (cfg, client) = self.turn_tier(rec);
        let (cfg, client) = (&cfg, &client);
        let request = Request {
            system: &system.text,
            messages,
            tools,
        };
        if !destination_of(cfg).is_remote() {
            let slot = match self
                .local_or_spill(system, messages, tools, confirm, cancel, sink, rec)
                .await?
            {
                Local::Slot(slot) => slot,
                Local::Spilled(reply) => return Ok(reply),
            };
            return self
                .send(cfg, client, request, false, Some(slot), cancel, sink, rec)
                .await;
        }

        // Every step of a spilled turn gets what a remote conversation would
        // send, and a refusal sends it back to the GPU queue - never to the
        // "answer locally?" prompt, since local is what was busy.
        use std::sync::atomic::Ordering::Relaxed;
        let spilled = rec.spilled.load(Relaxed);
        let (text, label) = match (&system.spill, spilled) {
            (Some(spill), true) => spill.clone(),
            _ => (system.text.clone(), system.label.clone()),
        };
        let Some(why) = self
            .refusal(cfg, client, &label, messages, confirm, rec)
            .await
        else {
            let request = Request {
                system: &text,
                messages,
                tools,
            };
            return self
                .send(cfg, client, request, true, None, cancel, sink, rec)
                .await;
        };
        if spilled {
            rec.spilled.store(false, Relaxed);
            rec.spill_withheld.store(true, Relaxed);
            let (local_cfg, local) = self.turn_tier(rec);
            let slot = self
                .local_slot(&local_cfg.default_model, cancel, rec)
                .await?;
            return self
                .send(
                    &local_cfg,
                    &local,
                    request,
                    false,
                    Some(slot),
                    cancel,
                    sink,
                    rec,
                )
                .await;
        }
        // The owner's rule: when nothing could be sent, ask each time whether
        // the local model should answer instead. Nobody to ask means no.
        if let (Some((local_cfg, local)), Some(confirm)) = (self.local_fallback(rec), confirm) {
            let (local_cfg, local) = (&local_cfg, &local);
            let args = json!({ "model": local_cfg.default_model, "reason": why });
            if confirm.confirm(EGRESS_LOCAL_TOOL, &args).await {
                return self
                    .send(local_cfg, local, request, false, None, cancel, sink, rec)
                    .await;
            }
        }
        rec.withheld
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(ModelReply::Withheld(why))
    }

    /// Why a request to a remote tier may not be sent (the answer to show),
    /// or `None` when it may: by policy, or because the user approved.
    async fn refusal(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        system_label: &Label,
        messages: &[ChatMessage],
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder<'_>,
    ) -> Option<String> {
        let mut labels = vec![system_label.clone()];
        labels.extend(egress::label(messages));
        match egress::check(&labels, &self.egress_policy()) {
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
        }
    }

    /// Whether a local call that finds the GPU busy may go to the remote
    /// tier instead: the turn is local, a remote tier exists and really is
    /// remote, spilling is on, and this turn has not spilled or been
    /// refused a spill already.
    pub(super) fn can_spill(&self, rec: &TurnRecorder<'_>) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(remote) = self.config.remote.as_ref() else {
            return false;
        };
        !self.on_remote(&rec.settings())
            && self.remote.is_some()
            && destination_of(&remote.model).is_remote()
            && remote.spill != Spill::Never
            && !rec.spilled.load(Relaxed)
            && !rec.spill_withheld.load(Relaxed)
    }

    /// A GPU slot for a local call, or - when the GPU stays busy past
    /// `spill_after` and spilling is allowed - the remote tier's answer.
    /// A call that waits keeps its place in line throughout, including
    /// while the user is asked something (6.5b).
    #[allow(clippy::too_many_arguments)]
    async fn local_or_spill(
        &self,
        system: &SystemPrompt,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder<'_>,
    ) -> Result<Local> {
        let (cfg, _) = self.turn_tier(rec);
        let model = cfg.default_model.as_str();
        let remote = self.config.remote.as_ref();
        if !self.can_spill(rec) {
            return Ok(Local::Slot(self.local_slot(model, cancel, rec).await?));
        }
        let wait = std::time::Duration::from_secs(remote.map_or(5, |r| r.spill_after));
        self.say_waiting(rec);
        let waiter = match self.lane.acquire(model, Some(wait), cancel).await {
            Admit::Slot(permit) => {
                rec.queued(permit.waited());
                return Ok(Local::Slot(permit));
            }
            Admit::Cancelled => return Err(JarvisError::Cancelled),
            Admit::Busy(waiter) => waiter,
        };

        use std::sync::atomic::Ordering::Relaxed;
        rec.spilled.store(true, Relaxed);
        let (remote_cfg, remote_client) = self.turn_tier(rec);
        let asked = remote.is_some_and(|r| r.spill == Spill::Ask);
        let agreed = !asked
            || match confirm {
                Some(confirm) => {
                    let args = json!({
                        "model": remote_cfg.default_model,
                        "waited_s": waiter.waited().as_secs(),
                    });
                    confirm.confirm(EGRESS_SPILL_TOOL, &args).await
                }
                None => false,
            };
        let (text, label) = system
            .spill
            .clone()
            .unwrap_or_else(|| (system.text.clone(), system.label.clone()));
        let refused = if agreed {
            self.refusal(&remote_cfg, &remote_client, &label, messages, confirm, rec)
                .await
        } else {
            None
        };
        if agreed && refused.is_none() {
            drop(waiter);
            rec.step(format!(
                "GPU busy: answering with {} (remote)",
                short_model(&remote_cfg.default_model)
            ));
            let request = Request {
                system: &text,
                messages,
                tools,
            };
            let reply = self
                .send(
                    &remote_cfg,
                    &remote_client,
                    request,
                    true,
                    None,
                    cancel,
                    sink,
                    rec,
                )
                .await?;
            return Ok(Local::Spilled(reply));
        }
        // Not sent: the user said no, or what it holds may not go. Back to
        // the queue, from the same place, and no more spills this turn.
        rec.spilled.store(false, Relaxed);
        if agreed {
            rec.spill_withheld.store(true, Relaxed);
        }
        match waiter.wait(cancel).await {
            Some(permit) => {
                rec.queued(permit.waited());
                Ok(Local::Slot(permit))
            }
            None => Err(JarvisError::Cancelled),
        }
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
        slot: Option<LanePermit>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder<'_>,
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
        // A local model call takes a GPU slot first, and says so when it
        // has to wait behind another window's (6.5b).
        let _slot = match slot {
            Some(slot) => Some(slot),
            None if remote => None,
            None => Some(self.local_slot(&cfg.default_model, cancel, rec).await?),
        };
        rec.step(format!(
            "asking {} (step {})",
            short_model(&cfg.default_model),
            rec.model_calls() + 1
        ));
        let started = std::time::Instant::now();
        let mut turn = match stream_to {
            Some(sink) => client.chat_turn_streaming(chat, cancel, sink).await?,
            None => client.chat_turn(chat, cancel).await?,
        };
        rec.model_time(&cfg.default_model, started.elapsed(), &turn);
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
        Ok(ModelReply::Turn(Box::new(turn)))
    }

    /// A slot in the local lane for one call to `model`.
    pub(super) async fn local_slot(
        &self,
        model: &str,
        cancel: Option<&Cancellation>,
        rec: &TurnRecorder<'_>,
    ) -> Result<LanePermit> {
        self.say_waiting(rec);
        match self.lane.acquire(model, None, cancel).await {
            Admit::Slot(permit) => {
                rec.queued(permit.waited());
                Ok(permit)
            }
            Admit::Busy(waiter) => match waiter.wait(cancel).await {
                Some(permit) => {
                    rec.queued(permit.waited());
                    Ok(permit)
                }
                None => Err(JarvisError::Cancelled),
            },
            Admit::Cancelled => Err(JarvisError::Cancelled),
        }
    }

    /// "waiting for the GPU" when every slot is taken.
    fn say_waiting(&self, rec: &TurnRecorder<'_>) {
        if self.lane.available() == 0 {
            if let Some(running) = self.lane.running() {
                rec.step(format!(
                    "waiting for the GPU: another window is using {running}"
                ));
            }
        }
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
