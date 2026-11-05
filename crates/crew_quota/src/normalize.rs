use crate::*;

pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
pub const MAX_WINDOWS: usize = 256;
pub const MAX_SCOPES: usize = 64;
pub const RETENTION_SECONDS: i64 = 86400;

pub fn normalize_percent(percent: Option<f64>) -> Result<Option<f64>, ErrorCode> {
    match percent {
        Some(value) if !value.is_finite() || value < 0.0 => Err(ErrorCode::InvalidData),
        value => Ok(value.map(|value| value / 100.0)),
    }
}

pub fn valid_text(text: &str, maximum: usize) -> bool {
    !text.is_empty()
        && text.len() <= maximum
        && !text.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        && !text.contains("://")
        && !text.contains("Bearer ")
        && !text.starts_with("sk-")
}

pub fn validate_window_key(key: &WindowKey) -> bool {
    valid_text(&key.bucket.scope, 512)
        && valid_text(&key.bucket.bucket, 512)
        && valid_text(&key.role, 512)
        && key
            .duration_seconds
            .is_none_or(|duration| duration > 0 && duration <= 10 * 366 * 86400)
        && match &key.bucket.applicability {
            Applicability::AllModels => true,
            Applicability::Model(model) | Applicability::Family(model) => valid_text(model, 512),
        }
}

fn valid_amount(amount: &Amount) -> bool {
    amount.value.is_finite()
        && match &amount.unit {
            Unit::Credits { denomination } => denomination
                .as_ref()
                .is_none_or(|text| valid_text(text, 256)),
            Unit::Money { currency, scale } => valid_text(currency, 32) && *scale <= 18,
            Unit::UnspecifiedUnits => true,
            _ => amount.value >= 0.0,
        }
}

pub fn validate_observation(observation: &Observation, now: Timestamp) -> Result<(), ErrorCode> {
    if !valid_text(&observation.scope, 512)
        || !valid_text(&observation.source_version, 256)
        || observation.windows.len() > MAX_WINDOWS
        || observation.observed_at > now.saturating_add(300)
        || observation
            .provider_sample_at
            .is_some_and(|time| time > now.saturating_add(300))
        || observation
            .metadata
            .plan
            .as_ref()
            .is_some_and(|plan| !valid_text(&plan.name, 256))
        || observation
            .metadata
            .credit_balance
            .as_ref()
            .is_some_and(|amount| !valid_amount(amount))
    {
        return Err(ErrorCode::InvalidData);
    }
    let mut keys = std::collections::BTreeSet::new();
    for patch in &observation.windows {
        if patch.key.bucket.scope != observation.scope
            || !validate_window_key(&patch.key)
            || !keys.insert(&patch.key)
        {
            return Err(ErrorCode::InvalidData);
        }
        if let Patch::Set(Some(value)) = patch.used_fraction
            && (!value.is_finite() || value < 0.0)
        {
            return Err(ErrorCode::InvalidData);
        }
        for amount in [&patch.limit, &patch.remaining] {
            if let Patch::Set(Some(amount)) = amount
                && !valid_amount(amount)
            {
                return Err(ErrorCode::InvalidData);
            }
        }
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StoredRecord {
    pub schema_version: u32,
    pub revision: u64,
    pub account: AccountState,
    pub failures: u32,
    pub retry_at: Timestamp,
}

pub fn encode_record(record: &StoredRecord) -> Result<String, ErrorCode> {
    let encoded = serde_json::to_string(record).map_err(|_| ErrorCode::InvalidData)?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(ErrorCode::InvalidData);
    }
    Ok(encoded)
}

pub fn decode_record(
    json: &str,
    scope: &AccountScope,
    now: Timestamp,
) -> Result<StoredRecord, ErrorCode> {
    if json.len() > MAX_RECORD_BYTES {
        return Err(ErrorCode::InvalidData);
    }
    let mut record: StoredRecord =
        serde_json::from_str(json).map_err(|_| ErrorCode::InvalidData)?;
    if record.schema_version != 1 {
        return Err(ErrorCode::UnsupportedVersion);
    }
    if record.retry_at > now.saturating_add(MAX_BACKOFF_SECONDS as i64)
        || scope
            .native_provider_id
            .as_ref()
            .is_some_and(|id| !valid_text(id, 128))
        || (scope.category == AccountCategory::LlmProvider && scope.native_provider_id.is_none())
        || record.account.scope != *scope
        || record.account.windows.len() > MAX_WINDOWS
        || !valid_text(&scope.id, 512)
        || record
            .account
            .last_attempt_at
            .is_some_and(|time| time > now.saturating_add(300))
        || record
            .account
            .last_success_at
            .is_some_and(|time| time > now.saturating_add(300))
        || record
            .account
            .source_version
            .as_ref()
            .is_some_and(|version| !valid_text(version, 256))
        || record.account.metadata.plan.as_ref().is_some_and(|plan| {
            !valid_text(&plan.name, 256) || plan.observed_at > now.saturating_add(300)
        })
        || record
            .account
            .metadata
            .credit_balance
            .as_ref()
            .is_some_and(|amount| !valid_amount(amount))
    {
        return Err(ErrorCode::InvalidData);
    }
    for (key, window) in &mut record.account.windows {
        if key != &window.key || key.bucket.scope != scope.id || !validate_window_key(key) {
            return Err(ErrorCode::InvalidData);
        }
        fn persisted<T>(
            sample: &mut Option<Measurement<T>>,
            now: Timestamp,
        ) -> Result<(), ErrorCode> {
            if let Some(sample) = sample {
                if sample.observed_at > now.saturating_add(300)
                    || sample
                        .provider_sample_at
                        .is_some_and(|time| time > now.saturating_add(300))
                {
                    return Err(ErrorCode::InvalidData);
                }
                sample.source = SourceKind::Persisted;
            }
            Ok(())
        }
        if window
            .used_fraction
            .as_ref()
            .and_then(|sample| sample.value)
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err(ErrorCode::InvalidData);
        }
        for amount in [&window.limit, &window.remaining]
            .into_iter()
            .flatten()
            .filter_map(|sample| sample.value.as_ref())
        {
            if !valid_amount(amount) {
                return Err(ErrorCode::InvalidData);
            }
        }
        persisted(&mut window.used_fraction, now)?;
        persisted(&mut window.limit, now)?;
        persisted(&mut window.remaining, now)?;
        persisted(&mut window.reset_at, now)?;
        persisted(&mut window.admission, now)?;
        persisted(&mut window.disabled, now)?;
    }
    if let Some(plan) = record.account.metadata.plan.as_mut() {
        plan.source = SourceKind::Persisted;
    }
    record.account.refresh = RefreshState::Idle;
    if record.account.scope.provider == ProviderId::Grok
        && record.account.scope.runtime == "native-xai"
        && record.account.scope.category == AccountCategory::ExternalAgent
    {
        record.account.windows.clear();
        record.account.metadata = AccountMetadata::default();
        record.account.source_version = None;
        record.account.last_success_at = None;
        record.account.complete = false;
    }
    Ok(record)
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AttemptRecord {
    pub schema_version: u32,
    pub scope: String,
    pub provider: ProviderId,
    pub at: Timestamp,
}

pub fn decode_attempt(
    json: &str,
    scope: &AccountScope,
    now: Timestamp,
) -> Result<AttemptRecord, ErrorCode> {
    if json.len() > 4096 {
        return Err(ErrorCode::InvalidData);
    }
    let record: AttemptRecord = serde_json::from_str(json).map_err(|_| ErrorCode::InvalidData)?;
    if record.schema_version != 1 {
        return Err(ErrorCode::UnsupportedVersion);
    }
    if record.scope != scope.id
        || record.provider != scope.provider
        || record.at > now.saturating_add(300)
    {
        return Err(ErrorCode::InvalidData);
    }
    Ok(record)
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
    pub status: Option<ClaudeAdmission>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAdmission {
    Allowed,
    AllowedWarning,
    Rejected,
}

impl From<ClaudeAdmission> for Admission {
    fn from(value: ClaudeAdmission) -> Self {
        match value {
            ClaudeAdmission::Allowed => Self::Allowed,
            ClaudeAdmission::AllowedWarning => Self::Warning,
            ClaudeAdmission::Rejected => Self::Exhausted,
        }
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRateLimit {
    #[serde(flatten)]
    pub representative: ClaudeWindow,
    pub rate_limit_type: Option<String>,
    pub unified_windows: Option<ClaudeWindows>,
    pub overage_status: Option<ClaudeAdmission>,
    pub overage_resets_at: Option<i64>,
}

#[derive(Default, serde::Deserialize)]
pub struct ClaudeWindows {
    pub five_hour: Option<ClaudeWindow>,
    pub seven_day: Option<ClaudeWindow>,
}

pub fn normalize_claude(
    data: ClaudeRateLimit,
    scope: &str,
    generation: u64,
    source: SourceKind,
    observed_at: Timestamp,
    sequence: u64,
) -> Result<Observation, ErrorCode> {
    let explicit = data.unified_windows.unwrap_or_default();
    let representative = data.rate_limit_type.as_deref();
    let mut windows = Vec::new();
    for (name, duration, wire) in [
        (
            "five_hour",
            Some(18000),
            explicit.five_hour.or_else(|| {
                (representative == Some("five_hour")).then(|| ClaudeWindow {
                    utilization: data.representative.utilization,
                    resets_at: data.representative.resets_at,
                    status: data.representative.status,
                })
            }),
        ),
        (
            "seven_day",
            Some(604800),
            explicit.seven_day.or_else(|| {
                (representative == Some("seven_day")).then(|| ClaudeWindow {
                    utilization: data.representative.utilization,
                    resets_at: data.representative.resets_at,
                    status: data.representative.status,
                })
            }),
        ),
        (
            "overage",
            None,
            (data.overage_status.is_some() || data.overage_resets_at.is_some()).then_some(
                ClaudeWindow {
                    utilization: None,
                    resets_at: data.overage_resets_at,
                    status: data.overage_status,
                },
            ),
        ),
    ] {
        let Some(wire) = wire else {
            continue;
        };
        if wire
            .utilization
            .is_some_and(|value| !value.is_finite() || value < 0.0)
            || wire.resets_at.is_some_and(|reset| reset < 0)
        {
            return Err(ErrorCode::InvalidData);
        }
        windows.push(WindowPatch {
            key: WindowKey {
                bucket: BucketKey {
                    scope: scope.into(),
                    bucket: name.into(),
                    applicability: Applicability::AllModels,
                },
                role: "base".into(),
                duration_seconds: duration,
            },
            used_fraction: wire
                .utilization
                .map_or(Patch::Missing, |value| Patch::Set(Some(value))),
            reset_at: wire
                .resets_at
                .map_or(Patch::Missing, |value| Patch::Set(Some(value))),
            admission: wire
                .status
                .map_or(Patch::Missing, |value| Patch::Set(value.into())),
            remaining: Patch::Missing,
            limit: Patch::Missing,
            disabled: Patch::Missing,
            precision: Precision::Fraction,
        });
    }
    let observation = Observation {
        scope: scope.into(),
        generation,
        source,
        source_version: "claude-unified-v1".into(),
        observed_at,
        provider_sample_at: None,
        sequence,
        start_sequence: None,
        complete: windows
            .iter()
            .any(|window| window.key.bucket.bucket == "five_hour")
            && windows
                .iter()
                .any(|window| window.key.bucket.bucket == "seven_day"),
        windows,
        metadata: Default::default(),
    };
    validate_observation(&observation, observed_at)?;
    Ok(observation)
}
