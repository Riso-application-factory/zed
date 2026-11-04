use super::*;
use gpui::{Action, FocusHandle, Focusable, Render, Window, actions, px, relative};
use serde::{Deserialize, Serialize};
use ui::{ButtonLike, Chip, Icon, IconButton, IconName, Label, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    crew_quota,
    [
        ToggleFocus,
        RefreshQuota,
        ToggleStatusSummary,
        OpenQuotaSummary
    ]
);

mod status;

#[cfg(debug_assertions)]
mod preview;
#[cfg(debug_assertions)]
actions!(crew_quota, [ToggleFixturePreview]);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    left: bool,
    compact: bool,
    used: bool,
    hide_status: bool,
    collapsed: BTreeSet<ProviderId>,
}

struct PanelPreferences {
    value: Preferences,
    write: Option<Task<()>>,
    error: bool,
}
struct GlobalPreferences(Entity<PanelPreferences>);
impl Global for GlobalPreferences {}

impl PanelPreferences {
    fn save(&mut self, cx: &mut Context<Self>) {
        let value = match serde_json::to_string(&self.value) {
            Ok(value) => value,
            Err(error) => {
                log::error!("crew_quota: preference serialization failed: {error}");
                self.error = true;
                cx.notify();
                return;
            }
        };
        let previous = self.write.take();
        let store = KeyValueStore::global(cx);
        self.write = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = cx
                .background_spawn(async move {
                    store
                        .scoped("crew_quota")
                        .write("panel_preferences".into(), value)
                        .await
                })
                .await;
            if let Err(error) = &result {
                log::error!("crew_quota: preference persistence failed: {error}");
            }
            this.update(cx, |this, cx| {
                this.error = result.is_err();
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }
}

pub(super) fn init(cx: &mut App) {
    let value = match KeyValueStore::global(cx)
        .scoped("crew_quota")
        .read("panel_preferences")
    {
        Ok(Some(value)) => match serde_json::from_str(&value) {
            Ok(value) => value,
            Err(error) => {
                log::error!("crew_quota: invalid panel preferences: {error}");
                Preferences::default()
            }
        },
        Ok(None) => Preferences::default(),
        Err(error) => {
            log::error!("crew_quota: preference read failed: {error}");
            Preferences::default()
        }
    };
    let preferences = cx.new(|_| PanelPreferences {
        value,
        write: None,
        error: false,
    });
    cx.set_global(GlobalPreferences(preferences));
    cx.observe_new::<Workspace>(register_workspace).detach();
}

fn register_workspace(
    workspace: &mut Workspace,
    window: Option<&mut Window>,
    cx: &mut Context<Workspace>,
) {
    workspace.register_action(|workspace, _: &OpenQuotaSummary, window, cx| {
        workspace.focus_panel::<QuotaPanel>(window, cx);
    });
    workspace.register_action(|_, _: &ToggleStatusSummary, _, cx| {
        let preferences = cx.global::<GlobalPreferences>().0.clone();
        preferences.update(cx, |preferences, cx| {
            preferences.value.hide_status = !preferences.value.hide_status;
            preferences.save(cx);
        });
    });
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<QuotaPanel>(window, cx);
    });
    #[cfg(debug_assertions)]
    workspace.register_action(|workspace, _: &ToggleFixturePreview, window, cx| {
        if let Some(panel) = workspace.panel::<QuotaPanel>(cx) {
            panel.update(cx, |panel, cx| {
                panel.set_active(false, window, cx);
                panel.preview = if panel.preview.is_some() {
                    None
                } else {
                    Some(preview::FixturePreview::new(
                        chrono::Utc::now().timestamp(),
                        false,
                    ))
                };
                panel.set_active(true, window, cx);
                cx.defer(|cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
                cx.notify();
            });
            workspace.focus_panel::<QuotaPanel>(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &RefreshQuota, _, cx| {
        if let Some(panel) = workspace.panel::<QuotaPanel>(cx) {
            panel.update(cx, |panel, cx| panel.refresh(cx));
        }
    });
    // Construct without reading Workspace again: observe_new already holds its mutable borrow.
    if let Some(window) = window {
        let panel = cx.new(QuotaPanel::new);
        workspace.add_panel(panel.clone(), window, cx);
        status::attach(workspace, panel, window, cx);
    } else {
        // A windowless workspace can acquire a window later; attach on its first render.
        workspace.register_action_renderer(|element, workspace, window, cx| {
            if workspace.panel::<QuotaPanel>(cx).is_none() {
                cx.defer_in(window, |workspace, window, cx| {
                    if workspace.panel::<QuotaPanel>(cx).is_none() {
                        let panel = cx.new(QuotaPanel::new);
                        workspace.add_panel(panel.clone(), window, cx);
                        status::attach(workspace, panel, window, cx);
                    }
                });
            }
            element
        });
    }
    cx.notify();
}

pub struct QuotaPanel {
    #[cfg(debug_assertions)]
    preview: Option<preview::FixturePreview>,
    focus: FocusHandle,
    provider_focus: BTreeMap<ProviderId, FocusHandle>,
    service: Option<Entity<QuotaService>>,
    preferences: Entity<PanelPreferences>,
    demand: Option<DemandLease>,
    tick: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    refresh_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}
impl QuotaPanel {
    fn display_preferences(&self, cx: &App) -> Preferences {
        #[cfg(debug_assertions)]
        if let Some(preview) = &self.preview {
            return preview.preferences.clone();
        }
        self.preferences.read(cx).value.clone()
    }

    fn is_preview(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.preview.is_some()
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let service = QuotaService::global(cx);
        let preferences = cx.global::<GlobalPreferences>().0.clone();
        let mut subscriptions = vec![cx.observe(&preferences, |_, _, cx| cx.notify())];
        if let Some(service) = &service {
            subscriptions.push(cx.subscribe(service, |_, _, _: &QuotaEvent, cx| cx.notify()));
        }
        Self {
            #[cfg(debug_assertions)]
            preview: preview::FixturePreview::from_environment(),
            focus: cx.focus_handle(),
            provider_focus: [
                ProviderId::Claude,
                ProviderId::Codex,
                ProviderId::Antigravity,
                ProviderId::Grok,
            ]
            .into_iter()
            .map(|provider| (provider, cx.focus_handle()))
            .collect(),
            service,
            preferences,
            demand: None,
            tick: None,
            refresh_task: None,
            refresh_error: None,
            _subscriptions: subscriptions,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        #[cfg(debug_assertions)]
        if let Some(preview) = &mut self.preview {
            let preferences = preview.preferences.clone();
            let left = preview.left;
            *preview = preview::FixturePreview::new(chrono::Utc::now().timestamp(), preview.states);
            preview.preferences = preferences;
            preview.left = left;
            cx.notify();
            return;
        }
        if self.refresh_task.is_some() {
            return;
        }
        let Some(service) = &self.service else {
            return;
        };
        let task = service.update(cx, |service, cx| service.refresh(Selector::default(), cx));
        self.refresh_error = None;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                this.refresh_error = result
                    .err()
                    .map(|error| format!("Refresh unavailable: {error:?}"));
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn toggle_provider(&mut self, provider: ProviderId, cx: &mut Context<Self>) {
        #[cfg(debug_assertions)]
        if let Some(preview) = &mut self.preview {
            toggle_collapsed(&mut preview.preferences, provider);
            cx.notify();
            return;
        }
        self.preferences.update(cx, |preferences, cx| {
            toggle_collapsed(&mut preferences.value, provider);
            preferences.save(cx);
        });
    }

    fn change_preference(&mut self, compact: bool, cx: &mut Context<Self>) {
        #[cfg(debug_assertions)]
        if let Some(preview) = &mut self.preview {
            if compact {
                preview.preferences.compact = !preview.preferences.compact;
            } else {
                preview.preferences.used = !preview.preferences.used;
            }
            cx.notify();
            return;
        }
        self.preferences.update(cx, |preferences, cx| {
            if compact {
                preferences.value.compact = !preferences.value.compact;
            } else {
                preferences.value.used = !preferences.value.used;
            }
            preferences.save(cx);
        });
    }
}
impl Focusable for QuotaPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<PanelEvent> for QuotaPanel {}
impl Panel for QuotaPanel {
    fn persistent_name() -> &'static str {
        "CrewQuotaPanel"
    }
    fn panel_key() -> &'static str {
        "crew_quota"
    }
    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        #[cfg(debug_assertions)]
        if let Some(preview) = &self.preview {
            return if preview.left {
                DockPosition::Left
            } else {
                DockPosition::Right
            };
        }
        if self.preferences.read(cx).value.left {
            DockPosition::Left
        } else {
            DockPosition::Right
        }
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        if !self.position_is_valid(position) {
            return;
        }
        #[cfg(debug_assertions)]
        if let Some(preview) = &mut self.preview {
            preview.left = position == DockPosition::Left;
            cx.defer(|cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
            cx.notify();
            return;
        }
        self.preferences.update(cx, |preferences, cx| {
            preferences.value.left = position == DockPosition::Left;
            preferences.save(cx);
        });
        // Dock relocation listens to SettingsStore; defer until the panel's borrow has ended.
        cx.defer(|cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }
    fn min_size(&self, _: &Window, _: &App) -> Option<Pixels> {
        Some(px(280.))
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::SignalHigh)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Quota")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn starts_open(&self, _: &Window, _: &App) -> bool {
        self.is_preview()
    }
    fn activation_priority(&self) -> u32 {
        100
    }
    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if !active {
            self.demand = None;
            self.tick = None;
            return;
        }
        if self.tick.is_some() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        if let Some(service) = &self.service
            && !self.is_preview()
        {
            self.demand = Some(service.update(cx, |service, cx| {
                service.acquire_demand(
                    FreshRequest {
                        selector: Selector::default(),
                        max_age: Duration::from_secs(120),
                        required_measurements: vec![],
                        deadline: now + 180,
                        allow_inference: true,
                    },
                    cx,
                )
            }));
        }
        self.tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if let Some(demand) = &this.demand {
                            demand.renew(chrono::Utc::now().timestamp(), Duration::from_secs(180));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}

struct ProviderRow<'a> {
    provider: ProviderId,
    accounts: Vec<&'a AccountState>,
    used: Option<f64>,
}
fn provider_rows(snapshot: &QuotaSnapshot) -> Vec<ProviderRow<'_>> {
    let mut rows = [
        ProviderId::Claude,
        ProviderId::Codex,
        ProviderId::Antigravity,
        ProviderId::Grok,
    ]
    .into_iter()
    .map(|provider| {
        let accounts = snapshot
            .accounts
            .values()
            .filter(|account| account.scope.provider == provider)
            .collect::<Vec<_>>();
        let used = accounts
            .iter()
            .flat_map(|account| account.windows.values())
            .filter(|window| !window.disabled.as_ref().is_some_and(|sample| sample.value))
            .filter_map(used_fraction)
            .max_by(f64::total_cmp);
        ProviderRow {
            provider,
            accounts,
            used,
        }
    })
    .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .used
            .partial_cmp(&left.used)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}
fn used_fraction(window: &QuotaWindow) -> Option<f64> {
    window
        .used_fraction
        .as_ref()?
        .value
        .filter(|value| value.is_finite() && *value >= 0.)
}
fn display_fraction(used: f64, show_used: bool) -> f64 {
    if show_used {
        used.clamp(0., 1.)
    } else {
        (1. - used).clamp(0., 1.)
    }
}
fn severity(used: f64) -> Color {
    if used >= 0.8 {
        Color::Error
    } else if used >= 0.6 {
        Color::Warning
    } else {
        Color::Muted
    }
}
fn countdown(reset: Option<Timestamp>, now: Timestamp) -> String {
    match reset {
        None => "Reset unknown".into(),
        Some(reset) if reset <= now => "Resets now".into(),
        Some(reset) => {
            let minutes = (reset - now + 59) / 60;
            if minutes < 60 {
                format!("Resets in {minutes}m")
            } else if minutes < 1440 {
                format!("Resets in {}h {}m", minutes / 60, minutes % 60)
            } else {
                format!("Resets in {}d {}h", minutes / 1440, minutes % 1440 / 60)
            }
        }
    }
}
fn age(updated: Option<Timestamp>, now: Timestamp) -> String {
    updated
        .map(|updated| {
            format!(
                "Updated {} min ago",
                now.saturating_sub(updated).max(0) / 60
            )
        })
        .unwrap_or_else(|| "Not yet updated".into())
}
fn account_status(account: &AccountState, now: Timestamp) -> String {
    let availability = match account.availability {
        Availability::Unknown => "Unknown",
        Availability::Unsupported(ErrorCode::StoredAuthUnsupported) => {
            "Stored OAuth unsupported — use producer shell login"
        }
        Availability::Unsupported(_) => "Unsupported",
        Availability::Unavailable(ErrorCode::StoredAuthUnsupported) => {
            "Stored OAuth unsupported — use producer shell login"
        }
        Availability::Unavailable(ErrorCode::InferenceAccess) => {
            "Probe access unavailable — listening for session updates"
        }
        Availability::NotInstalled => "Not installed",
        Availability::NotLoggedIn => "Not logged in",
        Availability::Unavailable(ErrorCode::AttributionAmbiguous) => {
            "Unavailable · cannot attribute: SuperGrok and API key both signed in"
        }
        Availability::Unavailable(_) => "Unavailable",
        Availability::Error(ErrorCode::CleanupFailed) => "Error — session pending cleanup",
        Availability::Error(_) if account.windows.is_empty() => "Error — quota unavailable",
        Availability::Error(_) => "Error — showing last known data",
        Availability::Ready => "",
    };
    let mut states = Vec::new();
    if !availability.is_empty() {
        states.push(availability);
    }
    if matches!(
        account.refresh,
        RefreshState::InFlight | RefreshState::Queued
    ) {
        states.push("Loading");
    }
    if account.windows.values().any(|window| {
        matches!(
            window.freshness(
                now,
                Duration::from_secs(if account.scope.provider == ProviderId::Antigravity {
                    900
                } else {
                    120
                })
            ),
            Freshness::Stale { .. }
        )
    }) {
        states.push("Stale");
    }
    if account
        .windows
        .values()
        .filter(|window| window.key.bucket.bucket != "overage")
        .any(|window| {
            used_fraction(window).is_some_and(|used| used >= 1.)
                || window
                    .admission
                    .as_ref()
                    .is_some_and(|value| value.value == Admission::Exhausted)
        })
    {
        states.push("Exhausted");
    }
    if states.is_empty() && account.windows.is_empty() {
        states.push("Unknown — no quota data");
    }
    states.join(" · ")
}
fn toggle_collapsed(preferences: &mut Preferences, provider: ProviderId) {
    if !preferences.collapsed.remove(&provider) {
        preferences.collapsed.insert(provider);
    }
}

fn badge(label: impl Into<SharedString>, color: Color, cx: &App) -> impl IntoElement {
    h_flex().child(
        Chip::new(label)
            .label_color(color)
            .bg_color(color.color(cx).opacity(0.10))
            .border_color(color.color(cx).opacity(0.20)),
    )
}

fn amount_text(window: &QuotaWindow) -> Option<String> {
    let remaining = window.remaining.as_ref()?.value.as_ref()?;
    if !remaining.value.is_finite() {
        return None;
    }
    let unit = match &remaining.unit {
        Unit::Requests => "requests".into(),
        Unit::Tokens => "tokens".into(),
        Unit::UnspecifiedUnits => "units".into(),
        unit => format!("{unit:?}"),
    };
    let numeric_value = |value| {
        if matches!(remaining.unit, Unit::Requests | Unit::Tokens) {
            grouped_number(value)
        } else {
            format!("{value}")
        }
    };
    let remaining_value = numeric_value(remaining.value);
    let limit = window
        .limit
        .as_ref()
        .and_then(|sample| sample.value.as_ref())
        .filter(|limit| limit.unit == remaining.unit && limit.value.is_finite());
    Some(if let Some(limit) = limit {
        format!(
            "{remaining_value} / {} {unit} left",
            numeric_value(limit.value)
        )
    } else {
        format!("{remaining_value} {unit} left")
    })
}

fn credit_text(balance: &Amount, reset_credits: Option<u64>) -> String {
    let reset_credits = reset_credits
        .map(|count| format!("{count} reset credits"))
        .unwrap_or_else(|| "Reset credits unknown".into());
    format!(
        "{} credits · {reset_credits}",
        grouped_number(balance.value)
    )
}

fn grouped_number(number: f64) -> String {
    let digits = format!("{number:.0}");
    let mut result = String::new();
    for (index, digit) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            result.push(',');
        }
        result.push(digit);
    }
    result.chars().rev().collect()
}

impl Render for QuotaPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = chrono::Utc::now().timestamp();
        let preferences = self.preferences.read(cx).value.clone();
        let snapshot = self
            .service
            .as_ref()
            .map(|service| service.read(cx).snapshot(&Selector::default(), now))
            .unwrap_or_default();
        #[cfg(debug_assertions)]
        let (preferences, snapshot) = self
            .preview
            .as_ref()
            .map(|preview| (preview.preferences.clone(), preview.snapshot.clone()))
            .unwrap_or((preferences, snapshot));
        let updated = snapshot
            .accounts
            .values()
            .flat_map(|account| account.windows.values())
            .filter_map(|window| {
                window
                    .used_fraction
                    .as_ref()
                    .map(|sample| sample.observed_at)
            })
            .min();
        let mut content = v_flex().gap_3();
        for (provider_index, row) in provider_rows(&snapshot).into_iter().enumerate() {
            let (name, icon) = match row.provider {
                ProviderId::Claude => ("Claude", IconName::AiClaude),
                ProviderId::Codex => ("Codex", IconName::AiOpenAi),
                ProviderId::Antigravity => ("Antigravity", IconName::AiGemini),
                ProviderId::Grok => ("Grok", IconName::AiXAi),
            };
            let earliest = row
                .accounts
                .iter()
                .flat_map(|account| account.windows.values())
                .filter_map(|window| window.reset_at.as_ref()?.value)
                .min();
            let provider_id = row.provider;
            let expanded = !preferences.collapsed.contains(&provider_id);
            let headline = row
                .used
                .map(|used| {
                    format!(
                        "{:.0}% {}",
                        display_fraction(used, preferences.used) * 100.,
                        if preferences.used { "used" } else { "left" }
                    )
                })
                .unwrap_or_else(|| "Unknown".into());
            let mut provider =
                v_flex()
                    .gap_2()
                    .pb_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        ButtonLike::new(format!("quota-provider-{provider_id:?}"))
                            .width(relative(1.))
                            .when_some(self.provider_focus.get(&provider_id), |button, focus| {
                                button.track_focus(focus)
                            })
                            .tab_index(6 + provider_index as isize)
                            .aria_label(format!("{name}, {headline}, quota windows"))
                            .aria_expanded(expanded)
                            .tooltip(Tooltip::text(if provider_id == ProviderId::Grok {
                                "Latest xAI response from recent usage only. Limits are per model and reflect the most recent request’s model; model is unknown unless exposed without reading bodies. Capture is unavailable when both providers are signed in: cannot attribute: SuperGrok and API key both signed in".to_string()
                            } else {
                                format!("{} {name} quota windows", if expanded { "Collapse" } else { "Expand" })
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_provider(provider_id, cx)
                            }))
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Icon::new(if expanded {
                                                    IconName::ChevronDown
                                                } else {
                                                    IconName::ChevronRight
                                                })
                                                .size(IconSize::XSmall),
                                            )
                                            .child(Icon::new(icon).size(IconSize::Small))
                                            .child(Label::new(name)),
                                    )
                                    .child(
                                        Label::new(headline)
                                            .size(LabelSize::Small)
                                            .color(row.used.map(severity).unwrap_or(Color::Muted)),
                                    ),
                            ),
                    );
            if !expanded {
                let mut statuses = BTreeSet::new();
                for account in &row.accounts {
                    for state in account_status(account, now)
                        .split(" · ")
                        .filter(|state| !state.is_empty())
                    {
                        statuses.insert(state.to_string());
                    }
                }
                if row
                    .accounts
                    .iter()
                    .flat_map(|account| account.windows.values())
                    .any(|window| window.disabled.as_ref().is_some_and(|sample| sample.value))
                {
                    statuses.insert("Disabled bucket".into());
                }
                if row.accounts.is_empty() {
                    statuses.insert("Unknown".into());
                }
                if provider_id == ProviderId::Grok {
                    statuses.insert("Latest xAI response · recent usage only".into());
                }
                provider = provider
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .children(statuses.into_iter().map(|state| {
                                let color = if state.starts_with("Error") || state == "Exhausted" {
                                    Color::Error
                                } else if state == "Stale" {
                                    Color::Warning
                                } else {
                                    Color::Muted
                                };
                                badge(state, color, cx)
                            })),
                    )
                    .child(
                        h_flex().justify_end().child(
                            Label::new(countdown(earliest, now))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    );
                content = content.child(provider);
                continue;
            }
            if row.accounts.is_empty() {
                provider = provider.child(badge("Unknown", Color::Muted, cx));
            }
            if row.provider == ProviderId::Grok {
                provider = provider.child(badge(
                    "Latest xAI response · recent usage only · limits are per model; this reflects the most recent request’s model (Unknown model)",
                    Color::Muted,
                    cx,
                ));
            }
            for (index, account) in row.accounts.iter().enumerate() {
                let mut account_header = h_flex().justify_between().gap_2();
                if let Some(plan) = &account.metadata.plan {
                    account_header = account_header.child(
                        Label::new(plan.name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                } else if row.accounts.len() > 1 {
                    account_header = account_header.child(
                        Label::new(format!("Login {}", index + 1))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                }
                account_header = account_header.child(
                    Label::new(
                        if account.scope.provider == ProviderId::Claude
                            && account
                                .windows
                                .values()
                                .filter_map(|window| window.used_fraction.as_ref())
                                .any(|sample| sample.source == SourceKind::Persisted)
                        {
                            "Cached · age unknown".into()
                        } else {
                            age(account.last_success_at, now)
                        },
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                );
                provider = provider.child(account_header);
                let status = account_status(account, now);
                let mut badges = h_flex().flex_wrap().gap_1();
                for state in status.split(" · ").filter(|state| !state.is_empty()) {
                    let color = if state.starts_with("Error") || state == "Exhausted" {
                        Color::Error
                    } else if state == "Stale" {
                        Color::Warning
                    } else {
                        Color::Muted
                    };
                    let label = if state.starts_with("Error")
                        && !state.contains("pending cleanup")
                        && !account.windows.is_empty()
                    {
                        "Error · cached data"
                    } else {
                        state
                    };
                    badges = badges.child(badge(label.to_string(), color, cx));
                }
                if !status.is_empty() {
                    provider = provider.child(badges);
                }
                #[cfg(debug_assertions)]
                if let Some(notes) = self
                    .preview
                    .as_ref()
                    .and_then(|preview| preview.notes.get(&account.scope.id))
                {
                    for note in notes {
                        provider = provider.child(
                            Label::new(*note)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        );
                    }
                }
                if let Some(balance) = &account.metadata.credit_balance {
                    provider = provider.child(
                        Label::new(credit_text(
                            balance,
                            account.metadata.available_reset_credits,
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    );
                }
                let mut windows = account.windows.values().collect::<Vec<_>>();
                windows.sort_by(|left, right| {
                    used_fraction(right)
                        .partial_cmp(&used_fraction(left))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for (index, window) in windows.iter().enumerate() {
                    if preferences.compact && index > 0 {
                        continue;
                    }
                    let disabled = window.disabled.as_ref().is_some_and(|sample| sample.value);
                    let used = used_fraction(window).filter(|_| !disabled);
                    let percentage = used.map(|used| {
                        format!(
                            "{:.0}% {}",
                            display_fraction(used, preferences.used) * 100.,
                            if preferences.used { "used" } else { "left" }
                        )
                    });
                    let reset = window.reset_at.as_ref().and_then(|sample| sample.value);
                    let bucket = window
                        .key
                        .bucket
                        .bucket
                        .strip_prefix("agy-v1/")
                        .unwrap_or(&window.key.bucket.bucket);
                    let title = if account.scope.provider == ProviderId::Grok {
                        format!("Latest xAI response · Unknown model · {}", window.key.role)
                    } else {
                        format!("{} · {}", bucket, window.key.role)
                    };
                    let mut section = v_flex().gap_1().pl_6().child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(Label::new(title).size(LabelSize::Small))
                            .when_some(percentage, |element, percentage| {
                                element.child(
                                    Label::new(percentage)
                                        .size(LabelSize::Small)
                                        .color(used.map(severity).unwrap_or(Color::Muted)),
                                )
                            }),
                    );
                    if let Some(used) = used
                        && !preferences.compact
                    {
                        section =
                            section.child(
                                div()
                                    .w_full()
                                    .h(px(6.))
                                    .rounded_full()
                                    .bg(cx.theme().colors().element_background)
                                    .child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .w(relative(
                                                display_fraction(used, preferences.used) as f32
                                            ))
                                            .bg(severity(used).color(cx)),
                                    ),
                            );
                    }
                    if let Some(amount) = amount_text(window) {
                        section = section.child(
                            Label::new(amount)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        );
                    }
                    let state = if disabled {
                        Some(("Disabled", Color::Muted))
                    } else {
                        match used {
                            Some(used) if used >= 1. => Some(("Exhausted", Color::Error)),
                            Some(used) if used >= 0.8 => Some(("Critical", Color::Error)),
                            Some(used) if used >= 0.6 => Some(("Warning", Color::Warning)),
                            None => Some(("Unknown", Color::Muted)),
                            _ => None,
                        }
                    };
                    section = section.child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .gap_2()
                            .child(h_flex().when_some(state, |element, (label, color)| {
                                element.child(badge(label, color, cx))
                            }))
                            .child(
                                Label::new(countdown(reset, now))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    );
                    provider = provider.child(
                        section
                            .id(format!("quota-window-{}-{index}", account.scope.id))
                            .tooltip(Tooltip::text(format!(
                                "Reset UTC: {} · Precision: {:?} · {}",
                                reset
                                    .and_then(|reset| chrono::DateTime::from_timestamp(reset, 0))
                                    .map(|reset| reset.to_rfc3339())
                                    .unwrap_or_else(|| "unknown".into()),
                                window.precision,
                                if window
                                    .used_fraction
                                    .as_ref()
                                    .is_some_and(|sample| sample.source == SourceKind::Persisted)
                                {
                                    "Cached · age unknown".into()
                                } else {
                                    age(
                                        window
                                            .used_fraction
                                            .as_ref()
                                            .map(|sample| sample.observed_at),
                                        now,
                                    )
                                }
                            ))),
                    );
                }
                if preferences.compact && account.windows.len() > 1 {
                    provider = provider.child(
                        Label::new(format!(
                            "{} windows · Detailed shows all",
                            account.windows.len()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    );
                }
            }
            content = content.child(provider);
        }
        let controls = h_flex()
            .child(
                IconButton::new("quota-status-visibility", IconName::SignalHigh)
                    .toggle_state(!self.preferences.read(cx).value.hide_status)
                    .tooltip(Tooltip::text("Show quota in status bar"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleStatusSummary), cx)
                    }),
            )
            .id("quota-header-controls")
            .debug_selector(|| "quota-header-controls".into())
            .gap_1()
            .child(
                h_flex()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        IconButton::new("quota-detailed", IconName::ExpandVertical)
                            .toggle_state(!preferences.compact)
                            .tab_index(0_isize)
                            .aria_label("Detailed quota view")
                            .tooltip(Tooltip::text("Detailed: show every quota window"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.display_preferences(cx).compact {
                                    this.change_preference(true, cx);
                                }
                            })),
                    )
                    .child(
                        IconButton::new("quota-compact", IconName::FoldVertical)
                            .toggle_state(preferences.compact)
                            .tab_index(1_isize)
                            .aria_label("Compact quota view")
                            .tooltip(Tooltip::text("Compact: show the most constrained window"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.display_preferences(cx).compact {
                                    this.change_preference(true, cx);
                                }
                            })),
                    ),
            )
            .child(
                h_flex()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        IconButton::new("quota-remaining", IconName::RotateCw)
                            .toggle_state(!preferences.used)
                            .tab_index(2_isize)
                            .aria_label("Show remaining quota")
                            .tooltip(Tooltip::text("Remaining quota; colors follow percent used"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.display_preferences(cx).used {
                                    this.change_preference(false, cx);
                                }
                            })),
                    )
                    .child(
                        IconButton::new("quota-used", IconName::RotateCcw)
                            .toggle_state(preferences.used)
                            .tab_index(3_isize)
                            .aria_label("Show used quota")
                            .tooltip(Tooltip::text("Used quota"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.display_preferences(cx).used {
                                    this.change_preference(false, cx);
                                }
                            })),
                    ),
            );
        #[cfg(debug_assertions)]
        let controls = if let Some(preview) = &self.preview {
            controls.child(
                IconButton::new("quota-preview-scene", IconName::Eye)
                    .toggle_state(preview.states)
                    .tab_index(5_isize)
                    .aria_label("Switch sample quota scene")
                    .tooltip(Tooltip::text(if preview.states {
                        "All states scene; switch to Overview"
                    } else {
                        "Overview scene; switch to All states"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(preview) = &mut this.preview {
                            let preferences = preview.preferences.clone();
                            let left = preview.left;
                            *preview = preview::FixturePreview::new(
                                chrono::Utc::now().timestamp(),
                                !preview.states,
                            );
                            preview.preferences = preferences;
                            preview.left = left;
                        }
                        cx.notify();
                    })),
            )
        } else {
            controls
        };
        let header = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .id("quota-header-row")
                    .debug_selector(|| "quota-header-row".into())
                    .justify_between()
                    .gap_2()
                    .child(Label::new("Quota"))
                    .child(
                        h_flex().gap_1().child(controls).child(
                            IconButton::new("refresh-quota", IconName::RotateCw)
                                .disabled(self.refresh_task.is_some())
                                .tab_index(4_isize)
                                .aria_label("Refresh quota")
                                .tooltip(Tooltip::text(if self.is_preview() {
                                    "Reload sample data"
                                } else {
                                    "Refresh quota"
                                }))
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        ),
                    ),
            )
            .child(
                Label::new(if self.refresh_task.is_some() {
                    format!("Refreshing · {}", age(updated, now))
                } else {
                    age(updated, now)
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        #[cfg(debug_assertions)]
        let header = if self.preview.is_some() {
            header.child(badge("Sample data · debug preview", Color::Warning, cx))
        } else {
            header
        };
        v_flex()
            .id("crew-quota-panel")
            .track_focus(&self.focus)
            .size_full()
            .p_3()
            .gap_3()
            .font_features(gpui::FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
            .on_action(cx.listener(|this, _: &RefreshQuota, _, cx| this.refresh(cx)))
            .child(header)
            .when_some(self.refresh_error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .when(self.preferences.read(cx).error, |element| {
                element.child(Label::new("Could not save panel preferences").color(Color::Error))
            })
            .child(
                v_flex()
                    .id("quota-provider-list")
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .overflow_y_scroll()
                    .child(content),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) struct InertPorts;
    impl QuotaStore for InertPorts {
        fn load(&self, _: String, _: &App) -> Task<Result<Option<String>, ErrorCode>> {
            Task::ready(Ok(None))
        }
        fn save(&self, _: String, _: String, _: &App) -> Task<Result<(), ErrorCode>> {
            Task::ready(Ok(()))
        }
    }
    impl QuotaSource for InertPorts {
        fn refresh(
            &self,
            _: String,
            _: u64,
            _: u64,
            _: &mut App,
        ) -> Task<Result<Observation, RefreshError>> {
            panic!("local display ticks must not access a provider");
        }
    }
    impl Clock for InertPorts {
        fn now(&self) -> Timestamp {
            chrono::Utc::now().timestamp()
        }
        fn monotonic(&self) -> u64 {
            0
        }
    }

    #[cfg(debug_assertions)]
    #[gpui::test]
    fn fixture_preview_is_isolated_and_draws_all_modes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(InertPorts),
                        store: Rc::new(InertPorts),
                        clock: Rc::new(InertPorts),
                    },
                    cx,
                )
            });
            cx.set_global(GlobalQuota(service));
        });
        let (panel, cx) = cx.add_window_view(|_, cx| QuotaPanel::new(cx));
        let before = panel.read_with(cx, |_, cx| {
            KeyValueStore::global(cx)
                .scoped("crew_quota")
                .read("panel_preferences")
                .expect("read preferences")
        });
        for states in [false, true] {
            for compact in [false, true] {
                for used in [false, true] {
                    panel.update_in(cx, |panel, window, cx| {
                        panel.set_active(false, window, cx);
                        let mut preview =
                            preview::FixturePreview::new(chrono::Utc::now().timestamp(), states);
                        preview.preferences.compact = compact;
                        preview.preferences.used = used;
                        panel.preview = Some(preview);
                        panel.set_active(true, window, cx);
                        panel.refresh(cx);
                        panel.change_preference(true, cx);
                        panel.change_preference(true, cx);
                        panel.change_preference(false, cx);
                        panel.change_preference(false, cx);
                        assert!(panel.demand.is_none());
                        assert!(panel.refresh_task.is_none());
                        assert_eq!(panel.display_preferences(cx).compact, compact);
                        assert_eq!(panel.display_preferences(cx).used, used);
                    });
                    cx.draw(
                        gpui::Point::default(),
                        gpui::size(
                            gpui::AvailableSpace::Definite(px(360.)),
                            gpui::AvailableSpace::Definite(px(1000.)),
                        ),
                        |_, _| panel.clone().into_any_element(),
                    );
                    panel.update_in(cx, |panel, window, cx| {
                        let focus = panel
                            .provider_focus
                            .get(&ProviderId::Grok)
                            .expect("provider focus");
                        window.focus(focus, cx);
                    });
                    cx.update(|window, cx| window.draw(cx).clear(cx));
                    cx.simulate_keystrokes("enter");
                    cx.simulate_event(gpui::KeyUpEvent {
                        keystroke: gpui::Keystroke::parse("enter").expect("enter key"),
                    });
                    panel.read_with(cx, |panel, cx| {
                        assert!(
                            panel
                                .display_preferences(cx)
                                .collapsed
                                .contains(&ProviderId::Grok)
                        )
                    });
                    cx.update(|window, cx| window.draw(cx).clear(cx));
                    cx.simulate_keystrokes("space");
                    cx.simulate_event(gpui::KeyUpEvent {
                        keystroke: gpui::Keystroke::parse("space").expect("space key"),
                    });
                    panel.read_with(cx, |panel, cx| {
                        assert!(
                            !panel
                                .display_preferences(cx)
                                .collapsed
                                .contains(&ProviderId::Grok)
                        )
                    });
                    panel.update_in(cx, |panel, _, cx| {
                        panel.toggle_provider(ProviderId::Antigravity, cx)
                    });
                    cx.draw(
                        gpui::Point::default(),
                        gpui::size(
                            gpui::AvailableSpace::Definite(px(280.)),
                            gpui::AvailableSpace::Definite(px(1000.)),
                        ),
                        |_, _| panel.clone().into_any_element(),
                    );
                    let header = cx.debug_bounds("quota-header-row").expect("header bounds");
                    let controls = cx
                        .debug_bounds("quota-header-controls")
                        .expect("controls bounds");
                    assert!(controls.right() <= header.right());
                    panel.update_in(cx, |panel, _, cx| {
                        panel.toggle_provider(ProviderId::Antigravity, cx)
                    });
                }
            }
        }
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
        panel.read_with(cx, |panel, cx| {
            assert!(panel.demand.is_none());
            assert_eq!(
                KeyValueStore::global(cx)
                    .scoped("crew_quota")
                    .read("panel_preferences")
                    .expect("read preferences"),
                before
            );
        });
        panel.update_in(cx, |panel, window, cx| panel.set_active(false, window, cx));
    }

    #[gpui::test]
    async fn workspace_attachment_restoration_and_dock_moves(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(InertPorts),
                        store: Rc::new(InertPorts),
                        clock: Rc::new(InertPorts),
                    },
                    cx,
                )
            });
            cx.set_global(GlobalQuota(service));
        });
        let filesystem = fs::FakeFs::new(cx.executor());
        let project = Project::test(filesystem, [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<QuotaPanel>(cx))
            .expect("observer must attach before restoration");
        workspace.update_in(cx, |workspace, window, cx| {
            let mut state = workspace.capture_dock_state(window, cx);
            state.right.visible = true;
            state.right.active_panel = Some("CrewQuotaPanel".into());
            workspace.set_dock_structure(state, window, cx);
            workspace.finish_dock_restoration(cx);
            assert!(workspace.right_dock().read(cx).is_open());
            assert_eq!(
                workspace
                    .right_dock()
                    .read(cx)
                    .active_panel()
                    .map(|panel| panel.persistent_name()),
                Some("CrewQuotaPanel")
            );
        });
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.set_panel_size_state::<QuotaPanel>(
                workspace::dock::PanelSizeState {
                    size: Some(px(420.)),
                    flex: None
                },
                window,
                cx
            ));
        });
        for position in [DockPosition::Left, DockPosition::Right] {
            panel.update_in(cx, |panel, window, cx| {
                panel.set_position(position, window, cx)
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(
                    workspace
                        .panel_size_state::<QuotaPanel>(cx)
                        .and_then(|state| state.size),
                    Some(px(420.))
                );
                assert!(
                    workspace
                        .dock_at_position(position)
                        .read(cx)
                        .panel_index_for_type::<QuotaPanel>()
                        .is_some()
                );
            });
        }
        panel.update_in(cx, |panel, window, cx| {
            panel.set_active(true, window, cx);
            assert!(panel.demand.is_some());
        });
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
        for availability in [
            Availability::Unknown,
            Availability::NotLoggedIn,
            Availability::Unsupported(ErrorCode::UnsupportedVersion),
            Availability::Error(ErrorCode::Transport),
            Availability::Ready,
        ] {
            let service = panel
                .read_with(cx, |panel, _| panel.service.clone())
                .expect("fake service");
            service.update(cx, |service, cx| {
                let mut account = fake_account();
                account.availability = availability;
                service
                    .core
                    .snapshot
                    .accounts
                    .insert("fake".into(), account);
                cx.emit(QuotaEvent::AvailabilityChanged("fake".into()));
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(
                    gpui::AvailableSpace::Definite(px(360.)),
                    gpui::AvailableSpace::Definite(px(800.)),
                ),
                |_, _| panel.clone().into_any_element(),
            );
        }
        panel.update_in(cx, |panel, _, cx| {
            panel.change_preference(true, cx);
            panel.change_preference(false, cx);
            panel.toggle_provider(ProviderId::Codex, cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |_, cx| {
            let value = KeyValueStore::global(cx)
                .scoped("crew_quota")
                .read("panel_preferences")
                .expect("read preferences")
                .expect("persisted preferences");
            let preferences: Preferences = serde_json::from_str(&value).expect("valid preferences");
            assert!(preferences.compact);
            assert!(preferences.used);
            assert!(preferences.collapsed.contains(&ProviderId::Codex));
            assert!(!preferences.left);
        });
        panel.update_in(cx, |panel, window, cx| {
            assert_eq!(panel.default_size(window, cx), px(360.));
            assert!(!panel.position_is_valid(DockPosition::Bottom));
            panel.set_active(false, window, cx);
            assert!(panel.tick.is_none());
            assert!(panel.demand.is_none());
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.remove_panel(&panel, window, cx);
            register_workspace(workspace, None, cx);
            assert!(workspace.panel::<QuotaPanel>(cx).is_none());
        });
        workspace.update_in(cx, |workspace, window, cx| {
            let _element = workspace.actions(gpui::div(), window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.panel::<QuotaPanel>(cx).is_some())
        });
    }

    fn fake_account() -> AccountState {
        AccountState {
            scope: AccountScope {
                id: "fake".into(),
                provider: ProviderId::Codex,
                runtime: "local".into(),
                identity_known: false,
            },
            generation: 0,
            availability: Availability::Ready,
            refresh: RefreshState::Idle,
            windows: BTreeMap::new(),
            metadata: AccountMetadata::default(),
            last_attempt_at: None,
            last_success_at: None,
            source_version: None,
            complete: false,
            sequence: 0,
        }
    }

    #[test]
    fn fake_snapshot_render_states() {
        let mut account = fake_account();
        for (availability, expected) in [
            (Availability::Unknown, "Unknown"),
            (Availability::NotLoggedIn, "Not logged in"),
            (
                Availability::Unsupported(ErrorCode::UnsupportedVersion),
                "Unsupported",
            ),
            (Availability::Error(ErrorCode::Transport), "Error"),
        ] {
            account.availability = availability;
            assert!(account_status(&account, 1000).contains(expected));
        }
        account.refresh = RefreshState::InFlight;
        assert!(account_status(&account, 1000).contains("Loading"));
        let key = WindowKey {
            bucket: BucketKey {
                scope: "fake".into(),
                bucket: "Session".into(),
                applicability: Applicability::AllModels,
            },
            role: "5h".into(),
            duration_seconds: Some(18000),
        };
        account.windows.insert(
            key.clone(),
            QuotaWindow {
                key,
                used_fraction: Some(Measurement {
                    value: Some(1.0),
                    observed_at: 0,
                    provider_sample_at: None,
                    sequence: 0,
                    source: SourceKind::CodexAppServerRead,
                }),
                remaining: None,
                limit: None,
                reset_at: None,
                admission: None,
                disabled: None,
                precision: Precision::RoundedPercent,
            },
        );
        let status = account_status(&account, 1000);
        assert!(status.contains("Stale"));
        assert!(status.contains("Exhausted"));
        let snapshot = QuotaSnapshot {
            revision: 1,
            accounts: BTreeMap::from([("fake".into(), account)]),
        };
        assert_eq!(
            provider_rows(&snapshot).first().map(|row| row.provider),
            Some(ProviderId::Codex)
        );
    }

    #[test]
    fn provider_expansion_defaults_and_round_trips() {
        let mut preferences: Preferences =
            serde_json::from_str(r#"{"left":false,"used":false,"compact":false}"#)
                .expect("legacy preferences");
        assert!(preferences.collapsed.is_empty());
        toggle_collapsed(&mut preferences, ProviderId::Codex);
        toggle_collapsed(&mut preferences, ProviderId::Claude);
        let encoded = serde_json::to_string(&preferences).expect("encode preferences");
        let restored: Preferences = serde_json::from_str(&encoded).expect("restore preferences");
        assert_eq!(
            restored.collapsed,
            BTreeSet::from([ProviderId::Codex, ProviderId::Claude])
        );
        toggle_collapsed(&mut preferences, ProviderId::Codex);
        assert!(!preferences.collapsed.contains(&ProviderId::Codex));
        assert!(preferences.collapsed.contains(&ProviderId::Claude));
    }

    #[test]
    fn unknown_credit_count_is_not_zero() {
        let balance = Amount {
            value: 120.,
            unit: Unit::Credits { denomination: None },
        };
        assert_eq!(
            credit_text(&balance, None),
            "120 credits · Reset credits unknown"
        );
        assert_eq!(
            credit_text(&balance, Some(3)),
            "120 credits · 3 reset credits"
        );
    }

    #[test]
    fn truthful_display_values_and_thresholds() {
        assert_eq!(display_fraction(0.9, false), 0.09999999999999998);
        assert_eq!(display_fraction(1.2, false), 0.0);
        assert_eq!(severity(0.59), Color::Muted);
        assert_eq!(severity(0.60), Color::Warning);
        assert_eq!(severity(0.80), Color::Error);
        assert_eq!(countdown(Some(60), 61), "Resets now");
        assert_eq!(countdown(None, 0), "Reset unknown");
    }

    #[test]
    fn empty_snapshot_keeps_pending_providers_honest() {
        let snapshot = QuotaSnapshot::default();
        let rows = provider_rows(&snapshot);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row.accounts.is_empty()));
        assert!(!Preferences::default().used);
        assert!(!Preferences::default().compact);
        assert!(!Preferences::default().left);
    }
}
