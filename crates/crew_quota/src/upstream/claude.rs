use super::*;
use acp_thread::AcpThread;
use agent_servers::{
    AcpConnection, AcpDebugMessage, AcpDebugMessageContent, AcpDebugMessageDirection,
    AcpV2Connection,
};
use futures::AsyncReadExt as _;
use http_client::{HttpRequestExt as _, RedirectPolicy};
use std::cell::RefCell;

const SHARED_SCOPE: &str = "claude-registry-shell-oauth";

#[derive(Default)]
pub(super) struct ClaudeState {
    readers: BTreeMap<usize, (Rc<RefCell<BTreeSet<String>>>, Task<()>)>,
    inventory: Option<Task<()>>,
}

trait DebugFeed {
    fn quota_messages(
        &self,
    ) -> (
        Vec<AcpDebugMessage>,
        async_channel::Receiver<AcpDebugMessage>,
    );
}
impl DebugFeed for AcpConnection {
    fn quota_messages(
        &self,
    ) -> (
        Vec<AcpDebugMessage>,
        async_channel::Receiver<AcpDebugMessage>,
    ) {
        self.subscribe_debug_messages()
    }
}
impl DebugFeed for AcpV2Connection {
    fn quota_messages(
        &self,
    ) -> (
        Vec<AcpDebugMessage>,
        async_channel::Receiver<AcpDebugMessage>,
    ) {
        self.subscribe_debug_messages()
    }
}

fn auth_scope(
    registry: bool,
    mut environment_names: Vec<String>,
    configured: bool,
    argument_count: usize,
    auth_mode: &str,
) -> String {
    use sha2::{Digest as _, Sha256};
    environment_names.sort();
    if registry
        && environment_names.is_empty()
        && !configured
        && argument_count == 0
        && auth_mode == "shell-oauth"
    {
        return SHARED_SCOPE.into();
    }
    let mut hash = Sha256::new();
    hash.update(b"claude-acp");
    hash.update([u8::from(registry), u8::from(configured)]);
    hash.update(argument_count.to_le_bytes());
    hash.update(auth_mode.as_bytes());
    for name in environment_names {
        hash.update(name.len().to_le_bytes());
        hash.update(name.as_bytes());
    }
    format!("claude-isolated-{:x}", hash.finalize())
}

fn configured_scope(cx: &App) -> String {
    let settings = cx
        .global::<SettingsStore>()
        .get::<AllAgentServersSettings>(None);
    let Some(CustomAgentServerSettings::Registry {
        env,
        default_mode,
        default_config_options,
        ..
    }) = settings.get("claude-acp")
    else {
        return auth_scope(false, Vec::new(), true, 0, "unknown");
    };
    let registry = AgentRegistryStore::try_global(cx).and_then(|store| {
        store
            .read(cx)
            .agents()
            .iter()
            .find(|agent| agent.id().0.as_ref() == "claude-acp")
            .cloned()
    });
    let Some(RegistryAgent::Npx(registry)) = registry else {
        return auth_scope(false, env.keys().cloned().collect(), true, 0, "unknown");
    };
    auth_scope(
        true,
        env.keys().chain(registry.env.keys()).cloned().collect(),
        default_mode.is_some() || !default_config_options.is_empty(),
        registry.args.len(),
        "shell-oauth",
    )
}

fn shared_scope(cx: &App) -> bool {
    configured_scope(cx) == SHARED_SCOPE
}

fn decode(message: AcpDebugMessage, sessions: &BTreeSet<String>) -> Option<ClaudeRateLimit> {
    if message.direction != AcpDebugMessageDirection::Incoming {
        return None;
    }
    let AcpDebugMessageContent::Notification {
        method,
        params: Some(params),
    } = message.message
    else {
        return None;
    };
    if method.as_ref() != "session/update" || !sessions.contains(params.get("sessionId")?.as_str()?)
    {
        return None;
    }
    let update = params.get("update")?;
    if update.get("sessionUpdate")?.as_str()? != "usage_update" {
        return None;
    }
    let raw = update.get("_meta")?.get("_claude/rateLimit")?;
    let window = |value: &serde_json::Value| {
        let mut safe = serde_json::Map::new();
        for key in ["utilization", "resetsAt", "status"] {
            if let Some(value) = value.get(key)
                && (value.is_number() || value.as_str().is_some_and(|value| value.len() <= 32))
            {
                safe.insert(key.into(), value.clone());
            }
        }
        serde_json::Value::Object(safe)
    };
    let mut safe = window(raw).as_object()?.clone();
    for key in ["rateLimitType", "overageStatus", "overageResetsAt"] {
        if let Some(value) = raw.get(key)
            && (value.is_number() || value.as_str().is_some_and(|value| value.len() <= 32))
        {
            safe.insert(key.into(), value.clone());
        }
    }
    if let Some(windows) = raw.get("unifiedWindows") {
        let mut safe_windows = serde_json::Map::new();
        for key in ["five_hour", "seven_day"] {
            if let Some(value) = windows.get(key) {
                safe_windows.insert(key.into(), window(value));
            }
        }
        safe.insert(
            "unifiedWindows".into(),
            serde_json::Value::Object(safe_windows),
        );
    }
    serde_json::from_value(serde_json::Value::Object(safe)).ok()
}

pub(super) fn init(service: &Entity<QuotaService>, project: &Entity<Project>, cx: &mut App) {
    sync(service, project, cx);
    let store = project.read(cx).agent_server_store().clone();
    let subscription = cx.subscribe(&store, {
        let service = service.downgrade();
        let project = project.downgrade();
        move |_, _: &AgentServersUpdated, cx| {
            if let (Some(service), Some(project)) = (service.upgrade(), project.upgrade()) {
                sync(&service, &project, cx);
            }
        }
    });
    service.update(cx, |service, _| service._subscriptions.push(subscription));
    cx.observe_new::<AcpThread>({
        let service = service.downgrade();
        move |thread, _, cx| {
            if thread.parent_session_id().is_some()
                || !thread.project().read(cx).is_local()
                || thread.connection().agent_id().0.as_ref() != "claude-acp"
            {
                return;
            }
            let connection = thread.connection().clone();
            let session = thread.session_id().to_string();
            let service = service.clone();
            cx.defer(move |cx| {
                let Some(service) = service.upgrade() else {
                    return;
                };
                let identity = Rc::as_ptr(&connection) as *const () as usize;
                let shared = shared_scope(cx);
                service.update(cx, |service, cx| {
                    if let Some((sessions, _)) = service.claude.readers.get(&identity) {
                        sessions.borrow_mut().insert(session);
                        return;
                    }
                    let subscription =
                        if let Some(connection) = connection.clone().downcast::<AcpConnection>() {
                            Some(connection.quota_messages())
                        } else {
                            connection
                                .clone()
                                .downcast::<AcpV2Connection>()
                                .map(|connection| connection.quota_messages())
                        };
                    let Some((backlog, receiver)) = subscription else {
                        return;
                    };
                    let scope = if shared {
                        SHARED_SCOPE.into()
                    } else {
                        format!("{}-{}", configured_scope(cx), uuid::Uuid::new_v4())
                    };
                    if !service.core.snapshot.accounts.contains_key(&scope) {
                        if service
                            .core
                            .inventory(
                                AccountScope {
                                    id: scope.clone(),
                                    provider: ProviderId::Claude,
                                    runtime: "local".into(),
                                    identity_known: false,
                                },
                                1,
                                Availability::Unknown,
                            )
                            .is_err()
                        {
                            return;
                        }
                        if shared {
                            service
                                .scheduler
                                .track(scope.clone(), ProviderId::Claude, 1);
                        }
                    }
                    let sessions = Rc::new(RefCell::new(BTreeSet::from([session])));
                    let generation = service
                        .core
                        .snapshot
                        .accounts
                        .get(&scope)
                        .map_or(1, |account| account.generation);
                    let latest = backlog
                        .into_iter()
                        .filter_map(|message| decode(message, &sessions.borrow()))
                        .next_back();
                    if let Some(data) = latest {
                        // Debug backlog has no timestamp; persisted provenance prevents replay from becoming fresh.
                        if let Ok(observation) = normalize_claude(
                            data,
                            &scope,
                            generation,
                            SourceKind::Persisted,
                            service.ports.clock.now(),
                            0,
                        ) {
                            if service.observe(observation, cx).is_err() {
                                log::debug!("crew_quota: invalid_claude_backlog");
                            }
                        }
                    }
                    let task = cx.spawn({
                        let sessions = sessions.clone();
                        async move |this, cx| {
                            while let Ok(message) = receiver.recv().await {
                                let data = decode(message, &sessions.borrow());
                                let Some(data) = data else {
                                    continue;
                                };
                                if this
                                    .update(cx, |service, cx| {
                                        let sequence = service
                                            .core
                                            .snapshot
                                            .accounts
                                            .get(&scope)
                                            .map_or(1, |account| {
                                                account.sequence.saturating_add(1)
                                            });
                                        match normalize_claude(
                                            data,
                                            &scope,
                                            generation,
                                            SourceKind::ClaudeAcpEvent,
                                            service.ports.clock.now(),
                                            sequence,
                                        ) {
                                            Ok(observation) => {
                                                if service.observe(observation, cx).is_err() {
                                                    log::debug!("crew_quota: invalid_claude_event");
                                                }
                                            }
                                            Err(_) => {
                                                log::debug!("crew_quota: invalid_claude_event")
                                            }
                                        }
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            if this
                                .update(cx, |service, _| {
                                    service.claude.readers.remove(&identity);
                                })
                                .is_err()
                            {
                                log::debug!("crew_quota: claude_service_closed");
                            }
                        }
                    });
                    service.claude.readers.insert(identity, (sessions, task));
                });
            });
        }
    })
    .detach();
}

fn sync(service: &Entity<QuotaService>, project: &Entity<Project>, cx: &mut App) {
    let eligible = shared_scope(cx);
    let status = project
        .read(cx)
        .agent_server_store()
        .clone()
        .update(cx, |store, cx| {
            store
                .get_external_agent(&AgentId::new("claude-acp"))
                .map(|agent| agent.install_status(cx))
        });
    let project = project.clone();
    service.update(cx, |service, cx| {
        service.claude.inventory = Some(cx.spawn(async move |this, cx| {
            let availability = match status {
                Some(status) => match status.await {
                    Ok(AgentInstallStatus::Installed { .. }) if eligible => Availability::Unknown,
                    Ok(AgentInstallStatus::NotInstalled) => Availability::NotInstalled,
                    _ => Availability::Unsupported(ErrorCode::UnsupportedVersion),
                },
                None => Availability::NotInstalled,
            };
            let attempt = cx.update(|cx| {
                QuotaService::global(cx).map(|service| {
                    service
                        .read(cx)
                        .ports
                        .store
                        .load(format!("attempt/{SHARED_SCOPE}"), cx)
                })
            });
            let attempt = match attempt {
                Some(attempt) => attempt.await,
                None => Ok(None),
            };
            if this
                .update(cx, |service, cx| {
                    if let Some(handle) = service.cancellations.remove(SHARED_SCOPE) {
                        handle.abort();
                    }
                    service.tasks.remove(SHARED_SCOPE);
                    let generation = service
                        .core
                        .snapshot
                        .accounts
                        .get(SHARED_SCOPE)
                        .map_or(1, |account| account.generation.saturating_add(1));
                    let scope = AccountScope {
                        id: SHARED_SCOPE.into(),
                        provider: ProviderId::Claude,
                        runtime: "local".into(),
                        identity_known: false,
                    };
                    if service
                        .core
                        .inventory(scope.clone(), generation, availability)
                        .is_err()
                    {
                        log::debug!("crew_quota: claude_inventory_invalid");
                        return;
                    }
                    service
                        .scheduler
                        .track(SHARED_SCOPE.into(), ProviderId::Claude, generation);
                    match attempt {
                        Ok(Some(record)) => {
                            match decode_attempt(&record, &scope, service.ports.clock.now()) {
                                Ok(attempt) => service.scheduler.restore_timing(
                                    SHARED_SCOPE,
                                    Some(attempt.at),
                                    0,
                                    0,
                                    service.ports.clock.now(),
                                    service.ports.clock.monotonic(),
                                ),
                                Err(_) => {
                                    service.persistence_blocked.insert(SHARED_SCOPE.into());
                                    service.persistence_failure(SHARED_SCOPE.into(), cx);
                                }
                            }
                        }
                        Err(_) => {
                            service.persistence_blocked.insert(SHARED_SCOPE.into());
                            service.persistence_failure(SHARED_SCOPE.into(), cx);
                        }
                        Ok(None) => {}
                    }
                    service.wake();
                    cx.notify();
                })
                .is_err()
            {
                log::debug!("crew_quota: claude_service_closed");
            }
            drop(project);
        }));
    });
}

pub(super) fn refresh(
    project: Entity<Project>,
    scope: String,
    generation: u64,
    sequence: u64,
    cx: &mut App,
) -> Task<Result<Observation, RefreshError>> {
    cx.spawn(async move |cx| {
        if scope != SHARED_SCOPE || !cx.update(|cx| shared_scope(cx)) { return Err(RefreshError::Unsupported); }
        let environment = project.read_with(cx, |project, _| project.environment().clone()).update(cx, |environment, cx| environment.default_environment(cx));
        let operation = async {
            let environment = environment.await.ok_or(RefreshError::Unsupported)?;
            if environment.contains_key("ANTHROPIC_API_KEY") || environment.contains_key("CLAUDE_CODE_USE_BEDROCK") || environment.contains_key("CLAUDE_CODE_USE_VERTEX") { return Err(RefreshError::Unsupported); }
            let token = match environment.get("CLAUDE_CODE_OAUTH_TOKEN").filter(|token| !token.is_empty()) {
                Some(token) => token.clone(),
                None => {
                    let directory = environment.get("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| paths::home_dir().join(".claude"));
                    // Stored credentials are detected by presence only, never opened.
                    if stored_auth_present(&directory).await? || smol::fs::metadata(directory.join(".credentials.json")).await.is_ok() { return Err(RefreshError::Failed(ErrorCode::StoredAuthUnsupported)); }
                    return Err(RefreshError::NotLoggedIn);
                }
            };
            let client = cx.update(|cx| cx.http_client());
            let (observation, retry_at) = probe(client, token, &scope, generation, sequence).await?;
            if let Some(retry_at) = retry_at {
                cx.update(|cx| { if let Some(service) = QuotaService::global(cx) { service.update(cx, |service, cx| { if service.observe(observation.clone(), cx).is_err() { log::debug!("crew_quota: invalid_claude_headers"); } }); } });
                return Err(RefreshError::Deferred { retry_at });
            }
            Ok(observation)
        }.fuse();
        let timeout = cx.background_executor().timer(Duration::from_secs(30)).fuse();
        futures::pin_mut!(operation, timeout);
        futures::select_biased! { result = operation => result, _ = timeout => Err(RefreshError::Failed(ErrorCode::Timeout)) }
    })
}

pub(super) async fn probe(
    client: Arc<dyn http_client::HttpClient>,
    token: String,
    scope: &str,
    generation: u64,
    sequence: u64,
) -> Result<(Observation, Option<Timestamp>), RefreshError> {
    let mut authorization = http_client::http::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
    authorization.set_sensitive(true);
    let request = http_client::Request::builder().method("POST").uri("https://api.anthropic.com/v1/messages")
        .follow_redirects(RedirectPolicy::NoFollow).timeout(Duration::from_secs(30))
        .header("authorization", authorization)
        .header("anthropic-version", "2023-06-01").header("anthropic-beta", "oauth-2025-04-20").header("content-type", "application/json")
        .body(http_client::AsyncBody::from(r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"."}]}"#))
        .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
    let response = client
        .send(request)
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
    let now = chrono::Utc::now().timestamp();
    let status = response.status().as_u16();
    if status == 401 || status == 403 || status == 400 {
        return Err(RefreshError::Failed(ErrorCode::InferenceAccess));
    }
    let headers = response.headers();
    let retry = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| retry_after(value, now));
    let window = |prefix: &str| -> Result<Option<ClaudeWindow>, RefreshError> {
        let get = |suffix: &str| {
            headers
                .get(format!("anthropic-ratelimit-unified-{prefix}-{suffix}"))
                .and_then(|value| value.to_str().ok())
        };
        let utilization = get("utilization")
            .map(str::parse::<f64>)
            .transpose()
            .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
        let resets_at = get("reset")
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
        let admission = get("status")
            .and_then(|value| serde_json::from_value(serde_json::Value::String(value.into())).ok());
        Ok(
            (utilization.is_some() || resets_at.is_some() || admission.is_some()).then_some(
                ClaudeWindow {
                    utilization,
                    resets_at,
                    status: admission,
                },
            ),
        )
    };
    let data = ClaudeRateLimit {
        unified_windows: Some(ClaudeWindows {
            five_hour: window("5h")?,
            seven_day: window("7d")?,
        }),
        overage_status: window("overage")?.and_then(|window| window.status),
        ..Default::default()
    };
    let mut observation = normalize_claude(
        data,
        scope,
        generation,
        SourceKind::ClaudeHeaderProbe,
        now,
        sequence.saturating_add(1),
    )
    .map_err(RefreshError::Failed)?;
    observation.start_sequence = Some(sequence);
    // Read through a cap without retaining generated content.
    let mut body = response.into_body().take((MAX_RECORD_BYTES + 1) as u64);
    let mut consumed = 0;
    let mut buffer = [0; 8192];
    loop {
        let count = body
            .read(&mut buffer)
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
        if count == 0 {
            break;
        }
        consumed += count;
        if consumed > MAX_RECORD_BYTES {
            return Err(RefreshError::Failed(ErrorCode::InvalidData));
        }
    }
    if status == 429 {
        let reset = observation
            .windows
            .iter()
            .filter(|window| window.admission == Patch::Set(Admission::Exhausted))
            .filter_map(|window| match window.reset_at {
                Patch::Set(reset) => reset,
                _ => None,
            })
            .min()
            .unwrap_or(now);
        return Ok((
            observation,
            Some(retry.unwrap_or(now).max(reset).max(now.saturating_add(120))),
        ));
    }
    if !(200..300).contains(&status) || observation.windows.is_empty() {
        return Err(RefreshError::Failed(ErrorCode::Protocol));
    }
    Ok((observation, None))
}

async fn stored_auth_present(directory: &std::path::Path) -> Result<bool, RefreshError> {
    #[cfg(target_os = "macos")]
    {
        use sha2::{Digest as _, Sha256};
        let service = if directory == paths::home_dir().join(".claude") {
            "Claude Code-credentials".to_string()
        } else {
            format!(
                "Claude Code-credentials-{:x}",
                Sha256::digest(directory.as_os_str().as_encoded_bytes())
            )
            .chars()
            .take("Claude Code-credentials-".len() + 8)
            .collect()
        };
        // Without -w/-g, security queries only item metadata and cannot return the password.
        let status = async_process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", &service])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::StoredAuthUnsupported))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(44) => Ok(false),
            _ => Err(RefreshError::Failed(ErrorCode::StoredAuthUnsupported)),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _directory = directory;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/claude-passive.sanitized.json"
        ))
        .expect("sanitized fixture")
    }
    fn message(params: serde_json::Value) -> AcpDebugMessage {
        AcpDebugMessage {
            direction: AcpDebugMessageDirection::Incoming,
            message: AcpDebugMessageContent::Notification {
                method: "session/update".into(),
                params: Some(params),
            },
        }
    }

    #[test]
    fn debug_decoder_filters_sessions_direction_and_context_usage() {
        let sessions = BTreeSet::from(["fixture".into()]);
        let decoded = decode(message(fixture()), &sessions).expect("quota metadata");
        let observation =
            normalize_claude(decoded, "scope", 1, SourceKind::ClaudeAcpEvent, 1000, 1)
                .expect("valid windows");
        assert_eq!(observation.windows.len(), 3);
        assert_eq!(
            observation.windows.first().expect("5h").used_fraction,
            Patch::Set(Some(0.28))
        );
        assert_eq!(
            observation.windows.last().expect("overage").used_fraction,
            Patch::Missing
        );
        assert!(decode(message(fixture()), &BTreeSet::new()).is_none());
        let mut outgoing = message(fixture());
        outgoing.direction = AcpDebugMessageDirection::Outgoing;
        assert!(decode(outgoing, &sessions).is_none());
        let mut context_only = fixture();
        context_only["update"]["_meta"] = serde_json::json!({});
        assert!(decode(message(context_only), &sessions).is_none());
    }

    #[test]
    fn partial_over_cap_and_backlog_never_freshen_other_windows() {
        let mut data = fixture();
        data["update"]["_meta"]["_claude/rateLimit"] =
            serde_json::json!({"rateLimitType":"five_hour","utilization":1.2});
        let data = decode(message(data), &BTreeSet::from(["fixture".into()])).expect("partial");
        let observation = normalize_claude(data, "scope", 1, SourceKind::Persisted, 1000, 1)
            .expect("over cap valid");
        assert!(!observation.complete);
        let mut core = QuotaCore::default();
        core.inventory(
            AccountScope {
                id: "scope".into(),
                provider: ProviderId::Claude,
                runtime: "local".into(),
                identity_known: false,
            },
            1,
            Availability::Unknown,
        )
        .expect("inventory");
        core.apply(observation, 1000).expect("apply");
        let window = core.snapshot.accounts["scope"]
            .windows
            .values()
            .next()
            .expect("window");
        assert_eq!(window.remaining_fraction(), Some(0.0));
        assert!(matches!(
            window.freshness(1000, Duration::from_secs(120)),
            Freshness::Stale {
                reason: StaleReason::Persisted,
                ..
            }
        ));
    }

    #[gpui::test]
    async fn probe_has_exact_request_and_normalizes_headers(_cx: &mut gpui::TestAppContext) {
        let client = http_client::FakeHttpClient::create(|request| async move {
            assert_eq!(request.uri(), "https://api.anthropic.com/v1/messages");
            assert_eq!(request.method(), "POST");
            assert!(request.headers()["authorization"].is_sensitive());
            assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
            assert_eq!(request.headers()["anthropic-beta"], "oauth-2025-04-20");
            assert_eq!(
                request.extensions().get::<RedirectPolicy>(),
                Some(&RedirectPolicy::NoFollow)
            );
            let mut bytes = Vec::new();
            request.into_body().read_to_end(&mut bytes).await?;
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes)?,
                serde_json::json!({"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"."}]})
            );
            Ok(http_client::Response::builder()
                .status(200)
                .header("anthropic-ratelimit-unified-5h-utilization", "0.28")
                .header("anthropic-ratelimit-unified-5h-reset", "1793800200")
                .header("anthropic-ratelimit-unified-7d-utilization", "0.17")
                .header("anthropic-ratelimit-unified-overage-status", "rejected")
                .body(http_client::AsyncBody::from("discard generated content"))?)
        });
        let (result, retry) = probe(client, "fake-token".into(), "scope", 1, 4)
            .await
            .expect("probe");
        assert!(retry.is_none());
        assert_eq!(result.windows.len(), 3);
        assert!(result.complete);
        assert_eq!(
            result.windows.first().expect("5h").used_fraction,
            Patch::Set(Some(0.28))
        );
    }

    #[gpui::test]
    async fn forbidden_and_rate_limited_are_single_attempts(_cx: &mut gpui::TestAppContext) {
        for status in [401, 403, 400, 429, 500] {
            let client = http_client::FakeHttpClient::create(move |_| async move {
                Ok(http_client::Response::builder()
                    .status(status)
                    .header("retry-after", "600")
                    .header("anthropic-ratelimit-unified-5h-utilization", "1.2")
                    .body(http_client::AsyncBody::empty())?)
            });
            let result = probe(client, "fake-token".into(), "scope", 1, 0).await;
            match status {
                401 | 403 | 400 => assert!(matches!(
                    result,
                    Err(RefreshError::Failed(ErrorCode::InferenceAccess))
                )),
                429 => {
                    let (observation, retry) = result.expect("429 headers retained");
                    assert_eq!(observation.windows.len(), 1);
                    assert!(retry.is_some_and(|time| time >= chrono::Utc::now().timestamp() + 599));
                }
                _ => assert!(result.is_err()),
            }
        }
    }

    #[test]
    fn scheduler_requires_demand_floor_backoff_and_one_billable_operation() {
        let mut scheduler = Scheduler::default();
        scheduler.track("first".into(), ProviderId::Claude, 1);
        scheduler.track("second".into(), ProviderId::Claude, 1);
        assert!(scheduler.start_due(1000, 0).is_empty());
        scheduler.demand("first", 2000);
        scheduler.demand("second", 2000);
        let started = scheduler.start_due(1000, 0);
        assert_eq!(started.len(), 1);
        let scope = &started[0].0;
        scheduler.complete(scope, 1000, 0, false, Some(1600));
        scheduler.force(scope);
        assert!(
            scheduler
                .start_due(1100, 100)
                .iter()
                .all(|(candidate, _)| candidate != scope)
        );
        assert!(Scheduler::next_for(&scheduler.scopes[scope], 1120, 120) >= 1600);
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    #[test]
    fn default_registry_scope_is_shared_and_overrides_are_isolated() {
        assert_eq!(
            auth_scope(true, Vec::new(), false, 0, "shell-oauth"),
            SHARED_SCOPE
        );
        for scope in [
            auth_scope(false, Vec::new(), false, 0, "shell-oauth"),
            auth_scope(
                true,
                vec!["CLAUDE_CODE_OAUTH_TOKEN".into()],
                false,
                0,
                "shell-oauth",
            ),
            auth_scope(true, Vec::new(), true, 0, "shell-oauth"),
            auth_scope(true, Vec::new(), false, 1, "shell-oauth"),
            auth_scope(true, Vec::new(), false, 0, "stored"),
        ] {
            assert_ne!(scope, SHARED_SCOPE);
            assert!(!scope.contains("TOKEN"));
        }
        assert_eq!(
            auth_scope(true, vec!["B".into(), "A".into()], false, 0, "shell-oauth"),
            auth_scope(true, vec!["A".into(), "B".into()], false, 0, "shell-oauth")
        );
    }
}

#[cfg(test)]
mod feed_contracts {
    use super::*;
    struct FakeConnection {
        backlog: RefCell<Vec<AcpDebugMessage>>,
        receiver: async_channel::Receiver<AcpDebugMessage>,
    }
    impl DebugFeed for FakeConnection {
        fn quota_messages(
            &self,
        ) -> (
            Vec<AcpDebugMessage>,
            async_channel::Receiver<AcpDebugMessage>,
        ) {
            (self.backlog.take(), self.receiver.clone())
        }
    }
    async fn check_feed<Connection: DebugFeed>(
        connection: &Connection,
        sender: async_channel::Sender<AcpDebugMessage>,
    ) {
        let (backlog, receiver) = connection.quota_messages();
        let sessions = BTreeSet::from(["fixture".into()]);
        assert_eq!(backlog.len(), 1);
        assert!(decode(backlog.into_iter().next().expect("backlog"), &sessions).is_some());
        let params = serde_json::from_str(include_str!(
            "../../tests/fixtures/claude-passive.sanitized.json"
        ))
        .expect("fixture");
        sender
            .send(AcpDebugMessage {
                direction: AcpDebugMessageDirection::Incoming,
                message: AcpDebugMessageContent::Notification {
                    method: "session/update".into(),
                    params: Some(params),
                },
            })
            .await
            .expect("send live");
        assert!(decode(receiver.recv().await.expect("live"), &sessions).is_some());
        drop(sender);
        assert!(receiver.recv().await.is_err());
    }
    fn fake() -> (FakeConnection, async_channel::Sender<AcpDebugMessage>) {
        let (sender, receiver) = async_channel::unbounded();
        let params = serde_json::from_str(include_str!(
            "../../tests/fixtures/claude-passive.sanitized.json"
        ))
        .expect("fixture");
        (
            FakeConnection {
                backlog: RefCell::new(vec![AcpDebugMessage {
                    direction: AcpDebugMessageDirection::Incoming,
                    message: AcpDebugMessageContent::Notification {
                        method: "session/update".into(),
                        params: Some(params),
                    },
                }]),
                receiver,
            },
            sender,
        )
    }
    #[gpui::test]
    async fn v1_debug_feed_contract(_cx: &mut gpui::TestAppContext) {
        fn accepts_v1<Connection: DebugFeed>() {}
        accepts_v1::<AcpConnection>();
        let (connection, sender) = fake();
        check_feed(&connection, sender).await;
    }
    #[gpui::test]
    async fn v2_debug_feed_contract(_cx: &mut gpui::TestAppContext) {
        fn accepts_v2<Connection: DebugFeed>() {}
        accepts_v2::<AcpV2Connection>();
        let (connection, sender) = fake();
        check_feed(&connection, sender).await;
    }
}
