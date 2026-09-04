//! First-launch onboarding: Welcome → Features (with the two telemetry switches, as Zed's Basics
//! page has them) → Provider + key.
//! Rendered as the Library window's content while `Config.onboarding_completed == false`.

use gpui::{prelude::*, px, Context, Entity, SharedString, Window};
use gpui_component::button::{ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _};
use majik_providers::{ProviderDescriptor, ProviderId, ProviderRegistry};

use crate::config::{update_config, Config};
use crate::state;
use crate::ui::{button, fade_to, icon, segmented, MOTION_NORMAL};
use crate::views::settings::telemetry_switches;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Features,
    Provider,
}

pub struct OnboardingView {
    step: Step,
    provider: ProviderId,
    key_input: Entity<InputState>,
    status: Option<SharedString>,
}

impl OnboardingView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let provider = cx.global::<Config>().provider_id();
        let key_input = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(placeholder_for(&provider)));
        cx.subscribe(&key_input, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        Self { step: Step::Welcome, provider, key_input, status: None }
    }

    fn descriptor(&self) -> Option<&'static ProviderDescriptor> {
        ProviderRegistry::shared().descriptor(&self.provider)
    }

    fn go(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.step = step;
        if step == Step::Provider {
            self.key_input.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    fn select_provider(&mut self, id: ProviderId, window: &mut Window, cx: &mut Context<Self>) {
        if self.provider == id {
            return;
        }
        self.provider = id.clone();
        self.status = None;
        let ph = placeholder_for(&id);
        self.key_input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(ph, window, cx);
            s.set_masked(true, window, cx);
        });
        cx.notify();
    }

    fn key(&self, cx: &Context<Self>) -> String {
        self.key_input.read(cx).value().trim().to_string()
    }

    /// Port of `saveAndContinueOnboarding`: store the key, select the provider, finish.
    fn connect(&mut self, cx: &mut Context<Self>) {
        let key = self.key(cx);
        if key.is_empty() {
            return;
        }
        let save = state::keys(cx).set(self.provider.as_str(), &key, cx);
        let provider = self.provider.0.clone();
        self.status = None;
        cx.notify();
        cx.spawn(async move |this, cx| match save.await {
            Ok(()) => cx.update(|cx| {
                majik_telemetry::event!("Onboarding Completed", provider = provider.clone());
                update_config(cx, |c| {
                    c.provider = provider;
                    c.onboarding_completed = true;
                });
            }),
            Err(e) => {
                this.update(cx, |v, cx| {
                    v.status = Some(format!("Couldn't save key: {e:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn skip(&mut self, cx: &mut Context<Self>) {
        majik_telemetry::event!("Onboarding Skipped", provider = self.provider.to_string());
        update_config(cx, |c| c.onboarding_completed = true);
    }

    // ----- steps ---------------------------------------------------------------------

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .items_center()
            .child(gpui::div().flex_1())
            .child(
                v_flex()
                    .gap_4()
                    .items_center()
                    .p_6()
                    .child(gpui::img("logos/app-icon.png").w(px(88.)).h(px(88.)))
                    .child(gpui::div().text_3xl().font_weight(gpui::FontWeight::BOLD).text_center().child("Every AI model. One app."))
                    .child(gpui::div().text_color(muted).text_center().child("Generate images and videos with Nano Banana, Flux, GPT-5, and more.")),
            )
            .child(gpui::div().flex_1())
            .child(
                gpui::div().p_6().w(px(320.)).child(
                    button("onboarding-welcome-continue").label("Get Started").primary().large().w_full().on_click(cx.listener(|this, _, window, cx| {
                        this.go(Step::Features, window, cx);
                    })),
                ),
            )
    }

    fn render_features(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let row = |name: &'static str, title: &'static str, subtitle: &'static str| {
            h_flex()
                .gap_2()
                .items_start()
                .child(gpui::div().w(px(32.)).flex_none().pt(px(2.)).child(icon(name).size_6()))
                .child(
                    v_flex()
                        .gap_1()
                        .flex_1()
                        .child(gpui::div().font_weight(gpui::FontWeight::BOLD).child(title))
                        .child(gpui::div().text_color(muted).child(subtitle)),
                )
        };
        v_flex()
            .size_full()
            .items_center()
            .child(gpui::div().flex_1())
            .child(
                v_flex()
                    .gap_6()
                    .items_center()
                    .p_6()
                    .child(gpui::div().text_3xl().font_weight(gpui::FontWeight::BOLD).text_center().child("How it works"))
                    .child(
                        v_flex()
                            .gap_4()
                            .max_w(px(450.))
                            .child(row("circle-dollar-sign", "No subscription. No markup.", "Bring your own API key. You pay what it costs."))
                            .child(row("shield-check", "No middleman.", "The app calls the provider directly. Images stay on your device."))
                            .child(row("monitor", "Mac, Windows, Linux.", "One library across all your devices, stored as plain files on your disk.")),
                    )
                    // The telemetry switches, on by default and explained here rather than buried
                    // in Settings, as Zed's onboarding does.
                    .child(v_flex().w(px(450.)).max_w_full().children(telemetry_switches(cx.global::<Config>().telemetry, cx))),
            )
            .child(gpui::div().flex_1())
            .child(
                gpui::div().p_6().w(px(320.)).child(
                    button("onboarding-features-continue").label("Continue").primary().large().w_full().on_click(cx.listener(|this, _, window, cx| {
                        this.go(Step::Provider, window, cx);
                    })),
                ),
            )
    }

    fn render_provider(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, tile, danger) = (theme.muted_foreground, theme.secondary, theme.danger);
        let providers = ProviderRegistry::shared().user_selectable();
        let current = self.provider.clone();
        let descriptor = self.descriptor();
        let requires_key = descriptor.map(|d| d.requires_api_key).unwrap_or(true);
        let can_connect = !requires_key || !self.key(cx).is_empty();

        // Logos (opacity 1 for the selected provider, 0.3 otherwise), clickable like the picker.
        let mut logos = h_flex().gap_4().items_center();
        for d in &providers {
            let selected = d.id == current;
            let id = d.id.clone();
            logos = logos.child(
                gpui::div()
                    .id(SharedString::from(format!("logo-{}", d.id)))
                    .p_3()
                    .rounded(px(20.))
                    .bg(tile)
                    // Unselected logos sit at 0.3 opacity, eased over 0.2 s.
                    .opacity(fade_to(("provider-logo", d.id.0.clone()), if selected { 1.0 } else { 0.3 }, MOTION_NORMAL, window, cx))
                    .cursor_pointer()
                    .child(gpui::div().w(px(56.)).h(px(56.)).children(crate::ui::logo(d.logo_asset_name, cx)))
                    .on_click(cx.listener(move |this, _, window, cx| this.select_provider(id.clone(), window, cx))),
            );
        }

        let selected = providers.iter().position(|d| d.id == current).unwrap_or(0);
        let picker = segmented("onboarding-provider", providers.iter().map(|d| (SharedString::from(format!("onboarding-prov-{}", d.id)), d.display_name)), selected, {
            let this = cx.weak_entity();
            let ids: Vec<ProviderId> = providers.iter().map(|d| d.id.clone()).collect();
            move |index, window, cx| {
                let Some(id) = ids.get(index).cloned() else { return };
                this.update(cx, |this, cx| this.select_provider(id, window, cx)).ok();
            }
        });

        let key_field = gpui::div().w_full().child(Input::new(&self.key_input).mask_toggle());

        let create_account = descriptor.map(|d| {
            let url = d.api_key_url;
            gpui::div().w_full().child(button("onboarding-create-account").label("Create an account →").ghost().xsmall().on_click(move |_, _, cx| cx.open_url(url)))
        });

        let explainer = gpui::div().w_full().text_sm().text_color(muted).child(
            "fal.ai and OpenRouter give you pay-as-you-go access to AI models. Sign up, add a payment method, and pay per generation. Majik uses your key to call them directly - your billing stays between you and them.",
        );

        v_flex()
            .size_full()
            .items_center()
            .child(gpui::div().flex_1())
            .child(
                v_flex()
                    .gap_6()
                    .items_center()
                    .p_6()
                    .w(px(520.))
                    .max_w_full()
                    .child(
                        v_flex()
                            .gap_5()
                            .items_center()
                            .child(logos)
                            .child(
                                v_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(gpui::div().text_3xl().font_weight(gpui::FontWeight::BOLD).text_center().child("Add your API key"))
                                    .child(gpui::div().text_color(muted).text_center().child("Pick a provider, paste your key, and you're ready to go.")),
                            ),
                    )
                    .child(picker)
                    .when(requires_key, |d| d.child(key_field))
                    .children(create_account)
                    .child(explainer)
                    .when_some(self.status.clone(), |d, msg| d.child(gpui::div().w_full().text_xs().text_color(danger).child(msg))),
            )
            .child(gpui::div().flex_1())
            .child(
                v_flex()
                    .gap_3()
                    .items_center()
                    .p_6()
                    .w(px(320.))
                    .child(
                        button("onboarding-connect-button").label("Connect").primary().large().w_full().disabled(!can_connect).on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                    )
                    .child(button("onboarding-skip-button").label("Skip for now").ghost().text_color(muted).on_click(cx.listener(|this, _, _, cx| this.skip(cx)))),
            )
    }
}

impl Render for OnboardingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: gpui::AnyElement = match self.step {
            Step::Welcome => self.render_welcome(cx).into_any_element(),
            Step::Features => self.render_features(cx).into_any_element(),
            Step::Provider => self.render_provider(window, cx).into_any_element(),
        };
        gpui::div().size_full().bg(cx.theme().background).text_color(cx.theme().foreground).child(content)
    }
}

fn placeholder_for(id: &ProviderId) -> String {
    ProviderRegistry::shared().descriptor(id).map(|d| d.api_key_placeholder.to_string()).unwrap_or_else(|| "API key".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::credentials::ApiKeys;
    use crate::test_support::{env, env_with_keys, TestBackend};
    use gpui::{Focusable as _, TestAppContext};

    fn set_key(view: &gpui::Entity<OnboardingView>, vcx: &mut gpui::VisualTestContext, k: &str) {
        let k = k.to_string();
        view.update_in(vcx, move |v, window, cx| {
            let input = v.key_input.clone();
            input.update(cx, |s, cx| s.set_value(k, window, cx));
        });
    }

    #[gpui::test]
    fn connect_saves_key_and_completes(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        view.update_in(vcx, |v, w, cx| v.select_provider(majik_providers::ProviderId::fal(), w, cx));
        // Clear any pre-seeded key, then set our own.
        vcx.update(|_, cx| crate::state::keys(cx).delete("fal.ai", cx).detach());
        set_key(&view, vcx, "  sk-test-123  ");
        view.update(vcx, |v, cx| v.connect(cx));
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            assert!(cx.global::<Config>().onboarding_completed, "onboarding marked complete");
            assert_eq!(cx.global::<Config>().provider, "fal.ai");
            assert_eq!(crate::state::keys(cx).get("fal.ai").as_deref(), Some("sk-test-123"), "key trimmed + stored");
        });
    }

    #[gpui::test]
    fn connect_with_empty_key_is_noop(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "fal.ai");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        vcx.update(|_, cx| crate::state::keys(cx).delete("fal.ai", cx).detach());
        set_key(&view, vcx, "   ");
        view.update(vcx, |v, cx| v.connect(cx));
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            assert!(!cx.global::<Config>().onboarding_completed, "not completed without a key");
            assert!(crate::state::keys(cx).get("fal.ai").is_none());
        });
    }

    #[gpui::test]
    fn connect_failure_shows_status_and_does_not_complete_onboarding(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        backend.fail_writes(true);
        let _e = env_with_keys(cx, 0, "fal.ai", ApiKeys::new(Box::new(backend)));
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        set_key(&view, vcx, "sk-test-123");
        view.update(vcx, |v, cx| v.connect(cx));
        vcx.run_until_parked();
        view.update(vcx, |v, _| assert!(v.status.as_deref().unwrap_or("").starts_with("Couldn't save key"), "{:?}", v.status));
        vcx.update(|_, cx| {
            assert!(!cx.global::<Config>().onboarding_completed);
            assert!(crate::state::keys(cx).get("fal.ai").is_none(), "failed save leaves no key behind");
        });
    }

    #[gpui::test]
    fn skip_completes_without_key(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "fal.ai");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        vcx.update(|_, cx| crate::state::keys(cx).delete("fal.ai", cx).detach());
        view.update(vcx, |v, cx| v.skip(cx));
        vcx.update(|_, cx| {
            assert!(cx.global::<Config>().onboarding_completed);
            assert!(crate::state::keys(cx).get("fal.ai").is_none(), "skip stores no key");
        });
    }

    #[gpui::test]
    fn selecting_provider_clears_the_key_field(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "fal.ai");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        set_key(&view, vcx, "abc");
        view.update_in(vcx, |v, w, cx| v.select_provider(majik_providers::ProviderId::replicate(), w, cx));
        view.update(vcx, |v, cx| {
            assert_eq!(v.provider, majik_providers::ProviderId::replicate());
            assert!(v.key(cx).is_empty(), "key field cleared on provider switch");
        });
    }

    #[gpui::test]
    fn get_started_then_continue_walk_welcome_features_provider(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        view.read_with(vcx, |v, _| assert!(v.step == Step::Welcome, "a fresh onboarding opens on Welcome"));
        // "Get Started" on the welcome page.
        view.update_in(vcx, |v, w, cx| v.go(Step::Features, w, cx));
        view.read_with(vcx, |v, _| assert!(v.step == Step::Features));
        // "Continue" on the features page.
        view.update_in(vcx, |v, w, cx| v.go(Step::Provider, w, cx));
        vcx.run_until_parked();
        view.update_in(vcx, |v, window, cx| {
            assert!(v.step == Step::Provider);
            assert!(v.key_input.read(cx).focus_handle(cx).is_focused(window), "the key field takes focus so the user can type straight away");
        });
        vcx.update(|_, cx| assert!(!cx.global::<Config>().onboarding_completed, "walking the steps completes nothing on its own"));
    }

    #[gpui::test]
    fn provider_step_starts_on_the_configured_provider(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Replicate");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        view.update_in(vcx, |v, w, cx| v.go(Step::Provider, w, cx));
        view.read_with(vcx, |v, _| assert_eq!(v.provider, majik_providers::ProviderId::replicate()));
    }

    #[gpui::test]
    fn the_features_step_shows_the_telemetry_switches(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        let draw = |vcx: &mut gpui::VisualTestContext| vcx.update(|window, cx| window.draw(cx).clear(cx));
        draw(vcx);
        assert!(vcx.debug_bounds("telemetry-metrics-row").is_none(), "not on Welcome");
        view.update_in(vcx, |v, w, cx| v.go(Step::Features, w, cx));
        draw(vcx);
        assert!(vcx.debug_bounds("telemetry-metrics-row").is_some(), "both switches on the Features step");
        assert!(vcx.debug_bounds("telemetry-diagnostics-row").is_some());
    }

    #[gpui::test]
    fn finishing_onboarding_is_reported_with_the_provider(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (view, vcx) = cx.add_window_view(OnboardingView::new);
        vcx.run_until_parked();
        view.update(vcx, |v, cx| v.skip(cx));
        vcx.run_until_parked();
        let skipped = e.events_named("Onboarding Skipped");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].event_properties["provider"], "Mock");
    }
}
