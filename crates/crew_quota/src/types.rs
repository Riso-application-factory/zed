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
    CleanupFailed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Ready,
    Unknown,
    Unsupported(ErrorCode),
    NotInstalled,
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
        let Some(measurement) = self
            .used_fraction
            .as_ref()
            .filter(|sample| sample.value.is_some())
        else {
            return Freshness::Unknown;
        };
        let age = Duration::from_secs(now.saturating_sub(measurement.observed_at).max(0) as u64);
        let reason = if measurement.source == SourceKind::Persisted {
            Some(StaleReason::Persisted)
        } else if now < measurement.observed_at {
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountMetadata {
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

#[derive(Clone, Debug)]
pub struct FreshRequest {
    pub selector: Selector,
    pub max_age: Duration,
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
