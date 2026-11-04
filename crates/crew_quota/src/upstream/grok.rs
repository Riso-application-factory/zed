use super::*;
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use settings::Settings;
use std::sync::Mutex;

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
        let endpoint = inference_endpoint(&request).map(str::to_owned);
        let target = endpoint
            .as_ref()
            .and_then(|_| self.target.try_lock().ok()?.clone());
        let future = self.inner.send(request);
        let sender = self.sender.clone();
        Box::pin(async move {
            let response = future.await?;
            if let (Some(endpoint), Some(target)) = (endpoint, target) {
                let now = chrono::Utc::now().timestamp();
                let windows = parse(response.headers(), &target.scope, &endpoint);
                if !windows.is_empty() {
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
                        metadata: AccountMetadata::default(),
                    };
                    // Losing telemetry must never apply backpressure to inference.
                    match sender.try_send(HeaderObservation {
                        quota: observation,
                        retry_at: retry_after(response.headers(), now),
                    }) {
                        Ok(())
                        | Err(async_channel::TrySendError::Full(_))
                        | Err(async_channel::TrySendError::Closed(_)) => {}
                    }
                }
            }
            Ok(response)
        })
    }
}

fn inference_endpoint(request: &Request<AsyncBody>) -> Option<&str> {
    let uri = request.uri();
    (request.method() == http_client::Method::POST
        && uri.scheme_str() == Some("https")
        && uri.host() == Some("api.x.ai")
        && uri.port_u16().is_none_or(|port| port == 443)
        && matches!(
            uri.path(),
            "/v1/chat/completions" | "/v1/completions" | "/v1/responses" | "/v1/embeddings"
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
                            "Latest xAI response · Unknown model ({endpoint})"
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
        .collect()
}

pub fn install_http_observer(cx: &mut App) {
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

fn sync(service: &Entity<QuotaService>, cx: &mut App) {
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
        cx.emit(QuotaEvent::AvailabilityChanged(scope.clone()));
        cx.emit(QuotaEvent::Changed {
            scopes: vec![scope],
            windows: Vec::new(),
            revision: service.core.snapshot.revision,
        });
        cx.notify();
    });
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
            while let Ok(headers) = receiver.recv().await {
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
        assert_eq!(windows.len(), 2);
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
