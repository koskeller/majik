//! Small shared UI helpers.

use gpui::prelude::*;
use gpui::{canvas, div, fill, percentage, px, Animation, AnimationElement, AnimationExt as _, App, Bounds, Canvas, Context, Div, ElementId, Hsla, Interactivity, Pixels, SharedString, Size, Stateful, StyleRefinement, Transformation, WeakEntity, Window};
use gpui_component::button::{Button, Toggle, ToggleGroup, ToggleVariants as _};
use gpui_component::menu::DropdownMenu;
use gpui_component::select::Caret;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, Selectable, Sizable as _, Size as ControlSize, StyledExt as _, Theme, ThemeRegistry};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Majik's own dark palette (`assets/themes/majik.json`): graphite, not black. gpui-component's
/// stock dark theme is shadcn's web palette on a `#0a0a0a` ground; media tools (Lightroom,
/// Ableton, Blender, Unity) sit three to five stops lighter so a dark image keeps its edge and
/// the chrome can step below the canvas; this is Unity's #383838 ground. Keys the file leaves out keep the stock dark values.
const THEME: &str = include_str!("../assets/themes/majik.json");
const DARK_THEME: &str = "Majik Dark";

/// Register the Majik palette and make it the dark theme, leaving the light theme stock. Call
/// once right after `gpui_component::init`, before the first `Theme::change`; any later
/// `Theme::change` / `sync_system_appearance` then resolves dark to it.
pub fn install_theme(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(e) = registry.load_themes_from_str(THEME) {
        tracing::warn!(target: "majik", "loading the Majik theme, keeping the stock dark theme: {e:#}");
        return;
    }
    let Some(dark) = registry.themes().get(DARK_THEME).cloned() else {
        tracing::warn!(target: "majik", "the Majik theme file does not define {DARK_THEME:?}");
        return;
    };
    Theme::global_mut(cx).dark_theme = dark;
    // gpui-component's init already applied a mode; re-apply it so an app that starts dark picks
    // the palette up without waiting for a change of appearance.
    let mode = cx.theme().mode;
    Theme::change(mode, None, cx);
}

/// A dialog is a raised surface like a menu or a popover, so it takes the popover colour.
/// gpui-component paints it on the window ground, which put the model picker on a different grey
/// from the settings menus beside it. It is dismissed by Escape or a click outside, like a menu,
/// so it carries no close button: the one gpui-component draws sat over the top-right corner of
/// the content.
pub trait Raised: Sized {
    fn raised(self, cx: &App) -> Self;
}

impl Raised for gpui_component::dialog::Dialog {
    fn raised(self, cx: &App) -> Self {
        // gpui-component's own backdrop press is dead at this revision: its listener sits on an
        // unsized wrapper that the pointer is never over. This guard fills the dialog's box and
        // turns a press outside it into the Cancel that Escape sends, so a dialog's on-cancel and
        // on-close callbacks run the same way. The title bar's band is left to window dragging.
        let click_out = div().absolute().inset_0().on_mouse_down_out(|event, window, cx| {
            if event.button == gpui::MouseButton::Left && event.position.y >= gpui_component::TITLE_BAR_HEIGHT {
                window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
            }
        });
        self.bg(cx.theme().popover).close_button(false).child(click_out)
    }
}

/// The fill of a chip or a field laid on a surface: a tenth of the foreground, so it stands off
/// the ground it sits on in either mode. The theme's `muted` is the popover colour in dark, and
/// a chip painted with it on a dialog disappears.
pub fn tint(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.1)
}

/// A small control drawn straight on the panel: the composer's setting pickers and toggles. It
/// draws the theme border on the panel, `muted` under the pointer and `accent` while selected or
/// while its menu is open, which is what the model row, the role cards and the segmented tabs
/// around it do. gpui-component's outline `Button` cannot be brought in line: it takes the
/// lighter input border and fill, and it installs its own hover style, which gpui lets no caller
/// replace.
#[derive(IntoElement)]
pub struct Chip {
    base: Stateful<Div>,
    icon: Option<Icon>,
    label: Option<SharedString>,
    caret: bool,
    selected: bool,
    tooltip: Option<SharedString>,
}

pub fn chip(id: impl Into<ElementId>) -> Chip {
    Chip { base: div().id(id), icon: None, label: None, caret: false, selected: false, tooltip: None }
}

impl Chip {
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// End with a drop-down caret, for a chip that opens a menu.
    pub fn caret(mut self) -> Self {
        self.caret = true;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }
}

impl Styled for Chip {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for Chip {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Chip {}

impl Selectable for Chip {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl DropdownMenu for Chip {}

impl RenderOnce for Chip {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, accent, accent_foreground, foreground) = (theme.border, theme.muted, theme.accent, theme.accent_foreground, theme.foreground);
        let mut base = self.base;
        // The caller's own styling wins over the defaults, as on a `Button`.
        let instance_style = base.style().clone();
        let icon_only = self.label.is_none() && !self.caret;
        base.h_6()
            .px_2()
            .when(icon_only, |d| d.w_6().px_0().justify_center())
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .text_sm()
            .flex()
            .flex_row()
            .items_center()
            .cursor_pointer()
            .when(self.selected, |d| d.bg(accent).text_color(accent_foreground))
            .when(!self.selected, |d| d.hover(move |s| s.bg(muted)))
            .refine_style(&instance_style)
            .when_some(self.tooltip, |d, text| d.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx)))
            .children(self.icon.map(|icon| icon.with_size(ControlSize::Small)))
            .children(self.label)
            .when(self.caret, |d| d.child(Caret::new(ControlSize::Small).text_color(foreground.opacity(0.75))))
    }
}

/// HugeIcons icon from the embedded asset bundle (`packaging/icons.json` maps the name to its export).
pub fn icon(name: &'static str) -> Icon {
    Icon::default().path(format!("icons/{name}.svg"))
}

/// The box a ratio glyph sits in, so a column of them lines up whatever their shape.
const RATIO_GLYPH_BOX: Pixels = px(14.);
/// The long edge of a ratio glyph's rectangle.
const RATIO_GLYPH_EDGE: Pixels = px(12.);

/// The shape an aspect ratio makes: a rectangle in its proportion, so 21:9 is visibly flatter than
/// 16:9 and 4:5 nearly square. `None` is "auto", which has no shape and takes the ratio icon.
#[derive(IntoElement)]
pub struct RatioGlyph {
    ratio: Option<(u32, u32)>,
}

pub fn ratio_glyph(ratio: Option<(u32, u32)>) -> RatioGlyph {
    RatioGlyph { ratio }
}

/// Width and height of the rectangle for `width:height`: the long edge is `RATIO_GLYPH_EDGE`, the
/// short one scaled down in proportion.
pub fn ratio_glyph_size((width, height): (u32, u32)) -> (Pixels, Pixels) {
    let (width, height) = (width.max(1) as f32, height.max(1) as f32);
    if width >= height {
        (RATIO_GLYPH_EDGE, RATIO_GLYPH_EDGE * (height / width))
    } else {
        (RATIO_GLYPH_EDGE * (width / height), RATIO_GLYPH_EDGE)
    }
}

impl RenderOnce for RatioGlyph {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let foreground = cx.theme().foreground;
        let shape = match self.ratio {
            Some(ratio) => {
                let (width, height) = ratio_glyph_size(ratio);
                div().w(width).h(height).border_1().border_color(foreground).rounded_xs().into_any_element()
            }
            None => icon("ratio").size(RATIO_GLYPH_BOX).text_color(foreground).into_any_element(),
        };
        div().size(RATIO_GLYPH_BOX).flex_none().flex().items_center().justify_center().child(shape)
    }
}

/// The executor clock: real time in the app, `advance_clock`-driven in tests. Every view-owned
/// motion model samples this so it can be tested headlessly.
pub fn now(cx: &App) -> Instant {
    cx.background_executor().now()
}

/// Indeterminate spinner: a HugeIcons icon turning once per 0.8 s. Phase-locked
/// to the app clock (`repeat_synced`) so every spinner turns in step and a cell that is re-indexed
/// or remounted by `uniform_list` doesn't restart it. Static under reduce-motion.
pub fn spin(icon: Icon) -> AnimationElement<Icon> {
    icon.with_animation("spin", Animation::new(Duration::from_millis(800)).repeat_synced(), |icon, delta| icon.transform(Transformation::rotate(percentage(delta))))
}

/// A window-wide toolbar row with Zed's `Toolbar` proportions: 44 px tall (an eight-unit row inside
/// six units of vertical padding) over a bottom rule, items six units apart, inset like the
/// sidebar's content so the two columns line up.
pub fn toolbar(cx: &App) -> gpui::Div {
    gpui_component::h_flex().h(px(44.)).flex_none().px_3().gap_1p5().items_center().border_b_1().border_color(cx.theme().border)
}

/// The app's button: gpui-component's [`Button`] with the pointing-hand cursor every clickable
/// control shows (Zed's `ButtonLike` does the same; gpui-component keeps the arrow except on link
/// buttons). Use this instead of `Button::new`.
pub fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id).cursor_pointer()
}

/// A single-choice segmented control, Zed's `ToggleButtonGroup`: one outlined run of small toggles
/// with `selected` checked. `on_select` receives the index of the item clicked.
pub fn segmented(id: impl Into<ElementId>, items: impl IntoIterator<Item = (impl Into<ElementId>, impl Into<SharedString>)>, selected: usize, on_select: impl Fn(usize, &mut Window, &mut App) + 'static) -> ToggleGroup {
    let toggles = items.into_iter().enumerate().map(|(index, (id, label))| Toggle::new(id).label(label).px_2().cursor_pointer().checked(index == selected));
    ToggleGroup::new(id).segmented().outline().small().children(toggles).on_click(move |next, window, cx| {
        // The group reports every item's next state; the one that flipped is the click.
        if let Some(index) = next.iter().enumerate().position(|(index, on)| *on != (index == selected)) {
            on_select(index, window, cx);
        }
    })
}

// ----- motion -----------------------------------------------------------------------------------

use gpui_base::motion::{transition, Transition, TransitionId};
use gpui_component::animation::{ease_in_cubic, ease_in_out_cubic, ease_out_cubic, EffectTransition};

/// Ease-in-out — drop-target highlights, card enter/exit.
pub const MOTION_FAST: Duration = Duration::from_millis(150);
/// Ease-in-out — cross-fades.
pub const MOTION_NORMAL: Duration = Duration::from_millis(200);

/// Ease the value keyed by `id` toward `target` (CSS-transition style): retargeting continues from
/// the current value; instant under reduce motion. Call from `render`.
pub fn fade_to(id: impl Into<TransitionId>, target: f32, duration: Duration, window: &mut Window, cx: &mut App) -> f32 {
    transition(id, target, Transition::new(duration).ease(ease_in_out_cubic), window, cx)
}

/// [`fade_to`] for colours.
pub fn color_to(id: impl Into<TransitionId>, target: Hsla, duration: Duration, window: &mut Window, cx: &mut App) -> Hsla {
    transition(id, target, Transition::new(duration).ease(ease_in_out_cubic), window, cx)
}

/// Composer asset card appearing. GPUI has no div transforms, so the card grows from 85 % of
/// `side` while fading in.
pub fn enter_card<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>, side: Pixels) -> AnimationElement<E> {
    EffectTransition::new(MOTION_FAST).ease(ease_out_cubic).fade(0.0, 1.0).width(side * 0.85, side).height(side * 0.85, side).apply(element, id)
}

/// Composer asset card leaving: fades while its width collapses so neighbours slide over.
pub fn exit_card<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>, side: Pixels) -> AnimationElement<E> {
    EffectTransition::new(MOTION_FAST).ease(ease_in_cubic).fade(1.0, 0.0).width(side, px(0.)).apply(element, id)
}

pub fn format_duration(secs: f64) -> String {
    let total = secs.round().max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Height of the badges in a thumbnail's bottom corners (favourite, duration, HD).
pub const THUMB_BADGE_HEIGHT: Pixels = px(18.);

/// A thumbnail's bottom-corner badge strip. The caller picks the corner (`.left_1()` /
/// `.right_1()`, or the half-step insets a small card uses) and fills it with [`thumb_badge`]s:
/// they lay out side by side, so a clip that is also upscaled shows its length and HD next to each
/// other instead of one on top of the other.
pub fn thumb_badges() -> Div {
    gpui_component::h_flex().absolute().bottom_1().gap_1()
}

/// The badges a thumbnail draws in its bottom-right corner, in order: how long a clip or a track
/// runs, then HD when its output came out of an upscaler. An upscaled clip carries both, which is
/// why they sit in a strip rather than in the corner on top of each other. A still has no length
/// to state, whatever its row says.
pub fn duration_badges(duration_secs: Option<f64>, media_type: majik_core::model::MediaType, is_upscaled: bool) -> Vec<SharedString> {
    let mut badges = Vec::new();
    if let Some(secs) = duration_secs.filter(|_| media_type != majik_core::model::MediaType::Image) {
        badges.push(format_duration(secs).into());
    }
    if is_upscaled {
        badges.push("HD".into());
    }
    badges
}

/// A pill in a badge strip: one height and one backdrop for every badge, whether it holds an icon
/// or text. `rounded_full` because a badge is a pill by nature, unlike the `rounded_sm` caption
/// that may sit in the opposite corner.
pub fn thumb_badge() -> Div {
    div()
        .flex_none()
        .h(THUMB_BADGE_HEIGHT)
        .min_w(THUMB_BADGE_HEIGHT)
        .px_1p5()
        .rounded_full()
        .bg(gpui::black().opacity(0.55))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(gpui::white())
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn format_date(ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|d| d.format("%b %-d, %Y %H:%M").to_string())
        .unwrap_or_default()
}

/// Shared measured bounds, written by a `measure` canvas and read on the next render.
pub type BoundsSlot = Rc<RefCell<Bounds<Pixels>>>;

pub fn bounds_slot() -> BoundsSlot {
    Rc::new(RefCell::new(Bounds::default()))
}

pub fn slot_size(slot: &BoundsSlot) -> Size<Pixels> {
    slot.borrow().size
}

/// Invisible full-size canvas that records its bounds and re-renders `owner`
/// when they change. Place it `absolute().inset_0()` inside a `relative` box.
pub fn measure<V: 'static>(slot: BoundsSlot, owner: WeakEntity<V>) -> Canvas<()> {
    measure_then(slot, owner, |_, cx| cx.notify())
}

/// [`measure`] whose owner reacts to the new bounds itself (`on_change` must `cx.notify()` if it
/// wants a redraw). The reaction is deferred to after the frame, because a notify issued while the
/// window is drawing only marks the view for re-render; it doesn't schedule the frame that would
/// show it.
pub fn measure_then<V: 'static>(slot: BoundsSlot, owner: WeakEntity<V>, on_change: impl Fn(&mut V, &mut Context<V>) + 'static) -> Canvas<()> {
    use gpui::Styled as _;
    let on_change = Rc::new(on_change);
    canvas(
        move |bounds: Bounds<Pixels>, _window: &mut Window, cx: &mut App| {
            if *slot.borrow() != bounds {
                *slot.borrow_mut() = bounds;
                let owner = owner.clone();
                let on_change = on_change.clone();
                cx.defer(move |cx| {
                    owner.update(cx, |view, cx| on_change(view, cx)).ok();
                });
            }
        },
        |_, _, _, _| {},
    )
    .size_full()
}

/// Like [`measure`] for many short-lived siblings (feed cells): records the box's bounds under
/// `key` and never re-renders anything. The owner clears the map each frame so it only holds what
/// was last drawn.
pub fn record_bounds<K: std::hash::Hash + Eq + 'static>(map: Rc<RefCell<HashMap<K, Bounds<Pixels>>>>, key: K) -> Canvas<()> {
    use gpui::Styled as _;
    canvas(
        move |bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut App| {
            map.borrow_mut().insert(key, bounds);
        },
        |_, _, _, _| {},
    )
    .size_full()
}

/// flarly's `SectionLabel`: a 12 px medium caption over a composer section with an optional
/// trailing note (a count, a limit) in 11 px muted text.
pub fn section_label(text: impl Into<SharedString>, trailing: Option<SharedString>, cx: &App) -> gpui::Div {
    use gpui::ParentElement as _;
    use gpui::Styled as _;
    use gpui_component::ActiveTheme as _;
    let muted_fg = cx.theme().muted_foreground;
    gpui_component::h_flex()
        .items_baseline()
        .justify_between()
        .gap_2()
        .child(gpui::div().text_xs().font_weight(gpui::FontWeight::MEDIUM).child(text.into()))
        .children(trailing.map(|note| gpui::div().text_size(px(11.)).text_color(muted_fg).child(note)))
}

/// Transparency checkerboard painted with quads.
pub fn checkerboard(a: Hsla, b: Hsla) -> Canvas<()> {
    use gpui::Styled as _;
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, _: (), window: &mut Window, _cx: &mut App| {
            let tile = px(12.);
            window.paint_quad(fill(bounds, a));
            let cols = (bounds.size.width / tile).ceil() as i32;
            let rows = (bounds.size.height / tile).ceil() as i32;
            for row in 0..rows {
                for col in 0..cols {
                    if (row + col) % 2 == 0 {
                        continue;
                    }
                    let origin = gpui::point(bounds.origin.x + tile * col as f32, bounds.origin.y + tile * row as f32);
                    let size = gpui::size(
                        tile.min(bounds.origin.x + bounds.size.width - origin.x),
                        tile.min(bounds.origin.y + bounds.size.height - origin.y),
                    );
                    window.paint_quad(fill(Bounds { origin, size }, b));
                }
            }
        },
    )
    .size_full()
}

// ----- logos ------------------------------------------------------------------------------------

use gpui::{Global, RenderImage};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
struct LogoCache(HashMap<String, Option<Arc<RenderImage>>>);
impl Global for LogoCache {}

/// Rasterize an embedded SVG logo (`assets/logos/<name>.svg`) to a BGRA `RenderImage` at `size` device px.
fn rasterize_svg(bytes: &[u8], size: u32) -> Option<Arc<RenderImage>> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
    let (w, h) = (tree.size().width(), tree.size().height());
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = size as f32 / w.max(h);
    let (pw, ph) = (((w * scale).round() as u32).max(1), ((h * scale).round() as u32).max(1));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pw, ph)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia is premultiplied RGBA; gpui wants straight BGRA.
    let mut data = Vec::with_capacity((pw * ph * 4) as usize);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        data.extend_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
    }
    let buf = image::RgbaImage::from_raw(pw, ph, data)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
}

/// A manufacturer / provider logo by asset name (`logo-google`, `logo-fal`, …). SVGs are rasterized
/// once and cached; PNGs load through the embedded asset source. Returns `None` when unknown.
pub fn logo(name: &str, dark: bool, cx: &mut App) -> Option<gpui::AnyElement> {
    use gpui::Styled as _;
    let svg_path = format!("logos/{name}.svg");
    if let Some(bytes) = crate::assets::Assets::get(&svg_path) {
        if !cx.has_global::<LogoCache>() {
            cx.set_global(LogoCache::default());
        }
        let cached = cx.global::<LogoCache>().0.get(name).cloned();
        let image = match cached {
            Some(v) => v,
            None => {
                let v = rasterize_svg(&bytes.data, 128);
                cx.global_mut::<LogoCache>().0.insert(name.to_string(), v.clone());
                v
            }
        };
        return image.map(|img| gpui::img(img).size_full().object_fit(gpui::ObjectFit::Contain).into_any_element());
    }
    let png = if dark && crate::assets::Assets::get(&format!("logos/{name}-dark.png")).is_some() { format!("logos/{name}-dark.png") } else { format!("logos/{name}.png") };
    if crate::assets::Assets::get(&png).is_some() {
        return Some(gpui::img(gpui::SharedString::from(png)).size_full().object_fit(gpui::ObjectFit::Contain).into_any_element());
    }
    None
}

/// A picture filling its frame edge to edge, cropped to it from the centre. gpui's image element
/// gives itself the picture's aspect ratio when its box is not pinned, so a `size_full()` picture in
/// a square frame came out as tall as the scaled picture and the clipped frame showed its top.
/// Positioned absolutely, the box is the nearest positioned ancestor's, which callers make the frame
/// (`.relative()` on it).
pub fn cover_image(source: impl Into<gpui::ImageSource>) -> gpui::Img {
    gpui::img(source).absolute().inset_0().size_full().object_fit(gpui::ObjectFit::Cover)
}

/// The file an image element may show for an asset: its thumbnail, or, for a still picture whose
/// thumbnail is not made yet, the picture itself. A clip or a sound file is never one: gpui reads
/// what isn't a raster image as an SVG and logs `Usvg(NotAnUtf8Str)` for every frame.
pub fn picture_for(kind: majik_core::model::MediaType, thumbnail: Option<&std::path::Path>, file: &std::path::Path) -> Option<std::path::PathBuf> {
    match (kind, thumbnail) {
        (_, Some(thumbnail)) => Some(thumbnail.to_path_buf()),
        (majik_core::model::MediaType::Image, None) => Some(file.to_path_buf()),
        (majik_core::model::MediaType::Video | majik_core::model::MediaType::Audio, None) => None,
    }
}

/// Rounded logo tile with initials fallback (port of `ModelLogoView`).
pub fn logo_tile(name: &str, fallback_label: &str, size: f32, cx: &mut App) -> gpui::AnyElement {
    use gpui::ParentElement as _;
    use gpui::Styled as _;
    use gpui_component::ActiveTheme as _;
    let dark = cx.theme().mode.is_dark();
    let (bg, fg) = (cx.theme().muted, cx.theme().muted_foreground);
    let tile = gpui::div().w(px(size)).h(px(size)).rounded_md().bg(bg).p(px(size * 0.15)).flex().items_center().justify_center().overflow_hidden();
    match logo(name, dark, cx) {
        Some(el) => tile.child(el).into_any_element(),
        None => {
            let initials: String = fallback_label.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect::<String>().to_uppercase();
            tile.child(gpui::div().text_xs().text_color(fg).child(initials)).into_any_element()
        }
    }
}

// ----- toast ------------------------------------------------------------------------------------

use gpui::{sampled_easing, SpringConfig, Task, WindowId};

/// The save-status toast: a card at the bottom of the window that springs up, stays for exactly
/// 2 s and slides back down. A new toast replaces the current one and restarts the clock.
pub const TOAST_DURATION: Duration = Duration::from_secs(2);
pub const TOAST_EXIT: Duration = Duration::from_millis(200);
/// Spring: response 0.25 s, damping fraction 0.85 (ω₀ = 2π / 0.25 s).
pub const TOAST_SPRING: SpringConfig = SpringConfig::new(631.7, 42.7, 1.0);
/// The rise from the bottom edge.
const TOAST_RISE: Pixels = px(24.);
const TOAST_BOTTOM: Pixels = px(24.);

struct Toast {
    message: SharedString,
    /// Distinguishes this toast from the one it replaced, so an old timer can't dismiss it.
    generation: u64,
    hiding: bool,
    /// Dropping the task (on replace) cancels the dismissal.
    _timer: Task<()>,
}

#[derive(Default)]
struct Toasts {
    by_window: HashMap<WindowId, Toast>,
    generation: u64,
}
impl Global for Toasts {}

/// How many toasts have been shown so far; lets tests assert that exactly one appeared.
#[cfg(test)]
pub(crate) fn toast_generation(cx: &App) -> u64 {
    cx.try_global::<Toasts>().map(|t| t.generation).unwrap_or(0)
}

fn toast_store(cx: &mut App) -> &mut Toasts {
    if !cx.has_global::<Toasts>() {
        cx.set_global(Toasts::default());
    }
    cx.global_mut::<Toasts>()
}

/// Show `message` in this window's toast slot for [`TOAST_DURATION`].
pub fn toast(window: &mut Window, message: impl Into<SharedString>, cx: &mut App) {
    let window_id = window.window_handle().window_id();
    let toasts = toast_store(cx);
    toasts.generation += 1;
    let generation = toasts.generation;
    let timer = window.spawn(cx, async move |cx| {
        cx.background_executor().timer(TOAST_DURATION).await;
        cx.update(|window, cx| {
            if let Some(toast) = toast_store(cx).by_window.get_mut(&window_id).filter(|t| t.generation == generation) {
                toast.hiding = true;
                window.refresh();
            }
        })
        .ok();
        cx.background_executor().timer(TOAST_EXIT).await;
        cx.update(|window, cx| {
            let toasts = toast_store(cx);
            if toasts.by_window.get(&window_id).is_some_and(|t| t.generation == generation) {
                toasts.by_window.remove(&window_id);
                window.refresh();
            }
        })
        .ok();
    });
    toast_store(cx).by_window.insert(window_id, Toast { message: message.into(), generation, hiding: false, _timer: timer });
    window.refresh();
}

/// The toast for this window, if any. Windows render it last, after the dialog and
/// notification layers.
pub fn toast_layer(window: &mut Window, cx: &mut App) -> Option<impl IntoElement> {
    let window_id = window.window_handle().window_id();
    let toast = cx.try_global::<Toasts>()?.by_window.get(&window_id)?;
    let (message, generation, hiding) = (toast.message.clone(), toast.generation, toast.hiding);
    let theme = cx.theme();
    let pill = gpui::div()
        .flex()
        .px(px(14.))
        .py(px(10.))
        .rounded(theme.radius_lg)
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.popover_foreground)
        .child(message);
    let id = SharedString::from(format!("toast-{generation}-{hiding}"));
    let animated = if hiding {
        pill.with_animation(id, Animation::new(TOAST_EXIT).with_easing(gpui::ease_in_out), |pill, t| pill.opacity(1.0 - t).relative().top(TOAST_RISE * t)).into_any_element()
    } else {
        let (duration, easing) = sampled_easing(TOAST_SPRING, 0.01);
        pill.with_animation(id, Animation::new(duration).with_easing(easing), |pill, t| pill.opacity(t.clamp(0.0, 1.0)).relative().top(TOAST_RISE * (1.0 - t))).into_any_element()
    };
    Some(gpui::div().absolute().left_0().right_0().bottom(TOAST_BOTTOM).flex().justify_center().child(animated))
}

#[cfg(test)]
pub fn current_toast(window: &Window, cx: &App) -> Option<(SharedString, bool)> {
    let toast = cx.try_global::<Toasts>()?.by_window.get(&window.window_handle().window_id())?;
    Some((toast.message.clone(), toast.hiding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{CursorStyle, TestAppContext};
    use std::cell::Cell;

    #[test]
    fn duration_badges_state_a_length_for_clips_and_tracks_but_never_stills() {
        use majik_core::model::MediaType;
        assert_eq!(duration_badges(Some(5.0), MediaType::Video, true), vec!["0:05", "HD"], "length first, HD outermost");
        assert_eq!(duration_badges(Some(5.0), MediaType::Video, false), vec!["0:05"]);
        assert_eq!(duration_badges(None, MediaType::Image, true), vec!["HD"]);
        assert_eq!(duration_badges(None, MediaType::Image, false), Vec::<SharedString>::new(), "a plain image gets no strip at all");
        assert_eq!(duration_badges(Some(5.0), MediaType::Image, false), Vec::<SharedString>::new(), "a still has no length to state");
        assert_eq!(duration_badges(Some(75.0), MediaType::Audio, false), vec!["1:15"]);
        assert_eq!(duration_badges(None, MediaType::Video, false), Vec::<SharedString>::new(), "a clip not yet probed says nothing");
    }

    #[test]
    fn only_a_still_picture_stands_in_for_its_own_thumbnail() {
        use majik_core::model::MediaType;
        use std::path::Path;
        let thumb = Path::new("t.jpg");
        assert_eq!(picture_for(MediaType::Image, Some(thumb), Path::new("a.png")).as_deref(), Some(thumb));
        assert_eq!(picture_for(MediaType::Image, None, Path::new("a.png")).as_deref(), Some(Path::new("a.png")));
        assert_eq!(picture_for(MediaType::Video, Some(thumb), Path::new("a.mp4")).as_deref(), Some(thumb));
        assert_eq!(picture_for(MediaType::Video, None, Path::new("a.mp4")), None, "an MP4 is not a picture");
        assert_eq!(picture_for(MediaType::Audio, None, Path::new("a.mp3")), None);
    }

    #[test]
    fn ratio_glyph_keeps_the_long_edge_and_scales_the_short_one() {
        assert_eq!(ratio_glyph_size((1, 1)), (px(12.), px(12.)));
        assert_eq!(ratio_glyph_size((16, 9)), (px(12.), px(6.75)));
        assert_eq!(ratio_glyph_size((9, 16)), (px(6.75), px(12.)), "portrait is the landscape shape turned");
        let (_, wide) = ratio_glyph_size((21, 9));
        let (_, standard) = ratio_glyph_size((4, 3));
        assert!(wide < px(6.75) && px(6.75) < standard, "the flatter the ratio, the shorter the rectangle");
        let (width, height) = ratio_glyph_size((0, 0));
        assert!(width > px(0.) && height > px(0.), "a degenerate ratio still draws something");
    }

    struct Glyphs;

    impl gpui::Render for Glyphs {
        fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
            gpui::div().child(ratio_glyph(Some((16, 9)))).child(ratio_glyph(Some((9, 16)))).child(ratio_glyph(None))
        }
    }

    /// Both shapes and the auto icon draw headlessly, in the theme's foreground.
    #[gpui::test]
    fn ratio_glyphs_render(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            install_theme(cx);
        });
        let (_view, vcx) = cx.add_window_view(|_, _| Glyphs);
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[test]
    fn app_buttons_show_the_pointing_hand() {
        assert_eq!(button("b").style().mouse_cursor, Some(CursorStyle::PointingHand));
        assert_eq!(Button::new("b").style().mouse_cursor, None, "the library default, which the app never uses directly");
    }

    /// Samples `fade_to` on every render so the test can watch a value transition.
    struct Probe {
        target: f32,
        sampled: Rc<Cell<f32>>,
    }

    impl gpui::Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            self.sampled.set(fade_to("probe", self.target, MOTION_NORMAL, window, cx));
            gpui::div()
        }
    }

    #[gpui::test]
    fn fade_to_transitions_and_respects_reduce_motion(cx: &mut TestAppContext) {
        let sampled = Rc::new(Cell::new(-1.0));
        let probe_sampled = sampled.clone();
        let (view, vcx) = cx.add_window_view(|_, _| Probe { target: 1.0, sampled: probe_sampled });
        let redraw = |view: &gpui::Entity<Probe>, vcx: &mut gpui::VisualTestContext, target: f32| {
            view.update(vcx, |p, cx| {
                p.target = target;
                cx.notify();
            });
            vcx.run_until_parked();
        };
        redraw(&view, vcx, 1.0);
        assert_eq!(sampled.get(), 1.0, "first value is adopted immediately");
        redraw(&view, vcx, 0.3);
        assert_eq!(sampled.get(), 1.0, "retarget starts from the current value");
        vcx.background_executor.advance_clock(Duration::from_millis(100));
        redraw(&view, vcx, 0.3);
        let mid = sampled.get();
        assert!(mid > 0.3 && mid < 1.0, "{mid}");
        vcx.background_executor.advance_clock(Duration::from_millis(300));
        redraw(&view, vcx, 0.3);
        assert_eq!(sampled.get(), 0.3);
        vcx.update(|_, cx| cx.set_reduce_motion(true));
        redraw(&view, vcx, 1.0);
        assert_eq!(sampled.get(), 1.0, "reduce motion jumps to the target");
    }

    #[gpui::test]
    fn toast_auto_clears_after_two_seconds_and_restarts_on_replace(cx: &mut TestAppContext) {
        let vcx = cx.add_empty_window();
        let current = |vcx: &mut gpui::VisualTestContext| vcx.update(|window, cx| current_toast(window, cx));
        vcx.update(|window, cx| toast(window, "Saved", cx));
        assert_eq!(current(vcx), Some(("Saved".into(), false)));
        vcx.background_executor.advance_clock(Duration::from_millis(1500));
        vcx.run_until_parked();
        // Replacing restarts the 2 s clock; the old timer must not dismiss the new toast.
        vcx.update(|window, cx| toast(window, "Copied", cx));
        vcx.background_executor.advance_clock(Duration::from_millis(1500));
        vcx.run_until_parked();
        assert_eq!(current(vcx), Some(("Copied".into(), false)));
        vcx.background_executor.advance_clock(Duration::from_millis(600));
        vcx.run_until_parked();
        assert_eq!(current(vcx), Some(("Copied".into(), true)), "sliding out");
        vcx.background_executor.advance_clock(TOAST_EXIT);
        vcx.run_until_parked();
        assert_eq!(current(vcx), None);
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::ThemeMode;

    fn install(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            install_theme(cx);
        });
    }

    /// The point of the palette: a graphite ground instead of shadcn's near-black.
    #[gpui::test]
    fn dark_mode_is_graphite_not_black(cx: &mut TestAppContext) {
        install(cx);
        cx.update(|cx| {
            Theme::change(ThemeMode::Dark, None, cx);
            let theme = cx.theme();
            assert_eq!(theme.background, gpui::rgb(0x383838).into(), "feed ground");
            assert_eq!(theme.sidebar, gpui::rgb(0x2e2e2e).into(), "chrome a step below the feed");
            assert_eq!(theme.foreground, gpui::rgb(0xd6d6d6).into(), "grey text, not white");
            assert_eq!(theme.border, gpui::rgb(0x232323).into(), "seams darker than the surface");
        });
    }

    /// The light theme stays gpui-component's stock one.
    #[gpui::test]
    fn light_mode_is_untouched(cx: &mut TestAppContext) {
        install(cx);
        cx.update(|cx| {
            Theme::change(ThemeMode::Light, None, cx);
            assert_eq!(cx.theme().background, gpui::white(), "stock light ground");
            assert_eq!(cx.theme().dark_theme.name.as_ref(), DARK_THEME, "the dark slot still points at Majik Dark");
        });
    }

    /// Switching modes back and forth keeps resolving dark to the Majik palette.
    #[gpui::test]
    fn dark_palette_survives_a_round_trip_through_light(cx: &mut TestAppContext) {
        install(cx);
        cx.update(|cx| {
            Theme::change(ThemeMode::Light, None, cx);
            Theme::change(ThemeMode::Dark, None, cx);
            assert_eq!(cx.theme().background, gpui::rgb(0x383838).into());
            assert_eq!(cx.theme().theme_name().as_ref(), DARK_THEME);
        });
    }

    /// Every key in the file is one gpui-component knows; a typo would otherwise be dropped
    /// silently by serde and the stock colour would show through.
    #[test]
    fn every_theme_key_is_recognised() {
        let raw: serde_json::Value = serde_json::from_str(THEME).unwrap();
        let file_keys: Vec<String> = raw["themes"][0]["colors"].as_object().unwrap().keys().cloned().collect();
        let parsed: gpui_component::ThemeSet = serde_json::from_str(THEME).unwrap();
        let known = serde_json::to_value(&parsed.themes[0].colors).unwrap();
        let known = known.as_object().unwrap();
        for key in file_keys {
            assert!(known.get(&key).is_some_and(|v| !v.is_null()), "{key} is not a gpui-component theme key");
        }
    }
}
