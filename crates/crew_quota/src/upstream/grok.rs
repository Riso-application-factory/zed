use super::*;
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use std::sync::Mutex;

#[derive(Clone)]
struct Target {
    scope: String,
    generation: u64,
    gate_reason: Option<String>,
}
struct HeaderObservation {
    quota: Observation,
    retry_at: Option<Timestamp>,
}
struct Bridge {
    target: Arc<Mutex<BTreeMap<String, Target>>>,
    sender: async_channel::Sender<HeaderObservation>,
    receiver: async_channel::Receiver<HeaderObservation>,
}
impl Global for Bridge {}

#[derive(Clone)]
pub(super) struct AgentProbe;

struct Observer {
    inner: Arc<dyn HttpClient>,
    target: Arc<Mutex<BTreeMap<String, Target>>>,
    sender: async_channel::Sender<HeaderObservation>,
}
impl HttpClient for Observer {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.inner.user_agent()
    }
    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }
    fn send(
        &self,
        mut request: Request<AsyncBody>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        if request.extensions_mut().remove::<AgentProbe>().is_some() {
            return self.inner.send(request);
        }
        let provider = match request.uri().host() {
            Some("api.x.ai") => "x_ai",
            Some("api.anthropic.com") => "anthropic",
            Some("api.openai.com") => "openai",
            _ => "other",
        };
        let host_category = match request.uri().host() {
            Some("api.x.ai") => "xai",
            Some("api.anthropic.com") => "anthropic",
            Some("api.openai.com") => "openai",
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]") => "localhost",
            _ => "other",
        };
        log::debug!("crew_quota: http_send host_category={host_category}");
        if trusted_host(&request).is_none() {
            return self.inner.send(request);
        }
        let endpoint = inference_endpoint(&request).map(str::to_owned);
        if endpoint.is_none() {
            log::debug!(
                "crew_quota: native_capture provider={provider} outcome=Unmatched path_kind=Other"
            );
            return self.inner.send(request);
        }
        let target = endpoint
            .as_ref()
            .and_then(|_| self.target.lock().ok()?.get(request.uri().host()?).cloned());
        log::debug!("crew_quota: native_capture provider={provider} outcome=Eligible");
        let target = match target {
            Some(target) if target.gate_reason.is_none() => Some(target),
            target => {
                let reason = target
                    .as_ref()
                    .and_then(|target| target.gate_reason.as_deref())
                    .unwrap_or("NoCaptureTarget");
                log::debug!(
                    "crew_quota: native_capture provider={provider} outcome=Gated reason={reason}"
                );
                None
            }
        };
        let host = request.uri().host().unwrap_or_default().to_owned();
        let future = self.inner.send(request);
        let sender = self.sender.clone();
        Box::pin(async move {
            let mut response = future.await.inspect_err(|_| {
                if endpoint.is_some() {
                    log::debug!(
                        "crew_quota: native_capture provider={provider} outcome=TransportError"
                    );
                }
            })?;
            if let (Some(endpoint), Some(target)) = (endpoint, target) {
                let now = chrono::Utc::now().timestamp();
                if response.status().is_redirection() {
                    log::debug!(
                        "crew_quota: native_capture provider={provider} outcome=Skipped reason=Redirect"
                    );
                    return Ok(response);
                }
                let windows =
                    parse_dialect(response.headers(), &target.scope, &endpoint, &host, now);
                let status = response.status().as_u16();
                let blocked = classify_response(&mut response, &host).await;
                if let Some(reason) = blocked {
                    log::debug!(
                        "crew_quota: native_capture provider={provider} outcome=Blocked reason={reason:?}"
                    );
                }
                let observation = Observation {
                    scope: target.scope,
                    generation: target.generation,
                    source: SourceKind::GrokResponseHeaders,
                    source_version: format!("{host}-headers-v2"),
                    observed_at: now,
                    provider_sample_at: None,
                    sequence: 0,
                    start_sequence: None,
                    windows,
                    complete: false,
                    metadata: AccountMetadata {
                        grok_response: Some(GrokResponse {
                            blocked,
                            status,
                            observed_at: now,
                            retry_at: retry_after(response.headers(), now),
                        }),
                        ..Default::default()
                    },
                };
                // Losing telemetry must never apply backpressure to inference.
                match sender.try_send(HeaderObservation {
                    quota: observation,
                    retry_at: retry_after(response.headers(), now),
                }) {
                    Ok(()) => {
                        log::debug!("crew_quota: native_capture provider={provider} outcome=Queued")
                    }
                    Err(_) => {
                        log::debug!(
                            "crew_quota: native_capture provider={provider} outcome=Dropped"
                        )
                    }
                }
            }
            Ok(response)
        })
    }
}

#[derive(serde::Deserialize)]
struct CodeBody {
    code: Option<String>,
}
#[derive(serde::Deserialize)]
struct NestedCodeBody {
    error: Option<CodeBody>,
}

#[allow(
    clippy::disallowed_methods,
    reason = "The HTTP decorator runs independently of a GPUI dispatcher and must enforce a wall-clock deadline."
)]
pub(super) async fn classify_response(
    response: &mut Response<AsyncBody>,
    host: &str,
) -> Option<GrokBlockReason> {
    use futures::AsyncReadExt as _;
    let status = response.status();
    if status.is_success() {
        return None;
    }
    let fallback = if status.as_u16() == 429 {
        Some(GrokBlockReason::RateLimited)
    } else if matches!(status.as_u16(), 401 | 403) {
        Some(GrokBlockReason::Forbidden)
    } else {
        None
    };
    if response.headers().get("content-type").is_some_and(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.starts_with("text/event-stream"))
    }) {
        return fallback;
    }
    let mut body = std::mem::replace(response.body_mut(), AsyncBody::empty());
    let mut prefix = Vec::new();
    // Retain the unread tail and replay even read errors to preserve the caller's response.
    let mut exhausted = false;
    let mut deadline = Box::pin(smol::Timer::after(Duration::from_millis(100)));
    while prefix.len() < 8192 {
        let mut buffer = [0; 1024];
        let length = (8192 - prefix.len()).min(buffer.len());
        let read = Box::pin(body.read(&mut buffer[..length]));
        let result = match futures::future::select(read, deadline).await {
            futures::future::Either::Left((result, remaining)) => {
                deadline = remaining;
                result
            }
            futures::future::Either::Right(_) => break,
        };
        match result {
            Ok(0) => {
                exhausted = true;
                break;
            }
            Ok(length) => prefix.extend_from_slice(&buffer[..length]),
            Err(error) => {
                let replay = futures::stream::once(async move { Err::<Vec<u8>, _>(error) });
                use futures::TryStreamExt as _;
                *response.body_mut() = AsyncBody::from_reader(
                    futures::io::Cursor::new(prefix)
                        .chain(replay.into_async_read())
                        .chain(body),
                );
                return fallback;
            }
        }
    }
    let reason = if exhausted {
        let code = if host == "api.x.ai" {
            serde_json::from_slice::<CodeBody>(&prefix)
                .ok()
                .and_then(|value| value.code)
        } else {
            serde_json::from_slice::<NestedCodeBody>(&prefix)
                .ok()
                .and_then(|value| value.error)
                .and_then(|error| error.code)
        };
        code.filter(|code| match host {
            "api.x.ai" => code.contains("spending-limit"),
            "api.openai.com" => code == "insufficient_quota",
            "api.anthropic.com" => code == "credit_balance_too_low",
            _ => false,
        })
        .map(|_| GrokBlockReason::SpendingLimit)
        .or(fallback)
    } else {
        fallback
    };
    *response.body_mut() = AsyncBody::from_reader(futures::io::Cursor::new(prefix).chain(body));
    reason
}

fn capture_provider(observation: &Observation) -> &'static str {
    if observation.source_version.starts_with("api.anthropic.com") {
        "anthropic"
    } else if observation.source_version.starts_with("api.openai.com") {
        "openai"
    } else {
        "x_ai"
    }
}

fn trusted_host(request: &Request<AsyncBody>) -> Option<&str> {
    let uri = request.uri();
    if uri.scheme_str() != Some("https") || !uri.port_u16().is_none_or(|port| port == 443) {
        return None;
    }
    uri.host()
        .filter(|host| matches!(*host, "api.x.ai" | "api.anthropic.com" | "api.openai.com"))
}

fn provider_label(host: &str) -> &str {
    match host {
        "api.x.ai" => "xAI",
        "api.anthropic.com" => "Anthropic",
        "api.openai.com" => "OpenAI",
        _ => host,
    }
}
#[cfg(test)]
fn is_xai_host(request: &Request<AsyncBody>) -> bool {
    trusted_host(request) == Some("api.x.ai")
}
fn inference_endpoint(request: &Request<AsyncBody>) -> Option<&str> {
    let host = trusted_host(request)?;
    (request.method() == http_client::Method::POST
        && match host {
            "api.anthropic.com" => request.uri().path() == "/v1/messages",
            _ => matches!(
                request.uri().path(),
                "/v1/chat/completions"
                    | "/v1//chat/completions"
                    | "/v1/completions"
                    | "/v1/responses"
                    | "/v1/embeddings"
            ),
        })
    .then_some(request.uri().path())
}

fn absolute_date(text: &str) -> Option<Timestamp> {
    if text.len() > 64 {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .or_else(|_| chrono::DateTime::parse_from_rfc2822(text))
        .ok()
        .map(|date| date.timestamp())
}
fn retry_after(headers: &http_client::http::HeaderMap, now: Timestamp) -> Option<Timestamp> {
    let text = headers.get("retry-after")?.to_str().ok()?;
    if text.len() > 64 {
        return None;
    }
    text.parse::<u32>()
        .ok()
        .map(|seconds| now.saturating_add(i64::from(seconds)))
        .or_else(|| absolute_date(text))
}

#[cfg(test)]
fn parse(headers: &http_client::http::HeaderMap, scope: &str, endpoint: &str) -> Vec<WindowPatch> {
    parse_dialect(
        headers,
        scope,
        endpoint,
        "api.x.ai",
        chrono::Utc::now().timestamp(),
    )
}
fn duration_reset(text: &str, now: Timestamp) -> Option<Timestamp> {
    if text.len() > 64 || text.is_empty() {
        return None;
    }
    let mut rest = text;
    let mut seconds = 0.0;
    while !rest.is_empty() {
        let end = rest.find(|character: char| !character.is_ascii_digit() && character != '.')?;
        let value = rest.get(..end)?.parse::<f64>().ok()?;
        rest = rest.get(end..)?;
        let (unit, scale) = if rest.starts_with("ms") {
            (2, 0.001)
        } else if rest.starts_with('s') {
            (1, 1.0)
        } else if rest.starts_with('m') {
            (1, 60.0)
        } else if rest.starts_with('h') {
            (1, 3600.0)
        } else {
            return None;
        };
        seconds += value * scale;
        rest = rest.get(unit..)?;
    }
    (seconds.is_finite() && seconds >= 0.0 && seconds < i64::MAX as f64)
        .then(|| now.checked_add(seconds.ceil() as i64))
        .flatten()
}
pub(super) fn parse_dialect(
    headers: &http_client::http::HeaderMap,
    scope: &str,
    endpoint: &str,
    host: &str,
    now: Timestamp,
) -> Vec<WindowPatch> {
    let number = |name: &str| {
        let text = headers.get(name)?.to_str().ok()?;
        if text.len() > 32 {
            return None;
        }
        let value = text.parse::<f64>().ok()?;
        (value.is_finite() && value >= 0.0).then_some(value)
    };
    ["requests", "tokens", "input-tokens", "output-tokens"]
        .into_iter()
        .filter(|dimension| host == "api.anthropic.com" || !dimension.contains('-'))
        .map(|dimension| {
            let header = |field: &str| {
                if host == "api.anthropic.com" {
                    format!("anthropic-ratelimit-{dimension}-{field}")
                } else {
                    format!("x-ratelimit-{field}-{dimension}")
                }
            };
            let limit = number(&header("limit"));
            let remaining = number(&header("remaining"));
            let unit = if dimension.ends_with("tokens") {
                Unit::Tokens
            } else {
                Unit::Requests
            };
            WindowPatch {
                key: WindowKey {
                    bucket: BucketKey {
                        scope: scope.into(),
                        bucket: format!("{host}{endpoint}/{dimension}"),
                        applicability: Applicability::Family(format!(
                            "Latest {} API response · Model unverified ({endpoint})",
                            provider_label(host)
                        )),
                    },
                    role: dimension.into(),
                    duration_seconds: None,
                },
                used_fraction: Patch::Set(
                    limit
                        .zip(remaining)
                        .filter(|(limit, remaining)| *limit > 0.0 && remaining <= limit)
                        .map(|(limit, remaining)| 1.0 - remaining / limit),
                ),
                remaining: Patch::Set(remaining.map(|value| Amount {
                    value,
                    unit: unit.clone(),
                })),
                limit: Patch::Set(limit.map(|value| Amount { value, unit })),
                // Only absolute dates are unambiguous; unknown bucket-relative formats stay unknown.
                reset_at: Patch::Set(
                    headers
                        .get(header("reset"))
                        .and_then(|value| value.to_str().ok())
                        .and_then(|text| {
                            if host == "api.openai.com" {
                                duration_reset(text, now)
                            } else {
                                absolute_date(text)
                            }
                        }),
                ),
                admission: Patch::Set(Admission::Unknown),
                disabled: Patch::Missing,
                precision: Precision::Exact,
            }
        })
        .filter(|window| {
            !matches!(window.remaining, Patch::Set(None))
                || !matches!(window.limit, Patch::Set(None))
        })
        .collect()
}

pub fn install_http_observer(cx: &mut App) {
    if cx.has_global::<Bridge>() {
        return;
    }
    log::info!("crew_quota: http_observer=Installed");
    let (sender, receiver) = async_channel::bounded(128);
    let target = Arc::new(Mutex::new(BTreeMap::new()));
    let inner = cx.http_client();
    cx.set_http_client(Arc::new(Observer {
        inner,
        target: target.clone(),
        sender: sender.clone(),
    }));
    cx.set_global(Bridge {
        target,
        sender,
        receiver,
    });
}

pub(super) fn sync(service: &Entity<QuotaService>, cx: &mut App) {
    if cx.try_global::<Bridge>().is_none() {
        return;
    }
    super::native::sync(service, cx);
    if let Some(bridge) = cx.try_global::<Bridge>() {
        let service = service.read(cx);
        let target = service
            .native_inventory
            .values()
            .filter_map(|candidate| {
                let host = candidate.capture_host.or(match candidate.id.as_str() {
                    "x_ai" => Some("api.x.ai"),
                    "anthropic" => Some("api.anthropic.com"),
                    "openai" => Some("api.openai.com"),
                    _ => None,
                })?;
                let account = service
                    .core
                    .snapshot
                    .accounts
                    .get(&format!("llm-provider:{}", candidate.id))?;
                Some((
                    host.to_owned(),
                    Target {
                        scope: account.scope.id.clone(),
                        generation: account.generation,
                        gate_reason: candidate.capture_host.is_none().then(|| {
                            candidate
                                .gate_reason
                                .clone()
                                .unwrap_or_else(|| "EndpointPolicy".into())
                        }),
                    },
                ))
            })
            .collect();
        match bridge.target.lock() {
            Ok(mut current) => *current = target,
            Err(_) => log::warn!("crew_quota: native_targets outcome=LockFailed"),
        }
    }
}

async fn buffer_until_ready(
    ready: impl std::future::Future<Output = Result<(), ErrorCode>>,
    receiver: &async_channel::Receiver<HeaderObservation>,
) -> Result<BTreeMap<String, HeaderObservation>, ErrorCode> {
    let mut pending = BTreeMap::new();
    let mut ready = Box::pin(ready);
    loop {
        match futures::future::select(ready, Box::pin(receiver.recv())).await {
            futures::future::Either::Left((result, _)) => {
                if result.is_err() {
                    return Err(ErrorCode::Persistence);
                }
                break;
            }
            futures::future::Either::Right((result, remaining)) => {
                ready = remaining;
                let Ok(headers) = result else {
                    return Err(ErrorCode::Persistence);
                };
                let provider = capture_provider(&headers.quota);
                if pending.len() < MAX_SCOPES || pending.contains_key(&headers.quota.scope) {
                    pending.insert(headers.quota.scope.clone(), headers);
                }
                log::debug!("crew_quota: native_capture provider={provider} outcome=Buffered");
            }
        }
    }
    Ok(pending)
}

pub(super) fn init(service: &Entity<QuotaService>, cx: &mut App) {
    if cx.try_global::<Bridge>().is_none() {
        return;
    }
    super::native::load_credentials(cx);
    sync(service, cx);
    let registry = language_model::LanguageModelRegistry::global(cx);
    let subscription = cx.subscribe(&registry, {
        let service = service.downgrade();
        move |_, event, cx| {
            if matches!(
                event,
                language_model::Event::AddedProvider(_) | language_model::Event::ProvidersChanged
            ) {
                super::native::load_credentials(cx);
            }
            if matches!(
                event,
                language_model::Event::ProviderStateChanged(_)
                    | language_model::Event::AddedProvider(_)
                    | language_model::Event::RemovedProvider(_)
                    | language_model::Event::ProvidersChanged
            ) && let Some(service) = service.upgrade()
            {
                sync(&service, cx);
            }
        }
    });
    let settings = cx.observe_global::<SettingsStore>({
        let service = service.downgrade();
        move |cx| {
            super::native::load_credentials(cx);
            if let Some(service) = service.upgrade() {
                sync(&service, cx);
            }
        }
    });
    let receiver = cx.global::<Bridge>().receiver.clone();
    // Keep the channel alive only as long as the quota service.
    let sender = cx.global::<Bridge>().sender.clone();
    service.update(cx, |service, cx| {
        service._subscriptions.extend([subscription, settings]);
        service.grok_task = Some(cx.spawn(async move |this, cx| {
            let _sender = sender;
            let ready = this.read_with(cx, |service, _| service.ready());
            let Ok(ready) = ready else {
                return;
            };
            let Ok(pending) = buffer_until_ready(ready, &receiver).await else {
                return;
            };
            let mut pending = pending.into_values();
            loop {
                let headers = match pending.next() {
                    Some(headers) => headers,
                    None => match receiver.recv().await {
                        Ok(headers) => headers,
                        Err(_) => break,
                    },
                };
                let mut observation = headers.quota;
                if this
                    .update(cx, |service, cx| {
                        if !service.hydrated {
                            return;
                        }
                        if let Some(account) =
                            service.core.snapshot.accounts.get(&observation.scope)
                        {
                            observation.sequence = account.sequence.saturating_add(1);
                        }
                        let provider = capture_provider(&observation);
                        let scope = observation.scope.clone();
                        let generation = observation.generation;
                        log::debug!(
                            "crew_quota: native_capture provider={provider} outcome=Received"
                        );
                        if validate_observation(&observation, service.ports.clock.now()).is_err() {
                            log::warn!("crew_quota: native_observation_invalid");
                            return;
                        }
                        if let Some(account) = service.core.snapshot.accounts.get_mut(&scope)
                            && account.generation == generation
                        {
                            account.windows.clear();
                        }
                        if service.observe(observation, cx).is_err() {
                            log::warn!("crew_quota: xai_observation_invalid");
                        }
                        if let Some(account) = service.core.snapshot.accounts.get_mut(&scope)
                            && account.generation == generation
                        {
                            account.refresh =
                                headers.retry_at.map_or(RefreshState::Idle, |retry_at| {
                                    RefreshState::Backoff { retry_at }
                                });
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_probe_bypasses_capture_but_native_anthropic_is_observed() -> anyhow::Result<()> {
        smol::block_on(async {
            let (sender, receiver) = async_channel::bounded(2);
            let observer = Observer {
                inner: http_client::FakeHttpClient::create(|request| async move {
                    assert!(request.extensions().get::<AgentProbe>().is_none());
                    Ok(Response::builder().status(200).body(AsyncBody::empty())?)
                }),
                target: Arc::new(Mutex::new(BTreeMap::from([(
                    "api.anthropic.com".into(),
                    Target {
                        scope: "native/anthropic".into(),
                        generation: 1,
                        gate_reason: None,
                    },
                )]))),
                sender,
            };
            // Hold the target lock: a probe must bypass even capture eligibility and its logs.
            let response = {
                let _target = observer.target.lock().expect("target lock");
                observer.send(
                    Request::post("https://api.anthropic.com/v1/messages")
                        .extension(AgentProbe)
                        .body(AsyncBody::empty())?,
                )
            };
            response.await?;
            assert!(receiver.try_recv().is_err());
            observer
                .send(
                    Request::post("https://api.anthropic.com/v1/messages")
                        .body(AsyncBody::empty())?,
                )
                .await?;
            assert_eq!(receiver.try_recv()?.quota.scope, "native/anthropic");
            Ok(())
        })
    }

    use super::*;
    use futures::{AsyncReadExt as _, StreamExt as _};
    use settings::Settings;

    #[test]
    fn target_reconciliation_contention_does_not_gate_capture() {
        let targets = Arc::new(Mutex::new(BTreeMap::from([(
            "api.x.ai".into(),
            Target {
                scope: "test".into(),
                generation: 1,
                gate_reason: None,
            },
        )])));
        let (locked_sender, locked_receiver) = std::sync::mpsc::channel();
        let reconciliation = std::thread::spawn({
            let targets = targets.clone();
            move || {
                let guard = targets.lock().expect("target lock");
                locked_sender.send(()).expect("locked signal");
                std::thread::sleep(Duration::from_millis(50));
                drop(guard);
            }
        });
        locked_receiver.recv().expect("reconciliation holds lock");
        let (sender, receiver) = async_channel::bounded(1);
        let observer = Observer {
            inner: http_client::FakeHttpClient::create(|_| async {
                Ok(Response::builder().status(429).body(AsyncBody::empty())?)
            }),
            target: targets,
            sender,
        };
        smol::block_on(
            observer.send(
                Request::post("https://api.x.ai/v1/chat/completions")
                    .body(AsyncBody::empty())
                    .expect("request"),
            ),
        )
        .expect("response");
        reconciliation.join().expect("reconciliation");
        let observation = receiver
            .try_recv()
            .expect("capture survives reconciliation");
        assert_eq!(
            observation
                .quota
                .metadata
                .grok_response
                .and_then(|response| response.blocked),
            Some(GrokBlockReason::RateLimited)
        );
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "Exercises the transport wall-clock deadline without GPUI scheduling."
    )]
    fn provider_errors_replay_bytes_and_slow_tails() -> anyhow::Result<()> {
        smol::block_on(async {
            for (status, host, bytes, expected) in [
                (
                    400u16,
                    "api.openai.com",
                    r#"{"error":{"code":"insufficient_quota","message":"private"}}"#,
                    Some(GrokBlockReason::SpendingLimit),
                ),
                (
                    429,
                    "api.openai.com",
                    r#"{"error":{"code":"insufficient_quota","message":"private"}}"#,
                    Some(GrokBlockReason::SpendingLimit),
                ),
                (
                    400,
                    "api.anthropic.com",
                    r#"{"error":{"code":"credit_balance_too_low"}}"#,
                    Some(GrokBlockReason::SpendingLimit),
                ),
                (
                    403,
                    "api.x.ai",
                    r#"{"code":"new-org-blocked:spending-limit"}"#,
                    Some(GrokBlockReason::SpendingLimit),
                ),
                (
                    400,
                    "api.openai.com",
                    r#"{"error":{"code":"unknown","message":"insufficient_quota"}}"#,
                    None,
                ),
                (
                    400,
                    "api.x.ai",
                    r#"{"error":{"code":"insufficient_quota"}}"#,
                    None,
                ),
            ] {
                let mut response = Response::builder()
                    .status(status)
                    .body(AsyncBody::from(bytes))?;
                assert_eq!(classify_response(&mut response, host).await, expected);
                let mut forwarded = String::new();
                response.body_mut().read_to_string(&mut forwarded).await?;
                assert_eq!(forwarded, bytes);
            }
            use futures::TryStreamExt as _;
            let chunks =
                futures::stream::once(async { Ok::<_, std::io::Error>(b"prefix".to_vec()) }).chain(
                    futures::stream::once(async {
                        smol::Timer::after(Duration::from_millis(150)).await;
                        Ok::<_, std::io::Error>(b"tail".to_vec())
                    }),
                );
            let mut response = Response::builder()
                .status(429)
                .body(AsyncBody::from_reader(chunks.into_async_read()))?;
            assert_eq!(
                classify_response(&mut response, "api.openai.com").await,
                Some(GrokBlockReason::RateLimited)
            );
            let mut bytes = String::new();
            response.body_mut().read_to_string(&mut bytes).await?;
            assert_eq!(bytes, "prefixtail");
            Ok(())
        })
    }

    #[test]
    fn native_dialects_and_exact_endpoints() -> anyhow::Result<()> {
        assert_eq!(duration_reset("1h2m3.5s", 100), Some(3824));
        for value in ["NaN", "-1s", "1", "1d", "infms"] {
            assert_eq!(duration_reset(value, 100), None);
        }
        let reset_at = |window: &WindowPatch| match window.reset_at {
            Patch::Set(value) => value,
            Patch::Missing => None,
        };
        let mut headers = http_client::http::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-requests-limit",
            HeaderValue::from_static("100"),
        );
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("80"),
        );
        headers.insert(
            "anthropic-ratelimit-requests-reset",
            HeaderValue::from_static("2026-11-05T12:00:00Z"),
        );
        headers.insert(
            "anthropic-ratelimit-input-tokens-limit",
            HeaderValue::from_static("1000"),
        );
        headers.insert(
            "anthropic-ratelimit-input-tokens-remaining",
            HeaderValue::from_static("900"),
        );
        headers.insert(
            "anthropic-ratelimit-output-tokens-limit",
            HeaderValue::from_static("500"),
        );
        let windows = parse_dialect(&headers, "test", "/v1/messages", "api.anthropic.com", 100);
        assert_eq!(windows.len(), 3);
        let requests = windows
            .iter()
            .find(|window| window.key.role == "requests")
            .expect("requests dimension");
        assert_eq!(
            reset_at(requests),
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-11-05T12:00:00Z")
                    .expect("reset")
                    .timestamp()
            )
        );
        assert!(
            windows
                .iter()
                .any(|window| window.key.role == "input-tokens")
        );
        assert!(!windows.iter().any(|window| window.key.role == "tokens"));

        let mut headers = http_client::http::HeaderMap::new();
        headers.insert(
            "x-ratelimit-limit-requests",
            HeaderValue::from_static("100"),
        );
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("80"),
        );
        headers.insert(
            "x-ratelimit-reset-requests",
            HeaderValue::from_static("6m0s"),
        );
        headers.insert(
            "x-ratelimit-reset-tokens",
            HeaderValue::from_static("nonsense"),
        );
        let windows = parse_dialect(&headers, "test", "/v1/responses", "api.openai.com", 100);
        assert_eq!(windows.len(), 1);
        assert_eq!(reset_at(&windows[0]), Some(460));
        assert!(windows.iter().all(|window| {
            window.key.bucket.applicability.eq(&Applicability::Family(
                "Latest OpenAI API response · Model unverified (/v1/responses)".into(),
            ))
        }));

        for (host, path, prefix) in [
            (
                "api.anthropic.com",
                "/v1/messages",
                "anthropic-ratelimit-requests-limit",
            ),
            (
                "api.openai.com",
                "/v1/responses",
                "x-ratelimit-limit-requests",
            ),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(format!("https://{host}{path}"))
                .body(AsyncBody::empty())?;
            assert!(inference_endpoint(&request).is_some());
            let mut headers = http_client::http::HeaderMap::new();
            headers.insert(prefix, HeaderValue::from_static("100"));
            assert_eq!(parse_dialect(&headers, "test", path, host, 100).len(), 1);
        }
        for uri in [
            "https://api.openai.com.evil/v1/responses",
            "https://api.openai.com:444/v1/responses",
            "https://api.anthropic.com/v1/messages/extra",
            "https://api.anthropic.com/v1/chat/completions",
            "http://api.openai.com/v1/responses",
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(uri)
                .body(AsyncBody::empty())?;
            assert!(inference_endpoint(&request).is_none());
        }
        Ok(())
    }

    #[test]
    fn non_xai_errors_are_untouched() -> anyhow::Result<()> {
        smol::block_on(async {
            let inner = http_client::FakeHttpClient::create(|_| async {
                Ok(Response::builder().status(403).body(AsyncBody::from(
                    r#"{"code":"personal-team-blocked:spending-limit"}"#,
                ))?)
            });
            let (sender, receiver) = async_channel::bounded(1);
            let observer = Observer {
                inner,
                sender,
                target: Arc::new(Mutex::new(BTreeMap::from([(
                    "api.x.ai".into(),
                    Target {
                        scope: "test".into(),
                        generation: 0,
                        gate_reason: None,
                    },
                )]))),
            };
            let mut response = observer
                .send(
                    Request::builder()
                        .method("POST")
                        .uri("https://example.com/v1/chat/completions")
                        .body(AsyncBody::empty())?,
                )
                .await?;
            let mut bytes = String::new();
            response.body_mut().read_to_string(&mut bytes).await?;
            assert_eq!(bytes, r#"{"code":"personal-team-blocked:spending-limit"}"#);
            assert_eq!(response.status(), 403);
            assert!(receiver.try_recv().is_err());
            Ok(())
        })
    }

    #[test]
    fn blocked_responses_are_transparent_and_success_clears() -> anyhow::Result<()> {
        smol::block_on(async {
            let mut core = QuotaCore::default();
            core.inventory(
                AccountScope {
                    category: crate::AccountCategory::ExternalAgent,
                    native_provider_id: None,
                    id: "test".into(),
                    provider: ProviderId::Grok,
                    runtime: "native-xai".into(),
                    identity_known: false,
                },
                0,
                Availability::Unknown,
            )
            .expect("inventory");
            let cases = [
                (403, r#"{"code":"personal-team-blocked:spending-limit","error":"never retain this"}"#.to_string(), Some(GrokBlockReason::SpendingLimit)),
                (429, "invalid".into(), Some(GrokBlockReason::RateLimited)),
                (403, "invalid".into(), Some(GrokBlockReason::Forbidden)),
                (403, format!(r#"{{"code":"personal-team-blocked:spending-limit","error":"{}"}}"#, "x".repeat(8192)), Some(GrokBlockReason::Forbidden)),
                (200, "success".into(), None),
            ];
            for (index, (status, bytes, expected)) in cases.into_iter().enumerate() {
                let original = bytes.clone();
                let inner = http_client::FakeHttpClient::create(move |_| {
                    let bytes = bytes.clone();
                    async move {
                        Ok(Response::builder()
                            .status(status)
                            .header("retry-after", "30")
                            .header("x-ratelimit-limit-requests", "100")
                            .body(AsyncBody::from(bytes))?)
                    }
                });
                let (sender, receiver) = async_channel::bounded(1);
                let observer = Observer {
                    inner,
                    sender,
                    target: Arc::new(Mutex::new(BTreeMap::from([(
                        "api.x.ai".into(),
                        Target {
                            scope: "test".into(),
                            generation: 0,
                            gate_reason: None,
                        },
                    )]))),
                };
                let mut response = observer
                    .send(
                        Request::builder()
                            .method("POST")
                            .uri("https://api.x.ai/v1/chat/completions")
                            .body(AsyncBody::empty())?,
                    )
                    .await?;
                assert_eq!(response.status().as_u16(), status);
                assert_eq!(response.headers()["retry-after"], "30");
                let mut forwarded = String::new();
                response.body_mut().read_to_string(&mut forwarded).await?;
                assert_eq!(forwarded, original);
                let mut observation = receiver.try_recv()?.quota;
                observation.sequence = index as u64 + 1;
                let now = observation.observed_at;
                let signal = observation.metadata.grok_response.as_ref().expect("signal");
                assert_eq!(signal.blocked, expected);
                assert_eq!(signal.retry_at, Some(now + 30));
                core.apply(observation, now).expect("apply");
                let account = core.snapshot.accounts.get("test").expect("account");
                assert_eq!(
                    account
                        .metadata
                        .grok_response
                        .as_ref()
                        .expect("signal")
                        .blocked,
                    expected
                );
                let stored = encode_record(&StoredRecord {
                    schema_version: 1,
                    revision: core.snapshot.revision,
                    account: account.clone(),
                    failures: 0,
                    retry_at: 0,
                })
                .expect("encode signal");
                assert!(!stored.contains("never retain this"));
                let restored =
                    decode_record(&stored, &account.scope, now + 60).expect("restore signal");
                assert!(restored.account.metadata.grok_response.is_none());
                let panel = super::super::panel::account_status(account, now);
                if expected == Some(GrokBlockReason::SpendingLimit) {
                    assert!(panel.contains("Out of credits"));
                }
            }
            Ok(())
        })
    }

    #[gpui::test]
    async fn native_grok_43_thinking_stream_observes_headers(cx: &mut gpui::TestAppContext) {
        async {
            use futures::{StreamExt as _, TryStreamExt as _};
            use language_model::{
                LanguageModelClient, LanguageModelProvider, LanguageModelRequest,
            };
            let transport = http_client::FakeHttpClient::create(|mut request| async move {
                assert_eq!(request.uri(), "https://api.x.ai/v1/chat/completions");
                let mut body = String::new();
                request.body_mut().read_to_string(&mut body).await?;
                let body: serde_json::Value = serde_json::from_str(&body)?;
                assert_eq!(body["model"], "grok-4.3");
                assert_eq!(body["reasoning_effort"], "low");
                assert_eq!(body["stream"], true);
                let chunks = futures::stream::iter([
                    Ok::<_, std::io::Error>(b"data: [DO".to_vec()),
                    Ok(b"NE]\n\n".to_vec()),
                ]);
                Ok(Response::builder()
                    .header("x-ratelimit-limit-requests", "100")
                    .header("x-ratelimit-remaining-requests", "80")
                    .body(AsyncBody::from_reader(chunks.into_async_read()))?)
            });
            let (provider, receiver) = cx.update(|cx| {
                let settings = settings::SettingsStore::test(cx);
                cx.set_global(settings);
                language_models::AllLanguageModelSettings::register(cx);
                client::ClientSettings::register(cx);
                cx.set_http_client(transport);
                install_http_observer(cx);
                let bridge = cx.global::<Bridge>();
                *bridge.target.lock().expect("target lock") = BTreeMap::from([(
                    "api.x.ai".into(),
                    Target {
                        scope: "test".into(),
                        generation: 1,
                        gate_reason: None,
                    },
                )]);
                let receiver = bridge.receiver.clone();
                let client = client::Client::production(cx);
                cx.set_http_client(client.http_client());
                let provider = language_models::provider::x_ai::XAiLanguageModelProvider::new(
                    client.http_client(),
                    Arc::new(
                        credentials_provider::test_support::FakeCredentialsProvider::default(),
                    ),
                    cx,
                );
                (provider, receiver)
            });
            let store = cx.update(|cx| provider.set_api_key(Some("fixture-only".into()), cx));
            store.await?;
            let model = cx.update(|cx| {
                provider
                    .provided_models(cx)
                    .into_iter()
                    .find(|model| model.id.0.as_ref() == "grok-4.3")
                    .expect("Grok 4.3")
            });
            for _ in 0..2 {
                let request = LanguageModelRequest {
                    thinking_allowed: true,
                    thinking_effort: Some("low".into()),
                    ..Default::default()
                };
                let mut stream = provider
                    .stream_completion(&model, request, &cx.to_async())
                    .await?;
                assert!(receiver.try_recv().is_ok());
                while let Some(event) = stream.next().await {
                    event?;
                }
                assert!(receiver.try_recv().is_err());
            }
            Ok::<(), anyhow::Error>(())
        }
        .await
        .expect("native streaming request");
    }

    #[test]
    fn other_host_errors_bypass_grok_observation() -> anyhow::Result<()> {
        smol::block_on(async {
            let (sender, receiver) = async_channel::bounded(1);
            let observer = Observer {
                inner: Arc::new(http_client::BlockedHttpClient::new()),
                sender,
                target: Arc::new(Mutex::new(BTreeMap::new())),
            };
            for uri in [
                "http://localhost:8080/v1/chat/completions",
                "https://example.com/v1/chat/completions",
            ] {
                let request = Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(AsyncBody::empty())?;
                assert!(!is_xai_host(&request));
                assert!(observer.send(request).await.is_err());
                assert!(receiver.try_recv().is_err());
            }
            Ok(())
        })
    }

    #[test]
    fn fake_streaming_startup_coalesces_and_restart_invalidates_legacy_native_cache()
    -> anyhow::Result<()> {
        smol::block_on(async {
            let inner = http_client::FakeHttpClient::create(|_| async {
                Ok(Response::builder()
                    .header("x-ratelimit-limit-requests", "100")
                    .header("x-ratelimit-remaining-requests", "80")
                    .body(AsyncBody::from("data: fixture\n\n"))?)
            });
            let (sender, receiver) = async_channel::bounded(128);
            let observer = Observer {
                inner,
                sender,
                target: Arc::new(Mutex::new(BTreeMap::from([(
                    "api.x.ai".into(),
                    Target {
                        scope: "test".into(),
                        generation: 1,
                        gate_reason: None,
                    },
                )]))),
            };
            let (ready_sender, ready_receiver) = oneshot::channel();
            let buffer = buffer_until_ready(
                async { ready_receiver.await.map_err(|_| ErrorCode::Persistence) },
                &receiver,
            );
            futures::pin_mut!(buffer);
            for _ in 0..3 {
                let mut response = observer
                    .send(
                        Request::builder()
                            .method("POST")
                            .uri("https://api.x.ai/v1/chat/completions")
                            .body(AsyncBody::empty())?,
                    )
                    .await?;
                let mut bytes = Vec::new();
                response.body_mut().read_to_end(&mut bytes).await?;
                assert_eq!(bytes, b"data: fixture\n\n");
                assert!(futures::poll!(buffer.as_mut()).is_pending());
            }
            ready_sender.send(()).expect("ready");
            let mut pending = buffer.await.map_err(|code| anyhow::anyhow!("{code:?}"))?;
            assert_eq!(pending.len(), 1);
            let observation = pending.remove("test").expect("latest").quota;
            let now = observation.observed_at;
            let scope = AccountScope {
                category: crate::AccountCategory::ExternalAgent,
                native_provider_id: None,
                id: "test".into(),
                provider: ProviderId::Grok,
                runtime: "native-xai".into(),
                identity_known: false,
            };
            let mut core = QuotaCore::default();
            core.inventory(scope.clone(), 1, Availability::Unknown)
                .expect("inventory");
            core.apply(observation, now).expect("headers");
            let encoded = encode_record(&StoredRecord {
                schema_version: 1,
                revision: core.snapshot.revision,
                account: core.snapshot.accounts["test"].clone(),
                failures: 0,
                retry_at: 0,
            })
            .expect("store");
            let restored = decode_record(&encoded, &scope, now + 60).expect("restart");
            assert!(restored.account.windows.is_empty());
            assert!(restored.account.last_success_at.is_none());
            Ok(())
        })
    }

    #[test]
    fn untargeted_hosts_forward_without_capture() -> anyhow::Result<()> {
        smol::block_on(async {
            for targets in [
                BTreeMap::new(),
                BTreeMap::from([(
                    "api.anthropic.com".into(),
                    Target {
                        scope: "other".into(),
                        generation: 0,
                        gate_reason: None,
                    },
                )]),
            ] {
                let inner = http_client::FakeHttpClient::create(|_| async {
                    Ok(Response::builder()
                        .status(200)
                        .header("x-ratelimit-limit-requests", "100")
                        .body(AsyncBody::from("bytes"))?)
                });
                let (sender, receiver) = async_channel::bounded(1);
                let observer = Observer {
                    inner,
                    sender,
                    target: Arc::new(Mutex::new(targets)),
                };
                let mut response = observer
                    .send(
                        Request::builder()
                            .method("POST")
                            .uri("https://api.x.ai/v1/chat/completions")
                            .body(AsyncBody::empty())?,
                    )
                    .await?;
                assert_eq!(response.status(), 200);
                assert_eq!(response.headers()["x-ratelimit-limit-requests"], "100");
                let mut bytes = String::new();
                response.body_mut().read_to_string(&mut bytes).await?;
                assert_eq!(bytes, "bytes");
                assert!(receiver.try_recv().is_err());
            }
            Ok(())
        })
    }

    #[test]
    fn transparent_transport() -> anyhow::Result<()> {
        smol::block_on(async {
            for streaming in [false, true] {
                for host in ["api.x.ai", "example.com"] {
                    let inner = http_client::FakeHttpClient::create(move |request| async move {
                        assert_eq!(request.method(), http_client::Method::POST);
                        let body = if streaming {
                            AsyncBody::from_reader(futures::io::Cursor::new(
                                b"data: one\n\ndata: two\n\n".to_vec(),
                            ))
                        } else {
                            AsyncBody::from("normal bytes")
                        };
                        Ok(Response::builder()
                            .status(201)
                            .header("x-ratelimit-limit-requests", "100")
                            .header("x-ratelimit-remaining-requests", "80")
                            .header("content-type", "text/event-stream")
                            .body(body)?)
                    });
                    let (sender, receiver) = async_channel::bounded(1);
                    let observer = Observer {
                        inner,
                        sender,
                        target: Arc::new(Mutex::new(BTreeMap::from([(
                            "api.x.ai".into(),
                            Target {
                                scope: "xai-test".into(),
                                generation: 0,
                                gate_reason: None,
                            },
                        )]))),
                    };
                    let mut response = observer
                        .send(
                            Request::builder()
                                .method("POST")
                                .uri(format!("https://{host}/v1/chat/completions"))
                                .body(AsyncBody::empty())?,
                        )
                        .await?;
                    assert_eq!(response.status(), 201);
                    assert_eq!(response.headers()["x-ratelimit-limit-requests"], "100");
                    assert_eq!(response.headers()["content-type"], "text/event-stream");
                    let mut bytes = Vec::new();
                    response.body_mut().read_to_end(&mut bytes).await?;
                    assert_eq!(
                        bytes,
                        if streaming {
                            b"data: one\n\ndata: two\n\n".as_slice()
                        } else {
                            b"normal bytes".as_slice()
                        }
                    );
                    assert_eq!(receiver.try_recv().is_ok(), host == "api.x.ai");
                }
            }
            let (sender, receiver) = async_channel::bounded(1);
            let observer = Observer {
                inner: Arc::new(http_client::BlockedHttpClient::new()),
                sender,
                target: Arc::new(Mutex::new(BTreeMap::new())),
            };
            assert!(
                observer
                    .send(
                        Request::builder()
                            .uri("https://api.x.ai/v1/chat/completions")
                            .body(AsyncBody::empty())?
                    )
                    .await
                    .is_err()
            );
            assert!(receiver.try_recv().is_err());
            Ok(())
        })
    }

    #[test]
    fn headers_endpoint_keying_and_staleness() -> anyhow::Result<()> {
        let mut headers = http_client::http::HeaderMap::new();
        headers.insert(
            "x-ratelimit-limit-requests",
            HeaderValue::from_static("100"),
        );
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("80"),
        );
        headers.insert("x-ratelimit-limit-tokens", HeaderValue::from_static("NaN"));
        headers.insert(
            "x-ratelimit-remaining-tokens",
            HeaderValue::from_static("-1"),
        );
        headers.insert("retry-after", HeaderValue::from_static("30"));
        assert_eq!(retry_after(&headers, 1000), Some(1030));
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 04 Nov 2026 12:00:00 +0000"),
        );
        assert!(retry_after(&headers, 1000).is_some());
        headers.insert(
            "x-ratelimit-reset-requests",
            HeaderValue::from_static("unknown-window"),
        );
        let windows = parse(&headers, "test", "/v1/chat/completions");
        assert!(windows.iter().all(|window| {
            !Selector {
                model: Some("grok-4.7".into()),
                ..Default::default()
            }
            .matches_window(&window.key)
        }));
        assert_eq!(windows.len(), 1);
        assert_ne!(
            windows.first().map(|window| &window.key),
            parse(&headers, "test", "/v1/responses")
                .first()
                .map(|window| &window.key)
        );
        let mut core = QuotaCore::default();
        core.inventory(
            AccountScope {
                category: crate::AccountCategory::ExternalAgent,
                native_provider_id: None,
                id: "test".into(),
                provider: ProviderId::Grok,
                runtime: "native-xai".into(),
                identity_known: false,
            },
            0,
            Availability::Unknown,
        )
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        core.apply(
            Observation {
                scope: "test".into(),
                generation: 0,
                source: SourceKind::GrokResponseHeaders,
                source_version: "xai-headers-v1".into(),
                observed_at: 1000,
                sequence: 1,
                start_sequence: None,
                provider_sample_at: None,
                windows,
                complete: false,
                metadata: AccountMetadata::default(),
            },
            1000,
        )
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let window = core
            .snapshot
            .accounts
            .values()
            .next()
            .and_then(|account| account.windows.values().next())
            .ok_or_else(|| anyhow::anyhow!("missing window"))?;
        assert_eq!(
            window.freshness(1001, Duration::from_secs(120)),
            Freshness::Fresh
        );
        assert!(matches!(
            window.freshness(1120, Duration::from_secs(120)),
            Freshness::Stale {
                reason: StaleReason::Age,
                ..
            }
        ));
        assert_eq!(
            window.reset_at.as_ref().and_then(|sample| sample.value),
            None
        );
        Ok(())
    }

    struct InertPorts;
    impl Clock for InertPorts {
        fn now(&self) -> Timestamp {
            1000
        }
        fn monotonic(&self) -> u64 {
            0
        }
    }
    impl QuotaSource for InertPorts {
        fn refresh(
            &self,
            _: String,
            _: u64,
            _: u64,
            _: &mut App,
        ) -> Task<Result<Observation, RefreshError>> {
            panic!("passive capture must not refresh")
        }
    }
    impl QuotaStore for InertPorts {
        fn load(&self, _: String, _: &App) -> Task<Result<Option<String>, ErrorCode>> {
            Task::ready(Ok(None))
        }
        fn save(&self, _: String, _: String, _: &App) -> Task<Result<(), ErrorCode>> {
            Task::ready(Ok(()))
        }
        fn delete(&self, _: String, _: &App) -> Task<Result<(), ErrorCode>> {
            Task::ready(Ok(()))
        }
    }

    fn window(scope: &str, role: &str, remaining: f64) -> WindowPatch {
        WindowPatch {
            key: WindowKey {
                bucket: BucketKey {
                    scope: scope.into(),
                    bucket: format!("api.x.ai/v1/chat/completions/{role}"),
                    applicability: Applicability::Family("fixture".into()),
                },
                role: role.into(),
                duration_seconds: None,
            },
            used_fraction: Patch::Missing,
            remaining: Patch::Set(Some(Amount {
                value: remaining,
                unit: Unit::Tokens,
            })),
            limit: Patch::Set(Some(Amount {
                value: 100.0,
                unit: Unit::Tokens,
            })),
            reset_at: Patch::Missing,
            admission: Patch::Missing,
            disabled: Patch::Missing,
            precision: Precision::Exact,
        }
    }

    fn headers(scope: &str, generation: u64, windows: Vec<WindowPatch>) -> HeaderObservation {
        HeaderObservation {
            quota: Observation {
                scope: scope.into(),
                generation,
                source: SourceKind::GrokResponseHeaders,
                source_version: "fixture".into(),
                observed_at: 1000,
                provider_sample_at: None,
                sequence: 0,
                start_sequence: None,
                windows,
                complete: false,
                metadata: AccountMetadata {
                    grok_response: Some(GrokResponse {
                        blocked: None,
                        status: 200,
                        observed_at: 1000,
                        retry_at: None,
                    }),
                    ..Default::default()
                },
            },
            retry_at: None,
        }
    }

    #[gpui::test]
    fn latest_response_replaces_windows_atomically(cx: &mut gpui::TestAppContext) {
        let scope = "llm-provider:x_ai";
        let (sender, service) = cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            language_models::AllLanguageModelSettings::register(cx);
            language_model::init(cx);
            cx.set_http_client(Arc::new(http_client::BlockedHttpClient::new()));
            install_http_observer(cx);
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        clock: Rc::new(InertPorts),
                        source: Rc::new(InertPorts),
                        store: Rc::new(InertPorts),
                    },
                    cx,
                )
            });
            init(&service, cx);
            service.update(cx, |service, _| {
                service
                    .core
                    .inventory(
                        AccountScope {
                            category: crate::AccountCategory::LlmProvider,
                            native_provider_id: Some("x_ai".into()),
                            id: scope.into(),
                            provider: ProviderId::LlmProvider,
                            runtime: "native-api".into(),
                            identity_known: false,
                        },
                        7,
                        Availability::Unknown,
                    )
                    .expect("inventory");
                service.finish_init(Ok(()));
            });
            let sender = cx.global::<Bridge>().sender.clone();
            (sender, service)
        });
        cx.run_until_parked();
        sender
            .try_send(headers(
                scope,
                7,
                vec![
                    window(scope, "requests", 80.0),
                    window(scope, "tokens", 40.0),
                ],
            ))
            .expect("send");
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert_eq!(service.core.snapshot.accounts[scope].windows.len(), 2);
        });
        sender
            .try_send(headers(scope, 7, vec![window(scope, "requests", 60.0)]))
            .expect("send");
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            let account = &service.core.snapshot.accounts[scope];
            assert_eq!(account.windows.len(), 1);
            assert_eq!(
                account
                    .windows
                    .values()
                    .next()
                    .and_then(|window| window.remaining.as_ref())
                    .and_then(|sample| sample.value.as_ref())
                    .map(|amount| amount.value),
                Some(60.0)
            );
        });
        sender
            .try_send(headers(scope, 99, vec![window(scope, "tokens", 1.0)]))
            .expect("send");
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            let account = &service.core.snapshot.accounts[scope];
            assert_eq!(account.windows.len(), 1);
            assert_eq!(
                account
                    .windows
                    .values()
                    .next()
                    .and_then(|window| window.remaining.as_ref())
                    .and_then(|sample| sample.value.as_ref())
                    .map(|amount| amount.value),
                Some(60.0)
            );
        });
    }
}
