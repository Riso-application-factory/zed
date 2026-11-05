use super::*;
use language_model::{LanguageModelProvider, LanguageModelRegistry};
use settings::Settings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub id: String,
    pub info: NativeProviderInfo,
    pub capture_host: Option<&'static str>,
    fingerprint: String,
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
        "deepseek" => (&settings.deepseek.api_url, ""),
        "google" => (&settings.google.api_url, ""),
        "mistral" => (&settings.mistral.api_url, ""),
        "openrouter" => (&settings.open_router.api_url, ""),
        "opencode" => (&settings.opencode.api_url, ""),
        "vercel_ai_gateway" => (&settings.vercel_ai_gateway.api_url, ""),
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

// These implementations only call ApiKeyState::load_if_needed. Anthropic,
// OpenRouter and Vercel also fetch models, so must remain observation-only.
pub(super) fn load_credentials(cx: &mut App) {
    let providers = LanguageModelRegistry::read_global(cx).providers();
    for provider in providers {
        let id = provider.id().0.to_string();
        if !matches!(id.as_str(), "x_ai" | "openai" | "deepseek" | "mistral")
            || provider.is_authenticated(cx)
        {
            continue;
        }
        let task = provider.authenticate(cx);
        cx.spawn(async move |_| {
            let state = if task.await.is_ok() {
                "Completed"
            } else {
                "Unavailable"
            };
            log::info!("crew_quota: native_credentials provider={id} state={state}");
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
            if let Some(host) = capture_host {
                if authenticated.iter().any(|other| {
                    other.id().0.as_ref() != id
                        && endpoint_for_provider(other.as_ref(), settings)
                            .is_some_and(|endpoint| shares_host(&endpoint, host))
                }) {
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
    for provider in LanguageModelRegistry::read_global(cx).visible_providers() {
        let id = provider.id().0;
        if valid_text(&id, 128) {
            let state = if provider.is_authenticated(cx) {
                "Authenticated"
            } else {
                "NotAuthenticated"
            };
            log::info!("crew_quota: native_inventory provider={id} state={state}");
        }
    }
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
                    if let Some(account) = service.core.snapshot.accounts.get_mut(&scope) {
                        account.metadata.native_provider = Some(candidate.info.clone());
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
