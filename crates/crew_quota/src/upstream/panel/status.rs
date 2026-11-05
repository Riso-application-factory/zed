use super::*;
use agent_ui::{AgentPanel, ConversationView};

pub(super) fn attach(
    workspace: &Workspace,
    panel: Entity<QuotaPanel>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = cx.entity();
    let item = cx.new(|cx| {
        let mut subscriptions = vec![cx.observe(&panel, |_, _, cx| cx.notify())];
        for dock in workspace.all_docks() {
            subscriptions.push(cx.observe(dock, |_, _, cx| cx.notify()));
        }
        if let Some(agent_panel) = workspace.panel::<AgentPanel>(cx) {
            subscriptions.push(cx.observe(&agent_panel, |_, _, cx| cx.notify()));
            let conversation = agent_panel.read(cx).active_conversation_view().cloned();
            if let Some(conversation) = conversation {
                subscriptions.push(cx.observe(&conversation, |_, _, cx| cx.notify()));
            }
        }
        QuotaStatusItem {
            panel,
            workspace: Some(workspace_handle.downgrade()),
            _subscriptions: subscriptions,
        }
    });
    let weak = item.downgrade();
    let subscription = cx.observe_new::<AgentPanel>(move |_, _, cx| {
        let panel = cx.entity();
        let weak = weak.clone();
        cx.defer(move |cx| {
            weak.update(cx, |item, cx| {
                let subscription = cx.observe(&panel, |_, _, cx| cx.notify());
                item._subscriptions.push(subscription);
                cx.notify();
            })
            .log_err();
        });
    });
    item.update(cx, |item, _| item._subscriptions.push(subscription));
    let weak = item.downgrade();
    let subscription = cx.observe_new::<ConversationView>(move |_, _, cx| {
        let conversation = cx.entity();
        let weak = weak.clone();
        cx.defer(move |cx| {
            weak.update(cx, |item, cx| {
                let subscription = cx.observe(&conversation, |_, _, cx| cx.notify());
                item._subscriptions.push(subscription);
                cx.notify();
            })
            .log_err();
        });
    });
    item.update(cx, |item, _| item._subscriptions.push(subscription));
    workspace
        .status_bar()
        .update(cx, |bar, cx| bar.add_right_item(item, window, cx));
}

struct QuotaStatusItem {
    panel: Entity<QuotaPanel>,
    workspace: Option<gpui::WeakEntity<Workspace>>,
    _subscriptions: Vec<Subscription>,
}

impl workspace::StatusItemView for QuotaStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn workspace::ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

impl QuotaStatusItem {
    fn visible_provider(&self, cx: &App) -> Option<ProviderId> {
        let workspace = self.workspace.as_ref()?.upgrade()?;
        if !AgentPanel::is_visible(&workspace, cx) {
            return None;
        }
        let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
        let conversation = panel.read(cx).active_conversation_view()?;
        let conversation = conversation.read(cx);
        let agent = conversation.agent_key().id();
        provider_for_thread(
            true,
            conversation.agent_key().is_native(),
            Some(agent.as_ref()),
        )
    }
}

fn provider_for_thread(visible: bool, native: bool, agent: Option<&str>) -> Option<ProviderId> {
    if !visible || native {
        return None;
    }
    match agent? {
        "codex-acp" => Some(ProviderId::Codex),
        "claude-acp" => Some(ProviderId::Claude),
        "antigravity" | "antigravity-acp" => Some(ProviderId::Antigravity),
        "grok" | "grok-acp" => Some(ProviderId::Grok),
        _ => None,
    }
}

fn numeric_summary(account: &AccountState, used: bool) -> Option<String> {
    if account.scope.category != AccountCategory::ExternalAgent {
        return None;
    }
    if matches!(
        account.availability,
        Availability::NotSetUp | Availability::NotLoggedIn
    ) {
        return None;
    }
    let summary = visible_windows(account)
        .iter()
        .filter(|window| {
            used_fraction(window).is_some()
                || window
                    .remaining
                    .as_ref()
                    .is_some_and(|sample| sample.value.is_some())
        })
        .map(|window| {
            format!(
                "{} {}",
                window_title(account, window),
                window_value(window, used)
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    (!summary.is_empty()).then_some(summary)
}

fn tooltip_text(account: &AccountState, now: Timestamp) -> String {
    let name = match account.scope.provider {
        ProviderId::Claude => "Claude",
        ProviderId::Codex => "Codex",
        ProviderId::Antigravity => "Antigravity",
        ProviderId::Grok => "Grok",
    };
    let mut lines = vec![name.to_string()];
    for window in visible_windows(account) {
        let reset = window.reset_at.as_ref().and_then(|sample| sample.value);
        let reset = if reset.is_some() {
            countdown(reset, now)
        } else {
            "Reset time unavailable".into()
        };
        lines.push(format!(
            "{} {} · {}",
            window_title(account, window),
            window_value(window, false),
            reset
        ));
    }
    let status = account_status(account, now);
    if !status.is_empty() {
        lines.push(status);
    }
    lines.join("\n")
}

impl Render for QuotaStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let provider = self.visible_provider(cx);
        let panel = self.panel.read(cx);
        let preferences = panel.display_preferences(cx);
        let now = chrono::Utc::now().timestamp();
        let snapshot = panel
            .service
            .as_ref()
            .map(|service| service.read(cx).snapshot(&Selector::default(), now))
            .unwrap_or_default();
        #[cfg(debug_assertions)]
        let snapshot = panel
            .preview
            .as_ref()
            .map(|preview| preview.snapshot.clone())
            .unwrap_or(snapshot);
        let mut snapshot = snapshot;
        Arc::make_mut(&mut snapshot)
            .accounts
            .retain(|_, account| account.scope.category == AccountCategory::ExternalAgent);
        let mut content = h_flex().gap_1();
        if !preferences.hide_status
            && let Some(provider) = provider
        {
            for row in provider_rows(&snapshot)
                .into_iter()
                .filter(|row| row.provider == provider)
            {
                for account in row
                    .accounts
                    .into_iter()
                    .filter(|account| account.scope.category == AccountCategory::ExternalAgent)
                {
                    let tooltip = tooltip_text(account, now);
                    let Some(summary) = numeric_summary(account, preferences.used) else {
                        continue;
                    };
                    content = content.child(
                        div()
                            .id("quota-active-thread")
                            .tooltip(Tooltip::text(tooltip))
                            .child(Label::new(summary).size(LabelSize::Small)),
                    );
                }
            }
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn healthy_footer_tooltip_has_provider_remaining_quota_and_resets() {
        let preview = super::super::preview::FixturePreview::new(1000, false);
        let mut account = preview
            .snapshot
            .accounts
            .get("codex")
            .expect("Codex fixture")
            .clone();
        account.availability = Availability::Ready;
        account.refresh = RefreshState::Idle;
        for window in account.windows.values_mut() {
            if let Some(used) = &mut window.used_fraction {
                used.value = Some(0.16);
            }
            if let Some(reset) = &mut window.reset_at {
                reset.value = Some(4600);
            }
        }
        let text = tooltip_text(&account, 1000);
        assert!(text.contains("Codex"), "tooltip has no provider: {text:?}");
        assert!(text.contains("5h 84% left"), "{text}");
        assert!(text.contains("Week 84% left"), "{text}");
        assert_eq!(text.matches("Resets in 1h 0m").count(), 2, "{text}");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn footer_only_renders_numbers_and_hides_empty_states() {
        let preview = super::super::preview::FixturePreview::new(1000, false);
        let mut account = preview.snapshot.accounts["codex"].clone();
        assert!(numeric_summary(&account, false).is_some());
        account.availability = Availability::NotSetUp;
        assert!(numeric_summary(&account, false).is_none());
        account.availability = Availability::NotLoggedIn;
        assert!(numeric_summary(&account, false).is_none());
        account.windows.clear();
        for availability in [
            Availability::Unknown,
            Availability::Ready,
            Availability::NotLoggedIn,
        ] {
            account.availability = availability;
            for refresh in [
                RefreshState::Idle,
                RefreshState::Queued,
                RefreshState::InFlight,
            ] {
                account.refresh = refresh;
                assert!(numeric_summary(&account, false).is_none());
            }
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn native_threads_and_native_accounts_never_select_status_quota() {
        for provider in [
            "x_ai",
            "openai",
            "anthropic",
            "zed.dev",
            "codex-acp",
            "grok-acp",
        ] {
            assert_eq!(provider_for_thread(true, true, Some(provider)), None);
        }
        let preview = super::super::preview::FixturePreview::new(1000, false);
        let mut native = preview.snapshot.accounts["codex"].clone();
        native.scope.category = AccountCategory::LlmProvider;
        native.scope.native_provider_id = Some("openai".into());
        assert!(numeric_summary(&native, false).is_none());
    }

    #[test]
    fn follows_visible_thread_and_hides_for_other_surfaces() {
        assert_eq!(
            provider_for_thread(true, false, Some("codex-acp")),
            Some(ProviderId::Codex)
        );
        assert_eq!(
            provider_for_thread(true, false, Some("claude-acp")),
            Some(ProviderId::Claude)
        );
        assert_eq!(provider_for_thread(false, false, Some("codex-acp")), None);
        assert_eq!(provider_for_thread(true, false, None), None);
        assert_eq!(provider_for_thread(true, false, Some("zed")), None);
    }

    #[gpui::test]
    fn footer_keeps_service_idle(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
            let service = cx.new(|cx| {
                QuotaService::new(
                    Ports {
                        source: Rc::new(super::super::tests::InertPorts),
                        store: Rc::new(super::super::tests::InertPorts),
                        clock: Rc::new(super::super::tests::InertPorts),
                    },
                    cx,
                )
            });
            service.update(cx, |service, _| {
                service
                    .scheduler
                    .track("codex".into(), ProviderId::Codex, 1)
            });
            cx.set_global(GlobalQuota(service));
        });
        let (footer, cx) = cx.add_window_view(|_, cx| {
            let panel = cx.new(QuotaPanel::new);
            let subscription = cx.observe(&panel, |_, _, cx| cx.notify());
            QuotaStatusItem {
                panel,
                _subscriptions: vec![subscription],
                workspace: None,
            }
        });
        cx.draw(
            gpui::Point::default(),
            gpui::size(
                gpui::AvailableSpace::Definite(px(900.)),
                gpui::AvailableSpace::Definite(px(30.)),
            ),
            |_, _| footer.clone().into_any_element(),
        );
        footer.read_with(cx, |footer, cx| {
            let panel = footer.panel.read(cx);
            assert!(panel.demand.is_none());
            let service = panel.service.as_ref().expect("service").read(cx);
            assert!(service.demands.is_empty());
            let schedule = service.scheduler.scopes.get("codex").expect("schedule");
            assert_eq!(
                Scheduler::interval(schedule, chrono::Utc::now().timestamp()),
                crate::scheduler::IDLE_SECONDS
            );
        });
    }
}
