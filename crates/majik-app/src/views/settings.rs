//! The Settings window, laid out the way Zed's `settings_ui` is: a nav pane of pages on the left,
//! the current page's rows on the right — title and description, control at the end. One window
//! (`windows::open_settings`), reached from ⌘, / the menu / the sidebar / the composer's provider
//! menu; error-recovery mode (a generation can't start without a key) lands on Providers with the
//! message as a banner and that provider's key field focused.
//!
//! Providers are laid out like Zed's agent settings: every provider is always listed with its own
//! API key field, link and status, and nothing here picks the active one — that is the composer's
//! provider menu, which offers the providers that have a key (`state::available_providers`).

use gpui::{prelude::*, px, App, AsKeystroke as _, ClickEvent, Context, Entity, FocusHandle, PathPromptOptions, PromptLevel, SharedString, Window};
use gpui_component::button::{ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::kbd::Kbd;
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::sidebar::{Sidebar, SidebarCollapsible, SidebarMenu, SidebarMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Root, Side, Sizable as _, Theme, ThemeMode, TitleBar};
use majik_providers::{ProviderId, ProviderRegistry};

use crate::actions::{CloseWindow, SelectDown, SelectUp, Shortcut};
use crate::config::{update_config, Config};
use crate::state;
use crate::ui::{button, icon, segmented};

const SUPPORT_EMAIL: &str = "hello@trymajik.com";
const NAV_WIDTH: f32 = 200.;

/// `Config::appearance` values in the order the theme control lists them; the last is the default.
const APPEARANCES: [(&str, &str); 3] = [("light", "Light"), ("dark", "Dark"), ("system", "System")];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsPage {
    #[default]
    General,
    Providers,
    Storage,
    Shortcuts,
    About,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 5] = [Self::General, Self::Providers, Self::Storage, Self::Shortcuts, Self::About];

    fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Providers => "Providers",
            Self::Storage => "Storage",
            Self::Shortcuts => "Shortcuts",
            Self::About => "About",
        }
    }

    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::General => "sliders-horizontal",
            Self::Providers => "globe",
            Self::Storage => "folder",
            Self::Shortcuts => "keyboard",
            Self::About => "info",
        }
    }
}

/// What to show when the window comes forward.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsTarget {
    pub page: SettingsPage,
    /// Error-recovery mode (`ProviderConfigurationMode.errorRecovery`): a banner on the Providers
    /// page, cleared once a key is saved.
    pub message: Option<SharedString>,
    /// The provider whose key field to focus on the Providers page.
    pub provider: Option<ProviderId>,
}

impl SettingsTarget {
    pub fn providers() -> Self {
        Self { page: SettingsPage::Providers, ..Default::default() }
    }

    pub fn missing_key(provider: ProviderId, message: impl Into<SharedString>) -> Self {
        Self { page: SettingsPage::Providers, message: Some(message.into()), provider: Some(provider) }
    }
}

pub struct SettingsWindow {
    page: SettingsPage,
    focus: FocusHandle,
    /// The nav pane: ↑ / ↓ step through the pages while it is focused.
    nav_focus: FocusHandle,
    /// One key field per provider that needs a key, in the order the page lists them.
    key_inputs: Vec<(ProviderId, Entity<InputState>)>,
    /// The outcome of the last save / remove, shown under that provider.
    status: Option<(ProviderId, SharedString)>,
    message: Option<SharedString>,
    /// The provider `show` was asked to focus, and what `target` reports back.
    focused_provider: Option<ProviderId>,
    shortcuts: Vec<Shortcut>,
}

impl SettingsWindow {
    pub fn new(target: SettingsTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let key_inputs = ProviderRegistry::shared()
            .user_selectable()
            .into_iter()
            .filter(|d| d.requires_api_key)
            .map(|d| (d.id.clone(), cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(d.api_key_placeholder))))
            .collect();
        window.set_window_title("Settings");
        crate::windows::track_frame(crate::windows::Singleton::Settings, window, cx);
        let mut this = Self {
            page: target.page,
            focus: cx.focus_handle(),
            nav_focus: cx.focus_handle(),
            key_inputs,
            status: None,
            message: None,
            focused_provider: None,
            shortcuts: crate::actions::shortcuts(),
        };
        this.show(target, window, cx);
        this
    }

    /// Re-target the window: switch page, set or clear the recovery banner, and focus the key field
    /// of the provider the user came for when it has none yet (else the nav, so the arrow keys work
    /// right away).
    pub fn show(&mut self, target: SettingsTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.page = target.page;
        self.message = target.message;
        self.focused_provider = target.provider;
        let key_field = match (&self.page, &self.focused_provider) {
            (SettingsPage::Providers, Some(provider)) if state::keys(cx).get(provider.as_str()).is_none() => self.key_input(provider),
            _ => None,
        };
        match key_field {
            Some(input) => input.update(cx, |input, cx| input.focus(window, cx)),
            None => self.nav_focus.focus(window, cx),
        }
        cx.notify();
    }

    /// What the window is currently showing, as a [`SettingsTarget`].
    #[cfg(test)]
    pub fn target(&self) -> SettingsTarget {
        SettingsTarget { page: self.page, message: self.message.clone(), provider: self.focused_provider.clone() }
    }

    fn key_input(&self, provider: &ProviderId) -> Option<Entity<InputState>> {
        self.key_inputs.iter().find(|(id, _)| id == provider).map(|(_, input)| input.clone())
    }

    pub fn select_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            cx.notify();
        }
    }

    fn step_page(&mut self, delta: isize, cx: &mut Context<Self>) {
        let pages = SettingsPage::ALL;
        let index = pages.iter().position(|p| *p == self.page).unwrap_or(0) as isize;
        let next = (index + delta).clamp(0, pages.len() as isize - 1) as usize;
        self.select_page(pages[next], cx);
    }

    fn save_key(&mut self, provider: ProviderId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.key_input(&provider) else { return };
        let key = input.read(cx).value().trim().to_string();
        if key.is_empty() {
            return;
        }
        // The cache holds the key at once (the page shows it configured); the status waits for it
        // to be persisted.
        let save = state::keys(cx).set(provider.as_str(), &key, cx);
        self.status = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = save.await;
            this.update_in(cx, |s, window, cx| {
                match result {
                    Ok(()) => {
                        s.status = Some((provider, "API key saved.".into()));
                        s.message = None;
                        input.update(cx, |i, cx| i.set_value("", window, cx));
                    }
                    Err(e) => s.status = Some((provider, format!("Couldn't save key: {e:#}").into())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn remove_key(&mut self, provider: ProviderId, cx: &mut Context<Self>) {
        let delete = state::keys(cx).delete(provider.as_str(), cx);
        self.status = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = delete.await;
            this.update(cx, |s, cx| {
                s.status = Some(match result {
                    Ok(()) => (provider, "API key removed.".into()),
                    Err(e) => (provider, format!("Couldn't remove key: {e:#}").into()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Debug only: show onboarding again in the Library window; this window closes so it's seen.
    fn reset_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        update_config(cx, |c| c.onboarding_completed = false);
        window.remove_window();
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The title bar is transparent (see `windows::options`): on macOS leave room for the
        // traffic lights, 40px from the top as in Zed (the sidebar's own list adds the first 12).
        let mut menu = SidebarMenu::new().gap_0p5();
        if cfg!(target_os = "macos") {
            menu = menu.pt_7();
        }
        for page in SettingsPage::ALL {
            menu = menu.child(SidebarMenuItem::new(page.title()).icon(icon(page.icon())).active(page == self.page).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| {
                    this.select_page(page, cx);
                    this.nav_focus.focus(window, cx);
                },
            )));
        }
        gpui::div()
            .id("settings-nav")
            .key_context("SettingsNav")
            .track_focus(&self.nav_focus)
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_page(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_page(1, cx)))
            .w(px(NAV_WIDTH))
            .h_full()
            .flex_none()
            .child(
                Sidebar::new("settings-sidebar")
                    .side(Side::Left)
                    .collapsible(SidebarCollapsible::None)
                    .w_full()
                    .h_full()
                    .child(menu),
            )
    }

    fn render_page(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.page {
            SettingsPage::General => self.render_general(cx),
            SettingsPage::Providers => self.render_providers(cx),
            SettingsPage::Storage => self.render_storage(cx),
            SettingsPage::Shortcuts => self.render_shortcuts(cx),
            SettingsPage::About => self.render_about(window, cx),
        };
        v_flex()
            .id("settings-page")
            .flex_1()
            .min_w_0()
            .h_full()
            .child(gpui::div().px_8().pt_6().pb_2().text_lg().font_weight(gpui::FontWeight::SEMIBOLD).child(self.page.title()))
            .child(gpui::div().id("settings-content").debug_selector(|| "settings-content".into()).flex_1().min_h_0().overflow_y_scrollbar().child(content.pb_8()))
    }

    fn render_general(&self, cx: &mut Context<Self>) -> gpui::Div {
        let appearance = cx.global::<Config>().appearance.clone();
        let selected = APPEARANCES.iter().position(|(id, _)| *id == appearance).unwrap_or(APPEARANCES.len() - 1);
        let theme = segmented("theme", APPEARANCES, selected, |index, window, cx| {
            let Some((id, _)) = APPEARANCES.get(index) else { return };
            set_appearance(id, window, cx);
        });
        let motion = Switch::new("reduce-motion").cursor_pointer().checked(cx.reduce_motion()).on_click(|on: &bool, _, cx| set_reduce_motion(*on, cx));
        v_flex()
            .child(section("Appearance", cx))
            .child(row("theme", "Theme", Some("Light, dark, or follow the system setting."), theme, cx))
            .child(section("Motion", cx))
            .child(row("reduce-motion", "Reduce motion", Some("Skip animations and transitions across the app."), motion, cx))
            .when(cfg!(debug_assertions), |this| {
                this.child(section("Debug", cx)).child(row(
                    "reset-onboarding",
                    "Reset Onboarding",
                    Some("Show the onboarding flow again in the Library window."),
                    button("reset-onboarding").label("Reset Onboarding").danger().outline().small().on_click(cx.listener(|this, _, window, cx| this.reset_onboarding(window, cx))),
                    cx,
                ))
            })
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted_fg, success, warning, secondary, radius) = (theme.muted_foreground, theme.success, theme.warning, theme.secondary, theme.radius);
        let banner = self.message.clone().map(|message| {
            h_flex()
                .mx_8()
                .mt_4()
                .gap_2()
                .items_center()
                .p_3()
                .rounded(radius)
                .bg(secondary)
                .child(icon("triangle-alert").size_4().text_color(warning))
                .child(gpui::div().text_sm().child(message))
        });

        let mut page = v_flex().children(banner);
        for descriptor in ProviderRegistry::shared().user_selectable() {
            let id = descriptor.id.clone();
            let name = descriptor.display_name;
            let heading = h_flex().gap_2().items_center().child(crate::ui::logo_tile(descriptor.logo_asset_name, name, 20., cx)).child(name);
            page = page.child(section_with(heading, cx));

            if !descriptor.requires_api_key {
                page = page.child(row(SharedString::from(format!("{id}-key")), "API key", Some("No API key needed."), gpui::div(), cx));
                continue;
            }
            // The key field is the whole row (no title beside it): the section heading above already
            // says whose key it is, and the field wants the width.
            let configured = state::keys(cx).get(id.as_str()).is_some();
            let field: gpui::AnyElement = if configured {
                let remove_id = id.clone();
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(icon("circle-check").size_4().text_color(success))
                    .child(gpui::div().flex_1().min_w_0().text_sm().child("Configured"))
                    .child(button(SharedString::from(format!("{id}-remove-key"))).label("Remove").danger().outline().small().on_click(cx.listener(move |this, _, _, cx| this.remove_key(remove_id.clone(), cx))))
                    .into_any_element()
            } else {
                let save_id = id.clone();
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .children(self.key_input(&id).map(|input| gpui::div().flex_1().min_w_0().child(Input::new(&input).mask_toggle())))
                    .child(button(SharedString::from(format!("{id}-save-key"))).label("Save").primary().small().on_click(cx.listener(move |this, _, window, cx| this.save_key(save_id.clone(), window, cx))))
                    .into_any_element()
            };
            let key_url = descriptor.api_key_url;
            let mut links = h_flex().gap_2().child(button(SharedString::from(format!("{id}-get-key"))).label("Get a key").icon(icon("external-link")).ghost().small().on_click(move |_, _, cx| cx.open_url(key_url)));
            if let Some(billing_url) = descriptor.billing_url {
                links = links.child(button(SharedString::from(format!("{id}-billing"))).label("Billing").icon(icon("external-link")).ghost().small().on_click(move |_, _, cx| cx.open_url(billing_url)));
            }
            let status = self.status.as_ref().filter(|(provider, _)| *provider == id).map(|(_, text)| text.clone());
            page = page
                .child(control_row(SharedString::from(format!("{id}-key")), field, cx))
                .child(row(SharedString::from(format!("{id}-account")), descriptor.api_key_instructions, None::<SharedString>, links, cx))
                .when_some(status, |this, status| this.child(gpui::div().px_8().pt_3().text_xs().text_color(muted_fg).child(status)));
        }
        page
    }

    fn render_storage(&self, cx: &mut Context<Self>) -> gpui::Div {
        // The live root plus the configured one when it differs (applies on next launch).
        let live_root = state::library(cx).read(cx).lib.root().display().to_string();
        let configured_root = cx.global::<Config>().library_root.clone();
        let folder: SharedString = match configured_root {
            Some(p) if p != live_root => format!("{p}  (next launch)").into(),
            _ => live_root.into(),
        };
        v_flex()
            .child(section("Library", cx))
            .child(row("library-folder", "Library folder", Some(folder), button("change-library").label("Change…").small().outline().on_click(|_, window, cx| choose_library_folder(window, cx)), cx))
            .child(section("Data", cx))
            .child(row(
                "delete-all",
                "Delete all items",
                Some("Permanently deletes every generated item from this device."),
                button("delete-all").label("Delete All Items").danger().outline().small().on_click(|_, window, cx| confirm_delete_all(window, cx)),
                cx,
            ))
    }

    fn render_shortcuts(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut page = v_flex();
        let mut current_group = None;
        for shortcut in &self.shortcuts {
            if current_group != Some(shortcut.group) {
                current_group = Some(shortcut.group);
                page = page.child(section(shortcut.group, cx));
            }
            // The same key bound in several contexts is one chip.
            let mut keys: Vec<gpui::Keystroke> = Vec::new();
            for binding in &shortcut.bindings {
                for key in binding.keystrokes() {
                    if !keys.contains(key.as_keystroke()) {
                        keys.push(key.as_keystroke().clone());
                    }
                }
            }
            let chips = h_flex().gap_1().children(keys.into_iter().map(Kbd::new));
            page = page.child(compact_row(SharedString::from(format!("shortcut-{}-{}", shortcut.group, shortcut.label)), shortcut.label, chips, cx));
        }
        page
    }

    fn render_about(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let muted_fg = cx.theme().muted_foreground;
        // The channel too, so a screenshot says which of the two installs it came from.
        let version = match crate::config::channel() {
            crate::config::Channel::Stable => SharedString::from(env!("CARGO_PKG_VERSION")),
            crate::config::Channel::Dev => SharedString::from(concat!(env!("CARGO_PKG_VERSION"), " · dev")),
        };
        let version = gpui::div().text_sm().text_color(muted_fg).child(version);
        v_flex()
            .child(section(crate::config::app_name(), cx))
            .child(row("version", "Version", Some("Made with ❤️ in Warsaw"), version, cx))
            .child(section("Help", cx))
            .child(row(
                "contact-support",
                "Contact Support",
                Some("Questions or trouble? Send us an email."),
                button("contact-support").label("Email Support").icon(icon("external-link")).ghost().small().on_click(|_, _, cx| send_email("Majik Support", cx)),
                cx,
            ))
            .child(row(
                "share-feedback",
                "Share Feedback",
                Some("Tell us what to improve."),
                button("share-feedback").label("Email Feedback").icon(icon("external-link")).ghost().small().on_click(|_, _, cx| send_email("Majik Feedback", cx)),
                cx,
            ))
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .id("settings-window")
            .key_context("Settings")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .child(
                v_flex()
                    .size_full()
                    // macOS shows only the traffic lights over the nav pane; elsewhere the window has
                    // no system title bar, so draw one with the window controls (as Zed does).
                    .when(!cfg!(target_os = "macos"), |this| this.child(TitleBar::new()))
                    .child(h_flex().flex_1().min_h_0().w_full().items_start().child(self.render_nav(cx)).child(self.render_page(window, cx))),
            )
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
            .children(crate::ui::toast_layer(window, cx))
    }
}

/// A page section: a muted heading over a rule, like Zed's `SettingsSectionHeader`.
fn section(label: &'static str, cx: &App) -> gpui::Div {
    section_with(label, cx)
}

/// A [`section`] whose heading is an element (a provider's logo and name).
fn section_with(heading: impl IntoElement, cx: &App) -> gpui::Div {
    let theme = cx.theme();
    v_flex().px_8().pt_6().pb_1().gap_1p5().child(gpui::div().text_xs().text_color(theme.muted_foreground).child(heading)).child(gpui::div().h(px(1.)).bg(theme.border))
}

/// One setting: title and description on the left, the control at the end (Zed's setting-item
/// layout). The divider is inset like the section rule above it.
fn row(id: impl Into<SharedString>, title: impl IntoElement, description: Option<impl Into<SharedString>>, control: impl IntoElement, cx: &App) -> gpui::Stateful<gpui::Div> {
    row_inner(id, title, description.map(Into::into), control, false, cx)
}

/// A row that is only its control, spanning the width — the provider key field and its Save button.
fn control_row(id: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let id = id.into();
    let selector = id.clone();
    gpui::div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .px_8()
        .child(h_flex().py_4().items_center().border_b_1().border_color(theme.border).child(control))
}

/// A [`row`] without a description, packed tighter — for tables such as the shortcut list.
fn compact_row(id: impl Into<SharedString>, title: impl IntoElement, control: impl IntoElement, cx: &App) -> gpui::Stateful<gpui::Div> {
    row_inner(id, title, None, control, true, cx)
}

fn row_inner(id: impl Into<SharedString>, title: impl IntoElement, description: Option<SharedString>, control: impl IntoElement, compact: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let id = id.into();
    let selector = id.clone();
    gpui::div().id(id).debug_selector(move || selector.to_string()).px_8().child(
        h_flex()
            .map(|this| if compact { this.py_2p5() } else { this.py_4() })
            .gap_4()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(gpui::div().text_sm().child(title))
                    .when_some(description, |this, description| this.child(gpui::div().text_xs().text_color(theme.muted_foreground).child(description))),
            )
            .child(gpui::div().flex_none().child(control)),
    )
}

/// Persist the appearance and apply it to every window. `Theme::change` only refreshes the window
/// it is given; without the loop the others keep their old colours until something else redraws
/// them (closing Settings did, which is how the bug showed).
pub fn set_appearance(appearance: &'static str, window: &mut Window, cx: &mut App) {
    match appearance {
        "light" => Theme::change(ThemeMode::Light, Some(window), cx),
        "dark" => Theme::change(ThemeMode::Dark, Some(window), cx),
        _ => Theme::sync_system_appearance(Some(window), cx),
    }
    update_config(cx, |c| c.appearance = appearance.into());
    // The calling window is mid-dispatch (its update fails) and was refreshed above; every other
    // window takes its refresh here.
    for handle in cx.windows() {
        handle.update(cx, |_, window, _| window.refresh()).ok();
    }
}

/// Persist the preference and apply it app-wide; every animation reads `cx.reduce_motion()`.
pub fn set_reduce_motion(on: bool, cx: &mut App) {
    update_config(cx, |c| c.reduce_motion = on);
    cx.set_reduce_motion(on);
}

/// Folder chooser → `Config.library_root`; the library itself is opened at launch (see `main.rs`).
fn choose_library_folder(window: &mut Window, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Choose".into()) });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = rx.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let root = path.display().to_string();
        cx.update(|cx| update_config(cx, |c| c.library_root = Some(root)));
        handle.update(cx, |_, window, cx| crate::ui::toast(window, "Library folder saved. Majik will use it on the next launch.", cx)).ok();
    })
    .detach();
}

fn confirm_delete_all(window: &mut Window, cx: &mut App) {
    let answer = window.prompt(
        PromptLevel::Critical,
        "Delete All Items",
        Some("Are you sure you want to delete all generated items? Their files stay in the library as assets."),
        &["Delete All", "Cancel"],
        cx,
    );
    let library = state::library(cx);
    cx.spawn(async move |cx| {
        if answer.await != Ok(0) {
            return;
        }
        cx.update(|cx| {
            library.update(cx, |m, cx| {
                let ids: Vec<_> = m.lib.generations().iter().map(|i| i.id.clone()).collect();
                if !ids.is_empty() {
                    m.delete(&ids, cx);
                }
            })
        });
    })
    .detach();
}

fn send_email(subject: &str, cx: &mut App) {
    cx.open_url(&format!("mailto:{SUPPORT_EMAIL}?subject={}", subject.replace(' ', "%20")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::ApiKeys;
    use crate::test_support::{env, env_with_keys, TestBackend};
    use crate::views::library_window::LibraryWindow;
    use crate::windows::{open_settings, Windows};
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};

    /// Open (or re-target) the Settings window the way the app does and return its view.
    fn open(cx: &mut TestAppContext, target: SettingsTarget) -> (Entity<SettingsWindow>, &mut VisualTestContext) {
        // `open_settings` defers; the deferred work runs when this update's effects flush.
        cx.update(|cx| open_settings(target, cx));
        let handle = cx.update(|cx| cx.global::<Windows>().settings.expect("settings window opened"));
        let root_view = cx.update(|cx| handle.update(cx, |root, _, _| root.view().clone()).unwrap());
        let Ok(view) = root_view.downcast::<SettingsWindow>() else { panic!("the settings window's root is a SettingsWindow") };
        let vcx = VisualTestContext::from_window(handle.into(), cx).into_mut();
        vcx.run_until_parked();
        (view, vcx)
    }

    fn draw(vcx: &mut VisualTestContext) {
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn forget_mock_key(cx: &mut TestAppContext) {
        cx.update(|cx| state::keys(cx).delete("Mock", cx).detach());
        cx.run_until_parked();
    }

    /// Type `key` into `provider`'s field and press Save.
    fn save_key(view: &Entity<SettingsWindow>, vcx: &mut VisualTestContext, provider: ProviderId, key: &str) {
        let key = key.to_string();
        view.update_in(vcx, move |s, window, cx| {
            let input = s.key_input(&provider).expect("a key field for the provider");
            input.update(cx, |i, cx| i.set_value(key, window, cx));
            s.save_key(provider, window, cx);
        });
        vcx.run_until_parked();
    }

    fn status_of(view: &Entity<SettingsWindow>, vcx: &mut VisualTestContext, provider: &str) -> Option<String> {
        view.read_with(vcx, |s, _| s.status.as_ref().filter(|(id, _)| id.as_str() == provider).map(|(_, text)| text.to_string()))
    }

    fn key_value(view: &Entity<SettingsWindow>, vcx: &mut VisualTestContext, provider: ProviderId) -> String {
        view.read_with(vcx, |s, cx| s.key_input(&provider).expect("a key field").read(cx).value().to_string())
    }

    #[gpui::test]
    fn open_settings_is_a_singleton_that_retargets(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget::default());
        assert_eq!(view.read_with(vcx, |s, _| s.page), SettingsPage::General);
        let (again, vcx) = open(cx, SettingsTarget::providers());
        assert_eq!(again.entity_id(), view.entity_id(), "same view");
        assert_eq!(vcx.windows().len(), 1, "one settings window");
        assert_eq!(view.read_with(vcx, |s, _| s.page), SettingsPage::Providers);
    }

    #[gpui::test]
    fn cmd_comma_in_the_library_window_opens_settings(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().onboarding_completed = true);
        let (_library, vcx) = cx.add_window_view(LibraryWindow::new);
        draw(vcx);
        assert!(vcx.update(|_, cx| cx.global::<Windows>().settings.is_none()));
        vcx.simulate_keystrokes("secondary-,");
        vcx.run_until_parked();
        assert!(vcx.update(|_, cx| cx.global::<Windows>().settings.is_some()), "settings window opened");
        assert_eq!(vcx.windows().len(), 2);
    }

    #[gpui::test]
    fn escape_and_cmd_w_close_the_window(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (_view, vcx) = open(cx, SettingsTarget::default());
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(vcx.windows().is_empty(), "closed with Escape");
        let (_view, vcx) = open(cx, SettingsTarget::default());
        vcx.simulate_keystrokes("secondary-w");
        vcx.run_until_parked();
        assert!(vcx.windows().is_empty(), "closed with ⌘W");
    }

    #[gpui::test]
    fn arrow_keys_step_through_the_pages_while_the_nav_is_focused(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget::default());
        let page = |vcx: &mut VisualTestContext| view.read_with(vcx, |s, _| s.page);
        vcx.simulate_keystrokes("down");
        assert_eq!(page(vcx), SettingsPage::Providers);
        vcx.simulate_keystrokes("down down");
        assert_eq!(page(vcx), SettingsPage::Shortcuts);
        vcx.simulate_keystrokes("down down");
        assert_eq!(page(vcx), SettingsPage::About, "clamped at the last page");
        vcx.simulate_keystrokes("up up up up up up");
        assert_eq!(page(vcx), SettingsPage::General, "clamped at the first page");
    }

    #[gpui::test]
    fn every_page_renders(cx: &mut TestAppContext) {
        let _e = env(cx, 1, "Mock");
        let (view, vcx) = open(cx, SettingsTarget::default());
        for page in SettingsPage::ALL {
            view.update(vcx, |s, cx| s.select_page(page, cx));
            draw(vcx);
            assert!(vcx.debug_bounds("settings-content").is_some(), "{page:?} laid out");
        }
    }

    #[gpui::test]
    fn shortcuts_page_lists_every_binding_group(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget { page: SettingsPage::Shortcuts, ..Default::default() });
        draw(vcx);
        view.read_with(vcx, |s, _| {
            let groups: Vec<&str> = s.shortcuts.iter().map(|shortcut| shortcut.group).collect();
            for group in ["Application", "Feed", "Feed & Detail", "Detail", "Composer", "Settings"] {
                assert!(groups.contains(&group), "{group} listed");
            }
            assert!(s.shortcuts.iter().all(|shortcut| !shortcut.bindings.is_empty()), "every row has a key");
        });
    }

    /// The page is generated from `actions::shortcuts()`, so a new binding shows up by being in that
    /// table — this pins the newest one (and its group) so a stray edit can't drop it.
    #[gpui::test]
    fn shortcuts_page_lists_the_composer_prompt_shortcuts(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget { page: SettingsPage::Shortcuts, ..Default::default() });
        draw(vcx);
        view.read_with(vcx, |s, _| {
            for (label, key) in [("Improve Prompt", "shift-i"), ("Generate", "enter"), ("Paste Image", "shift-v")] {
                let row = s.shortcuts.iter().find(|shortcut| shortcut.label == label).unwrap_or_else(|| panic!("{label} is listed"));
                assert_eq!(row.group, "Composer", "{label} sits with the composer's shortcuts");
                let keys: Vec<String> =
                    row.bindings.iter().flat_map(|b| b.keystrokes().iter().map(|k| k.as_keystroke().unparse())).collect();
                assert!(keys.iter().any(|k| k.contains(key)), "{label} shows {key}: {keys:?}");
            }
        });
    }

    #[gpui::test]
    fn missing_key_lands_on_providers_with_the_banner_and_that_providers_key_field_focused(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        forget_mock_key(cx);
        let (view, vcx) = open(cx, SettingsTarget::missing_key(ProviderId::mock(), "Please configure your Mock API key to generate images."));
        draw(vcx);
        view.read_with(vcx, |s, _| {
            assert_eq!(s.page, SettingsPage::Providers);
            assert_eq!(s.message.as_deref(), Some("Please configure your Mock API key to generate images."));
        });
        let focused = |vcx: &mut VisualTestContext, provider: ProviderId| vcx.update(|window, cx| view.read(cx).key_input(&provider).unwrap().read(cx).focus_handle(cx).is_focused(window));
        assert!(focused(vcx, ProviderId::mock()), "Mock's key field focused for typing");
        assert!(!focused(vcx, ProviderId::replicate()));
        // Saving the key clears the banner.
        save_key(&view, vcx, ProviderId::mock(), "secret");
        view.read_with(vcx, |s, _| assert!(s.message.is_none(), "banner cleared once the key is saved"));
    }

    #[gpui::test]
    fn missing_key_for_a_provider_that_already_has_one_focuses_the_nav(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget::missing_key(ProviderId::mock(), "stale"));
        draw(vcx);
        let nav_focused = vcx.update(|window, cx| view.read(cx).nav_focus.is_focused(window));
        assert!(nav_focused, "nothing to type: the key is already there");
    }

    #[gpui::test]
    fn providers_page_lists_every_provider_with_its_own_key_field(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget::providers());
        draw(vcx);
        let listed: Vec<String> = view.read_with(vcx, |s, _| s.key_inputs.iter().map(|(id, _)| id.to_string()).collect());
        let expected: Vec<String> = ProviderRegistry::shared().user_selectable().iter().filter(|d| d.requires_api_key).map(|d| d.id.to_string()).collect();
        assert_eq!(listed, expected, "one key field per provider, in page order");
        assert!(listed.len() >= 3, "fal.ai, Replicate and OpenRouter are always there: {listed:?}");
        // Every provider's row is on the page at once, whichever one the composer uses.
        for id in &listed {
            let selector = |suffix: &str| -> &'static str { Box::leak(format!("{id}-{suffix}").into_boxed_str()) };
            assert!(vcx.debug_bounds(selector("key")).is_some(), "{id}'s key row laid out");
            assert!(vcx.debug_bounds(selector("account")).is_some(), "{id}'s account row laid out");
        }
    }

    #[gpui::test]
    fn saving_one_providers_key_leaves_the_others_alone(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| {
            state::keys(cx).delete("Replicate", cx).detach();
            state::keys(cx).delete("fal.ai", cx).detach();
        });
        cx.run_until_parked();
        let (view, vcx) = open(cx, SettingsTarget::providers());
        save_key(&view, vcx, ProviderId::replicate(), "r-secret");
        vcx.update(|_, cx| {
            assert_eq!(state::keys(cx).get("Replicate").as_deref(), Some("r-secret"));
            assert!(state::keys(cx).get("fal.ai").is_none(), "fal.ai still has no key");
            assert_eq!(state::keys(cx).get("Mock").as_deref(), Some("k"), "Mock's key untouched");
            assert_eq!(cx.global::<Config>().provider, "Mock", "Settings never changes the composer's pick");
        });
        assert_eq!(status_of(&view, vcx, "Replicate").as_deref(), Some("API key saved."));
        assert_eq!(status_of(&view, vcx, "fal.ai"), None, "the status belongs to the provider that was saved");
    }

    /// A window whose root counts its renders, to see whether a theme change reaches it.
    struct RenderCounter(std::rc::Rc<std::cell::Cell<usize>>);

    impl Render for RenderCounter {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.set(self.0.get() + 1);
            gpui::div()
        }
    }

    #[gpui::test]
    fn appearance_change_redraws_every_window(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let renders = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = renders.clone();
        let (_other, other_cx) = cx.add_window_view(|_, _| RenderCounter(counted));
        other_cx.run_until_parked();
        let before = renders.get();
        let (_view, vcx) = open(cx, SettingsTarget::default());
        vcx.update(|window, cx| set_appearance("dark", window, cx));
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            assert!(cx.theme().mode.is_dark());
            assert_eq!(cx.global::<Config>().appearance, "dark");
        });
        assert!(renders.get() > before, "the other window re-rendered with the new theme");
        vcx.update(|window, cx| set_appearance("light", window, cx));
        vcx.update(|_, cx| assert!(!cx.theme().mode.is_dark()));
    }

    #[gpui::test]
    fn reduce_motion_toggle_updates_app_flag(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| {
            assert!(!cx.reduce_motion());
            set_reduce_motion(true, cx);
            assert!(cx.reduce_motion());
            assert!(cx.global::<Config>().reduce_motion);
            set_reduce_motion(false, cx);
            assert!(!cx.reduce_motion());
        });
    }

    #[gpui::test]
    fn save_and_remove_key(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        forget_mock_key(cx);
        let (view, vcx) = open(cx, SettingsTarget::providers());
        save_key(&view, vcx, ProviderId::mock(), "secret");
        assert_eq!(status_of(&view, vcx, "Mock").as_deref(), Some("API key saved."));
        assert_eq!(key_value(&view, vcx, ProviderId::mock()), "", "input cleared after saving");
        vcx.update(|_, cx| assert_eq!(state::keys(cx).get("Mock").as_deref(), Some("secret")));
        view.update(vcx, |s, cx| s.remove_key(ProviderId::mock(), cx));
        vcx.run_until_parked();
        assert_eq!(status_of(&view, vcx, "Mock").as_deref(), Some("API key removed."));
        vcx.update(|_, cx| assert!(state::keys(cx).get("Mock").is_none()));
    }

    #[gpui::test]
    fn save_key_failure_shows_status_and_keeps_no_key(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        backend.fail_writes(true);
        let _e = env_with_keys(cx, 0, "Mock", ApiKeys::new(Box::new(backend)));
        let (view, vcx) = open(cx, SettingsTarget::providers());
        save_key(&view, vcx, ProviderId::mock(), "secret");
        let status = status_of(&view, vcx, "Mock").unwrap_or_default();
        assert!(status.starts_with("Couldn't save key"), "{status:?}");
        assert_eq!(key_value(&view, vcx, ProviderId::mock()), "secret", "input kept so the user can retry");
        vcx.update(|_, cx| assert!(state::keys(cx).get("Mock").is_none()));
    }

    #[gpui::test]
    fn remove_key_failure_keeps_key(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("Mock", "k")]);
        let _e = env_with_keys(cx, 0, "Mock", ApiKeys::new(Box::new(backend.clone())));
        cx.update(|cx| state::keys(cx).load(cx).detach());
        cx.run_until_parked();
        backend.fail_writes(true);
        let (view, vcx) = open(cx, SettingsTarget::providers());
        view.update(vcx, |s, cx| s.remove_key(ProviderId::mock(), cx));
        vcx.run_until_parked();
        let status = status_of(&view, vcx, "Mock").unwrap_or_default();
        assert!(status.starts_with("Couldn't remove key"), "{status:?}");
        vcx.update(|_, cx| assert_eq!(state::keys(cx).get("Mock").as_deref(), Some("k")));
    }

    #[gpui::test]
    fn closing_saves_the_settings_frame(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (_view, vcx) = open(cx, SettingsTarget::default());
        assert!(vcx.simulate_close(), "close proceeds");
        vcx.update(|_, cx| assert!(cx.global::<Config>().settings_frame.is_some()));
    }

    #[gpui::test]
    fn reset_onboarding_sits_on_the_general_page(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = open(cx, SettingsTarget::default());
        draw(vcx);
        assert!(vcx.debug_bounds("reset-onboarding").is_some(), "the debug row is on General");
        view.update(vcx, |s, cx| s.select_page(SettingsPage::About, cx));
        draw(vcx);
        assert!(vcx.debug_bounds("reset-onboarding").is_none(), "and nowhere else");
    }

    #[gpui::test]
    fn reset_onboarding_closes_settings_and_clears_the_flag(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().onboarding_completed = true);
        let (view, vcx) = open(cx, SettingsTarget::default());
        view.update_in(vcx, |s, window, cx| s.reset_onboarding(window, cx));
        vcx.run_until_parked();
        assert!(vcx.windows().is_empty(), "settings closed so onboarding is seen");
        cx.update(|cx| assert!(!cx.global::<Config>().onboarding_completed));
    }
}
