//! Model picker, a searchable palette: a dialog with a search field on top and one row per model
//! (logo tile, name, maker and capability chips), with the current model checked. Typing filters
//! the rows, ↑/↓ move the highlight, Enter picks and Escape closes. Built on gpui-component's
//! `List`; the search field is our own, drawn as a filled box with no rule under it, and hands
//! the keys the list binds in its `"List"` context back to the list.
//!
//! The models generated with most recently (`Config::recent_models`, per tab) sit in a `Recent`
//! section above `All models`, so alternating between a few is one click; the section goes away
//! while a search is typed, since the matches are the shortcut then.

use gpui::{prelude::*, px, AnyElement, App, Entity, Focusable as _, ScrollStrategy, Task, WeakEntity, Window};
use std::rc::Rc;
use gpui_base::actions::{Confirm, SelectDown, SelectUp};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::{List, ListDelegate, ListItem, ListState};
use crate::ui::Raised as _;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, IndexPath, Selectable, WindowExt as _};
use majik_core::model::{MediaType, ToolId};
use majik_providers::{ImageResolution, ProviderDescriptor, VideoResolution};

use crate::composer_state::ComposeTab;
use crate::config::Config;
use crate::ui::{icon, logo_tile, pill, tint};
use crate::views::compose::ComposeView;

#[derive(Clone, Debug)]
pub struct ModelRow {
    pub index: usize,
    /// The catalog id, what `Config::recent_models` remembers a model by.
    pub id: &'static str,
    pub name: &'static str,
    pub manufacturer: &'static str,
    pub logo: &'static str,
    pub description: &'static str,
    pub chips: Vec<String>,
}

impl ModelRow {
    /// Case-insensitive match: every whitespace-separated term of `query` must occur in the name,
    /// the manufacturer or the description. An empty query matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {} {}", self.name, self.manufacturer, self.description).to_lowercase();
        query.split_whitespace().all(|term| haystack.contains(&term.to_lowercase()))
    }
}

fn image_res_range(res: &[ImageResolution]) -> Option<String> {
    let min = res.iter().min()?;
    let max = res.iter().max()?;
    Some(if min == max { min.raw().to_string() } else { format!("{} - {}", min.raw(), max.raw()) })
}

fn video_res_range(res: &[VideoResolution]) -> Option<String> {
    let min = res.iter().min()?;
    let max = res.iter().max()?;
    Some(if min == max { min.display_name().to_string() } else { format!("{} - {}", min.display_name(), max.display_name()) })
}

/// The rows of `all` that `recent` names, in `recent`'s (newest-first) order. A remembered model
/// the provider doesn't offer is simply not shown.
pub fn recent_rows(all: &[ModelRow], recent: &[String]) -> Vec<ModelRow> {
    recent.iter().filter_map(|id| all.iter().find(|row| row.id == id)).cloned().collect()
}

/// Rows for the given provider / tab, with their capability chips.
pub fn rows(provider: &'static ProviderDescriptor, tab: ComposeTab) -> Vec<ModelRow> {
    match tab {
        // The media chip is what tells the two upscalers apart: picking one is how the Upscale
        // tab switches between taking an image and taking a clip.
        ComposeTab::Tool(tool) => provider
            .tool_models(tool)
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                let mut chips = vec![m.media.label().to_string()];
                if let Some(c) = provider.tool_capabilities(m) {
                    if !c.upscale_factors.is_empty() {
                        chips.push(c.upscale_factors.iter().map(|f| format!("{f}×")).collect::<Vec<_>>().join(" / "));
                    }
                }
                // An upscaler's description only restates the chips beside it, so its rows are
                // the name, the maker and the chips.
                let description = if tool == ToolId::Upscale { "" } else { m.short_description };
                ModelRow { index: i, id: m.id, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description, chips }
            })
            .collect(),
        ComposeTab::Media(MediaType::Image) => provider
            .supported_image_models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut chips = Vec::new();
                if let Some(c) = provider.image_capabilities(m) {
                    if c.max_input_images > 0 {
                        chips.push(format!("{} reference{}", c.max_input_images, if c.max_input_images == 1 { "" } else { "s" }));
                    }
                    if let Some(r) = image_res_range(&c.supported_resolutions) {
                        chips.push(r);
                    }
                }
                ModelRow { index: i, id: m.id, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips }
            })
            .collect(),
        ComposeTab::Media(MediaType::Video) => provider
            .supported_video_models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut chips = Vec::new();
                if let Some(c) = provider.video_capabilities(m) {
                    let d = &c.duration_range;
                    chips.push(if d.min == d.max { format!("{}s", d.min) } else { format!("{} - {}s", d.min, d.max) });
                    if let Some(r) = video_res_range(&c.resolutions) {
                        chips.push(r);
                    }
                    let first = c.asset_constraints.accepts(majik_providers::AssetRole::FirstFrame);
                    let last = c.asset_constraints.accepts(majik_providers::AssetRole::LastFrame);
                    match (first, last) {
                        (true, true) => chips.push("Start / End".into()),
                        (true, false) => chips.push("Start".into()),
                        (false, true) => chips.push("End".into()),
                        _ => {}
                    }
                    if let Some(references) = c.references {
                        chips.push(format!("{} reference{}", references.images, if references.images == 1 { "" } else { "s" }));
                    }
                    // The audio slot of a model with reference audio is a reference list, not the
                    // single conditioning track this chip means.
                    if c.asset_constraints.accepts(majik_providers::AssetRole::Audio) && c.references.map(|r| r.audio).unwrap_or(0) == 0 {
                        chips.push("Audio input".into());
                    }
                    if c.supports_audio {
                        chips.push("Audio".into());
                    }
                }
                ModelRow { index: i, id: m.id, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips }
            })
            .collect(),
        ComposeTab::Media(MediaType::Audio) => provider
            .supported_audio_models
            .iter()
            .enumerate()
            .map(|(i, m)| ModelRow { index: i, id: m.id, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips: Vec::new() })
            .collect(),
    }
}

/// Every row is one line, logo, name, maker and chips, at one height, because the list is
/// virtualised and measures a single row for all of them.
const ROW_HEIGHT: f32 = 40.;
const LOGO_SIZE: f32 = 24.;

pub struct ModelPickerDelegate {
    compose: WeakEntity<ComposeView>,
    all: Vec<ModelRow>,
    matched: Vec<ModelRow>,
    /// The tab's recently used models, newest first; listed in their own section while nothing
    /// is being searched for.
    recent: Vec<ModelRow>,
    searching: bool,
    /// Index into `all` of the model the composer currently uses.
    current: usize,
    selected: Option<IndexPath>,
}

impl ModelPickerDelegate {
    fn new(compose: WeakEntity<ComposeView>, all: Vec<ModelRow>, recent: Vec<ModelRow>, current: usize) -> Self {
        Self { compose, matched: all.clone(), all, recent, searching: false, current, selected: None }
    }

    /// Whether the list opens with the `Recent` section: only while nothing is typed, since the
    /// matches are the shortcut then. With it, the sections are `Recent` then `All models`;
    /// without, the one section is the (filtered) catalog.
    fn shows_recent(&self) -> bool {
        !self.searching && !self.recent.is_empty()
    }

    fn row_at(&self, ix: IndexPath) -> Option<&ModelRow> {
        if self.shows_recent() && ix.section == 0 {
            self.recent.get(ix.row)
        } else {
            self.matched.get(ix.row)
        }
    }

    /// Where the composer's current model is listed: in `Recent` when it is there (the list opens
    /// at the top, both sections in view), otherwise at its place in the catalog.
    fn path_of_current(&self) -> IndexPath {
        if !self.shows_recent() {
            return IndexPath::new(self.current);
        }
        match self.recent.iter().position(|row| row.index == self.current) {
            Some(row) => IndexPath::new(row),
            None => IndexPath::new(self.current).section(1),
        }
    }

    /// Names of the catalog rows currently shown, in order.
    #[cfg(test)]
    pub(crate) fn matched_names(&self) -> Vec<&'static str> {
        self.matched.iter().map(|row| row.name).collect()
    }

    /// Names in the `Recent` section, newest first; empty when the section is not shown.
    #[cfg(test)]
    pub(crate) fn recent_names(&self) -> Vec<&'static str> {
        if self.shows_recent() {
            self.recent.iter().map(|row| row.name).collect()
        } else {
            Vec::new()
        }
    }
}

impl ListDelegate for ModelPickerDelegate {
    type Item = ModelListItem;

    fn sections_count(&self, _cx: &App) -> usize {
        if self.shows_recent() {
            2
        } else {
            1
        }
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        if self.shows_recent() && section == 0 {
            self.recent.len()
        } else {
            self.matched.len()
        }
    }

    fn perform_search(&mut self, query: &str, _window: &mut Window, _cx: &mut Context<ListState<Self>>) -> Task<()> {
        self.searching = !query.trim().is_empty();
        self.matched = self.all.iter().filter(|row| row.matches(query)).cloned().collect();
        Task::ready(())
    }

    fn render_item(&mut self, ix: IndexPath, _window: &mut Window, _cx: &mut Context<ListState<Self>>) -> Option<Self::Item> {
        let row = self.row_at(ix)?.clone();
        let current = row.index == self.current;
        Some(ModelListItem { base: ListItem::new(ix), row, current, selected: false })
    }

    /// Headings only once there are two sections to tell apart. The list measures every header
    /// by the first one, so both are built the same way.
    fn render_section_header(&mut self, section: usize, _window: &mut Window, cx: &mut Context<ListState<Self>>) -> Option<impl IntoElement> {
        if !self.shows_recent() {
            return None;
        }
        let text = if section == 0 { "RECENT" } else { "ALL MODELS" };
        Some(gpui::div().px_2().pt_3().pb_1().text_xs().font_weight(gpui::FontWeight::SEMIBOLD).text_color(cx.theme().muted_foreground).child(text))
    }

    fn render_empty(&mut self, _window: &mut Window, cx: &mut Context<ListState<Self>>) -> impl IntoElement {
        gpui::div().p_4().text_sm().text_color(cx.theme().muted_foreground).child("No models match")
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _window: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(row) = self.selected.and_then(|ix| self.row_at(ix)) else { return };
        let index = row.index;
        self.compose.update(cx, |view, cx| view.select_model(index, cx)).ok();
        window.close_dialog(cx);
    }
}

/// One model row: the list highlights it (`selected`) as the keyboard moves; the composer's current
/// model carries a check mark.
#[derive(IntoElement)]
pub struct ModelListItem {
    base: ListItem,
    row: ModelRow,
    current: bool,
    selected: bool,
}

impl Selectable for ModelListItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self.base = self.base.selected(selected);
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for ModelListItem {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (muted_fg, primary) = (theme.muted_foreground, theme.primary);
        let tile = logo_tile(self.row.logo, self.row.manufacturer, LOGO_SIZE, cx);
        let mut chips = h_flex().min_w_0().gap_1().flex_nowrap().overflow_hidden();
        for chip in &self.row.chips {
            chips = chips.child(pill(chip.clone(), cx));
        }
        self.base.h(px(ROW_HEIGHT)).px_2().py_0().rounded_md().overflow_hidden().child(
            h_flex()
                .w_full()
                .h_full()
                .gap_3()
                .items_center()
                .child(tile)
                .child(gpui::div().flex_none().font_weight(gpui::FontWeight::MEDIUM).whitespace_nowrap().child(self.row.name))
                .child(gpui::div().flex_1().min_w_0().text_sm().text_color(muted_fg).whitespace_nowrap().text_ellipsis().overflow_hidden().child(self.row.manufacturer))
                .when(!self.row.chips.is_empty(), |d| d.child(chips))
                .when(self.current, |d| d.child(icon("check").size_4().flex_none().text_color(primary))),
        )
    }
}

/// What a palette draws besides its search box and list: a line above the list (the voice
/// picker's preview error) and what to do when the dialog is dismissed with Escape, a click
/// outside or the close gesture (a pick closes the dialog itself and is not a dismissal).
#[derive(Default)]
pub struct PaletteExtras {
    pub banner: Option<PaletteBanner>,
    pub on_dismiss: Option<PaletteDismiss>,
}

pub type PaletteBanner = Rc<dyn Fn(&mut Window, &mut App) -> Option<AnyElement>>;
pub type PaletteDismiss = Rc<dyn Fn(&mut Window, &mut App)>;

/// Opens `list` as a palette over the window: a filled search box on top that filters the list,
/// ↑ / ↓ / Enter handed to the list, Escape closing. The search field is focused and returned so
/// tests can type into it. The caller has already highlighted and scrolled to the current row.
pub fn open_palette<D: ListDelegate>(list: Entity<ListState<D>>, placeholder: &str, extras: PaletteExtras, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    let search = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
    // The list's own search field sits on a rule; ours is a filled box, so the list is not
    // searchable and we feed it the query instead.
    let list_for_search = list.clone();
    window
        .subscribe(&search, cx, move |input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                let query = input.read(cx).value().to_string();
                list_for_search.update(cx, |list, cx| list.set_query(&query, window, cx));
            }
        })
        .detach();
    let (list_for_dialog, search_for_dialog) = (list.clone(), search.clone());
    // No scrollbar: the list's overlay bar rides over the rows and appears on open, since
    // scrolling to the current row counts as a scroll.
    window.open_dialog(cx, move |dialog, window, cx| {
        let list = list_for_dialog.clone();
        let (up, down, confirm) = (list.clone(), list.clone(), list.clone());
        // A single-line input registers no handler for ↑ / ↓ and propagates Enter, so the keys
        // fall through to the bindings of the `"List"` context this box takes, and go to the list.
        let search_box = h_flex()
            .key_context("List")
            .on_action(move |action: &SelectUp, window, cx| up.focus_handle(cx).dispatch_action(action, window, cx))
            .on_action(move |action: &SelectDown, window, cx| down.focus_handle(cx).dispatch_action(action, window, cx))
            .on_action(move |action: &Confirm, window, cx| confirm.focus_handle(cx).dispatch_action(action, window, cx))
            .h_9()
            .px_3()
            .gap_2()
            .rounded_md()
            .bg(tint(cx))
            .items_center()
            .child(icon("search").size_4().flex_none().text_color(cx.theme().muted_foreground))
            .child(Input::new(&search_for_dialog).appearance(false).cleanable(true).p_0().flex_1());
        let banner = extras.banner.as_ref().and_then(|banner| banner(window, cx));
        let dialog = dialog.raised(cx).w(px(560.)).child(v_flex().gap_2().child(search_box).children(banner).child(List::new(&list).scrollbar_visible(false).max_h(px(480.))));
        match extras.on_dismiss.clone() {
            Some(on_dismiss) => dialog.on_close(move |_, window, cx| on_dismiss(window, cx)),
            None => dialog,
        }
    });
    // `open_dialog` focuses the dialog itself; the search field has to win.
    search.focus_handle(cx).focus(window, cx);
    search
}

/// Opens the picker over the composer with the search field focused and the current model highlighted.
/// Returns the list and the search field so tests can inspect them.
pub fn open_model_picker(compose: WeakEntity<ComposeView>, provider: &'static ProviderDescriptor, tab: ComposeTab, current: usize, window: &mut Window, cx: &mut App) -> (Entity<ListState<ModelPickerDelegate>>, Entity<InputState>) {
    let all = rows(provider, tab);
    let recent = recent_rows(&all, cx.global::<Config>().recent_models.get(tab));
    let delegate = ModelPickerDelegate::new(compose, all, recent, current);
    let path = delegate.path_of_current();
    let list = cx.new(|cx| ListState::new(delegate, window, cx));
    list.update(cx, |list, cx| {
        list.set_selected_index(Some(path), window, cx);
        list.scroll_to_item(path, ScrollStrategy::Center, window, cx);
    });
    let search = open_palette(list.clone(), "Search models", PaletteExtras::default(), window, cx);
    (list, search)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::update_config;
    use crate::test_support::compose_with_dialogs as compose_window;
    use gpui::{TestAppContext, VisualTestContext};
    use majik_providers::catalog::image;

    fn open(view: &Entity<ComposeView>, vcx: &mut VisualTestContext) -> Entity<ListState<ModelPickerDelegate>> {
        open_with_search(view, vcx).0
    }

    fn open_with_search(view: &Entity<ComposeView>, vcx: &mut VisualTestContext) -> (Entity<ListState<ModelPickerDelegate>>, Entity<InputState>) {
        let opened = view.update_in(vcx, |view, window, cx| {
            let state = view.composer_state();
            let (provider, tab, current) = (state.provider, state.tab, state.model_index());
            open_model_picker(cx.entity().downgrade(), provider, tab, current, window, cx)
        });
        vcx.run_until_parked();
        opened
    }

    fn select_model(view: &Entity<ComposeView>, vcx: &mut VisualTestContext, index: usize) {
        view.update(vcx, |view, cx| view.select_model(index, cx));
    }

    fn model_index(view: &Entity<ComposeView>, vcx: &mut VisualTestContext) -> usize {
        view.read_with(vcx, |view, _| view.composer_state().model_index())
    }

    fn shown(list: &Entity<ListState<ModelPickerDelegate>>, vcx: &mut VisualTestContext) -> Vec<&'static str> {
        list.read_with(vcx, |list, _| list.delegate().matched_names())
    }

    fn highlighted(list: &Entity<ListState<ModelPickerDelegate>>, vcx: &mut VisualTestContext) -> Option<usize> {
        list.read_with(vcx, |list, _| list.selected_index().map(|ix| ix.row))
    }

    /// The highlighted row as `(section, row)`: section 0 is `Recent` while that section is shown.
    fn highlighted_path(list: &Entity<ListState<ModelPickerDelegate>>, vcx: &mut VisualTestContext) -> Option<(usize, usize)> {
        list.read_with(vcx, |list, _| list.selected_index().map(|ix| (ix.section, ix.row)))
    }

    fn recent(list: &Entity<ListState<ModelPickerDelegate>>, vcx: &mut VisualTestContext) -> Vec<&'static str> {
        list.read_with(vcx, |list, _| list.delegate().recent_names())
    }

    /// What the image tab remembers, newest first, the way generating would have left it.
    fn set_recent_images(vcx: &mut VisualTestContext, ids: &[&str]) {
        let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        vcx.update(|_, cx| update_config(cx, |c| c.recent_models.image = ids));
    }

    fn image_name(id: &str) -> &'static str {
        image::ALL[image_index(id)].name
    }

    fn dialog_open(vcx: &mut VisualTestContext) -> bool {
        vcx.update(|window, cx| window.has_active_dialog(cx))
    }

    fn image_index(id: &str) -> usize {
        image::ALL.iter().position(|m| m.id == id).expect("model in the Mock catalog")
    }

    #[gpui::test]
    fn a_click_outside_closes_the_picker_and_keeps_the_model(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let flux_pro = image_index("flux-2-pro");
        select_model(&view, vcx, flux_pro);
        let _list = open(&view, vcx);
        assert!(dialog_open(vcx));
        vcx.simulate_click(gpui::point(px(4.), px(200.)), gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(!dialog_open(vcx), "the picker has no close button; a click on the backdrop dismisses it");
        assert_eq!(model_index(&view, vcx), flux_pro);
    }

    #[gpui::test]
    fn opens_with_the_search_focused_and_the_current_model_highlighted(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let flux_pro = image_index("flux-2-pro");
        select_model(&view, vcx, flux_pro);
        let (list, search) = open_with_search(&view, vcx);
        assert!(dialog_open(vcx), "the picker is a dialog over the composer");
        assert!(vcx.update(|window, cx| search.focus_handle(cx).is_focused(window)), "typing goes straight into the search field");
        assert_eq!(highlighted(&list, vcx), Some(flux_pro));
        assert_eq!(shown(&list, vcx).len(), image::ALL.len(), "every model of the tab is listed before searching");
    }

    #[gpui::test]
    fn without_recents_there_is_one_section(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let list = open(&view, vcx);
        assert!(recent(&list, vcx).is_empty());
        assert_eq!(list.read_with(vcx, |list, cx| list.delegate().sections_count(cx)), 1);
        assert_eq!(highlighted_path(&list, vcx), Some((0, model_index(&view, vcx))));
    }

    #[gpui::test]
    fn recently_used_models_are_listed_first_newest_first(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        set_recent_images(vcx, &["seedream-4.5", "flux-2-pro"]);
        let list = open(&view, vcx);
        assert_eq!(recent(&list, vcx), [image_name("seedream-4.5"), image_name("flux-2-pro")]);
        assert_eq!(list.read_with(vcx, |list, cx| list.delegate().sections_count(cx)), 2, "Recent above All models");
        assert_eq!(shown(&list, vcx).len(), image::ALL.len(), "the catalog below is still complete");
    }

    #[gpui::test]
    fn a_remembered_model_the_provider_does_not_offer_is_skipped(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        set_recent_images(vcx, &["no-such-model", "seedream-4.5"]);
        let list = open(&view, vcx);
        assert_eq!(recent(&list, vcx), [image_name("seedream-4.5")]);
    }

    /// The current model is highlighted where it is nearest the top: in `Recent` when it is
    /// there, so the list opens with both sections in view; otherwise at its place in the catalog.
    #[gpui::test]
    fn the_current_model_is_highlighted_in_recent_when_it_is_there(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        set_recent_images(vcx, &["seedream-4.5", "flux-2-pro"]);
        select_model(&view, vcx, image_index("flux-2-pro"));
        let list = open(&view, vcx);
        assert_eq!(highlighted_path(&list, vcx), Some((0, 1)));
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();

        let gpt = image_index("gpt-5-image");
        select_model(&view, vcx, gpt);
        let list = open(&view, vcx);
        assert_eq!(highlighted_path(&list, vcx), Some((1, gpt)), "not recent: highlighted in the catalog");
    }

    #[gpui::test]
    fn arrow_keys_cross_between_the_sections_and_enter_picks_a_recent_row(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        set_recent_images(vcx, &["seedream-4.5"]);
        select_model(&view, vcx, 0);
        let list = open(&view, vcx);
        assert_eq!(highlighted_path(&list, vcx), Some((1, 0)), "the first catalog row, below the one recent");
        vcx.simulate_keystrokes("up");
        assert_eq!(highlighted_path(&list, vcx), Some((0, 0)), "up from the top of the catalog lands on the last recent");
        vcx.simulate_keystrokes("down");
        assert_eq!(highlighted_path(&list, vcx), Some((1, 0)));
        vcx.simulate_keystrokes("up enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(model_index(&view, vcx), image_index("seedream-4.5"), "a recent row picks its catalog model");
    }

    #[gpui::test]
    fn typing_hides_the_recent_section_and_enter_picks_the_first_match(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        set_recent_images(vcx, &["flux-2-pro"]);
        select_model(&view, vcx, 0);
        let list = open(&view, vcx);
        vcx.simulate_input("seedream 4.5");
        vcx.run_until_parked();
        assert!(recent(&list, vcx).is_empty(), "the matches are the shortcut while searching");
        assert_eq!(shown(&list, vcx), ["Seedream 4.5"]);
        assert_eq!(highlighted_path(&list, vcx), Some((0, 0)), "the first match, now in the only section");

        vcx.simulate_keystrokes("secondary-a backspace");
        vcx.run_until_parked();
        assert_eq!(recent(&list, vcx), [image_name("flux-2-pro")], "clearing the search brings the section back");

        vcx.simulate_input("seedream 4.5");
        vcx.run_until_parked();
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(model_index(&view, vcx), image_index("seedream-4.5"));
    }

    #[gpui::test]
    fn typing_filters_the_rows_and_highlights_the_first_match(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        select_model(&view, vcx, image_index("seedream-4.5"));
        let list = open(&view, vcx);
        vcx.simulate_input("flux");
        vcx.run_until_parked();
        let names = shown(&list, vcx);
        assert!(!names.is_empty() && names.iter().all(|n| n.starts_with("FLUX")), "{names:?}");
        assert_eq!(highlighted(&list, vcx), Some(0), "the highlight moves to the first match");
        assert!(dialog_open(vcx));
    }

    #[gpui::test]
    fn search_matches_the_manufacturer_too(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let list = open(&view, vcx);
        vcx.simulate_input("black forest");
        vcx.run_until_parked();
        let names = shown(&list, vcx);
        assert!(!names.is_empty() && names.iter().all(|n| n.starts_with("FLUX")), "{names:?}");
    }

    #[gpui::test]
    fn clearing_the_search_brings_every_row_back(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let list = open(&view, vcx);
        vcx.simulate_input("gpt");
        vcx.run_until_parked();
        assert!(shown(&list, vcx).len() < image::ALL.len());
        list.update_in(vcx, |list, window, cx| list.set_query("", window, cx));
        vcx.run_until_parked();
        assert_eq!(shown(&list, vcx).len(), image::ALL.len());
    }

    #[gpui::test]
    fn arrow_keys_move_the_highlight_and_enter_picks(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        select_model(&view, vcx, 0);
        let list = open(&view, vcx);
        vcx.simulate_keystrokes("down down");
        assert_eq!(highlighted(&list, vcx), Some(2));
        vcx.simulate_keystrokes("up");
        assert_eq!(highlighted(&list, vcx), Some(1));
        assert_eq!(model_index(&view, vcx), 0, "moving the highlight does not pick yet");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx), "enter closes the picker");
        assert_eq!(model_index(&view, vcx), 1);
    }

    #[gpui::test]
    fn enter_picks_the_highlighted_match_not_its_row_number(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        select_model(&view, vcx, 0);
        let seedream = image_index("seedream-4.5");
        assert!(seedream > 0, "the test needs a model that is not the first row");
        let list = open(&view, vcx);
        // Both terms have to match, which is what narrows this to one row now that the catalog
        // carries Seedream 5.0 Pro and Lite alongside 4.5.
        vcx.simulate_input("seedream 4.5");
        vcx.run_until_parked();
        assert_eq!(shown(&list, vcx), ["Seedream 4.5"]);
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(model_index(&view, vcx), seedream, "the pick maps the filtered row back to the catalog index");
    }

    #[gpui::test]
    fn escape_closes_without_changing_the_model(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let gpt = image_index("gpt-5-image");
        select_model(&view, vcx, gpt);
        let list = open(&view, vcx);
        vcx.simulate_keystrokes("down");
        assert_ne!(highlighted(&list, vcx), Some(gpt));
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx), "escape closes the picker");
        assert_eq!(model_index(&view, vcx), gpt);
    }

    #[gpui::test]
    fn enter_with_no_match_keeps_the_picker_open(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let gpt = image_index("gpt-5-image");
        select_model(&view, vcx, gpt);
        let list = open(&view, vcx);
        vcx.simulate_input("no such model");
        vcx.run_until_parked();
        assert!(shown(&list, vcx).is_empty(), "the empty state is shown");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(dialog_open(vcx), "nothing to pick, nothing to close");
        assert_eq!(model_index(&view, vcx), gpt);
    }

    /// A tool row's first chip is the media it works on. That is the only thing distinguishing the
    /// image upscaler from the video one in the picker, and picking one is how the Upscale tab
    /// switches between taking a picture and taking a clip.
    #[gpui::test]
    fn video_rows_carry_capability_chips_and_tool_rows_carry_their_media(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let provider = view.read_with(vcx, |view, _| view.composer_state().provider);
        let video = rows(provider, ComposeTab::Media(MediaType::Video));
        assert!(video.iter().all(|row| row.chips.iter().any(|c| c.ends_with('s'))), "every video row states its duration: {video:?}");

        let upscalers = rows(provider, ComposeTab::Tool(majik_core::model::ToolId::Upscale));
        assert!(!upscalers.is_empty());
        assert!(upscalers.iter().all(|row| matches!(row.chips.first().map(String::as_str), Some("Image" | "Video"))), "{upscalers:?}");
        assert_eq!(upscalers.iter().filter(|row| row.chips.first().is_some_and(|c| c == "Video")).count(), 1, "one video upscaler: {upscalers:?}");
        // The factors it offers ride along, so the two upscalers are told apart before selecting one.
        assert!(upscalers.iter().all(|row| row.chips.iter().any(|c| c.contains('×'))), "{upscalers:?}");

        let remove_bg = rows(provider, ComposeTab::Tool(majik_core::model::ToolId::RemoveBackground));
        assert!(remove_bg.iter().all(|row| row.chips == vec!["Image".to_string()]), "background removal has no factors: {remove_bg:?}");
    }

    /// Upscalers are picked by their maker and their factors, not by a blurb that repeats the
    /// chips, so their rows carry no description to search. Background removal keeps its own.
    #[gpui::test]
    fn upscale_rows_have_no_description(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let provider = view.read_with(vcx, |view, _| view.composer_state().provider);

        let upscalers = rows(provider, ComposeTab::Tool(ToolId::Upscale));
        assert!(!upscalers.is_empty());
        assert!(upscalers.iter().all(|row| row.description.is_empty()), "{upscalers:?}");

        let remove_bg = rows(provider, ComposeTab::Tool(ToolId::RemoveBackground));
        assert!(remove_bg.iter().all(|row| !row.description.is_empty()), "{remove_bg:?}");
    }

    /// The models added in the 2026-08 catalog sweep have to reach the picker, with the capability
    /// chips their tables declare. A model in the catalog but missing from a provider's tables
    /// renders no chips and produces no request when you hit Generate.
    #[gpui::test]
    fn the_newest_models_are_offered_with_their_chips(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let provider = view.read_with(vcx, |view, _| view.composer_state().provider);

        let video = rows(provider, ComposeTab::Media(MediaType::Video));
        let named = |name: &str| video.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("{name} missing from the video tab"));
        assert!(named("Seedance 2.5").chips.contains(&"4 - 30s".to_string()), "{:?}", named("Seedance 2.5").chips);
        assert!(named("FLUX 3").chips.contains(&"5 - 20s".to_string()));
        assert!(named("MiniMax H3").chips.contains(&"480p - 4K".to_string()), "{:?}", named("MiniMax H3").chips);
        for name in ["WAN 3.0", "WAN 3.0 Prime", "Gemini Omni Flash 1.1", "HappyHorse 1.1", "Grok Imagine Video 1.5", "MiniMax H3 Max", "MiniMax H3 Max Turbo"] {
            assert!(!named(name).chips.is_empty(), "{name} has no chips");
        }

        let image = rows(provider, ComposeTab::Media(MediaType::Image));
        for name in ["Muse Image", "Qwen Image 3", "Qwen Image 3 Pro", "Seedream 5.0 Pro", "Seedream 5.0 Lite", "Grok Imagine Image 2"] {
            assert!(image.iter().any(|r| r.name == name), "{name} missing from the image tab");
        }
    }

    #[test]
    fn matches_is_case_insensitive_over_name_maker_and_description() {
        let row = ModelRow { index: 0, id: "flux-2-pro", name: "FLUX.2 Pro", manufacturer: "Black Forest Labs", logo: "", description: "Fast photoreal images", chips: Vec::new() };
        assert!(row.matches(""));
        assert!(row.matches("flux"));
        assert!(row.matches("forest"));
        assert!(row.matches("photoreal"));
        assert!(row.matches("flux pro"), "every term has to match, in any order");
        assert!(row.matches("pro flux"));
        assert!(!row.matches("flux max"));
        assert!(!row.matches("kling"));
    }
}
