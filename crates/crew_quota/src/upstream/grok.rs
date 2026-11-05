use super::*;
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use settings::Settings;
use std::sync::Mutex;
use util::ResultExt as _;

#[derive(Clone)]
struct Target {
    scope: String,
    generation: u64,
}
struct HeaderObservation {
    quota: Observation,
    retry_at: Option<Timestamp>,
}
struct Bridge {
    target: Arc<Mutex<Option<Target>>>,
    sender: async_channel::Sender<HeaderObservation>,
    receiver: async_channel::Receiver<HeaderObservation>,
}
impl Global for Bridge {}

struct Observer {
    inner: Arc<dyn HttpClient>,
    target: Arc<Mutex<Option<Target>>>,
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
        request: Request<AsyncBody>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        if !is_xai_host(&request) {
            return self.inner.send(request);
        }
        let endpoint = inference_endpoint(&request).map(str::to_owned);
        if endpoint.is_none() {
            log::info!("crew_quota: agent=Grok source=Headers outcome=Unmatched path_kind=Other");
            return self.inner.send(request);
        }
        let target = endpoint
            .as_ref()
            .and_then(|_| self.target.try_lock().ok()?.clone());
        log::info!("crew_quota: agent=Grok source=Headers outcome=Eligible");
        if target.is_none() {
            log::info!("crew_quota: agent=Grok source=Headers outcome=Gated");
        }
        let future = self.inner.send(request);
        let sender = self.sender.clone();
        Box::pin(async move {
            let mut response = future.await.inspect_err(|_| {
                if endpoint.is_some() {
                    log::info!("crew_quota: agent=Grok source=Headers outcome=TransportError");
                }
            })?;
            if let (Some(endpoint), Some(target)) = (endpoint, target) {
                let now = chrono::Utc::now().timestamp();
                let windows = parse(response.headers(), &target.scope, &endpoint);
                let status = response.status().as_u16();
                let blocked = classify_response(&mut response).await;
                if let Some(reason) = blocked {
                    log::info!(
                        "crew_quota: agent=Grok source=Headers outcome=Blocked reason={reason:?}"
                    );
                }
                if blocked.is_some() || (response.status().is_success() && !windows.is_empty()) {
                    let observation = Observation {
                        scope: target.scope,
                        generation: target.generation,
                        source: SourceKind::GrokResponseHeaders,
                        source_version: "xai-headers-v1".into(),
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
                            log::info!("crew_quota: agent=Grok source=Headers outcome=Queued")
                        }
                        Err(_) => {
                            log::info!("crew_quota: agent=Grok source=Headers outcome=Dropped")
                        }
                    }
                } else {
                    log::info!("crew_quota: agent=Grok source=Headers outcome=NoQuotaHeaders");
                }
            }
            Ok(response)
        })
    }
}

#[derive(serde::Deserialize)]
struct ErrorCodeBody {
    code: Option<String>,
}

async fn classify_response(response: &mut Response<AsyncBody>) -> Option<GrokBlockReason> {
    use futures::AsyncReadExt as _;
    let status = response.status();
    if status.is_success() {
        return None;
    }
    let fallback = if status.as_u16() == 429 {
        Some(GrokBlockReason::RateLimited)
    } else if status.is_client_error() {
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
    while prefix.len() < 8192 {
        let mut buffer = [0; 1024];
        let length = (8192 - prefix.len()).min(buffer.len());
        match body.read(&mut buffer[..length]).await {
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
        serde_json::from_slice::<ErrorCodeBody>(&prefix)
            .ok()
            .and_then(|value| value.code)
            .filter(|code| code.contains("spending-limit"))
            .map(|_| GrokBlockReason::SpendingLimit)
            .or(fallback)
    } else {
        fallback
    };
    *response.body_mut() = AsyncBody::from_reader(futures::io::Cursor::new(prefix).chain(body));
    reason
}

fn is_xai_host(request: &Request<AsyncBody>) -> bool {
    let uri = request.uri();
    uri.scheme_str() == Some("https")
        && uri.host() == Some("api.x.ai")
        && uri.port_u16().is_none_or(|port| port == 443)
}

fn inference_endpoint(request: &Request<AsyncBody>) -> Option<&str> {
    let uri = request.uri();
    (request.method() == http_client::Method::POST
        && is_xai_host(request)
        && matches!(
            uri.path(),
            "/v1/chat/completions"
                | "/v1//chat/completions"
                | "/v1/completions"
                | "/v1/responses"
                | "/v1/embeddings"
        ))
    .then_some(uri.path())
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

fn parse(headers: &http_client::http::HeaderMap, scope: &str, endpoint: &str) -> Vec<WindowPatch> {
    let number = |name: &str| {
        let text = headers.get(name)?.to_str().ok()?;
        if text.len() > 32 {
            return None;
        }
        let value = text.parse::<f64>().ok()?;
        (value.is_finite() && value >= 0.0).then_some(value)
    };
    ["requests", "tokens"]
        .into_iter()
        .map(|dimension| {
            let limit = number(&format!("x-ratelimit-limit-{dimension}"));
            let remaining = number(&format!("x-ratelimit-remaining-{dimension}"));
            let unit = if dimension == "tokens" {
                Unit::Tokens
            } else {
                Unit::Requests
            };
            WindowPatch {
                key: WindowKey {
                    bucket: BucketKey {
                        scope: scope.into(),
                        bucket: format!("xai{endpoint}/{dimension}"),
                        applicability: Applicability::Family(format!(
                            "Latest xAI response · Model unverified ({endpoint})"
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
                        .get(format!("x-ratelimit-reset-{dimension}"))
                        .and_then(|value| value.to_str().ok())
                        .and_then(absolute_date),
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
    let target = Arc::new(Mutex::new(None));
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

fn capture_allowed(present: bool, supported: bool, supergrok: bool) -> bool {
    present && supported && !supergrok
}

pub(super) fn sync(service: &Entity<QuotaService>, cx: &mut App) {
    if cx.try_global::<Bridge>().is_none() {
        return;
    }
    let configured = &language_models::AllLanguageModelSettings::get_global(cx)
        .x_ai
        .api_url;
    let api_url = if configured.is_empty() {
        "https://api.x.ai/v1"
    } else {
        configured.as_str()
    };
    let present = language_model::LanguageModelRegistry::read_global(cx)
        .providers()
        .iter()
        .any(|provider| provider.id().0.as_ref() == "x_ai" && provider.is_authenticated(cx));
    let supergrok = language_model::LanguageModelRegistry::read_global(cx)
        .providers()
        .iter()
        .any(|provider| {
            provider.id().0.as_ref() == "x_ai_subscribed" && provider.is_authenticated(cx)
        });
    use sha2::{Digest, Sha256};
    let scope = format!(
        "xai-{:x}",
        Sha256::digest(format!("{api_url}/credential-present={present}"))
    );
    let supported = api_url.trim_end_matches('/') == "https://api.x.ai/v1";
    let bridge = cx.global::<Bridge>().target.clone();
    service.update(cx, |service, cx| {
        let identity = (scope.clone(), supergrok);
        if service.grok_identity.as_ref() == Some(&identity) {
            return;
        }
        service.grok_identity = Some(identity);
        let old = service
            .core
            .snapshot
            .accounts
            .values()
            .find(|account| account.scope.provider == ProviderId::Grok)
            .map(|account| (account.scope.id.clone(), account.generation));
        let generation = old
            .as_ref()
            .map_or(0, |(_, generation)| generation.saturating_add(1));
        if old.as_ref().is_some_and(|(previous, _)| previous != &scope) {
            service
                .core
                .snapshot
                .accounts
                .retain(|_, account| account.scope.provider != ProviderId::Grok);
        }
        if service
            .core
            .inventory(
                AccountScope {
                    id: scope.clone(),
                    provider: ProviderId::Grok,
                    runtime: "native-xai".into(),
                    identity_known: false,
                },
                generation,
                if !present {
                    Availability::NotLoggedIn
                } else if supergrok {
                    Availability::Unavailable(ErrorCode::AttributionAmbiguous)
                } else if supported {
                    Availability::Unknown
                } else {
                    Availability::Unsupported(ErrorCode::Unknown)
                },
            )
            .is_err()
        {
            log::warn!("crew_quota: xai_inventory_invalid");
        }
        if let Ok(mut target) = bridge.lock() {
            *target = capture_allowed(present, supported, supergrok).then_some(Target {
                scope: scope.clone(),
                generation,
            });
        }
        if capture_allowed(present, supported, supergrok) {
            let load = service.ports.store.load(format!("snapshot/{scope}"), cx);
            cx.spawn({
                let scope = scope.clone();
                async move |this, cx| {
                    let result = load.await;
                    this.update(cx, |service, cx| {
                        match result {
                            Ok(Some(record)) => {
                                if let Some(account) = service.core.snapshot.accounts.get_mut(&scope)
                                    && account.generation == generation && account.windows.is_empty() && account.metadata.grok_response.is_none()
                                {
                                    match decode_record(&record, &account.scope, service.ports.clock.now()) {
                                        Ok(record) => {
                                            account.windows = record.account.windows;
                                            account.metadata.grok_response = record.account.metadata.grok_response;
                                            account.last_success_at = record.account.last_success_at;
                                            account.availability = Availability::Ready;
                                            service.core.snapshot.revision = service.core.snapshot.revision.saturating_add(1);
                                            log::info!("crew_quota: agent=Grok source=Cache outcome=RestoredUnverified");
                                            cx.notify();
                                        }
                                        Err(code) => log::info!("crew_quota: agent=Grok source=Cache outcome={code:?}"),
                                    }
                                }
                            }
                            Err(code) => log::info!("crew_quota: agent=Grok source=Cache outcome={code:?}"),
                            Ok(None) => log::info!("crew_quota: agent=Grok source=Cache outcome=Empty"),
                        }
                    }).log_err();
                }
            }).detach();
        }
        cx.emit(QuotaEvent::AvailabilityChanged(scope.clone()));
        cx.emit(QuotaEvent::Changed {
            scopes: vec![scope],
            windows: Vec::new(),
            revision: service.core.snapshot.revision,
        });
        cx.notify();
    });
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
                if pending.len() < MAX_SCOPES || pending.contains_key(&headers.quota.scope) {
                    pending.insert(headers.quota.scope.clone(), headers);
                }
                log::info!("crew_quota: agent=Grok source=Headers outcome=Buffered");
            }
        }
    }
    Ok(pending)
}

pub(super) fn init(service: &Entity<QuotaService>, cx: &mut App) {
    if cx.try_global::<Bridge>().is_none() {
        return;
    }
    sync(service, cx);
    let registry = language_model::LanguageModelRegistry::global(cx);
    let subscription = cx.observe(&registry, {
        let service = service.downgrade();
        move |_, cx| {
            if let Some(service) = service.upgrade() {
                sync(&service, cx);
            }
        }
    });
    let settings = cx.observe_global::<SettingsStore>({
        let service = service.downgrade();
        move |cx| {
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
                        let scope = observation.scope.clone();
                        let generation = observation.generation;
                        log::info!("crew_quota: agent=Grok source=Headers outcome=Received");
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
    use super::*;
    use futures::AsyncReadExt as _;

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
                target: Arc::new(Mutex::new(Some(Target {
                    scope: "test".into(),
                    generation: 0,
                }))),
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
                    target: Arc::new(Mutex::new(Some(Target {
                        scope: "test".into(),
                        generation: 0,
                    }))),
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
                assert_eq!(
                    restored.account.metadata.grok_response,
                    account.metadata.grok_response
                );
                let panel = super::super::panel::account_status(account, now);
                if expected == Some(GrokBlockReason::SpendingLimit) {
                    assert!(panel.contains("Out of credits"));
                    assert!(panel.contains("grok.com"));
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
                cx.set_http_client(transport);
                install_http_observer(cx);
                let bridge = cx.global::<Bridge>();
                *bridge.target.lock().expect("target lock") = Some(Target {
                    scope: "test".into(),
                    generation: 1,
                });
                let receiver = bridge.receiver.clone();
                // Client::production wraps the decorated app client in HttpClientWithUrl.
                let client = Arc::new(http_client::HttpClientWithUrl::new_url(
                    cx.http_client(),
                    "https://zed.dev",
                    None,
                ));
                let provider = language_models::provider::x_ai::XAiLanguageModelProvider::new(
                    client,
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
                target: Arc::new(Mutex::new(None)),
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
    fn fake_streaming_startup_coalesces_and_restart_restores_unverified_age() -> anyhow::Result<()>
    {
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
                target: Arc::new(Mutex::new(Some(Target {
                    scope: "test".into(),
                    generation: 1,
                }))),
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
            let window = restored.account.windows.values().next().expect("window");
            let sample = window.remaining.as_ref().expect("sample");
            assert_eq!(sample.source, SourceKind::Persisted);
            assert_eq!(sample.observed_at, now);
            assert!(matches!(
                window.freshness(now + 60, Duration::from_secs(120)),
                Freshness::Stale { .. }
            ));
            Ok(())
        })
    }

    #[test]
    fn coexistence_fails_closed() {
        assert!(capture_allowed(true, true, false));
        assert!(!capture_allowed(true, true, true));
        assert!(!capture_allowed(false, true, false));
        assert!(!capture_allowed(true, false, false));
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
                        target: Arc::new(Mutex::new(Some(Target {
                            scope: "xai-test".into(),
                            generation: 0,
                        }))),
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
                target: Arc::new(Mutex::new(None)),
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
}
