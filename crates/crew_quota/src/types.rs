use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};

pub type Timestamp = i64;
pub type AuthGeneration = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Claude,
    Codex,
    Antigravity,
    Grok,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstallationKey {
    pub id: String,
    pub runtime: String,
    pub version: Option<String>,
    pub config_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountScope {
    pub id: String,
    pub provider: ProviderId,
    pub runtime: String,
    pub identity_known: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Applicability {
    AllModels,
    Model(String),
    Family(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BucketKey {
    pub scope: String,
    pub bucket: String,
    pub applicability: Applicability,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowKey {
    pub bucket: BucketKey,
    pub role: String,
    pub duration_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unit {
    Fraction,
    Requests,
    Tokens,
    UnspecifiedUnits,
    Credits { denomination: Option<String> },
    Money { currency: String, scale: u32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Amount {
    pub value: f64,
    pub unit: Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Admission {
    Allowed,
    Warning,
    Exhausted,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Precision {
    Fraction,
    RoundedPercent,
    Exact,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    CodexAppServerRead,
    ClaudeAcpEvent,
    ClaudeHeaderProbe,
    AntigravityAcpUsage,
    GrokResponseHeaders,
    Persisted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    Age,
    Persisted,
    ResetPassed,
    ClockChanged,
    GenerationChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale { age: Duration, reason: StaleReason },
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    Timeout,
    Transport,
    Protocol,
    InvalidData,
    Persistence,
    UnsupportedVersion,
    UnsupportedPlatform,
    CleanupFailed,
    InferenceAccess,
    StoredAuthUnsupported,
    AttributionAmbiguous,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Ready,
    Unknown,
    Unsupported(ErrorCode),
    NotInstalled,
    NotSetUp,
    NotLoggedIn,
    Unavailable(ErrorCode),
    Error(ErrorCode),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefreshState {
    Idle,
    Queued,
    InFlight,
    Backoff { retry_at: Timestamp },
    Suspended(ErrorCode),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measurement<T> {
    pub value: T,
    pub observed_at: Timestamp,
    pub provider_sample_at: Option<Timestamp>,
    pub sequence: u64,
    pub source: SourceKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub key: WindowKey,
    pub used_fraction: Option<Measurement<Option<f64>>>,
    pub remaining: Option<Measurement<Option<Amount>>>,
    pub limit: Option<Measurement<Option<Amount>>>,
    pub reset_at: Option<Measurement<Option<Timestamp>>>,
    pub admission: Option<Measurement<Admission>>,
    pub disabled: Option<Measurement<bool>>,
    pub precision: Precision,
}

impl QuotaWindow {
    pub fn remaining_fraction(&self) -> Option<f64> {
        Some((1.0 - self.used_fraction.as_ref()?.value?).clamp(0.0, 1.0))
    }

    pub fn freshness(&self, now: Timestamp, max_age: Duration) -> Freshness {
        self.freshness_for(now, max_age, &[])
    }

    pub fn freshness_for(
        &self,
        now: Timestamp,
        max_age: Duration,
        required: &[MeasurementKind],
    ) -> Freshness {
        let measurements = [
            (
                MeasurementKind::UsedFraction,
                self.used_fraction
                    .as_ref()
                    .filter(|sample| sample.value.is_some())
                    .map(|sample| (sample.observed_at, sample.source)),
            ),
            (
                MeasurementKind::Remaining,
                self.remaining
                    .as_ref()
                    .filter(|sample| sample.value.is_some())
                    .map(|sample| (sample.observed_at, sample.source)),
            ),
            (
                MeasurementKind::Limit,
                self.limit
                    .as_ref()
                    .filter(|sample| sample.value.is_some())
                    .map(|sample| (sample.observed_at, sample.source)),
            ),
        ];
        if required.iter().any(|kind| {
            measurements
                .iter()
                .any(|(candidate, sample)| candidate == kind && sample.is_none())
        }) {
            return Freshness::Unknown;
        }
        let samples = measurements
            .into_iter()
            .filter(|(kind, _)| required.is_empty() || required.contains(kind))
            .filter_map(|(_, sample)| sample)
            .collect::<Vec<_>>();
        if samples.is_empty() {
            return Freshness::Unknown;
        }
        let age = samples
            .iter()
            .map(|(at, _)| Duration::from_secs(now.saturating_sub(*at).max(0) as u64))
            .max()
            .unwrap_or_default();
        let reason = if samples
            .iter()
            .any(|(_, source)| *source == SourceKind::Persisted)
        {
            Some(StaleReason::Persisted)
        } else if samples.iter().any(|(at, _)| now < *at) {
            Some(StaleReason::ClockChanged)
        } else if self
            .reset_at
            .as_ref()
            .and_then(|sample| sample.value)
            .is_some_and(|reset| reset <= now)
        {
            Some(StaleReason::ResetPassed)
        } else if age >= max_age {
            Some(StaleReason::Age)
        } else {
            None
        };
        match reason {
            Some(reason) => Freshness::Stale { age, reason },
            None => Freshness::Fresh,
        }
    }

    pub(crate) fn expire(&mut self, now: Timestamp) -> bool {
        fn expire<T>(sample: &mut Option<Measurement<T>>, now: Timestamp) {
            if sample.as_ref().is_some_and(|sample| {
                now.saturating_sub(sample.observed_at) > crate::RETENTION_SECONDS
            }) {
                *sample = None;
            }
        }
        expire(&mut self.used_fraction, now);
        expire(&mut self.remaining, now);
        expire(&mut self.limit, now);
        expire(&mut self.reset_at, now);
        expire(&mut self.admission, now);
        expire(&mut self.disabled, now);
        self.used_fraction.is_some()
            || self.remaining.is_some()
            || self.limit.is_some()
            || self.reset_at.is_some()
            || self.admission.is_some()
            || self.disabled.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Patch<T> {
    #[default]
    Missing,
    Set(T),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPatch {
    pub key: WindowKey,
    pub used_fraction: Patch<Option<f64>>,
    pub remaining: Patch<Option<Amount>>,
    pub limit: Patch<Option<Amount>>,
    pub reset_at: Patch<Option<Timestamp>>,
    pub admission: Patch<Admission>,
    pub disabled: Patch<bool>,
    pub precision: Precision,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanInfo {
    pub name: String,
    pub observed_at: Timestamp,
    pub source: SourceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrokBlockReason {
    SpendingLimit,
    RateLimited,
    Forbidden,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrokResponse {
    pub blocked: Option<GrokBlockReason>,
    pub status: u16,
    pub observed_at: Timestamp,
    pub retry_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountMetadata {
    #[serde(default)]
    pub grok_response: Option<GrokResponse>,
    pub plan: Option<PlanInfo>,
    pub ordinary_usage_allowed: Option<bool>,
    pub spend_control_reached: Option<bool>,
    pub has_credits: Option<bool>,
    pub unlimited_credits: Option<bool>,
    pub credit_balance: Option<Amount>,
    pub available_reset_credits: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub scope: String,
    pub generation: AuthGeneration,
    pub source: SourceKind,
    pub source_version: String,
    pub observed_at: Timestamp,
    pub provider_sample_at: Option<Timestamp>,
    pub sequence: u64,
    pub start_sequence: Option<u64>,
    pub windows: Vec<WindowPatch>,
    pub complete: bool,
    pub metadata: AccountMetadata,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    pub scope: AccountScope,
    pub generation: AuthGeneration,
    pub availability: Availability,
    pub refresh: RefreshState,
    #[serde(with = "window_records")]
    pub windows: BTreeMap<WindowKey, QuotaWindow>,
    pub metadata: AccountMetadata,
    pub last_attempt_at: Option<Timestamp>,
    pub last_success_at: Option<Timestamp>,
    pub source_version: Option<String>,
    pub complete: bool,
    pub sequence: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaSnapshot {
    pub revision: u64,
    pub accounts: BTreeMap<String, AccountState>,
}

#[derive(Clone, Debug, Default)]
pub struct Selector {
    pub scopes: Vec<String>,
    pub provider: Option<ProviderId>,
    pub windows: Vec<WindowKey>,
    pub model: Option<String>,
}

impl Selector {
    pub fn matches(&self, scope: &AccountScope) -> bool {
        (self.scopes.is_empty() || self.scopes.contains(&scope.id))
            && self
                .provider
                .is_none_or(|provider| provider == scope.provider)
    }
    pub fn matches_window(&self, window: &WindowKey) -> bool {
        (self.windows.is_empty() || self.windows.contains(window))
            && self
                .model
                .as_ref()
                .is_none_or(|model| match &window.bucket.applicability {
                    Applicability::AllModels => true,
                    Applicability::Model(value) => value == model,
                    Applicability::Family(_) => false,
                })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasurementKind {
    UsedFraction,
    Remaining,
    Limit,
}

#[derive(Clone, Debug)]
pub struct FreshRequest {
    pub selector: Selector,
    pub max_age: Duration,
    pub required_measurements: Vec<MeasurementKind>,
    pub deadline: Timestamp,
    pub allow_inference: bool,
}

#[derive(Clone, Debug)]
pub struct FreshResult {
    pub revision: u64,
    pub as_of: Timestamp,
    pub coverage: Vec<WindowKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshError {
    Unknown,
    Partial,
    NotInstalled,
    NotLoggedIn,
    Unsupported,
    UnsupportedPlatform,
    NotRefreshable,
    Deferred { retry_at: Timestamp },
    Failed(ErrorCode),
    Deadline,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuotaEvent {
    Changed {
        scopes: Vec<String>,
        windows: Vec<WindowKey>,
        revision: u64,
    },
    AvailabilityChanged(String),
    FreshnessChanged(String),
    RefreshStateChanged(String),
    PersistenceFailed(String),
}

mod window_records {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        windows: &BTreeMap<WindowKey, QuotaWindow>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        windows.values().collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<WindowKey, QuotaWindow>, D::Error> {
        let windows = Vec::<QuotaWindow>::deserialize(deserializer)?;
        let mut result = BTreeMap::new();
        for window in windows {
            if result.insert(window.key.clone(), window).is_some() {
                return Err(serde::de::Error::custom("duplicate window"));
            }
        }
        Ok(result)
    }
}
