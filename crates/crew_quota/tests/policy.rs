use crew_quota::*;

#[test]
fn percent_validation_preserves_overage_and_unknown() {
    assert_eq!(normalize_percent(Some(125.0)), Ok(Some(1.25)));
    assert_eq!(normalize_percent(None), Ok(None));
    assert_eq!(normalize_percent(Some(0.0)), Ok(Some(0.0)));
    assert!(normalize_percent(Some(f64::NAN)).is_err());
    assert!(normalize_percent(Some(-1.0)).is_err());
}

#[test]
fn scheduler_never_probes_grok() {
    let mut scheduler = Scheduler::default();
    scheduler.track("scope".into(), ProviderId::Grok, 1);
    assert!(scheduler.start_due(0, 0).is_empty());
    scheduler.demand("scope", 10000);
    assert!(scheduler.start_due(1000, 1000).is_empty());
}

#[test]
fn demand_floor_backoff_and_single_flight() {
    let mut scheduler = Scheduler::default();
    scheduler.track("scope".into(), ProviderId::Codex, 1);
    assert_eq!(scheduler.start_due(0, 0).len(), 1);
    assert!(scheduler.start_due(1, 1).is_empty());
    scheduler.complete("scope", 0, 0, true, None);
    assert!(scheduler.start_due(899, 899).is_empty());
    scheduler.demand("scope", 2000);
    assert_eq!(scheduler.start_due(120, 120).len(), 1);
    scheduler.complete("scope", 120, 120, false, Some(500));
    assert!(scheduler.start_due(499, 499).is_empty());
    assert_eq!(scheduler.start_due(500, 500).len(), 1);
}

#[test]
fn rollback_cannot_bypass_floor_and_reset_cannot_grant_quota() {
    let mut scheduler = Scheduler::default();
    scheduler.track("scope".into(), ProviderId::Codex, 1);
    assert_eq!(scheduler.start_due(1000, 0).len(), 1);
    scheduler.complete("scope", 1000, 0, true, None);
    scheduler.reset_due("scope", 1001);
    assert!(scheduler.start_due(2000, 1).is_empty());
    assert_eq!(scheduler.start_due(900, 120).len(), 1);
}

#[test]
fn concurrency_is_bounded_and_waiting_scope_runs_after_a_slot_frees() {
    let mut scheduler = Scheduler::default();
    for scope in ["one", "two", "three"] {
        scheduler.track(scope.into(), ProviderId::Codex, 1);
    }
    let initial = scheduler.start_due(1000, 0);
    assert_eq!(initial.len(), 2);
    assert!(scheduler.start_due(1000, 0).is_empty());
    scheduler.complete("one", 1000, 0, true, None);
    assert_eq!(scheduler.start_due(1000, 0), vec![("three".into(), 1)]);
}

#[test]
fn generation_change_preserves_attempt_floor_and_backoff() {
    let mut scheduler = Scheduler::default();
    scheduler.track("scope".into(), ProviderId::Codex, 1);
    assert_eq!(scheduler.start_due(1000, 0).len(), 1);
    scheduler.complete("scope", 1000, 0, false, Some(1600));
    scheduler.track("scope".into(), ProviderId::Codex, 2);
    assert!(scheduler.start_due(1120, 120).is_empty());
    assert_eq!(scheduler.start_due(1600, 600), vec![("scope".into(), 2)]);
}

#[test]
fn retry_after_seconds_date_and_overflow_are_checked() {
    assert_eq!(retry_after("120", 1000), Some(1120));
    assert_eq!(
        retry_after("Wed, 04 Nov 2026 00:00:00 GMT", 0),
        Some(1793750400)
    );
    assert_eq!(retry_after("18446744073709551615", 1000), None);
    assert_eq!(retry_after("unknown", 1000), None);
}
