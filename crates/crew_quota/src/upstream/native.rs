use super::*;
use language_model::{LanguageModelProvider, LanguageModelRegistry};
use settings::Settings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub id: String,
    pub info: NativeProviderInfo,
    pub capture_host: Option<&'static str>,
    fingerprint: String,
    pub gate_reason: Option<String>,
}

fn endpoint(id: &str, settings: &language_models::AllLanguageModelSettings) -> Option<String> {
    let (configured, default) = match id {
        "x_ai" => (&settings.x_ai.api_url, "https://api.x.ai/v1"),
        "anthropic" => (&settings.anthropic.api_url, "https://api.anthropic.com"),
        "openai" => (&settings.openai.api_url, "https://api.openai.com/v1"),
        "x_ai_subscribed" => return Some("https://api.x.ai/v1".into()),
        "lmstudio" => (&settings.lmstudio.api_url, ""),
        "llama.cpp" => (&settings.llama_cpp.api_url, ""),
        "ollama" => (&settings.ollama.api_url, ""),
        "deepseek" => (&settings.deepseek.api_url, "https://api.deepseek.com/v1"),
        "google" => (&settings.google.api_url, ""),
        "mistral" => (&settings.mistral.api_url, ""),
        "openrouter" => (
            &settings.open_router.api_url,
            "https://openrouter.ai/api/v1",
        ),
        "opencode" => (&settings.opencode.api_url, ""),
        "vercel_ai_gateway" => (
            &settings.vercel_ai_gateway.api_url,
            "https://ai-gateway.vercel.sh/v1",
        ),
        id => {
            return settings
                .openai_compatible
                .get(id)
                .map(|settings| settings.api_url.clone())
                .or_else(|| {
                    settings
                        .anthropic_compatible
                        .get(id)
                        .map(|settings| settings.api_url.clone())
                });
        }
    };
    if configured.is_empty() && default.is_empty() {
        return None;
    }
    Some(if configured.is_empty() {
        default.into()
    } else {
        configured.clone()
    })
}

fn trusted_host(id: &str, endpoint: &str) -> Option<&'static str> {
    let (expected, host) = match id {
        "x_ai" => ("https://api.x.ai/v1", "api.x.ai"),
        "anthropic" => ("https://api.anthropic.com", "api.anthropic.com"),
        "openai" => ("https://api.openai.com/v1", "api.openai.com"),
        "deepseek" => ("https://api.deepseek.com/v1", "api.deepseek.com"),
        "openrouter" => ("https://openrouter.ai/api/v1", "openrouter.ai"),
        "vercel_ai_gateway" => ("https://ai-gateway.vercel.sh/v1", "ai-gateway.vercel.sh"),
        _ => return None,
    };
    (endpoint.trim_end_matches('/') == expected).then_some(host)
}

fn shares_host(endpoint: &str, host: &str) -> bool {
    http_client::Url::parse(endpoint).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(host)
            && url.port_or_known_default() == Some(443)
    })
}

#[derive(Default)]
struct CredentialLoads(BTreeMap<String, CredentialLoad>);
impl Global for CredentialLoads {}

#[derive(Default)]
struct CredentialLoad {
    pending: bool,
    last_state: Option<&'static str>,
}

// These implementations only call ApiKeyState::load_if_needed. Anthropic,
// OpenRouter and Vercel also fetch models, so must remain observation-only.
pub(super) fn load_credentials(cx: &mut App) {
    if !cx.has_global::<CredentialLoads>() {
        cx.set_global(CredentialLoads::default());
    }
    let providers = LanguageModelRegistry::read_global(cx).providers();
    for provider in providers {
        let id = provider.id().0.to_string();
        if !matches!(id.as_str(), "x_ai" | "openai" | "deepseek" | "mistral")
            || provider.is_authenticated(cx)
        {
            continue;
        }
        let load = cx
            .global_mut::<CredentialLoads>()
            .0
            .entry(id.clone())
            .or_default();
        if load.pending {
            continue;
        }
        load.pending = true;
        let task = provider.authenticate(cx);
        cx.spawn(async move |cx| {
            let state = if task.await.is_ok() {
                "Completed"
            } else {
                "Unavailable"
            };
            cx.update(|cx| {
                let load = cx
                    .global_mut::<CredentialLoads>()
                    .0
                    .entry(id.clone())
                    .or_default();
                load.pending = false;
                if load.last_state != Some(state) {
                    log::info!("crew_quota: native_credentials provider={id} state={state}");
                    load.last_state = Some(state);
                }
            });
        })
        .detach();
    }
}

pub(super) fn collect(cx: &App) -> Vec<Candidate> {
    let registry = LanguageModelRegistry::read_global(cx);
    let settings = language_models::AllLanguageModelSettings::get_global(cx);
    let authenticated = registry
        .providers()
        .into_iter()
        .filter(|provider| provider.is_authenticated(cx))
        .collect::<Vec<_>>();
    registry
        .visible_providers()
        .into_iter()
        .filter(|provider| provider.is_authenticated(cx))
        .enumerate()
        .filter_map(|(order, provider)| {
            let id = provider.id().0.to_string();
            let name = provider.name().0.to_string();
            if !valid_text(&id, 128) || !valid_text(&name, 256) {
                return None;
            }
            let endpoint = endpoint(&id, settings);
            let mut capture_host = endpoint
                .as_deref()
                .and_then(|endpoint| trusted_host(&id, endpoint));
            let mut kind = if matches!(id.as_str(), "ollama" | "lmstudio" | "llama.cpp") {
                NativeProviderKind::Local
            } else if capture_host.is_some() {
                NativeProviderKind::Passive
            } else if endpoint.is_some() && !matches!(id.as_str(), "x_ai_subscribed") {
                NativeProviderKind::CustomEndpoint
            } else {
                NativeProviderKind::Unavailable
            };
            let mut gate_reason = None;
            if let Some(host) = capture_host {
                if let Some(other) = authenticated.iter().find(|other| {
                    other.id().0.as_ref() != id
                        && endpoint_for_provider(other.as_ref(), settings)
                            .is_some_and(|endpoint| shares_host(&endpoint, host))
                }) {
                    gate_reason = Some(format!("SharedHost({})", other.id().0));
                    kind = NativeProviderKind::Ambiguous;
                    capture_host = None;
                }
            }
            use sha2::{Digest, Sha256};
            let fingerprint = format!("{:x}", Sha256::digest(endpoint.unwrap_or_default()));
            Some(Candidate {
                id,
                info: NativeProviderInfo { name, kind, order },
                capture_host,
                fingerprint,
                gate_reason,
            })
        })
        .collect()
}

fn endpoint_for_provider(
    provider: &dyn LanguageModelProvider,
    settings: &language_models::AllLanguageModelSettings,
) -> Option<String> {
    endpoint(provider.id().0.as_ref(), settings)
}

pub(super) fn sync(service: &Entity<QuotaService>, cx: &mut App) {
    let authentication = LanguageModelRegistry::read_global(cx)
        .visible_providers()
        .into_iter()
        .filter_map(|provider| {
            let id = provider.id().0.to_string();
            valid_text(&id, 128).then(|| (id, provider.is_authenticated(cx)))
        })
        .collect::<BTreeMap<_, _>>();
    service.update(cx, |service, _| {
        for (id, authenticated) in &authentication {
            if service.native_authentication.get(id) != Some(authenticated) {
                let state = if *authenticated {
                    "Authenticated"
                } else {
                    "NotAuthenticated"
                };
                log::info!("crew_quota: native_inventory provider={id} state={state}");
            }
        }
        service.native_authentication = authentication;
    });
    let candidates = collect(cx);
    service.update(cx, |service, cx| {
        let retained = candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<BTreeSet<_>>();
        let removed = service
            .core
            .snapshot
            .accounts
            .values()
            .filter(|account| {
                account.scope.category == AccountCategory::LlmProvider
                    && account
                        .scope
                        .native_provider_id
                        .as_ref()
                        .is_none_or(|id| !retained.contains(id))
                    || account.scope.runtime == "native-xai"
            })
            .map(|account| account.scope.id.clone())
            .collect::<Vec<_>>();
        let mut changed = Vec::new();
        for scope in removed {
            service.remove_scope(&scope, cx);
            changed.push(scope);
        }
        service
            .native_inventory
            .retain(|id, _| retained.contains(id));
        for candidate in candidates {
            if service.native_inventory.get(&candidate.id) == Some(&candidate) {
                continue;
            }
            service.native_generation = service.native_generation.saturating_add(1);
            let scope = format!("llm-provider:{}", candidate.id);
            let availability = if candidate.info.kind == NativeProviderKind::Ambiguous {
                Availability::Unavailable(ErrorCode::AttributionAmbiguous)
            } else {
                Availability::Unknown
            };
            let restore = !service.core.snapshot.accounts.contains_key(&scope);
            match service.core.inventory(
                AccountScope {
                    category: AccountCategory::LlmProvider,
                    native_provider_id: Some(candidate.id.clone()),
                    id: scope.clone(),
                    provider: ProviderId::LlmProvider,
                    runtime: "native-api".into(),
                    identity_known: false,
                },
                service.native_generation,
                availability,
            ) {
                Ok(()) => {
                    if restore && let Some(account) = service.core.snapshot.accounts.get(&scope) {
                        service.restore_snapshot(account.scope.clone(), account.generation, cx);
                    }
                    if let Some(account) = service.core.snapshot.accounts.get_mut(&scope) {
                        account.metadata.native_provider = Some(candidate.info.clone());
                    }
                    if active(&candidate.id) && candidate.capture_host.is_some() {
                        service.scheduler.startup(&scope);
                    } else if let Some(schedule) = service.scheduler.scopes.get_mut(&scope) {
                        schedule.suspended = true;
                    }
                    service
                        .native_inventory
                        .insert(candidate.id.clone(), candidate);
                    changed.push(scope);
                }
                Err(code) => log::warn!("crew_quota: native_inventory outcome={code:?}"),
            }
        }
        if !changed.is_empty() {
            service.core.snapshot.revision = service.core.snapshot.revision.saturating_add(1);
            for scope in &changed {
                cx.emit(QuotaEvent::AvailabilityChanged(scope.clone()));
            }
            cx.emit(QuotaEvent::Changed {
                scopes: changed,
                windows: Vec::new(),
                revision: service.core.snapshot.revision,
            });
            cx.notify();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use gpui::{AsyncApp, BorrowAppContext};
    use language_model::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FakeProvider {
        id: &'static str,
        authenticated: AtomicBool,
        authenticate_calls: AtomicUsize,
        state: Entity<()>,
    }
    impl LanguageModelProviderState for FakeProvider {
        type ObservableEntity = ();
        fn observable_entity(&self) -> Option<Entity<()>> {
            Some(self.state.clone())
        }
    }
    impl LanguageModelClient for FakeProvider {
        fn stream_completion(
            &self,
            _: &LanguageModel,
            _: LanguageModelRequest,
            _: &AsyncApp,
        ) -> BoxFuture<'static, Result<LanguageModelCompletionStream, LanguageModelCompletionError>>
        {
            panic!("inventory must not send inference")
        }
    }
    impl LanguageModelProvider for FakeProvider {
        fn id(&self) -> LanguageModelProviderId {
            LanguageModelProviderId::new(self.id)
        }
        fn name(&self) -> LanguageModelProviderName {
            LanguageModelProviderName(self.id.into())
        }
        fn default_model(&self, _: &App) -> Option<LanguageModel> {
            panic!("no model discovery")
        }
        fn default_fast_model(&self, _: &App) -> Option<LanguageModel> {
            panic!("no model discovery")
        }
        fn provided_models(&self, _: &App) -> Vec<LanguageModel> {
            panic!("no model discovery")
        }
        fn is_authenticated(&self, _: &App) -> bool {
            self.authenticated.load(Ordering::SeqCst)
        }
        fn authenticate(&self, _: &mut App) -> Task<Result<(), AuthenticateError>> {
            self.authenticate_calls.fetch_add(1, Ordering::SeqCst);
            Task::ready(Err(AuthenticateError::CredentialsNotFound))
        }
        fn settings_view(&self, _: &mut App) -> Option<ProviderSettingsView> {
            panic!("no settings views")
        }
    }

    struct InMemoryPorts;
    impl Clock for InMemoryPorts {
        fn now(&self) -> Timestamp {
            1000
        }
        fn monotonic(&self) -> u64 {
            0
        }
    }
    impl QuotaSource for InMemoryPorts {
        fn refresh(
            &self,
            _: String,
            _: u64,
            _: u64,
            _: &mut App,
        ) -> Task<Result<Observation, RefreshError>> {
            panic!("no provider reads")
        }
    }
    impl QuotaStore for InMemoryPorts {
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

    #[gpui::test]
    async fn unauthenticated_subscription_does_not_gate_xai(cx: &mut gpui::TestAppContext) {
        struct LiveClock;
        impl Clock for LiveClock {
            fn now(&self) -> Timestamp {
                chrono::Utc::now().timestamp()
            }
            fn monotonic(&self) -> u64 {
                0
            }
        }
        let service = cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            language_models::AllLanguageModelSettings::register(cx);
            language_model::init(cx);
            let registry = LanguageModelRegistry::global(cx);
            for (id, authenticated) in [("x_ai", true), ("x_ai_subscribed", false)] {
                let provider = Arc::new(FakeProvider {
                    id,
                    authenticated: AtomicBool::new(authenticated),
                    authenticate_calls: AtomicUsize::new(0),
                    state: cx.new(|_| ()),
                });
                registry.update(cx, |registry, cx| registry.register_provider(provider, cx));
            }
            let candidates = collect(cx);
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.id == "x_ai")
                .expect("xai");
            assert_eq!(candidate.capture_host, Some("api.x.ai"));
            cx.set_http_client(http_client::FakeHttpClient::create(|_| async {
                Ok(http_client::Response::builder()
                    .status(429)
                    .body(http_client::AsyncBody::empty())?)
            }));
            super::super::grok::install_http_observer(cx);
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(InMemoryPorts),
                        store: Rc::new(InMemoryPorts),
                        clock: Rc::new(LiveClock),
                    },
                    cx,
                )
            });
            service.update(cx, |service, _| service.finish_init(Ok(())));
            super::super::grok::init(&service, cx);
            service
        });
        let client = cx.read(|cx| cx.http_client());
        client
            .send(
                http_client::Request::post("https://api.x.ai/v1/chat/completions")
                    .body(http_client::AsyncBody::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            let account = service
                .core
                .snapshot
                .accounts
                .get("llm-provider:x_ai")
                .expect("xai row");
            assert_eq!(
                account
                    .metadata
                    .grok_response
                    .as_ref()
                    .and_then(|response| response.blocked),
                Some(GrokBlockReason::RateLimited)
            );
        });
    }

    #[gpui::test]
    fn startup_loads_only_local_credentials(cx: &mut gpui::TestAppContext) {
        let providers = cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            language_models::AllLanguageModelSettings::register(cx);
            language_model::init(cx);
            cx.set_http_client(http_client::FakeHttpClient::create(|_| async {
                panic!("credential loading must not call HTTP")
            }));
            super::super::grok::install_http_observer(cx);
            let registry = LanguageModelRegistry::global(cx);
            let mut providers = Vec::new();
            for id in [
                "x_ai",
                "openai",
                "deepseek",
                "mistral",
                "anthropic",
                "openrouter",
                "vercel_ai_gateway",
                "google",
                "copilot_chat",
                "chatgpt",
                "zed.dev",
                "bedrock",
                "custom",
            ] {
                let provider = Arc::new(FakeProvider {
                    id,
                    authenticated: AtomicBool::new(false),
                    authenticate_calls: AtomicUsize::new(0),
                    state: cx.new(|_| ()),
                });
                registry.update(cx, |registry, cx| {
                    registry.register_provider(provider.clone(), cx)
                });
                providers.push(provider);
            }
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(InMemoryPorts),
                        store: Rc::new(InMemoryPorts),
                        clock: Rc::new(InMemoryPorts),
                    },
                    cx,
                )
            });
            super::super::grok::init(&service, cx);
            (providers, service)
        });
        cx.run_until_parked();
        for provider in &providers.0 {
            let expected = usize::from(matches!(
                provider.id,
                "x_ai" | "openai" | "deepseek" | "mistral"
            ));
            assert_eq!(
                provider.authenticate_calls.load(Ordering::SeqCst),
                expected,
                "{}",
                provider.id
            );
        }
    }

    #[gpui::test]
    fn late_provider_state_change_creates_row(cx: &mut gpui::TestAppContext) {
        let (provider, service) = cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            language_models::AllLanguageModelSettings::register(cx);
            language_model::init(cx);
            cx.set_http_client(http_client::FakeHttpClient::create(|_| async {
                panic!("inventory must not call HTTP")
            }));
            super::super::grok::install_http_observer(cx);
            let provider = Arc::new(FakeProvider {
                id: "x_ai",
                authenticate_calls: AtomicUsize::new(0),
                authenticated: AtomicBool::new(false),
                state: cx.new(|_| ()),
            });
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(provider.clone(), cx)
            });
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(InMemoryPorts),
                        store: Rc::new(InMemoryPorts),
                        clock: Rc::new(InMemoryPorts),
                    },
                    cx,
                )
            });
            super::super::grok::init(&service, cx);
            (provider, service)
        });
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert!(
                !service
                    .core
                    .snapshot
                    .accounts
                    .contains_key("llm-provider:x_ai")
            )
        });
        provider.authenticated.store(true, Ordering::SeqCst);
        provider.state.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            let account = service
                .core
                .snapshot
                .accounts
                .get("llm-provider:x_ai")
                .expect("late ProviderStateChanged must create the provider row");
            assert_eq!(account.scope.category, AccountCategory::LlmProvider);
            assert_eq!(account.scope.native_provider_id.as_deref(), Some("x_ai"));
        });
    }

    #[gpui::test]
    fn registry_events_reconcile_rows_and_invalidate_generations(cx: &mut gpui::TestAppContext) {
        let (registry, provider, service) = cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            language_models::AllLanguageModelSettings::register(cx);
            language_model::init(cx);
            cx.set_http_client(http_client::FakeHttpClient::create(|_| async {
                panic!("inventory must not call HTTP")
            }));
            super::super::grok::install_http_observer(cx);
            let registry = LanguageModelRegistry::global(cx);
            let provider = Arc::new(FakeProvider {
                id: "x_ai",
                authenticate_calls: AtomicUsize::new(0),
                authenticated: AtomicBool::new(true),
                state: cx.new(|_| ()),
            });
            registry.update(cx, |registry, cx| {
                registry.register_provider(provider.clone(), cx)
            });
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(InMemoryPorts),
                        store: Rc::new(InMemoryPorts),
                        clock: Rc::new(InMemoryPorts),
                    },
                    cx,
                )
            });
            super::super::grok::init(&service, cx);
            (registry, provider, service)
        });
        cx.run_until_parked();
        let generation = service.read_with(cx, |service, _| {
            service.core.snapshot.accounts["llm-provider:x_ai"].generation
        });
        provider.state.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert!(service.core.snapshot.accounts["llm-provider:x_ai"].generation > generation)
        });
        provider.authenticated.store(false, Ordering::SeqCst);
        provider.state.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert!(service.core.snapshot.accounts.is_empty())
        });
        provider.authenticated.store(true, Ordering::SeqCst);
        provider.state.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert!(service.core.snapshot.accounts["llm-provider:x_ai"].generation > generation)
        });
        registry.update(cx, |registry, cx| {
            registry.unregister_provider(LanguageModelProviderId::new("x_ai"), cx)
        });
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert!(service.core.snapshot.accounts.is_empty())
        });
    }

    #[gpui::test]
    fn inventory_tracks_auth_visibility_and_custom_endpoints_without_io(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            language_models::AllLanguageModelSettings::register(cx);
            language_model::init(cx);
            cx.set_http_client(Arc::new(http_client::BlockedHttpClient::new()));
            let registry = LanguageModelRegistry::global(cx);
            let mut providers = Vec::new();
            for id in ["x_ai", "anthropic", "openai", "ollama", "lmstudio", "llama.cpp", "copilot_chat", "zed.dev", "custom"] {
                let provider = Arc::new(FakeProvider { id, authenticate_calls: AtomicUsize::new(0), authenticated: AtomicBool::new(true), state: cx.new(|_| ()) });
                registry.update(cx, |registry, cx| registry.register_provider(provider.clone(), cx));
                providers.push(provider);
            }
            let inventory = collect(cx);
            assert_eq!(inventory.len(), 9);
            assert_eq!(inventory.first().expect("hosted first").id, "zed.dev");
            assert_eq!(inventory.iter().filter(|provider| provider.info.kind == NativeProviderKind::Local).count(), 3);
            assert_eq!(inventory.iter().filter(|provider| provider.info.kind == NativeProviderKind::Passive).count(), 3);
            providers.first().expect("xai").authenticated.store(false, Ordering::SeqCst);
            assert!(!collect(cx).iter().any(|provider| provider.id == "x_ai"));
            providers.first().expect("xai").authenticated.store(true, Ordering::SeqCst);
            cx.update_global::<SettingsStore, _>(|settings, cx| settings.set_user_settings(
                r#"{"language_models":{"lmstudio":{"api_url":"https://api.anthropic.com"},"x_ai":{"api_url":"https://custom.example/v1"},"openai_compatible":{"custom":{"api_url":"https://api.openai.com/v1","available_models":[]}}}}"#, cx).expect("custom settings"));
            let inventory = collect(cx);
            assert_eq!(inventory.iter().find(|provider| provider.id == "anthropic").expect("anthropic").info.kind, NativeProviderKind::Ambiguous);
            assert_eq!(inventory.iter().find(|provider| provider.id == "lmstudio").expect("local").info.kind, NativeProviderKind::Local);
            assert_eq!(inventory.iter().find(|provider| provider.id == "x_ai").expect("xai").info.kind, NativeProviderKind::CustomEndpoint);
            assert_eq!(inventory.iter().find(|provider| provider.id == "openai").expect("openai").info.kind, NativeProviderKind::Ambiguous);
            assert_eq!(inventory.iter().find(|provider| provider.id == "custom").expect("custom").info.kind, NativeProviderKind::CustomEndpoint);
            registry.update(cx, |registry, cx| {
                registry.set_builtin_provider_hiding_fn(Box::new(|id| (id == "custom").then_some("collision")));
                registry.extension_installed("collision".into(), cx);
            });
            assert!(!collect(cx).iter().any(|provider| provider.id == "custom"));
            assert_eq!(collect(cx).iter().find(|provider| provider.id == "openai").expect("openai").info.kind, NativeProviderKind::Ambiguous);

            registry.update(cx, |registry, cx| {
                registry.unregister_provider(LanguageModelProviderId::new("custom"), cx);
                registry.set_builtin_provider_hiding_fn(Box::new(|id| (id == "openai").then_some("fixture")));
                registry.extension_installed("fixture".into(), cx);
            });
            assert!(!collect(cx).iter().any(|provider| provider.id == "custom" || provider.id == "openai"));
        });
    }
}

pub(super) fn active(id: &str) -> bool {
    matches!(id, "deepseek" | "openrouter" | "vercel_ai_gateway")
}

pub(super) fn refresh(
    scope: String,
    generation: u64,
    sequence: u64,
    cx: &mut App,
) -> Task<Result<Observation, RefreshError>> {
    let id = scope.trim_start_matches("llm-provider:").to_owned();
    let candidate = collect(cx)
        .into_iter()
        .find(|candidate| candidate.id == id && candidate.capture_host.is_some());
    let key = LanguageModelRegistry::read_global(cx)
        .providers()
        .into_iter()
        .find(|provider| provider.id().0.as_ref() == id)
        .and_then(|provider| provider.loaded_api_key(cx));
    let client = cx.http_client();
    let timer = cx.background_executor().timer(Duration::from_secs(30));
    cx.background_spawn(async move {
        let operation = async {
            if candidate.is_none() {
                return Err(RefreshError::NotRefreshable);
            }
            let key = key.ok_or(RefreshError::NotLoggedIn)?;
            read_balance(client, &id, key, scope, generation, sequence).await
        }
        .fuse();
        let timer = timer.fuse();
        futures::pin_mut!(operation, timer);
        let result = futures::select_biased! {
            result = operation => result,
            _ = timer => Err(RefreshError::Failed(ErrorCode::Timeout)),
        };
        match &result {
            Ok(_) => {
                log::info!("crew_quota: native_active provider={id} outcome=Ok reason=Balance")
            }
            Err(error) => {
                log::info!("crew_quota: native_active provider={id} outcome=Error reason={error:?}")
            }
        }
        result
    })
}

async fn read_balance(
    client: Arc<dyn http_client::HttpClient>,
    id: &str,
    key: String,
    scope: String,
    generation: u64,
    sequence: u64,
) -> Result<Observation, RefreshError> {
    use futures::AsyncReadExt;
    use http_client::HttpRequestExt as _;
    let url = match id {
        "deepseek" => "https://api.deepseek.com/user/balance",
        "openrouter" => "https://openrouter.ai/api/v1/key",
        "vercel_ai_gateway" => "https://ai-gateway.vercel.sh/v1/credits",
        _ => return Err(RefreshError::NotRefreshable),
    };
    let mut authorization = http_client::http::HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
    authorization.set_sensitive(true);
    let request = http_client::Request::get(url)
        .extension(super::grok::AgentProbe)
        .follow_redirects(http_client::RedirectPolicy::NoFollow)
        .timeout(Duration::from_secs(30))
        .header("authorization", authorization)
        .body(http_client::AsyncBody::empty())
        .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
    let response = client
        .send(request)
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
    let now = chrono::Utc::now().timestamp();
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(RefreshError::NotLoggedIn),
        429 => {
            return Err(RefreshError::Deferred {
                retry_at: response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| retry_after(value, now))
                    .unwrap_or(now + 120),
            });
        }
        _ => return Err(RefreshError::Failed(ErrorCode::Transport)),
    }
    let mut bytes = Vec::new();
    response
        .into_body()
        .take(65537)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
    if bytes.len() > 65536 {
        return Err(RefreshError::Failed(ErrorCode::InvalidData));
    }
    parse_balance(id, &bytes, scope, generation, sequence, now)
}

fn parse_balance(
    id: &str,
    bytes: &[u8],
    scope: String,
    generation: u64,
    sequence: u64,
    now: Timestamp,
) -> Result<Observation, RefreshError> {
    let invalid = || RefreshError::Failed(ErrorCode::InvalidData);
    let body: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let number = |value: &serde_json::Value| -> Option<f64> {
        let number = value.as_f64().or_else(|| value.as_str()?.parse().ok())?;
        number.is_finite().then_some(number)
    };
    let mut balances = Vec::new();
    let available = match id {
        "deepseek" => {
            for balance in body["balance_infos"].as_array().ok_or_else(invalid)? {
                let currency = balance["currency"]
                    .as_str()
                    .filter(|currency| {
                        currency.len() == 3
                            && currency.bytes().all(|byte| byte.is_ascii_uppercase())
                    })
                    .ok_or_else(invalid)?;
                balances.push((
                    currency.to_owned(),
                    number(&balance["total_balance"]).ok_or_else(invalid)?,
                    None,
                ));
            }
            body["is_available"].as_bool().ok_or_else(invalid)?
        }
        "openrouter" => {
            let data = &body["data"];
            let limit = number(&data["limit"]).filter(|limit| *limit >= 0.0);
            let remaining =
                number(&data["limit_remaining"]).or_else(|| Some(limit? - number(&data["usage"])?));
            if let Some(remaining) = remaining {
                balances.push(("USD".into(), remaining, limit));
            } else if !data["limit"].is_null() || number(&data["usage"]).is_none() {
                return Err(invalid());
            }
            true
        }
        "vercel_ai_gateway" => {
            number(&body["total_used"]).ok_or_else(invalid)?;
            balances.push((
                "USD".into(),
                number(&body["balance"]).ok_or_else(invalid)?,
                None,
            ));
            true
        }
        _ => return Err(invalid()),
    };
    let blocked = !available
        || (!balances.is_empty() && balances.iter().all(|(_, remaining, _)| *remaining <= 0.0));
    let windows = balances
        .into_iter()
        .map(|(currency, remaining, limit)| {
            let unit = Unit::Money {
                currency: currency.clone(),
                scale: 0,
            };
            WindowPatch {
                key: WindowKey {
                    bucket: BucketKey {
                        scope: scope.clone(),
                        bucket: currency,
                        applicability: Applicability::AllModels,
                    },
                    role: "balance".into(),
                    duration_seconds: None,
                },
                used_fraction: Patch::Set(
                    limit
                        .filter(|limit| *limit > 0.0 && remaining >= 0.0 && remaining <= *limit)
                        .map(|limit| 1.0 - remaining / limit),
                ),
                remaining: Patch::Set(Some(Amount {
                    value: remaining.max(0.0),
                    unit: unit.clone(),
                })),
                limit: Patch::Set(limit.map(|value| Amount { value, unit })),
                reset_at: Patch::Set(None),
                admission: Patch::Set(Admission::Unknown),
                disabled: Patch::Missing,
                precision: Precision::Exact,
            }
        })
        .collect();
    Ok(Observation {
        scope,
        generation,
        source: SourceKind::NativeBalanceRead,
        source_version: "native-balance-v1".into(),
        observed_at: now,
        provider_sample_at: None,
        sequence: sequence.saturating_add(1),
        start_sequence: Some(sequence),
        windows,
        complete: false,
        metadata: AccountMetadata {
            grok_response: Some(GrokResponse {
                blocked: blocked.then_some(GrokBlockReason::SpendingLimit),
                status: 200,
                observed_at: now,
                retry_at: None,
            }),
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod balance_tests {
    use super::*;

    #[test]
    fn documented_balances_and_zero() {
        for (id, body, expected) in [
            (
                "deepseek",
                r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"12.50"}]}"#,
                12.5,
            ),
            (
                "openrouter",
                r#"{"data":{"limit":10,"usage":3,"limit_remaining":7}}"#,
                7.0,
            ),
            (
                "vercel_ai_gateway",
                r#"{"balance":"4.25","total_used":"5.75"}"#,
                4.25,
            ),
        ] {
            let observation =
                parse_balance(id, body.as_bytes(), "scope".into(), 1, 0, 100).expect("schema");
            assert!(
                matches!(observation.windows.first().expect("window").remaining, Patch::Set(Some(Amount {value, ..})) if value == expected)
            );
        }
        let observation = parse_balance(
            "deepseek",
            br#"{"is_available":false,"balance_infos":[{"currency":"USD","total_balance":"0"}]}"#,
            "scope".into(),
            1,
            0,
            100,
        )
        .expect("zero");
        assert_eq!(
            observation.metadata.grok_response.expect("state").blocked,
            Some(GrokBlockReason::SpendingLimit)
        );
        assert!(parse_balance("deepseek", b"{}", "scope".into(), 1, 0, 100).is_err());
        assert_eq!(trusted_host("deepseek", "https://custom.example"), None);
        assert_eq!(
            trusted_host("openrouter", "https://openrouter.ai/api/v1/evil"),
            None
        );
    }

    #[gpui::test]
    async fn fake_transport_statuses(cx: &mut gpui::TestAppContext) {
        for status in [200, 401, 429] {
            let client = http_client::FakeHttpClient::create(move |request| async move {
                assert_eq!(request.uri(), "https://ai-gateway.vercel.sh/v1/credits");
                assert!(
                    request
                        .extensions()
                        .get::<super::super::grok::AgentProbe>()
                        .is_some()
                );
                Ok(http_client::Response::builder().status(status).body(
                    http_client::AsyncBody::from(r#"{"balance":"0","total_used":"2"}"#),
                )?)
            });
            let result = read_balance(
                client,
                "vercel_ai_gateway",
                "fake-key".into(),
                "scope".into(),
                1,
                0,
            )
            .await;
            match status {
                200 => assert_eq!(
                    result
                        .expect("balance")
                        .metadata
                        .grok_response
                        .expect("state")
                        .blocked,
                    Some(GrokBlockReason::SpendingLimit)
                ),
                401 => assert_eq!(result, Err(RefreshError::NotLoggedIn)),
                _ => assert!(matches!(result, Err(RefreshError::Deferred { .. }))),
            }
        }
        cx.run_until_parked();
    }
}
