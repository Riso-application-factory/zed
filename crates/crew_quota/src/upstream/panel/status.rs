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
        if conversation.agent_key().is_native() {
            let thread = conversation.as_native_thread(cx)?;
            return thread
                .read(cx)
                .model()
                .filter(|model| {
                    model.provider_id() == language_model::LanguageModelProviderId::new("x_ai")
                })
                .map(|_| ProviderId::Grok);
        }
        let agent = conversation.agent_key().id();
        provider_for_thread(true, Some(agent.as_ref()))
    }
}

fn provider_for_thread(visible: bool, agent: Option<&str>) -> Option<ProviderId> {
    if !visible {
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
        let mut content = h_flex().gap_1();
        if !preferences.hide_status
            && let Some(provider) = provider
        {
            for row in provider_rows(&snapshot)
                .into_iter()
                .filter(|row| row.provider == provider)
            {
                for account in row.accounts {
                    let status = account_status(account, now);
                    let summary = if account.availability == Availability::NotLoggedIn {
                        "Not logged in".to_string()
                    } else {
                        visible_windows(account)
                            .iter()
                            .map(|window| {
                                format!(
                                    "{} {}",
                                    window_title(account, window),
                                    window_value(window, preferences.used)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" · ")
                    };
                    let summary =
                        if status.is_empty() || account.availability == Availability::NotLoggedIn {
                            summary
                        } else {
                            format!("{status} · {summary}")
                        };
                    content = content.child(
                        div()
                            .id("quota-active-thread")
                            .tooltip(Tooltip::text(status))
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

    #[test]
    fn follows_visible_thread_and_hides_for_other_surfaces() {
        assert_eq!(
            provider_for_thread(true, Some("codex-acp")),
            Some(ProviderId::Codex)
        );
        assert_eq!(
            provider_for_thread(true, Some("claude-acp")),
            Some(ProviderId::Claude)
        );
        assert_eq!(provider_for_thread(false, Some("codex-acp")), None);
        assert_eq!(provider_for_thread(true, None), None);
        assert_eq!(provider_for_thread(true, Some("zed")), None);
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
