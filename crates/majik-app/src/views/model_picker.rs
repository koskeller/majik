//! Model picker, a searchable palette: a dialog with a search field on top and one row per model
//! (logo tile, name, maker, description and capability chips), with the current model checked.
//! Typing filters the rows, ↑/↓ move the highlight, Enter picks and Escape closes. Built on
//! gpui-component's `List`, which owns the search input and the `"List"` key context, so the
//! keyboard behaviour is the same as its `Select` / `ComboBox`.

use gpui::{prelude::*, px, App, Entity, ScrollStrategy, Task, WeakEntity, Window};
use gpui_component::list::{List, ListDelegate, ListItem, ListState};
use gpui_component::{h_flex, v_flex, ActiveTheme as _, IndexPath, Selectable, WindowExt as _};
use majik_core::model::MediaType;
use majik_providers::{ImageResolution, ProviderDescriptor, VideoResolution};

use crate::ui::{icon, logo_tile};
use crate::composer_state::ComposeTab;
use crate::views::compose::ComposeView;

#[derive(Clone, Debug)]
pub struct ModelRow {
    pub index: usize,
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
                ModelRow { index: i, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips }
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
                ModelRow { index: i, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips }
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
                ModelRow { index: i, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips }
            })
            .collect(),
        ComposeTab::Media(MediaType::Audio) => provider
            .supported_audio_models
            .iter()
            .enumerate()
            .map(|(i, m)| ModelRow { index: i, name: m.name, manufacturer: m.manufacturer, logo: m.logo, description: m.short_description, chips: Vec::new() })
            .collect(),
    }
}

/// Row height without and with a chips line. Every row gets the same explicit height because the
/// list is virtualised and measures a single row for all of them: a name line plus a description,
/// or plus a chip row when any model in the list has chips.
const ROW_HEIGHT: f32 = 52.;
const ROW_HEIGHT_WITH_CHIPS: f32 = 64.;
/// Air between the cards. The list can't space items itself (it lays them out by the measured
/// size, which excludes margins), so each row carries half the gap above and below its card.
const ROW_GAP: f32 = 6.;

pub struct ModelPickerDelegate {
    compose: WeakEntity<ComposeView>,
    all: Vec<ModelRow>,
    matched: Vec<ModelRow>,
    /// Index into `all` of the model the composer currently uses.
    current: usize,
    selected: Option<IndexPath>,
    /// Whether any row of this tab has chips; decides the (uniform) row height.
    has_chips: bool,
}

impl ModelPickerDelegate {
    fn new(compose: WeakEntity<ComposeView>, all: Vec<ModelRow>, current: usize) -> Self {
        let has_chips = all.iter().any(|row| !row.chips.is_empty());
        Self { compose, matched: all.clone(), all, current, selected: None, has_chips }
    }

    /// Names of the rows currently shown, in order.
    #[cfg(test)]
    pub(crate) fn matched_names(&self) -> Vec<&'static str> {
        self.matched.iter().map(|row| row.name).collect()
    }
}

impl ListDelegate for ModelPickerDelegate {
    type Item = ModelListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matched.len()
    }

    fn perform_search(&mut self, query: &str, _window: &mut Window, _cx: &mut Context<ListState<Self>>) -> Task<()> {
        self.matched = self.all.iter().filter(|row| row.matches(query)).cloned().collect();
        Task::ready(())
    }

    fn render_item(&mut self, ix: IndexPath, _window: &mut Window, _cx: &mut Context<ListState<Self>>) -> Option<Self::Item> {
        let row = self.matched.get(ix.row)?.clone();
        let current = row.index == self.current;
        Some(ModelListItem { base: ListItem::new(ix), row, current, selected: false, has_chips: self.has_chips })
    }

    fn render_empty(&mut self, _window: &mut Window, cx: &mut Context<ListState<Self>>) -> impl IntoElement {
        gpui::div().p_4().text_sm().text_color(cx.theme().muted_foreground).child("No models match")
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _window: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(row) = self.selected.and_then(|ix| self.matched.get(ix.row)) else { return };
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
    has_chips: bool,
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
        let (muted, muted_fg, primary) = (theme.muted, theme.muted_foreground, theme.primary);
        let tile = logo_tile(self.row.logo, self.row.manufacturer, 36., cx);
        let description = self.row.description;
        let mut chips = h_flex().gap_1().flex_nowrap().overflow_hidden();
        for chip in &self.row.chips {
            chips = chips.child(gpui::div().flex_none().px_1p5().py_0p5().rounded_full().bg(muted).text_xs().whitespace_nowrap().text_color(muted_fg).child(chip.clone()));
        }
        let card_height = if self.has_chips { ROW_HEIGHT_WITH_CHIPS } else { ROW_HEIGHT };
        let card = self
            .base
            .h_full()
            .px_2()
            .py_1()
            .rounded_md()
            .overflow_hidden()
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_center()
                    .child(tile)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_baseline()
                                    .child(gpui::div().font_weight(gpui::FontWeight::SEMIBOLD).whitespace_nowrap().child(self.row.name))
                                    .child(gpui::div().text_xs().text_color(muted_fg).whitespace_nowrap().child(self.row.manufacturer)),
                            )
                            .when(!description.is_empty() && description != "TBD", |d| d.child(gpui::div().text_sm().text_color(muted_fg).whitespace_nowrap().text_ellipsis().overflow_hidden().child(description)))
                            .when(!self.row.chips.is_empty(), |d| d.child(chips)),
                    )
                    .when(self.current, |d| d.child(icon("check").size_4().flex_none().text_color(primary))),
            );
        gpui::div().h(px(card_height + ROW_GAP)).py(px(ROW_GAP / 2.)).child(card)
    }
}

/// Opens the picker over the composer with the search field focused and the current model highlighted.
/// Returns the list so tests can inspect it.
pub fn open_model_picker(compose: WeakEntity<ComposeView>, provider: &'static ProviderDescriptor, tab: ComposeTab, current: usize, window: &mut Window, cx: &mut App) -> Entity<ListState<ModelPickerDelegate>> {
    let delegate = ModelPickerDelegate::new(compose, rows(provider, tab), current);
    let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
    list.update(cx, |list, cx| {
        list.set_selected_index(Some(IndexPath::new(current)), window, cx);
        list.scroll_to_item(IndexPath::new(current), ScrollStrategy::Center, window, cx);
    });
    let list_for_dialog = list.clone();
    // No scrollbar: the list's overlay bar rides over the rows (they run to the list's edge) and
    // appears on open, since scrolling to the current model counts as a scroll.
    window.open_dialog(cx, move |dialog, _window, _cx| dialog.title("Choose a model").w(px(560.)).child(List::new(&list_for_dialog).search_placeholder("Search models").scrollbar_visible(false).max_h(px(520.))));
    // `open_dialog` focuses the dialog itself; the search field has to win.
    list.update(cx, |list, cx| list.focus(window, cx));
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env;
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use majik_providers::catalog::image;

    /// Stands in for `LibraryWindow`: the composer plus the dialog layer, inside a `Root`, so the
    /// picker is actually drawn and its key handlers are on the focus path.
    struct Host {
        compose: Entity<ComposeView>,
    }

    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div().size_full().child(self.compose.clone()).children(gpui_component::Root::render_dialog_layer(window, cx))
        }
    }

    fn compose_window(cx: &mut TestAppContext) -> (Entity<ComposeView>, &mut VisualTestContext) {
        env(cx, 1, "Mock");
        let slot: std::rc::Rc<std::cell::RefCell<Option<Entity<ComposeView>>>> = Default::default();
        let slot_for_window = slot.clone();
        let (_root, vcx) = cx.add_window_view(move |window, cx| {
            let compose = cx.new(|cx| ComposeView::new(window, cx));
            *slot_for_window.borrow_mut() = Some(compose.clone());
            let host = cx.new(|_| Host { compose });
            gpui_component::Root::new(gpui::AnyView::from(host), window, cx)
        });
        vcx.run_until_parked();
        let view = slot.borrow().clone().unwrap();
        (view, vcx)
    }

    fn open(view: &Entity<ComposeView>, vcx: &mut VisualTestContext) -> Entity<ListState<ModelPickerDelegate>> {
        let list = view.update_in(vcx, |view, window, cx| {
            let state = view.composer_state();
            let (provider, tab, current) = (state.provider, state.tab, state.model_index());
            open_model_picker(cx.entity().downgrade(), provider, tab, current, window, cx)
        });
        vcx.run_until_parked();
        list
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

    fn dialog_open(vcx: &mut VisualTestContext) -> bool {
        vcx.update(|window, cx| window.has_active_dialog(cx))
    }

    fn image_index(id: &str) -> usize {
        image::ALL.iter().position(|m| m.id == id).expect("model in the Mock catalog")
    }

    #[gpui::test]
    fn opens_with_the_search_focused_and_the_current_model_highlighted(cx: &mut TestAppContext) {
        let (view, vcx) = compose_window(cx);
        let flux_pro = image_index("flux-2-pro");
        select_model(&view, vcx, flux_pro);
        let list = open(&view, vcx);
        assert!(dialog_open(vcx), "the picker is a dialog over the composer");
        assert!(vcx.update(|window, cx| list.read(cx).focus_handle(cx).is_focused(window)), "typing goes straight into the search field");
        assert_eq!(highlighted(&list, vcx), Some(flux_pro));
        assert_eq!(shown(&list, vcx).len(), image::ALL.len(), "every model of the tab is listed before searching");
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
        for name in ["WAN 3.0", "WAN 3.0 Prime", "Gemini Omni Flash 1.1", "HappyHorse 1.1", "Grok Imagine Video 1.5", "MiniMax H3 Max"] {
            assert!(!named(name).chips.is_empty(), "{name} has no chips");
        }

        let image = rows(provider, ComposeTab::Media(MediaType::Image));
        for name in ["Muse Image", "Qwen Image 3", "Qwen Image 3 Pro", "Seedream 5.0 Pro", "Seedream 5.0 Lite", "Grok Imagine Image 2"] {
            assert!(image.iter().any(|r| r.name == name), "{name} missing from the image tab");
        }
    }

    #[test]
    fn matches_is_case_insensitive_over_name_maker_and_description() {
        let row = ModelRow { index: 0, name: "FLUX.2 Pro", manufacturer: "Black Forest Labs", logo: "", description: "Fast photoreal images", chips: Vec::new() };
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
