use super::*;

pub(super) struct FixturePreview {
    pub snapshot: Arc<QuotaSnapshot>,
    pub notes: BTreeMap<String, Vec<&'static str>>,
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
        Some(preview)
    }

    pub fn new(now: Timestamp, states: bool) -> Self {
        let mut snapshot = QuotaSnapshot::default();
        let mut notes = BTreeMap::new();
        let mut codex = account("codex", ProviderId::Codex, "ChatGPT Plus", now - 60);
        codex.metadata.available_reset_credits = Some(3);
        codex.metadata.credit_balance = Some(Amount {
            value: 120.,
            unit: Unit::Credits { denomination: None },
        });
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
        claude.metadata.ordinary_usage_allowed = Some(true);
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
        notes.insert(
            "claude".into(),
            vec!["Overage rejected · extra usage unavailable"],
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
        add_window(
            &mut antigravity,
            "Experimental models",
            "Access",
            None,
            None,
            SourceKind::AntigravityAcpUsage,
            now - 60,
        );
        if let Some(window) = antigravity
            .windows
            .values_mut()
            .find(|window| window.key.bucket.bucket == "Experimental models")
        {
            window.disabled = Some(sample(true, SourceKind::AntigravityAcpUsage, now - 60));
        }
        notes.insert(
            "antigravity".into(),
            vec!["Separate shared model groups; overlapping limits are not added."],
        );
        let mut grok = account("grok", ProviderId::Grok, "API · per model", now - 60);
        for (model, role, used, remaining, limit, unit) in [
            ("grok-4", "Requests", 1., 0., 2000., Unit::Requests),
            ("grok-4", "Tokens", 0.80, 2000000., 10000000., Unit::Tokens),
            (
                "grok-4-fast",
                "Requests",
                0.10,
                1800.,
                2000.,
                Unit::Requests,
            ),
            (
                "grok-4-fast",
                "Tokens",
                0.42,
                5800000.,
                10000000.,
                Unit::Tokens,
            ),
        ] {
            add_window(
                &mut grok,
                model,
                role,
                Some(used),
                None,
                SourceKind::GrokResponseHeaders,
                now - 60,
            );
            if let Some(window) = grok
                .windows
                .values_mut()
                .find(|window| window.key.bucket.bucket == model && window.key.role == role)
            {
                window.key.bucket.applicability = Applicability::Model(model.into());
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
            }
        }
        // Model applicability is part of the map key, so changing it requires re-keying.
        grok.windows = grok
            .windows
            .into_values()
            .map(|window| (window.key.clone(), window))
            .collect();
        if states {
            let mut disabled = account(
                "disabled",
                ProviderId::Antigravity,
                "Disabled bucket",
                now - 60,
            );
            if let Some(window) = antigravity
                .windows
                .values()
                .find(|window| window.disabled.as_ref().is_some_and(|sample| sample.value))
            {
                let mut window = window.clone();
                window.key.bucket.scope = disabled.scope.id.clone();
                disabled.windows.insert(window.key.clone(), window);
            }
            snapshot
                .accounts
                .insert(disabled.scope.id.clone(), disabled);
            claude.availability = Availability::NotLoggedIn;
            claude.windows.clear();
            claude.last_success_at = None;
            notes.remove("claude");
            let mut unsupported = account("unsupported", ProviderId::Claude, "Source pending", now);
            unsupported.windows.clear();
            unsupported.availability = Availability::Unsupported(ErrorCode::UnsupportedVersion);
            unsupported.last_success_at = None;
            snapshot
                .accounts
                .insert(unsupported.scope.id.clone(), unsupported);
            let mut loading = account("loading", ProviderId::Codex, "Waiting for first read", now);
            loading.last_success_at = None;
            loading.refresh = RefreshState::InFlight;
            loading.availability = Availability::Unknown;
            snapshot.accounts.insert(loading.scope.id.clone(), loading);
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
        for account in [codex, claude, antigravity, grok] {
            snapshot.accounts.insert(account.scope.id.clone(), account);
        }
        Self {
            snapshot: Arc::new(snapshot),
            notes,
            preferences: Preferences::default(),
            left: false,
            states,
        }
    }
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
        assert_eq!(preview.snapshot.accounts.len(), 4);
        let codex = preview
            .snapshot
            .accounts
            .get("codex")
            .expect("Codex fixture");
        assert_eq!(codex.windows.len(), 2);
        assert_eq!(codex.metadata.available_reset_credits, Some(3));
        let grok = preview.snapshot.accounts.get("grok").expect("Grok fixture");
        assert_eq!(grok.windows.len(), 4);
        let mut amount_only = grok
            .windows
            .values()
            .find(|window| {
                window.key.bucket.bucket == "grok-4-fast" && window.key.role == "Requests"
            })
            .expect("request fixture")
            .clone();
        amount_only.limit = None;
        assert_eq!(
            amount_text(&amount_only).as_deref(),
            Some("1,800 requests left")
        );
        assert!(
            grok.windows
                .iter()
                .all(|(key, window)| key == &window.key && window.reset_at.is_none())
        );
        assert_eq!(
            provider_rows(&preview.snapshot)
                .first()
                .map(|row| row.provider),
            Some(ProviderId::Grok)
        );
        let states = FixturePreview::new(now, true);
        let statuses = states
            .snapshot
            .accounts
            .values()
            .map(|account| account_status(account, now))
            .collect::<Vec<_>>()
            .join(" ");
        for state in [
            "Error",
            "Stale",
            "Not logged in",
            "Unsupported",
            "Loading",
            "Unknown",
            "Exhausted",
        ] {
            assert!(statuses.contains(state), "missing {state}");
        }
        assert!(
            states
                .snapshot
                .accounts
                .values()
                .flat_map(|account| account.windows.values())
                .any(|window| window.disabled.as_ref().is_some_and(|sample| sample.value))
        );
    }
}
