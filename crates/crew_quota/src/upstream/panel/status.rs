use super::*;

pub(super) fn attach(
    workspace: &Workspace,
    panel: Entity<QuotaPanel>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let item = cx.new(|cx| {
        let subscription = cx.observe(&panel, |_, _, cx| cx.notify());
        QuotaStatusItem {
            panel,
            _subscription: subscription,
        }
    });
    workspace
        .status_bar()
        .update(cx, |bar, cx| bar.add_right_item(item, window, cx));
}

struct QuotaStatusItem {
    panel: Entity<QuotaPanel>,
    _subscription: Subscription,
}

impl workspace::StatusItemView for QuotaStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn workspace::ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

impl Render for QuotaStatusItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel.read(cx);
        let preferences = panel.display_preferences(cx);
        let mut content = h_flex().gap_1();
        if preferences.hide_status {
            return content;
        }
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
        let rows = provider_rows(&snapshot);
        let capacity = if window.viewport_size().width < px(1000.) {
            1
        } else {
            4
        };
        let mut hidden = Vec::new();
        let mut hidden_used: f64 = 0.;
        for (index, row) in rows.iter().enumerate() {
            let (name, icon) = match row.provider {
                ProviderId::Claude => ("Claude", IconName::AiClaude),
                ProviderId::Codex => ("Codex", IconName::AiOpenAi),
                ProviderId::Antigravity => ("Antigravity", IconName::AiGemini),
                ProviderId::Grok => ("Grok", IconName::AiXAi),
            };
            let mut details = vec![name.to_string()];
            let mut headline = None;
            let mut reset = None;
            let mut headline_label = String::new();
            let mut fresh = false;
            for account in &row.accounts {
                details.push(account_status(account, now));
                for quota_window in account.windows.values() {
                    if quota_window
                        .disabled
                        .as_ref()
                        .is_some_and(|sample| sample.value)
                    {
                        continue;
                    }
                    let Some(used) = used_fraction(quota_window) else {
                        continue;
                    };
                    let label = match quota_window.key.role.as_str() {
                        "Weekly" | "7d" => "wk",
                        role => role,
                    };
                    details.push(format!(
                        "{} · {}: {:.0}% {} · {}",
                        quota_window.key.bucket.bucket,
                        label,
                        display_fraction(used, preferences.used) * 100.,
                        if preferences.used {
                            "Used"
                        } else {
                            "Remaining"
                        },
                        countdown(
                            quota_window
                                .reset_at
                                .as_ref()
                                .and_then(|sample| sample.value),
                            now
                        )
                    ));
                    if headline.is_none_or(|previous| used > previous) {
                        headline = Some(used);
                        headline_label = label.to_string();
                        reset = quota_window
                            .reset_at
                            .as_ref()
                            .and_then(|sample| sample.value);
                        fresh = account.availability == Availability::Ready
                            && matches!(
                                quota_window.freshness(now, Duration::from_secs(900)),
                                Freshness::Fresh
                            );
                    }
                }
            }
            let color = headline
                .filter(|_| fresh)
                .map(severity)
                .unwrap_or(Color::Muted);
            let text = headline
                .map(|used| {
                    format!(
                        "{} {:.0}%{}",
                        headline_label,
                        display_fraction(used, preferences.used) * 100.,
                        if fresh { "" } else { " ◷" }
                    )
                })
                .unwrap_or_else(|| "--".into());
            if index >= capacity {
                hidden.push(details.join("\n"));
                if fresh {
                    hidden_used = hidden_used.max(headline.unwrap_or(0.));
                }
                continue;
            }
            content = content.child(
                ButtonLike::new(("quota-summary", index))
                    .tooltip(Tooltip::text(details.join("\n")))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenQuotaSummary), cx)
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(icon).size(IconSize::Small).color(color))
                            .child(Label::new(text).size(LabelSize::Small).color(color))
                            .when(headline.is_some_and(|used| used >= 0.8), |element| {
                                element.child(
                                    Label::new(
                                        countdown(reset, now)
                                            .replace("Resets in ", "")
                                            .replace("Reset unknown", ""),
                                    )
                                    .size(LabelSize::Small)
                                    .color(color),
                                )
                            }),
                    ),
            );
        }
        if !hidden.is_empty() {
            content = content.child(
                ButtonLike::new("quota-overflow")
                    .tooltip(Tooltip::text(hidden.join("\n\n")))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenQuotaSummary), cx)
                    })
                    .child(Label::new(format!("+{}", hidden.len())).color(severity(hidden_used))),
            );
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                _subscription: subscription,
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
