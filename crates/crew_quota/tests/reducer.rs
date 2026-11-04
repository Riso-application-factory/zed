use crew_quota::*;
use std::time::Duration;

fn scope() -> AccountScope {
    AccountScope {
        id: "scope".into(),
        provider: ProviderId::Codex,
        runtime: "local".into(),
        identity_known: false,
    }
}
fn observation(at: i64, sequence: u64) -> Observation {
    Observation {
        scope: "scope".into(),
        generation: 1,
        source: SourceKind::CodexAppServerRead,
        source_version: "2.5.0".into(),
        observed_at: at,
        provider_sample_at: None,
        sequence,
        start_sequence: None,
        windows: vec![WindowPatch {
            key: WindowKey {
                bucket: BucketKey {
                    scope: "scope".into(),
                    bucket: "codex".into(),
                    applicability: Applicability::AllModels,
                },
                role: "primary".into(),
                duration_seconds: Some(18000),
            },
            used_fraction: Patch::Set(Some(1.0)),
            remaining: Patch::Missing,
            limit: Patch::Missing,
            reset_at: Patch::Set(Some(200)),
            admission: Patch::Set(Admission::Exhausted),
            disabled: Patch::Missing,
            precision: Precision::Fraction,
        }],
        complete: true,
        metadata: AccountMetadata::default(),
    }
}
fn core() -> QuotaCore {
    let mut core = QuotaCore::default();
    assert!(core.inventory(scope(), 1, Availability::Ready).is_ok());
    core
}

#[test]
fn reset_marks_stale_without_granting_allowance() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let window = core
        .snapshot
        .accounts
        .get("scope")
        .and_then(|account| account.windows.values().next())
        .ok_or(ErrorCode::Unknown)?;
    assert_eq!(window.remaining_fraction(), Some(0.0));
    assert_eq!(
        window.freshness(201, Duration::from_secs(900)),
        Freshness::Stale {
            age: Duration::from_secs(101),
            reason: StaleReason::ResetPassed
        }
    );
    assert_eq!(
        window.admission.as_ref().map(|sample| sample.value),
        Some(Admission::Exhausted)
    );
    Ok(())
}

#[test]
fn status_only_and_missing_fields_never_freshen_numeric_data() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let mut update = observation(150, 2);
    for patch in &mut update.windows {
        patch.used_fraction = Patch::Missing;
        patch.reset_at = Patch::Missing;
        patch.admission = Patch::Set(Admission::Warning);
    }
    core.apply(update, 150)?;
    let window = core
        .snapshot
        .accounts
        .get("scope")
        .and_then(|account| account.windows.values().next())
        .ok_or(ErrorCode::Unknown)?;
    assert_eq!(
        window
            .used_fraction
            .as_ref()
            .map(|sample| sample.observed_at),
        Some(100)
    );
    assert_eq!(
        window.admission.as_ref().map(|sample| sample.observed_at),
        Some(150)
    );
    assert!(matches!(
        window.freshness(160, Duration::from_secs(60)),
        Freshness::Stale {
            reason: StaleReason::Age,
            ..
        }
    ));
    Ok(())
}

#[test]
fn in_flight_read_does_not_overwrite_newer_event_or_other_generation() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 3), 100)?;
    let mut read = observation(110, 4);
    read.start_sequence = Some(1);
    for patch in &mut read.windows {
        patch.used_fraction = Patch::Set(Some(0.2));
    }
    assert!(core.apply(read, 110)?.is_empty());
    let mut wrong = observation(120, 5);
    wrong.generation = 0;
    assert!(core.apply(wrong, 120)?.is_empty());
    let window = core
        .snapshot
        .accounts
        .get("scope")
        .and_then(|account| account.windows.values().next())
        .ok_or(ErrorCode::Unknown)?;
    assert_eq!(window.remaining_fraction(), Some(0.0));
    Ok(())
}

#[test]
fn explicit_null_clears_numeric_value_instead_of_setting_zero() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let mut update = observation(110, 2);
    for patch in &mut update.windows {
        patch.used_fraction = Patch::Set(None);
    }
    core.apply(update, 110)?;
    let window = core
        .snapshot
        .accounts
        .get("scope")
        .and_then(|account| account.windows.values().next())
        .ok_or(ErrorCode::Unknown)?;
    assert_eq!(window.remaining_fraction(), None);
    assert_eq!(
        window.freshness(110, Duration::from_secs(120)),
        Freshness::Unknown
    );
    Ok(())
}

#[test]
fn cache_round_trip_is_stale_and_future_schema_is_rejected() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let account = core
        .snapshot
        .accounts
        .remove("scope")
        .ok_or(ErrorCode::Unknown)?;
    let mut record = StoredRecord {
        schema_version: 1,
        revision: 1,
        account,
        failures: 2,
        retry_at: 400,
    };
    let encoded = encode_record(&record)?;
    let decoded = decode_record(&encoded, &scope(), 110)?;
    let window = decoded
        .account
        .windows
        .values()
        .next()
        .ok_or(ErrorCode::Unknown)?;
    assert!(matches!(
        window.freshness(110, Duration::from_secs(120)),
        Freshness::Stale {
            reason: StaleReason::Persisted,
            ..
        }
    ));
    assert_eq!(decoded.retry_at, 400);
    record.schema_version = 2;
    assert!(matches!(
        decode_record(&encode_record(&record)?, &scope(), 110),
        Err(ErrorCode::UnsupportedVersion)
    ));
    assert!(
        decode_record(
            &encoded,
            &AccountScope {
                id: "other".into(),
                ..scope()
            },
            110
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn ensure_fresh_requires_all_windows_and_exact_model_coverage() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let request = FreshRequest {
        selector: Selector::default(),
        max_age: Duration::from_secs(120),
        required_measurements: Vec::new(),
        deadline: 140,
        allow_inference: false,
    };
    assert!(core.fresh(&request, 110).is_ok());
    assert!(core.fresh(&request, 201).is_err());
    let mut missing = request.clone();
    missing.selector.scopes.push("missing".into());
    assert!(matches!(
        core.fresh(&missing, 110),
        Err(RefreshError::Unknown)
    ));
    let mut patch = observation(110, 2);
    for window in &mut patch.windows {
        window.key.bucket.applicability = Applicability::Model("gpt-codex".into());
    }
    let mut isolated = QuotaCore::default();
    isolated.inventory(scope(), 1, Availability::Ready)?;
    isolated.apply(patch, 110)?;
    let mut other = request;
    other.selector.model = Some("different-model".into());
    assert!(isolated.fresh(&other, 115).is_err());
    Ok(())
}

#[test]
fn metadata_only_changes_revision_without_freshening_numeric_fields() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let previous_revision = core.snapshot.revision;
    let mut metadata = observation(150, 2);
    metadata.windows.clear();
    metadata.metadata.ordinary_usage_allowed = Some(false);
    assert!(core.apply(metadata, 150)?.is_empty());
    assert!(core.snapshot.revision > previous_revision);
    let account = core
        .snapshot
        .accounts
        .get("scope")
        .ok_or(ErrorCode::Unknown)?;
    assert_eq!(account.metadata.ordinary_usage_allowed, Some(false));
    assert_eq!(
        account
            .windows
            .values()
            .next()
            .and_then(|window| window.used_fraction.as_ref())
            .map(|sample| sample.observed_at),
        Some(100)
    );
    Ok(())
}

#[test]
fn expired_history_changes_revision_and_removes_numbers() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let previous_revision = core.snapshot.revision;
    core.expire(100000);
    assert!(core.snapshot.revision > previous_revision);
    assert!(
        core.snapshot
            .accounts
            .get("scope")
            .is_some_and(|account| account.windows.is_empty())
    );
    Ok(())
}

#[test]
fn future_retry_timestamp_is_rejected() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let account = core
        .snapshot
        .accounts
        .remove("scope")
        .ok_or(ErrorCode::Unknown)?;
    let record = StoredRecord {
        schema_version: 1,
        revision: 1,
        account,
        failures: 1,
        retry_at: i64::MAX,
    };
    assert!(matches!(
        decode_record(&encode_record(&record)?, &scope(), 100),
        Err(ErrorCode::InvalidData)
    ));
    Ok(())
}

#[test]
fn future_attempt_timestamp_is_rejected() -> Result<(), ErrorCode> {
    let record = serde_json::to_string(&AttemptRecord {
        schema_version: 1,
        scope: "scope".into(),
        provider: ProviderId::Codex,
        at: i64::MAX,
    })
    .map_err(|_| ErrorCode::InvalidData)?;
    assert!(matches!(
        decode_attempt(&record, &scope(), 100),
        Err(ErrorCode::InvalidData)
    ));
    Ok(())
}

#[test]
fn amount_only_measurements_are_fresh_and_expire() -> Result<(), ErrorCode> {
    let mut core = core();
    let mut sample = observation(100, 1);
    for window in &mut sample.windows {
        window.used_fraction = Patch::Missing;
        window.reset_at = Patch::Set(None);
        window.remaining = Patch::Set(Some(Amount {
            value: 40.0,
            unit: Unit::Requests,
        }));
        window.limit = Patch::Set(Some(Amount {
            value: 100.0,
            unit: Unit::Requests,
        }));
    }
    core.apply(sample, 100)?;
    let request = FreshRequest {
        selector: Selector::default(),
        max_age: Duration::from_secs(120),
        required_measurements: Vec::new(),
        deadline: 200,
        allow_inference: false,
    };
    assert!(core.fresh(&request, 110).is_ok());
    core.expire(100000);
    assert!(
        core.snapshot
            .accounts
            .get("scope")
            .is_some_and(|account| account.windows.is_empty())
    );
    Ok(())
}

#[test]
fn fresh_fraction_does_not_freshen_an_old_amount() -> Result<(), ErrorCode> {
    let mut core = core();
    let mut sample = observation(100, 1);
    for window in &mut sample.windows {
        window.remaining = Patch::Set(Some(Amount {
            value: 40.0,
            unit: Unit::Requests,
        }));
        window.reset_at = Patch::Set(None);
    }
    core.apply(sample, 100)?;
    let mut sample = observation(180, 2);
    for window in &mut sample.windows {
        window.reset_at = Patch::Set(None);
    }
    core.apply(sample, 180)?;
    let window = core
        .snapshot
        .accounts
        .get("scope")
        .and_then(|account| account.windows.values().next())
        .ok_or(ErrorCode::Unknown)?;
    assert!(
        matches!(window.freshness(190, Duration::from_secs(60)), Freshness::Stale { age, reason: StaleReason::Age } if age == Duration::from_secs(90))
    );
    Ok(())
}

#[test]
fn quarantined_future_records_do_not_block_restart_reads() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let account = core
        .snapshot
        .accounts
        .remove("scope")
        .ok_or(ErrorCode::Unknown)?;
    let record = StoredRecord {
        schema_version: 1,
        revision: 1,
        account,
        failures: 1,
        retry_at: i64::MAX,
    };
    let attempt = serde_json::to_string(&AttemptRecord {
        schema_version: 1,
        scope: "scope".into(),
        provider: ProviderId::Codex,
        at: i64::MAX,
    })
    .map_err(|_| ErrorCode::InvalidData)?;
    let mut scheduler = Scheduler::default();
    scheduler.track("scope".into(), ProviderId::Codex, 1);
    if let Ok(record) = decode_record(&encode_record(&record)?, &scope(), 100) {
        scheduler.restore_timing(
            "scope",
            record.account.last_attempt_at,
            record.retry_at,
            record.failures,
            100,
            0,
        );
    }
    if let Ok(attempt) = decode_attempt(&attempt, &scope(), 100) {
        scheduler.restore_timing("scope", Some(attempt.at), 0, 0, 100, 0);
    }
    assert_eq!(scheduler.start_due(100, 0).len(), 1);
    Ok(())
}

#[test]
fn required_coverage_and_retention_keep_measurement_ages_separate() -> Result<(), ErrorCode> {
    let mut core = core();
    let mut sample = observation(100, 1);
    for window in &mut sample.windows {
        window.reset_at = Patch::Set(None);
    }
    core.apply(sample, 100)?;
    let mut sample = observation(100000, 2);
    for window in &mut sample.windows {
        window.used_fraction = Patch::Missing;
        window.reset_at = Patch::Set(None);
        window.remaining = Patch::Set(Some(Amount {
            value: 10.0,
            unit: Unit::Tokens,
        }));
    }
    core.apply(sample, 100000)?;
    let snapshot = core.snapshot(&Selector::default(), 100010);
    let window = snapshot
        .accounts
        .get("scope")
        .and_then(|account| account.windows.values().next())
        .ok_or(ErrorCode::Unknown)?;
    assert!(window.used_fraction.is_none());
    assert_eq!(
        window.freshness_for(
            100010,
            Duration::from_secs(120),
            &[MeasurementKind::Remaining]
        ),
        Freshness::Fresh
    );
    assert_eq!(
        window.freshness_for(100010, Duration::from_secs(120), &[MeasurementKind::Limit]),
        Freshness::Unknown
    );
    core.expire(100010);
    assert!(
        core.snapshot
            .accounts
            .get("scope")
            .and_then(|account| account.windows.values().next())
            .is_some_and(|window| window.remaining.is_some() && window.used_fraction.is_none())
    );
    Ok(())
}

#[test]
fn persisted_scheduling_bounds_allow_supported_guards_only() -> Result<(), ErrorCode> {
    let mut core = core();
    core.apply(observation(100, 1), 100)?;
    let account = core
        .snapshot
        .accounts
        .remove("scope")
        .ok_or(ErrorCode::Unknown)?;
    for (retry_at, accepted) in [(3700, true), (3701, false)] {
        let record = StoredRecord {
            schema_version: 1,
            revision: 1,
            account: account.clone(),
            failures: 1,
            retry_at,
        };
        assert_eq!(
            decode_record(&encode_record(&record)?, &scope(), 100).is_ok(),
            accepted
        );
    }
    for (at, accepted) in [(400, true), (401, false)] {
        let record = serde_json::to_string(&AttemptRecord {
            schema_version: 1,
            scope: "scope".into(),
            provider: ProviderId::Codex,
            at,
        })
        .map_err(|_| ErrorCode::InvalidData)?;
        assert_eq!(decode_attempt(&record, &scope(), 100).is_ok(), accepted);
    }
    Ok(())
}
