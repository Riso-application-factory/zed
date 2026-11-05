use super::*;

pub(super) struct FixturePreview {
    pub snapshot: Arc<QuotaSnapshot>,
    pub preferences: Preferences,
    pub left: bool,
    pub states: bool,
}

impl FixturePreview {
    pub fn from_environment() -> Option<Self> {
        let scene = std::env::var("CREW_QUOTA_PREVIEW").ok()?;
        if scene != "overview" && scene != "states" {
            return None;
        }
        let mut preview = Self::new(chrono::Utc::now().timestamp(), scene == "states");
        preview.preferences.compact =
            std::env::var("CREW_QUOTA_PREVIEW_COMPACT").as_deref() == Ok("1");
        preview.preferences.used = std::env::var("CREW_QUOTA_PREVIEW_USED").as_deref() == Ok("1");
        preview.left = std::env::var("CREW_QUOTA_PREVIEW_LEFT").as_deref() == Ok("1");
        let collapsed = std::env::var("CREW_QUOTA_PREVIEW_COLLAPSED").ok();
        if collapsed.as_deref() == Some("mixed") {
            preview
                .preferences
                .collapsed
                .extend([ProviderId::Grok, ProviderId::Antigravity]);
        }
        preview.preferences.agents_collapsed = collapsed.as_deref() == Some("agents");
        preview.preferences.llm_providers_collapsed = collapsed.as_deref() == Some("llm");
        Some(preview)
    }

    pub fn new(now: Timestamp, states: bool) -> Self {
        let mut snapshot = QuotaSnapshot::default();
        let mut codex = account("codex", ProviderId::Codex, "ChatGPT Plus", now - 60);
        add_window(
            &mut codex,
            "ChatGPT",
            "5h",
            Some(0.26),
            Some(now + 8040),
            SourceKind::CodexAppServerRead,
            now - 60,
        );
        add_window(
            &mut codex,
            "ChatGPT",
            "Weekly",
            Some(0.03),
            Some(now + 3 * 86400 + 4 * 3600),
            SourceKind::CodexAppServerRead,
            now - 60,
        );
        let mut claude = account("claude", ProviderId::Claude, "Max", now - 60);
        add_window(
            &mut claude,
            "Subscription",
            "5h",
            Some(0.67),
            Some(now + 8040),
            SourceKind::ClaudeAcpEvent,
            now - 60,
        );
        add_window(
            &mut claude,
            "Subscription",
            "7d",
            Some(0.88),
            Some(now + 2 * 86400 + 6 * 3600),
            SourceKind::ClaudeAcpEvent,
            now - 60,
        );
        let mut antigravity = account(
            "antigravity",
            ProviderId::Antigravity,
            "Google AI Ultra",
            now - 60,
        );
        add_window(
            &mut antigravity,
            "Gemini Models",
            "5h",
            Some(0.01),
            Some(now + 3 * 3600),
            SourceKind::AntigravityAcpUsage,
            now - 60,
        );
        add_window(
            &mut antigravity,
            "Gemini Models",
            "Weekly",
            Some(0.10),
            Some(now + 4 * 86400),
            SourceKind::AntigravityAcpUsage,
            now - 60,
        );
        add_window(
            &mut antigravity,
            "Claude / GPT",
            "5h",
            Some(0.),
            Some(now + 8040),
            SourceKind::AntigravityAcpUsage,
            now - 60,
        );
        add_window(
            &mut antigravity,
            "Claude / GPT",
            "Weekly",
            Some(0.89),
            Some(now + 86400 + 2 * 3600),
            SourceKind::AntigravityAcpUsage,
            now - 60,
        );
        let mut grok = native_account("x_ai", "xAI API", NativeProviderKind::Passive, 0, now - 60);
        for (bucket, role, used, remaining, limit, unit) in [
            (
                "api.x.ai/v1/chat/completions/requests",
                "requests",
                1.,
                0.,
                2000.,
                Unit::Requests,
            ),
            (
                "api.x.ai/v1/chat/completions/tokens",
                "tokens",
                0.80,
                2000000.,
                10000000.,
                Unit::Tokens,
            ),
        ] {
            add_window(
                &mut grok,
                bucket,
                role,
                Some(used),
                Some(now + 60),
                SourceKind::GrokResponseHeaders,
                now - 60,
            );
            if let Some(window) = grok
                .windows
                .values_mut()
                .find(|window| window.key.bucket.bucket == bucket && window.key.role == role)
            {
                window.remaining = Some(sample(
                    Some(Amount {
                        value: remaining,
                        unit: unit.clone(),
                    }),
                    SourceKind::GrokResponseHeaders,
                    now - 60,
                ));
                window.limit = Some(sample(
                    Some(Amount { value: limit, unit }),
                    SourceKind::GrokResponseHeaders,
                    now - 60,
                ));
                window.key.bucket.applicability = Applicability::Family(
                    "Latest api.x.ai response · Model unverified (/v1/chat/completions)".into(),
                );
            }
        }
        // Applicability is part of the map key, so changing it requires re-keying.
        grok.windows = grok
            .windows
            .into_values()
            .map(|window| (window.key.clone(), window))
            .collect();
        if states {
            claude.availability = Availability::NotLoggedIn;
            claude.windows.clear();
            claude.last_success_at = None;

            codex.availability = Availability::Error(ErrorCode::Transport);
            codex.last_success_at = Some(now - 23 * 60);
            for window in codex.windows.values_mut() {
                if let Some(sample) = &mut window.used_fraction {
                    sample.observed_at = now - 23 * 60;
                    if window.key.role == "5h" {
                        sample.value = Some(0.67);
                    }
                }
            }
        }
        let mut anthropic = native_account(
            "anthropic",
            "Anthropic API",
            NativeProviderKind::Passive,
            1,
            now - 60,
        );
        anthropic.metadata.grok_response = Some(GrokResponse {
            blocked: Some(GrokBlockReason::SpendingLimit),
            status: 400,
            observed_at: now - 60,
            retry_at: None,
        });
        let mut openai =
            native_account("openai", "OpenAI API", NativeProviderKind::Passive, 2, now);
        openai.last_success_at = None;
        for account in [
            codex,
            claude,
            antigravity,
            grok,
            anthropic,
            openai,
            native_account("ollama", "Ollama", NativeProviderKind::Local, 3, now),
            native_account(
                "copilot_chat",
                "Copilot",
                NativeProviderKind::Unavailable,
                4,
                now,
            ),
            native_account(
                "custom",
                "Custom endpoint",
                NativeProviderKind::CustomEndpoint,
                5,
                now,
            ),
        ] {
            snapshot.accounts.insert(account.scope.id.clone(), account);
        }
        Self {
            snapshot: Arc::new(snapshot),
            preferences: Preferences::default(),
            left: false,
            states,
        }
    }
}

fn native_account(
    id: &str,
    name: &str,
    kind: NativeProviderKind,
    order: usize,
    now: Timestamp,
) -> AccountState {
    let mut account = account(id, ProviderId::LlmProvider, "", now);
    account.scope.category = AccountCategory::LlmProvider;
    account.scope.native_provider_id = Some(id.into());
    account.metadata.plan = None;
    account.metadata.native_provider = Some(NativeProviderInfo {
        name: name.into(),
        kind,
        order,
    });
    account
}

fn sample<T>(value: T, source: SourceKind, observed_at: Timestamp) -> Measurement<T> {
    Measurement {
        value,
        observed_at,
        provider_sample_at: None,
        sequence: 1,
        source,
    }
}

fn account(id: &str, provider: ProviderId, plan: &str, now: Timestamp) -> AccountState {
    AccountState {
        scope: AccountScope {
            category: crate::AccountCategory::ExternalAgent,
            native_provider_id: None,
            id: id.into(),
            provider,
            runtime: "sanitized-fixture".into(),
            identity_known: false,
        },
        generation: 0,
        availability: Availability::Ready,
        refresh: RefreshState::Idle,
        windows: BTreeMap::new(),
        metadata: AccountMetadata {
            plan: Some(PlanInfo {
                name: plan.into(),
                observed_at: now,
                source: SourceKind::Persisted,
            }),
            ..Default::default()
        },
        last_attempt_at: Some(now),
        last_success_at: Some(now),
        source_version: None,
        complete: true,
        sequence: 1,
    }
}

fn add_window(
    account: &mut AccountState,
    bucket: &str,
    role: &str,
    used: Option<f64>,
    reset: Option<Timestamp>,
    source: SourceKind,
    now: Timestamp,
) {
    let key = WindowKey {
        bucket: BucketKey {
            scope: account.scope.id.clone(),
            bucket: bucket.into(),
            applicability: Applicability::AllModels,
        },
        role: role.into(),
        duration_seconds: match role {
            "5h" => Some(18000),
            "7d" | "Weekly" => Some(604800),
            _ => None,
        },
    };
    account.windows.insert(
        key.clone(),
        QuotaWindow {
            key,
            used_fraction: used.map(|used| sample(Some(used), source, now)),
            remaining: None,
            limit: None,
            reset_at: reset.map(|reset| sample(Some(reset), source, now)),
            admission: used.map(|used| {
                sample(
                    if used >= 1. {
                        Admission::Exhausted
                    } else if used >= 0.6 {
                        Admission::Warning
                    } else {
                        Admission::Allowed
                    },
                    source,
                    now,
                )
            }),
            disabled: None,
            precision: Precision::RoundedPercent,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures_keep_windows_states_and_units_distinct() {
        let now = 100000;
        let preview = FixturePreview::new(now, false);
        assert_eq!(preview.snapshot.accounts.len(), 9);
        let codex = preview
            .snapshot
            .accounts
            .get("codex")
            .expect("Codex fixture");
        assert_eq!(codex.windows.len(), 2);
        assert!(codex.metadata.credit_balance.is_none());
        let grok = preview.snapshot.accounts.get("x_ai").expect("Grok fixture");
        assert_eq!(grok.windows.len(), 2);
        let mut amount_only = grok
            .windows
            .values()
            .find(|window| {
                window.key.bucket.bucket == "api.x.ai/v1/chat/completions/requests"
                    && window.key.role == "requests"
            })
            .expect("request fixture")
            .clone();
        amount_only.limit = None;
        assert_eq!(
            amount_text(&amount_only).as_deref(),
            Some("0 requests left")
        );
        assert!(
            grok.windows
                .iter()
                .all(|(key, window)| key == &window.key && window.reset_at.is_some())
        );
        assert_eq!(
            provider_rows(&preview.snapshot)
                .first()
                .map(|row| row.provider),
            Some(ProviderId::Antigravity)
        );
        let states = FixturePreview::new(now, true);
        let statuses = states
            .snapshot
            .accounts
            .values()
            .map(|account| account_status(account, now))
            .collect::<Vec<_>>()
            .join(" ");
        for state in ["Error", "Stale", "Not logged in"] {
            assert!(statuses.contains(state), "missing {state}");
        }
        assert_eq!(states.snapshot.accounts.len(), 9);
        let antigravity = preview
            .snapshot
            .accounts
            .get("antigravity")
            .expect("Antigravity fixture");
        assert_eq!(antigravity.windows.len(), 4);
    }
}
