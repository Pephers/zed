//! Status bar indicator showing the signed-in GitHub Copilot account's usage
//! for the current billing cycle.

use std::time::Duration;

use copilot_chat::{CopilotChat, CopilotUsage};
use gpui::{Context, IntoElement, ParentElement, Render, Styled, Subscription, Task, Window, div};
use project::DisableAiSettings;
use settings::{Settings as _, SettingsStore};
use ui::{Label, LabelSize, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusBarSettings, StatusItemView};

/// GitHub's quota snapshots only change as requests are made, so a slow poll
/// keeps the indicator roughly current without adding meaningful traffic to an
/// undocumented endpoint.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Why the indicator is or isn't polling. Tracked so the reason can be logged
/// on transitions only: `sync_polling` runs on every settings change and every
/// `CopilotChat` notification, which is far too often to log unconditionally.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PollState {
    AiDisabled,
    Disabled,
    NotSignedIn,
    Polling,
}

impl PollState {
    fn reason(self) -> &'static str {
        match self {
            Self::AiDisabled => "not polling: AI features are disabled",
            Self::Disabled => "not polling: status_bar.copilot_usage_button is disabled",
            Self::NotSignedIn => "not polling: not signed in to GitHub Copilot Chat",
            Self::Polling => "polling for usage",
        }
    }
}

/// Whether the indicator may run at all. `copilot_chat::init` does not itself
/// check `disable_ai`, so `CopilotChat` can still hold a valid token while AI is
/// turned off; without this gate the indicator would keep calling GitHub's API.
fn is_allowed(cx: &App) -> bool {
    !DisableAiSettings::get_global(cx).disable_ai
        && StatusBarSettings::get_global(cx).copilot_usage_button
}

pub struct CopilotUsageIndicator {
    usage: Option<CopilotUsage>,
    poll_task: Option<Task<()>>,
    poll_state: Option<PollState>,
    _subscriptions: Vec<Subscription>,
}

impl CopilotUsageIndicator {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(copilot_chat) = CopilotChat::global(cx) {
            subscriptions.push(cx.observe(&copilot_chat, |this, _, cx| {
                this.sync_polling(cx);
            }));
        }
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_polling(cx);
        }));

        let mut this = Self {
            usage: None,
            poll_task: None,
            poll_state: None,
            _subscriptions: subscriptions,
        };
        this.sync_polling(cx);
        this
    }

    /// Starts polling when the indicator is enabled and Copilot is signed in,
    /// and stops it otherwise. Cheap to call repeatedly: an already-running poll
    /// is left alone so unrelated notifications don't restart the loop.
    fn sync_polling(&mut self, cx: &mut Context<Self>) {
        // `is_authenticated` rather than `status() == Authorized`: this is the
        // same check the agent panel's Copilot Chat provider uses, and it also
        // covers the brief startup window where a token supplied through the
        // environment is present but the status is still `Starting`.
        let signed_in = CopilotChat::global(cx)
            .is_some_and(|copilot_chat| copilot_chat.read(cx).is_authenticated());

        let state = if DisableAiSettings::get_global(cx).disable_ai {
            PollState::AiDisabled
        } else if !StatusBarSettings::get_global(cx).copilot_usage_button {
            PollState::Disabled
        } else if !signed_in {
            PollState::NotSignedIn
        } else {
            PollState::Polling
        };

        if self.poll_state != Some(state) {
            self.poll_state = Some(state);
            // Only the polling case is logged at `info`. The inactive cases are
            // the norm for the many users who never sign in to Copilot Chat, so
            // logging those unconditionally would add noise to every session;
            // reach them with `ZED_LOG=copilot_ui=debug` when diagnosing a
            // missing indicator.
            if state == PollState::Polling {
                log::info!("GitHub Copilot usage indicator {}", state.reason());
            } else {
                log::debug!("GitHub Copilot usage indicator {}", state.reason());
            }
        }

        if state != PollState::Polling {
            // Not `||`: both fields must be cleared, so short-circuiting here
            // would leave a stale reading on screen after signing out.
            let had_poll_task = self.poll_task.take().is_some();
            let had_usage = self.usage.take().is_some();
            if had_poll_task || had_usage {
                cx.notify();
            }
            return;
        }

        if self.poll_task.is_some() {
            return;
        }

        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let fetch = this.read_with(cx, |_, cx| {
                    CopilotChat::global(cx)
                        .map(|copilot_chat| copilot_chat.read(cx).fetch_usage(cx))
                });
                let Ok(Some(fetch)) = fetch else {
                    return;
                };

                match fetch.await {
                    Ok(usage) => {
                        let updated = this.update(cx, |this, cx| {
                            this.usage = Some(usage);
                            cx.notify();
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                    // Keep any previously fetched value on screen rather than
                    // flickering the indicator away on a transient failure.
                    Err(error) => {
                        log::warn!("Failed to fetch GitHub Copilot usage: {error:#}")
                    }
                }

                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        }));
    }
}

impl Render for CopilotUsageIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !is_allowed(cx) {
            return div().hidden();
        }

        // Nothing to show for accounts without a metered AI-credit quota, such
        // as unlimited plans or plans that only meter chat and completions.
        let Some((usage, quota)) = self
            .usage
            .as_ref()
            .and_then(|usage| Some((usage, usage.premium_interactions?)))
        else {
            return div().hidden();
        };

        let label = format!("Copilot Chat: {}/{}", quota.used, quota.entitlement);

        // Spelled out for screen readers, which read "/" poorly.
        let aria_label = format!(
            "GitHub Copilot Chat usage: {} of {} AI credits",
            quota.used, quota.entitlement
        );

        let mut meta_lines = Vec::new();
        if let Some(plan) = usage.plan.as_deref() {
            meta_lines.push(format!("Plan: {plan}"));
        }
        meta_lines.push(format!(
            "Usage this cycle: {}/{} AI credits",
            quota.used, quota.entitlement
        ));
        if let Some(reset_date) = usage.reset_date() {
            meta_lines.push(format!("Resets on {reset_date}"));
        }
        let meta = meta_lines.join("\n");

        // Purely informational, so this is a labelled div rather than a
        // `Button`: a button with no `on_click` would still be tab-focusable
        // while doing nothing.
        div().child(
            div()
                .id("copilot-usage")
                .child(Label::new(label).size(LabelSize::Small))
                .aria_label(aria_label)
                .tooltip(move |_window, cx| {
                    Tooltip::with_meta("GitHub Copilot Chat Usage", None, meta.clone(), cx)
                }),
        )
    }
}

impl StatusItemView for CopilotUsageIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Account-level usage is independent of the active item.
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings
                .status_bar
                .get_or_insert_default()
                .copilot_usage_button = Some(false);
        }))
    }
}
