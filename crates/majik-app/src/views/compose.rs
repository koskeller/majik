//! Composer panel (the right-hand side of the Library window): media type, model, options, input
//! assets, prompt, Generate — a [`crate::composer_state::ComposerState`] on top of
//! provider descriptors.

use gpui::{prelude::*, px, relative, App, Context, DragMoveEvent, Entity, EventEmitter, ExternalPaths, FocusHandle, ObjectFit, PathPromptOptions, Pixels, SharedString, Task, WeakEntity, Window};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _};
use majik_core::model::{AlbumId, AssetId, MediaType, ToolId};
use majik_generation::{build_requests, improve, validate_requests, validation, AssetInput, Request};
use majik_providers::{AspectRatio, AssetRole, AudioVoice, ImageResolution, ProviderDescriptor, ProviderId, VideoAspectRatio, VideoResolution};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::actions::{ClearPrompt, FocusFeed, Generate, ImprovePrompt, PasteImage};
use crate::composer_state::{unsupported_message, ComposeTab, ComposerState, DraftAsset, RecreateOutcome, MAX_COUNT};
use crate::config::{update_config, Config};
use crate::drafts::Drafts;
use crate::state::{self, DraggedAssets, LibraryModel, PendingCompose};
use crate::ui::{button, color_to, enter_card, exit_card, icon, now, section_label, segmented, spin, MOTION_FAST};

/// Asset cards are `ASSET_CARD` square.
const ASSET_CARD: Pixels = px(84.);
/// Most images one request may ask for.
/// The prompt box is at least this tall (about eight lines); it grows into whatever height the
/// panel has left and scrolls inside past that.
const PROMPT_HEIGHT: Pixels = px(180.);
/// How long a removed card keeps shrinking before it is dropped (matches `ui::exit_card`).
const CARD_EXIT: Duration = crate::ui::MOTION_FAST;

enum ExitKind {
    /// A removed asset, still drawn at its old position.
    Thumb { asset: DraftAsset, index: usize },
    /// The dashed picker card of a role that just became full.
    Picker(AssetRole),
}

/// A card still shrinking away.
struct ExitingCard {
    kind: ExitKind,
    started: Instant,
    /// Fires `prune_exits`; dropped with the card.
    _timer: Task<()>,
}

fn thumb_key(asset: &AssetId) -> SharedString {
    format!("asset-{asset}").into()
}

fn picker_key(role: AssetRole) -> SharedString {
    format!("asset-add-{}", role.raw()).into()
}

pub struct ComposeView {
    state: ComposerState,
    exiting: Vec<ExitingCard>,
    /// Cards added since the panel was created; only these play the enter transition.
    entering: HashSet<SharedString>,
    /// The asset role whose picker card an external drag is currently over.
    drop_target: Option<AssetRole>,
    /// The album new generations go into: the sidebar's selection, kept current by the owner.
    album: Option<AlbumId>,
    prompt: Entity<TextareaState>,
    /// The rewrite in flight, if any. Dropping the task cancels it and unfreezes the panel.
    improving: Option<Task<()>>,
    focus: FocusHandle,
    library: Entity<LibraryModel>,
    drafts: Drafts,
}

pub enum ComposeEvent {}
impl EventEmitter<ComposeEvent> for ComposeView {}

impl ComposeView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let provider = state::selected_provider(cx);
        let drafts = Drafts::load();
        let state = ComposerState::new(provider, &drafts.get(provider.id.as_str()));
        let draft = cx.global::<Config>().draft_prompt.clone();
        let dialogue = state.tab == ComposeTab::Media(MediaType::Audio) && state.audio.speaker2.is_some();
        let prompt = cx.new(|cx| TextareaState::new(window, cx).placeholder(placeholder(state.tab, dialogue)).default_value(draft));
        cx.subscribe_in(&prompt, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { secondary: true, .. } => this.generate(window, cx),
            InputEvent::Change => {
                let text = this.prompt.read(cx).value().to_string();
                update_config(cx, |c| c.draft_prompt = text);
                cx.notify();
            }
            _ => {}
        })
        .detach();
        Self {
            state,
            exiting: Vec::new(),
            entering: HashSet::new(),
            drop_target: None,
            album: None,
            prompt,
            improving: None,
            focus: cx.focus_handle(),
            library: state::library(cx),
            drafts,
        }
    }

    fn save_draft(&mut self) {
        self.drafts.set(self.state.provider.id.as_str(), self.state.to_draft());
    }

    /// The composer's provider menu: pick one of the providers that can run right now. Persisted
    /// app-wide (`Config::provider`) so tools in the feed and detail go to the same provider.
    pub fn select_provider(&mut self, id: ProviderId, window: &mut Window, cx: &mut Context<Self>) {
        update_config(cx, |c| c.provider = id.0.clone());
        self.sync_provider(window, cx);
    }

    /// Re-read the selected provider (after the menu, or Settings adding / removing a key, changed
    /// it). Drafts are per provider; input assets are not, so they carry over.
    fn sync_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let p = state::selected_provider(cx);
        if p.id != self.state.provider.id {
            // The rewrite in flight was for the old provider's model.
            self.improving = None;
            self.state.set_provider(p, &self.drafts.get(p.id.as_str()));
            self.reset_transients();
            self.refresh_placeholder(window, cx);
            cx.notify();
        }
    }

    /// Drop in-flight card animations and drag state: they index into a tab list that was just
    /// swapped out from under them.
    fn reset_transients(&mut self) {
        self.exiting.clear();
        self.drop_target = None;
    }

    fn refresh_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialogue = self.state.tab == ComposeTab::Media(MediaType::Audio) && self.state.audio.speaker2.is_some();
        let text = placeholder(self.state.tab, dialogue);
        self.prompt.update(cx, |s, cx| s.set_placeholder(text, window, cx));
    }

    // ----- external requests -------------------------------------------------------

    /// Put keyboard focus in the panel: the prompt, or the panel itself on a tool tab (which has no
    /// prompt box to focus — focus on an element that isn't drawn is dropped), so Escape and the
    /// ⌘N cycle behave the same on every tab.
    pub fn focus_prompt(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.takes_prompt() {
            self.prompt.update(cx, |s, cx| s.focus(window, cx));
        } else {
            window.focus(&self.focus, cx);
        }
    }

    /// Whether keyboard focus is anywhere inside the panel (the prompt, a picker button, …).
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    #[cfg(test)]
    pub(crate) fn prompt_text(&self, cx: &App) -> String {
        self.prompt.read(cx).value().to_string()
    }

    /// The album the sidebar has selected (`None` for Library / Favorites / a tool).
    /// The active tab's draft, for the window's end-to-end tests.
    #[cfg(test)]
    pub(crate) fn draft_assets(&self) -> Vec<DraftAsset> {
        self.state.active_assets().to_vec()
    }

    pub fn set_album(&mut self, album: Option<AlbumId>, cx: &mut Context<Self>) {
        if self.album != album {
            self.album = album;
            cx.notify();
        }
    }

    /// Consume a request coming from the feed / detail view.
    pub fn apply(&mut self, pending: PendingCompose, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_provider(window, cx);
        if let Some(id) = pending.recreate {
            // The row's stored request and the assets it referenced, read here rather than passed
            // in, because the composer is what knows how to interpret them.
            let (request, inputs) = {
                let library = self.library.read(cx);
                let request = library.lib.get(&id).and_then(|item| item.request_json.as_deref()).and_then(Request::from_json);
                let inputs = library.linked_inputs(&id).into_iter().map(|(role, asset)| DraftAsset { asset: asset.id, role }).collect();
                (request, inputs)
            };
            match request {
                Some(request) => self.load_recreate(request, inputs, window, cx),
                None => crate::ui::toast(window, "This was made with a model or setting not available in this version.", cx),
            }
        }
        self.focus_prompt(window, cx);
        cx.notify();
    }

    /// Port of `loadRecreateSettings`: adopt the request's tab, its stored inputs and its prompt —
    /// or change nothing when this provider can't make that kind of media. A tool request has no
    /// prompt, so what is typed stays.
    fn load_recreate(&mut self, request: Request, inputs: Vec<DraftAsset>, window: &mut Window, cx: &mut Context<Self>) {
        let provider_name = self.state.provider.display_name;
        match self.state.load_recreate(&request, inputs) {
            RecreateOutcome::Loaded { state, warning } => {
                self.state = *state;
                self.reset_transients();
                self.refresh_placeholder(window, cx);
                if request.generation_type.takes_prompt() {
                    self.prompt.update(cx, |s, cx| s.set_value(request.prompt, window, cx));
                }
                if let Some(warning) = warning {
                    crate::ui::toast(window, warning.message(provider_name), cx);
                }
            }
            RecreateOutcome::Unsupported(tab) => crate::ui::toast(window, unsupported_message(provider_name, tab), cx),
        }
    }

    /// A drag from a grid dropped on `role`'s card: every dragged asset of a kind the role takes.
    fn add_dragged(&mut self, role: AssetRole, dragged: &DraggedAssets, cx: &mut Context<Self>) {
        self.drop_target = None;
        for asset in dragged.assets.iter().filter(|a| role.accepts_kind(a.kind)) {
            self.add_asset(asset.id.clone(), role, cx);
        }
    }

    /// A drag from a grid dropped on the panel itself rather than a card: images go to the first
    /// free image role (as "Use Image" does), audio to the audio role.
    fn drop_on_panel(&mut self, dragged: &DraggedAssets, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_target = None;
        for asset in &dragged.assets {
            match asset.kind {
                MediaType::Audio => {
                    self.add_as(asset.id.clone(), AssetRole::Audio, window, cx);
                }
                MediaType::Image => {
                    self.add_image(asset.id.clone(), window, cx);
                }
                MediaType::Video => {
                    self.add_video(asset.id.clone(), window, cx);
                }
            }
        }
    }

    /// Attach `asset` in `role`, moving to the tab that owns the role when the current one has no
    /// such slot (frames and audio belong to video, the rest to image). The user is told when the
    /// model there doesn't take the role or has no room left.
    fn add_as(&mut self, asset: AssetId, role: AssetRole, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.state.asset_constraints().accepts(role) {
            let tab = match role {
                AssetRole::FirstFrame | AssetRole::LastFrame | AssetRole::Audio | AssetRole::ReferenceVideo => MediaType::Video,
                _ => MediaType::Image,
            };
            self.set_media_type(tab, window, cx);
        }
        let name = role.display_name().to_lowercase();
        if !self.state.asset_constraints().accepts(role) {
            crate::ui::toast(window, format!("The selected model doesn't take a {name} input."), cx);
            return false;
        }
        if self.state.role_is_full(role) {
            crate::ui::toast(window, format!("No room for another {name} input."), cx);
            return false;
        }
        self.add_asset(asset, role, cx)
    }

    /// Paste and a drop on the panel: the current tab's first free image slot
    /// (`firstAvailableImageRole`), falling back to the Image tab; a toast when nothing has room.
    fn add_image(&mut self, asset: AssetId, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut role = self.state.first_available_image_role();
        if role.is_none() {
            self.set_media_type(MediaType::Image, window, cx);
            role = self.state.first_available_image_role();
        }
        match role {
            Some(role) => self.add_asset(asset, role, cx),
            None => {
                crate::ui::toast(window, "No room for another input image.", cx);
                false
            }
        }
    }

    /// A dropped video: the reference video slot of the video tab, the only role that takes one.
    fn add_video(&mut self, asset: AssetId, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.state.first_available_video_role().is_none() {
            self.set_media_type(MediaType::Video, window, cx);
        }
        match self.state.first_available_video_role() {
            Some(role) => self.add_asset(asset, role, cx),
            None if self.state.has_frames() => {
                crate::ui::toast(window, "Remove the start or end frame to use references instead.", cx);
                false
            }
            None => {
                crate::ui::toast(window, "This model doesn't take a video reference.", cx);
                false
            }
        }
    }

    pub fn set_voice(&mut self, speaker: crate::views::voice_picker::Speaker, voice: Option<AudioVoice>, window: &mut Window, cx: &mut Context<Self>) {
        match speaker {
            crate::views::voice_picker::Speaker::One => {
                if voice.is_some() {
                    self.state.audio.speaker1 = voice;
                }
            }
            crate::views::voice_picker::Speaker::Two => self.state.audio.speaker2 = voice,
        }
        self.refresh_placeholder(window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn composer_state(&self) -> &ComposerState {
        &self.state
    }

    pub fn select_model(&mut self, ix: usize, cx: &mut Context<Self>) {
        // A rewrite is written for one model; a different one gets a fresh ask.
        self.improving = None;
        self.state.select_model(ix);
        cx.notify();
    }

    fn set_tab(&mut self, tab: ComposeTab, window: &mut Window, cx: &mut Context<Self>) {
        let changed = self.state.tab != tab;
        if changed {
            self.improving = None;
        }
        if !self.state.set_tab(tab) {
            return;
        }
        if changed {
            self.reset_transients();
        }
        self.refresh_placeholder(window, cx);
        cx.notify();
    }

    fn set_media_type(&mut self, t: MediaType, window: &mut Window, cx: &mut Context<Self>) {
        self.set_tab(ComposeTab::Media(t), window, cx);
    }

    /// Tool tabs have no prompt: the images are the whole input.
    fn takes_prompt(&self) -> bool {
        !self.state.tab.is_tool()
    }

    // ----- prompt / generate -------------------------------------------------------

    /// The selected model's prompt cap; `None` when it declares none, in which case nothing is
    /// limited here and the provider decides.
    fn prompt_limit(&self) -> Option<usize> {
        let generation_type = self.state.generation_type()?;
        validation::prompt_character_limit(&generation_type, self.state.provider).ok().flatten()
    }

    /// What the user typed, trimmed. The only prompt this panel holds.
    fn prompt_value(&self, cx: &App) -> String {
        self.prompt.read(cx).value().trim().to_string()
    }

    /// How many items Generate will make. Only assets the model accepts can stand in for a prompt;
    /// on a tool tab every image becomes an item.
    fn total(&self, cx: &App) -> usize {
        if !self.takes_prompt() {
            return self.state.accepted_assets().len();
        }
        if self.prompt_value(cx).is_empty() && !(self.state.prompt_optional() && !self.state.accepted_assets().is_empty()) {
            return 0;
        }
        self.state.count()
    }

    /// The estimate under the Generate button: what pressing it right now would cost.
    ///
    /// `None` draws no line at all. That happens only on an audio tab with nothing typed, where
    /// the cost depends entirely on the text's length, so there is nothing to say yet.
    fn cost_caption(&self, cx: &App) -> Option<SharedString> {
        let characters = self.prompt_value(cx).chars().count();
        let total = self.total(cx);
        if self.state.tab == ComposeTab::Media(MediaType::Audio) && characters == 0 {
            return None;
        }
        let Some(amount) = self.state.unit_price(characters).times(total.max(1)).amount() else {
            return Some("No estimate available".into());
        };
        if amount.is_zero() {
            return Some("Free".into());
        }
        // Before there is anything to generate, the number is still worth showing: it is what the
        // settings the user is turning cost per output.
        Some(if total == 0 { format!("≈ {amount} each").into() } else { format!("≈ {amount} estimated").into() })
    }

    fn can_generate(&self, cx: &App) -> bool {
        if !self.takes_prompt() {
            return self.total(cx) > 0;
        }
        let within_limit = match self.prompt_limit() {
            Some(limit) => self.prompt_value(cx).chars().count() <= limit,
            None => true,
        };
        self.total(cx) > 0 && within_limit && self.state.generation_type().is_some()
    }

    /// Whether an accepted card has no file behind it any more (see `generate`).
    fn missing_input(&self, cx: &App) -> bool {
        let library = self.library.read(cx);
        self.state.accepted_assets().iter().any(|a| library.lib.asset(&a.asset).is_none_or(|asset| asset.missing))
    }

    /// The accepted assets' bytes for the provider. An asset whose file is gone is left out (the
    /// row still references it, so Retry picks it up once the file is back).
    fn load_asset_inputs(&self, cx: &App) -> Vec<AssetInput> {
        let library = self.library.read(cx);
        self.state.accepted_assets().into_iter().filter_map(|a| library.asset_input(library.lib.asset(&a.asset)?, a.role)).collect()
    }

    /// Whether the Improve button is drawn at all: a media tab with a prompt, on a provider that
    /// routes a text model. (It is then disabled until something is typed.)
    fn can_improve(&self) -> bool {
        matches!(self.state.tab, ComposeTab::Media(MediaType::Image) | ComposeTab::Media(MediaType::Video)) && self.state.provider.supports_prompt_improvement()
    }

    /// The button and ⌘⇧I: start a rewrite, or cancel the one running.
    pub(crate) fn toggle_improve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.improving.is_some() {
            self.cancel_improve(cx);
        } else {
            self.improve(window, cx);
        }
    }

    /// Drop the in-flight rewrite. The prompt is untouched, since it was never modified, and the
    /// panel unfreezes on the next render.
    fn cancel_improve(&mut self, cx: &mut Context<Self>) {
        if self.improving.take().is_some() {
            cx.notify();
        }
    }

    /// Rewrite the prompt with the provider's text model, then put the result in the field as one
    /// undoable edit (⌘Z restores what the user wrote).
    fn improve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_provider(window, cx);
        if !self.can_improve() {
            return;
        }
        let prompt = self.prompt_value(cx);
        if prompt.is_empty() {
            return;
        }
        if self.state.provider.requires_api_key && state::keys(cx).get(self.state.provider.id.as_str()).is_none() {
            let message = format!("Please configure your {} API key to improve prompts.", self.state.provider.display_name);
            crate::windows::open_settings(crate::views::settings::SettingsTarget::missing_key(self.state.provider.id.clone(), message), cx);
            return;
        }
        let Some(generation_type) = self.state.generation_type() else {
            crate::ui::toast(window, "Pick a model first.", cx);
            return;
        };
        let roles: Vec<AssetRole> = self.state.accepted_assets().iter().map(|a| a.role).collect();
        let request = improve::text_request(&prompt, &generation_type, self.state.provider, &roles);
        let receiver = self.library.read(cx).improve_prompt(request);
        self.improving = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = receiver.recv().await;
            this.update_in(cx, |view, window, cx| {
                view.improving = None;
                match outcome {
                    Ok(Ok(text)) => view.apply_improved(text, window, cx),
                    Ok(Err(e)) => crate::ui::toast(window, format!("Couldn't improve the prompt: {e}"), cx),
                    // The runner answered nothing (it has no engine); nothing to show the user.
                    Err(e) => tracing::warn!(target: "majik", "the prompt rewrite reported nothing: {e:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Put the rewrite in the prompt. `replace_all` records it as one undo transaction — unlike
    /// `set_value`, which clears the history — so ⌘Z gives the user their own words back.
    fn apply_improved(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim();
        if text.is_empty() {
            crate::ui::toast(window, "The model returned an empty prompt.", cx);
            return;
        }
        let text = text.to_string();
        self.prompt.update(cx, |state, cx| state.replace_all(text, window, cx));
        self.focus_prompt(window, cx);
    }

    pub(crate) fn generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Submitting supersedes a rewrite of the prompt being submitted.
        self.improving = None;
        self.sync_provider(window, cx);
        if self.state.provider.requires_api_key && state::keys(cx).get(self.state.provider.id.as_str()).is_none() {
            let message = format!("Please configure your {} API key to generate {}.", self.state.provider.display_name, plural(self.state.tab));
            crate::windows::open_settings(crate::views::settings::SettingsTarget::missing_key(self.state.provider.id.clone(), message), cx);
            return;
        }
        if let ComposeTab::Tool(tool) = self.state.tab {
            self.run_tool(tool, window, cx);
            return;
        }
        let Some(gt) = self.state.generation_type() else {
            crate::ui::toast(window, "Pick a model first.", cx);
            return;
        };
        // Hidden roles aren't sent, but too many of an accepted role is refused here rather than
        // as a failed row from the provider.
        let roles: Vec<AssetRole> = self.state.accepted_assets().iter().map(|a| a.role).collect();
        if let Err(e) = self.state.asset_constraints().validate(&roles) {
            crate::ui::toast(window, e.to_string(), cx);
            return;
        }
        // A card whose file is gone (a recreated row's input, deleted behind the app) counts
        // above but would be left out of the request: say so rather than send something else.
        if self.missing_input(cx) {
            crate::ui::toast(window, "An input's file is no longer available. Remove it or add it again.", cx);
            return;
        }
        let text = self.prompt.read(cx).value().to_string();
        let assets = self.load_asset_inputs(cx);
        let inputs: Vec<(AssetId, AssetRole)> = self.state.accepted_assets().iter().map(|a| (a.asset.clone(), a.role)).collect();
        let requests = build_requests(&text, &assets, gt, self.state.provider, self.state.count());
        if requests.is_empty() {
            crate::ui::toast(window, "Write a prompt first.", cx);
            return;
        }
        if let Err(e) = validate_requests(&requests, self.state.provider) {
            crate::ui::toast(window, e.to_string(), cx);
            return;
        }
        let total = requests.len();
        let album = self.album.clone();
        self.library.update(cx, |m, cx| {
            m.generate(requests, &inputs, album, cx);
        });
        self.prompt.update(cx, |s, cx| s.set_value("", window, cx));
        update_config(cx, |c| c.draft_prompt.clear());
        self.state.clear_active_assets();
        crate::ui::toast(window, format!("Generating {total} item(s) with {}…", self.state.provider.display_name), cx);
        cx.notify();
    }

    /// Submit on a tool tab: one row per attached image with the tab's selected model. The images
    /// stay attached when nothing was queued, so the user can fix the input and try again.
    fn run_tool(&mut self, tool: ToolId, window: &mut Window, cx: &mut Context<Self>) {
        let assets: Vec<AssetId> = self.state.accepted_assets().iter().map(|a| a.asset.clone()).collect();
        if assets.is_empty() {
            crate::ui::toast(window, "Add an image first.", cx);
            return;
        }
        let roles: Vec<AssetRole> = self.state.accepted_assets().iter().map(|a| a.role).collect();
        if let Err(e) = self.state.asset_constraints().validate(&roles) {
            crate::ui::toast(window, e.to_string(), cx);
            return;
        }
        let Some(model) = self.state.active_tool_model() else {
            crate::ui::toast(window, "Pick a model first.", cx);
            return;
        };
        let provider = self.state.provider.id.clone();
        let album = self.album.clone();
        let n = self.library.update(cx, |m, cx| m.run_tool_on_assets(model, &assets, provider, album, cx));
        if n == 0 {
            crate::ui::toast(window, "No supported images (PNG, JPEG, WebP or GIF).", cx);
            return;
        }
        self.state.clear_active_assets();
        self.reset_transients();
        crate::ui::toast(window, format!("{}: processing {n} image(s)…", tool.label()), cx);
        cx.notify();
    }

    /// Paste a clipboard image as a reference asset. ⌘⇧V.
    pub(crate) fn paste_image(&mut self, _: &PasteImage, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        for entry in item.into_entries() {
            if let gpui::ClipboardEntry::Image(image) = entry {
                let png = match image.format {
                    gpui::ImageFormat::Png => image.bytes.clone(),
                    _ => match majik_providers::transcode::transcode_to_png(&image.bytes) {
                        Some(p) => p,
                        None => continue,
                    },
                };
                match self.import(AssetRole::ReferenceImage, "image/png", png, cx) {
                    Ok(asset) => {
                        if self.add_image(asset, window, cx) {
                            crate::ui::toast(window, "Pasted image", cx);
                        }
                    }
                    Err(e) => crate::ui::toast(window, e.to_string(), cx),
                }
                return;
            }
        }
        crate::ui::toast(window, "No image on the clipboard.", cx);
    }

    /// The count stepper's ± buttons: one output at a time, clamped to 1–`MAX_COUNT`, on the
    /// active tab's count (image or video).
    fn step_count(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(count) = self.state.count_mut() else { return };
        *count = (*count as isize + delta).clamp(1, MAX_COUNT as isize) as usize;
        cx.notify();
    }

    /// Splice a reference handle in at the caret and hand focus back to the prompt, so a chip click
    /// reads as typing. A space is added ahead of it only when the caret isn't already after one.
    fn insert_handle(&mut self, handle: String, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt.update(cx, |state, cx| {
            let text = state.value();
            let caret = state.cursor().min(text.len());
            let needs_space = !text[..caret].is_empty() && !text[..caret].ends_with(char::is_whitespace);
            let insertion = if needs_space { format!(" {handle} ") } else { format!("{handle} ") };
            state.insert(insertion, window, cx);
        });
        self.focus_prompt(window, cx);
        cx.notify();
    }

    pub(crate) fn clear_prompt(&mut self, _: &ClearPrompt, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    // ----- assets ------------------------------------------------------------------

    /// Add to the active tab; `false` when the state refused it (see `ComposerState::add_asset`).
    fn add_asset(&mut self, asset: AssetId, role: AssetRole, cx: &mut Context<Self>) -> bool {
        let key = thumb_key(&asset);
        let added = self.state.add_asset(DraftAsset { asset, role });
        if added {
            self.entering.insert(key);
            if self.state.role_is_full(role) {
                // The role is full: its picker card scale-fades away.
                self.entering.remove(&picker_key(role));
                self.start_exit(ExitKind::Picker(role), cx);
            }
        }
        cx.notify();
        added
    }

    fn remove_asset(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(asset) = self.state.remove_asset(index) else { return };
        self.entering.remove(&thumb_key(&asset.asset));
        let max = self.state.asset_constraints().range(asset.role).map(|r| *r.end()).unwrap_or(0);
        let was_full = self.state.role_count(asset.role) + 1 >= max;
        // The role's picker card comes back: cancel a pending exit, or re-enter if it had finished.
        let picker_exiting = self.exiting.iter().any(|e| matches!(e.kind, ExitKind::Picker(r) if r == asset.role));
        self.exiting.retain(|e| !matches!(e.kind, ExitKind::Picker(r) if r == asset.role));
        if was_full && !picker_exiting && !cx.reduce_motion() {
            self.entering.insert(picker_key(asset.role));
        }
        self.start_exit(ExitKind::Thumb { asset, index }, cx);
        cx.notify();
    }

    fn start_exit(&mut self, kind: ExitKind, cx: &mut Context<Self>) {
        if cx.reduce_motion() {
            return;
        }
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CARD_EXIT).await;
            this.update(cx, |v, cx| v.prune_exits(cx)).ok();
        });
        self.exiting.push(ExitingCard { kind, started: now(cx), _timer: timer });
    }

    fn prune_exits(&mut self, cx: &mut Context<Self>) {
        let now = now(cx);
        self.exiting.retain(|e| now.saturating_duration_since(e.started) < CARD_EXIT);
        cx.notify();
    }

    /// `on_drag_move` reports every card's hit-test while an external drag is in flight.
    fn set_drop_target(&mut self, role: AssetRole, inside: bool, cx: &mut Context<Self>) {
        let next = if inside {
            Some(role)
        } else if self.drop_target == Some(role) {
            None
        } else {
            self.drop_target
        };
        if next != self.drop_target {
            self.drop_target = next;
            cx.notify();
        }
    }

    /// Validate the bytes for `role`, make them a library asset and return its id. Dedupes by content.
    fn import(&mut self, role: AssetRole, content_type: &str, bytes: Vec<u8>, cx: &mut Context<Self>) -> anyhow::Result<AssetId> {
        let input = AssetInput::new(role, content_type, bytes);
        validation::validate_asset(&input)?;
        let content_type = match role {
            AssetRole::Audio | AssetRole::ReferenceVideo => input.content_type.clone(),
            _ => majik_providers::transcode::sniff_image_mime(&input.data).unwrap_or("image/png").to_string(),
        };
        self.library.update(cx, |m, cx| m.import_asset(&content_type, &input.data, cx))
    }

    /// Files picked or dropped in: each becomes an asset, then a draft entry. Failures are toasted
    /// together.
    fn add_paths(&mut self, role: AssetRole, paths: impl IntoIterator<Item = PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let mut failures: Vec<String> = Vec::new();
        for p in paths {
            let ext = p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
            let content_type = match role {
                AssetRole::Audio => match ext.as_str() {
                    "wav" => "audio/wav",
                    "mp3" => "audio/mpeg",
                    other => other,
                }
                .to_string(),
                AssetRole::ReferenceVideo => "video/mp4".to_string(),
                _ => "image".to_string(),
            };
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(_) => {
                    failures.push(format!("{} couldn't be read.", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                    continue;
                }
            };
            match self.import(role, &content_type, bytes, cx) {
                Ok(asset) => {
                    self.add_asset(asset, role, cx);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !failures.contains(&msg) {
                        failures.push(msg);
                    }
                }
            }
        }
        if !failures.is_empty() {
            crate::ui::toast(window, failures.join("\n"), cx);
        }
    }

    fn pick_assets(&mut self, role: AssetRole, window: &mut Window, cx: &mut Context<Self>) {
        let multiple = self.state.asset_constraints().range(role).map(|r| *r.end() > 1).unwrap_or(false);
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple, prompt: Some("Add".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |v, window, cx| v.add_paths(role, paths, window, cx)).ok();
            }
        })
        .detach();
    }

    // ----- menus -------------------------------------------------------------------

    fn options_menu<T: Clone + PartialEq + 'static>(
        this: WeakEntity<Self>,
        options: Vec<(T, String)>,
        current: Option<T>,
        set: fn(&mut Self, T, &mut Context<Self>),
    ) -> impl Fn(PopupMenu, &mut Window, &mut gpui::Context<PopupMenu>) -> PopupMenu + 'static {
        move |mut menu, _, _| {
            for (value, label) in options.clone() {
                let this = this.clone();
                let checked = current.as_ref() == Some(&value);
                menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(move |_, _, cx| {
                    let v = value.clone();
                    this.update(cx, |view, cx| set(view, v, cx)).ok();
                }));
            }
            menu
        }
    }

    /// The providers with a key (or needing none), the current one checked, then a way into
    /// Settings → Providers, which is the only entry when no provider is ready yet.
    fn provider_menu(this: WeakEntity<Self>, providers: Vec<&'static ProviderDescriptor>, current: ProviderId) -> impl Fn(PopupMenu, &mut Window, &mut gpui::Context<PopupMenu>) -> PopupMenu + 'static {
        move |mut menu, _, _| {
            for descriptor in &providers {
                let this = this.clone();
                let id = descriptor.id.clone();
                menu = menu.item(PopupMenuItem::new(descriptor.display_name).checked(descriptor.id == current).on_click(move |_, window, cx| {
                    let id = id.clone();
                    this.update(cx, |view, cx| view.select_provider(id, window, cx)).ok();
                }));
            }
            if !providers.is_empty() {
                menu = menu.separator();
            }
            let label = if providers.is_empty() { "Add an API key…" } else { "Provider settings…" };
            menu.item(PopupMenuItem::new(label).icon(icon("settings")).on_click(|_, _, cx| crate::windows::open_settings(crate::views::settings::SettingsTarget::providers(), cx)))
        }
    }

    fn capsule(id: &'static str, icon_name: &'static str, label: impl Into<SharedString>, tooltip: &'static str) -> Button {
        button(id).icon(icon(icon_name)).label(label).small().outline().dropdown_caret(true).tooltip(tooltip)
    }

    /// An asset thumbnail. With `remove_index` it carries the ⊗ badge; exit ghosts pass `None`.
    fn render_asset_card(&self, asset: &DraftAsset, remove_index: Option<usize>, cx: &mut Context<Self>) -> gpui::Div {
        let muted = cx.theme().muted;
        let stored = self.library.read(cx).lib.asset(&asset.asset).cloned();
        let is_audio = asset.role == AssetRole::Audio;
        gpui::div()
            .relative()
            .w(ASSET_CARD)
            .h(ASSET_CARD)
            .rounded_lg()
            .overflow_hidden()
            .bg(muted)
            .child(match stored.filter(|a| !a.missing) {
                Some(a) if is_audio => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(icon("audio-lines").size_5())
                    .child(gpui::div().text_xs().child(a.path.extension().and_then(|e| e.to_str()).unwrap_or("audio").to_uppercase()))
                    .into_any_element(),
                // Round the image itself so its corners match the card (parent clip alone doesn't).
                Some(a) => gpui::img(a.thumbnail.clone().unwrap_or(a.path)).size_full().rounded_lg().object_fit(ObjectFit::Cover).into_any_element(),
                // The asset's file is gone (or the asset itself): the card stays so it can be removed.
                None => v_flex().size_full().items_center().justify_center().child(icon("file-x").size_5()).into_any_element(),
            })
            .child(gpui::div().absolute().bottom_0p5().left_0p5().px_1().rounded_sm().bg(gpui::black().opacity(0.5)).text_xs().text_color(gpui::white()).child(asset.role.display_name()))
            // Always-visible remove badge (a clear dark circle); it does not depend on hover.
            .when_some(remove_index, |d, i| {
                d.child(
                    gpui::div()
                        .id(("asset-remove", i))
                        .absolute()
                        .top_1()
                        .right_1()
                        .size_5()
                        .rounded_full()
                        .bg(gpui::black().opacity(0.55))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::black().opacity(0.85)))
                        .child(icon("x").size_3().text_color(gpui::white()))
                        .on_click(cx.listener(move |v, _, _, cx| v.remove_asset(i, cx))),
                )
            })
    }

    /// The handles for the attached references, as chips under the prompt: they appear with the
    /// first reference, and a click drops one in at the caret. Typing `@Image2` by hand does the
    /// same thing — the chips are a reminder of what is attached and in which order, since that is
    /// what the number means.
    fn render_reference_tags(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let handles = self.state.reference_handles();
        if handles.is_empty() {
            return None;
        }
        let muted_fg = cx.theme().muted_foreground;
        Some(
            h_flex()
                .gap_1p5()
                .flex_wrap()
                .items_center()
                .child(gpui::div().text_xs().text_color(muted_fg).child("Reference tags"))
                .children(handles.into_iter().map(|(role, index)| {
                    let handle = majik_providers::references::handle(role, index);
                    button(SharedString::from(format!("reference-tag-{handle}")))
                        .ghost()
                        .xsmall()
                        .label(handle.clone())
                        .tooltip("Insert into the prompt")
                        .on_click(cx.listener(move |v, _, window, cx| v.insert_handle(handle.clone(), window, cx)))
                })),
        )
    }

    /// The dashed "add" card for `role`. Interactive cards pick / accept drops and ease their border
    /// and label to the accent colour while a drag is over them (150 ms);
    /// exit ghosts are plain.
    fn render_picker_card(&self, role: AssetRole, interactive: bool, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let (primary, border, muted, muted_fg, fg) = (theme.primary, theme.border, theme.muted, theme.muted_foreground, theme.foreground);
        let hot = interactive && self.drop_target == Some(role);
        let (border_color, text_color) = if interactive {
            (
                color_to(("asset-border", role.raw()), if hot { primary } else { border }, MOTION_FAST, window, cx),
                color_to(("asset-fg", role.raw()), if hot { fg } else { muted_fg }, MOTION_FAST, window, cx),
            )
        } else {
            (border, muted_fg)
        };
        gpui::div()
            .id(if interactive { picker_key(role) } else { format!("exit-{}", picker_key(role)).into() })
            .when(interactive, |d| d.debug_selector(move || picker_key(role).to_string()))
            .w(ASSET_CARD)
            .h(ASSET_CARD)
            .rounded_lg()
            .when(hot, |d| d.border_2())
            .when(!hot, |d| d.border_1().border_dashed())
            .border_color(border_color)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .text_color(text_color)
            .when(interactive, |d| {
                d.cursor_pointer()
                    .hover(move |s| s.bg(muted))
                    .on_drag_move::<ExternalPaths>(cx.listener(move |v, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                        v.set_drop_target(role, event.bounds.contains(&event.event.position), cx)
                    }))
                    .on_drop(cx.listener(move |v, paths: &ExternalPaths, window, cx| {
                        v.drop_target = None;
                        v.add_paths(role, paths.paths().iter().cloned(), window, cx)
                    }))
                    // Drags from the feed or the Assets grid: only cards whose role takes one of
                    // the dragged kinds light up and accept.
                    .can_drop(move |value, _, _| value.downcast_ref::<DraggedAssets>().is_none_or(|d| d.assets.iter().any(|a| role.accepts_kind(a.kind))))
                    .on_drag_move::<DraggedAssets>(cx.listener(move |v, event: &DragMoveEvent<DraggedAssets>, _, cx| {
                        let takes = event.drag(cx).assets.iter().any(|a| role.accepts_kind(a.kind));
                        v.set_drop_target(role, takes && event.bounds.contains(&event.event.position), cx)
                    }))
                    .on_drop(cx.listener(move |v, dragged: &DraggedAssets, _, cx| v.add_dragged(role, dragged, cx)))
                    .on_click(cx.listener(move |v, _, window, cx| v.pick_assets(role, window, cx)))
            })
            .child(icon(role.icon()).size_5())
            .child(gpui::div().text_xs().child(role.display_name()))
    }
}

/// Where a role sits in the References row: the frames first, then everything else in the order
/// `AssetRole` itself declares.
fn role_rank(role: AssetRole) -> u8 {
    match role {
        AssetRole::FirstFrame => 0,
        AssetRole::LastFrame => 1,
        _ => 2,
    }
}

/// What the tab produces, for the missing-key message ("… API key to generate {plural}").
fn plural(tab: ComposeTab) -> &'static str {
    match tab {
        ComposeTab::Media(MediaType::Image) => "images",
        ComposeTab::Media(MediaType::Video) => "videos",
        ComposeTab::Media(MediaType::Audio) => "audio",
        ComposeTab::Tool(ToolId::Upscale) => "upscaled images",
        ComposeTab::Tool(ToolId::RemoveBackground) => "images without a background",
    }
}

fn placeholder(tab: ComposeTab, dialogue: bool) -> &'static str {
    match (tab, dialogue) {
        (ComposeTab::Media(MediaType::Image), _) => "Describe an image…",
        (ComposeTab::Media(MediaType::Video), _) => "Describe a video…",
        (ComposeTab::Media(MediaType::Audio), false) => "What should the speaker say?",
        (ComposeTab::Media(MediaType::Audio), true) => "Speaker 1: …\nSpeaker 2: …",
        (ComposeTab::Tool(_), _) => "",
    }
}

fn ratio_icon(portrait: Option<bool>) -> &'static str {
    match portrait {
        None => "square",
        Some(true) => "rectangle-vertical",
        Some(false) => "rectangle-horizontal",
    }
}

impl Render for ComposeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_provider(window, cx);
        self.save_draft();
        let theme = cx.theme();
        let (bg, fg, muted_fg, border) = (theme.background, theme.foreground, theme.muted_foreground, theme.border);
        let (muted, ring) = (theme.muted, theme.ring);
        let prompt_focused = gpui::Focusable::focus_handle(self.prompt.read(cx), cx).is_focused(window);
        let this = cx.weak_entity();
        let tabs = self.state.supported_tabs();
        let tab = self.state.tab;
        let takes_prompt = self.takes_prompt();

        // --- tab + provider ---
        let selected_tab = tabs.iter().position(|t| *t == tab).unwrap_or(0);
        let type_items: Vec<(SharedString, &'static str)> = tabs.iter().map(|t| (SharedString::from(format!("type-{}", t.raw())), t.label())).collect();
        let type_row = segmented("type", type_items, selected_tab, {
            let this = this.clone();
            move |index, window, cx| {
                let Some(t) = tabs.get(index).copied() else { return };
                this.update(cx, |v, cx| v.set_tab(t, window, cx)).ok();
            }
        });
        let provider_button = button("provider")
            .icon(icon("globe"))
            .label(self.state.provider.display_name)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu(Self::provider_menu(this.clone(), state::available_providers(cx), self.state.provider.id.clone()));

        // --- model ---
        let (model_name, model_maker, model_desc): (&str, &str, &str) = match tab {
            ComposeTab::Media(MediaType::Image) => self.state.image_model().map(|m| (m.name, m.manufacturer, m.short_description)).unwrap_or(("No model", "", "")),
            ComposeTab::Media(MediaType::Video) => self.state.video_model().map(|m| (m.name, m.manufacturer, m.short_description)).unwrap_or(("No model", "", "")),
            ComposeTab::Media(MediaType::Audio) => self.state.audio_model().map(|m| (m.name, m.manufacturer, m.short_description)).unwrap_or(("No model", "", "")),
            ComposeTab::Tool(t) => self.state.tool_model(t).map(|m| (m.name, m.manufacturer, m.short_description)).unwrap_or(("No model", "", "")),
        };
        let current_model = self.state.model_index();
        let model_logo = match tab {
            ComposeTab::Media(MediaType::Image) => self.state.image_model().map(|m| (m.logo, m.manufacturer)),
            ComposeTab::Media(MediaType::Video) => self.state.video_model().map(|m| (m.logo, m.manufacturer)),
            ComposeTab::Media(MediaType::Audio) => self.state.audio_model().map(|m| (m.logo, m.manufacturer)),
            ComposeTab::Tool(t) => self.state.tool_model(t).map(|m| (m.logo, m.manufacturer)),
        };
        let provider_static: &'static ProviderDescriptor = self.state.provider;
        let picker_this = this.clone();
        let model_button = gpui::div()
            .id("model")
            .tooltip(|window, cx| Tooltip::new("Choose a model").build(window, cx))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .hover(move |s| s.bg(muted))
            .on_click(move |_, window, cx| {
                crate::views::model_picker::open_model_picker(picker_this.clone(), provider_static, tab, current_model, window, cx);
            })
            .children(model_logo.map(|(l, maker)| crate::ui::logo_tile(l, maker, 28., cx)))
            .child(
                v_flex()
                    .child(gpui::div().text_sm().line_height(relative(1.2)).font_weight(gpui::FontWeight::SEMIBOLD).child(model_name.to_string()))
                    .child(gpui::div().text_xs().line_height(relative(1.2)).text_color(muted_fg).child(model_maker.to_string())),
            )
            .child(gpui::div().flex_1())
            .child(icon("chevron-down").size_4().text_color(muted_fg));

        // --- options ---
        let mut options = h_flex().flex_wrap().gap_2();
        match tab {
            ComposeTab::Media(MediaType::Image) => {
                if let Some(caps) = self.state.image_caps() {
                    if caps.supports_aspect_ratio() {
                        let ratios: Vec<(AspectRatio, String)> = caps.supported_aspect_ratios.iter().map(|a| (*a, a.raw().to_string())).collect();
                        let label = self.state.image.aspect_ratio.map(|a| a.raw()).unwrap_or("Ratio");
                        options = options.child(Self::capsule("ratio", ratio_icon(self.state.image.aspect_ratio.map(|a| a.is_portrait()).filter(|_| self.state.image.aspect_ratio != Some(AspectRatio::Square))), label, "Aspect ratio").dropdown_menu(Self::options_menu(this.clone(), ratios, self.state.image.aspect_ratio, |v, ar, cx| {
                            v.state.image.aspect_ratio = Some(ar);
                            cx.notify();
                        })));
                    }
                    if caps.supports_resolution() {
                        let res: Vec<(ImageResolution, String)> = caps.supported_resolutions.iter().map(|r| (*r, r.raw().to_string())).collect();
                        let label = self.state.image.resolution.map(|r| r.raw()).unwrap_or("Size");
                        options = options.child(Self::capsule("res", "scaling", label, "Resolution").dropdown_menu(Self::options_menu(this.clone(), res, self.state.image.resolution, |v, r, cx| {
                            v.state.image.resolution = Some(r);
                            cx.notify();
                        })));
                    }
                }
            }
            ComposeTab::Media(MediaType::Video) => {
                if let Some(caps) = self.state.video_caps() {
                    if !caps.aspect_ratios.is_empty() {
                        let ratios: Vec<(VideoAspectRatio, String)> = caps.aspect_ratios.iter().map(|a| (*a, a.raw().to_string())).collect();
                        let label = self.state.video.aspect_ratio.map(|a| a.raw()).unwrap_or("Ratio");
                        let portrait = self.state.video.aspect_ratio.and_then(|a| a.ratio()).filter(|(n, d)| n != d).map(|(n, d)| n < d);
                        options = options.child(Self::capsule("vratio", ratio_icon(portrait), label, "Aspect ratio").dropdown_menu(Self::options_menu(this.clone(), ratios, self.state.video.aspect_ratio, |v, ar, cx| {
                            v.state.video.aspect_ratio = Some(ar);
                            cx.notify();
                        })));
                    }
                    if caps.supports_resolution() {
                        let res: Vec<(VideoResolution, String)> = caps.resolutions.iter().map(|r| (*r, r.display_name().to_string())).collect();
                        let label = self.state.video.resolution.map(|r| r.display_name()).unwrap_or("Size");
                        options = options.child(Self::capsule("vres", "scaling", label, "Resolution").dropdown_menu(Self::options_menu(this.clone(), res, self.state.video.resolution, |v, r, cx| {
                            v.state.video.resolution = Some(r);
                            cx.notify();
                        })));
                    }
                    let durations: Vec<(u32, String)> = caps.duration_range.presets_or_range().into_iter().map(|d| (d, format!("{d}s"))).collect();
                    options = options.child(Self::capsule("duration", "timer", format!("{}s", self.state.video.duration), "Duration").dropdown_menu(Self::options_menu(this.clone(), durations, Some(self.state.video.duration), |v, d, cx| {
                        v.state.video.duration = d;
                        cx.notify();
                    })));
                    if caps.supports_audio_toggle() {
                        options = options.child(
                            button("audio").icon(icon(if self.state.video.audio { "volume-2" } else { "volume-x" })).small().outline().selected(self.state.video.audio).tooltip("Generate audio").on_click(cx.listener(|v, _, _, cx| {
                                v.state.video.audio = !v.state.video.audio;
                                cx.notify();
                            })),
                        );
                    }
                }
            }
            ComposeTab::Media(MediaType::Audio) => {
                if let Some(caps) = self.state.audio_caps() {
                    let voices: Vec<AudioVoice> = caps.supported_voices.clone();
                    let s1 = self.state.audio.speaker1.as_ref().map(|v| v.display_name.clone()).unwrap_or_else(|| "Speaker 1".into());
                    let (t1, v1, cur1) = (this.clone(), voices.clone(), self.state.audio.speaker1.clone());
                    options = options.child(Self::capsule("speaker1", "mic", s1, "Speaker 1").on_click(move |_, window, cx| {
                        crate::views::voice_picker::open_voice_picker(t1.clone(), crate::views::voice_picker::Speaker::One, v1.clone(), cur1.clone(), false, window, cx)
                    }));
                    if caps.supports_two_speakers {
                        let s2 = self.state.audio.speaker2.as_ref().map(|v| v.display_name.clone()).unwrap_or_else(|| "Speaker 2: None".into());
                        let (t2, v2, cur2) = (this.clone(), voices, self.state.audio.speaker2.clone());
                        options = options.child(Self::capsule("speaker2", "user", s2, "Speaker 2").on_click(move |_, window, cx| {
                            crate::views::voice_picker::open_voice_picker(t2.clone(), crate::views::voice_picker::Speaker::Two, v2.clone(), cur2.clone(), true, window, cx)
                        }));
                    }
                }
            }
            // No tool has parameters yet; the row stays so future ones slot in here.
            ComposeTab::Tool(_) => {}
        }

        // --- assets ---
        if self.drop_target.is_some() && !cx.has_active_drag() {
            self.drop_target = None;
        }
        let constraints = self.state.asset_constraints();
        let mut cards: Vec<gpui::AnyElement> = Vec::new();
        // Only roles this model takes are shown; the rest stay in the tab for when the user switches back.
        for (i, asset) in self.state.active_assets().iter().enumerate().filter(|(_, a)| constraints.accepts(a.role)) {
            let card = self.render_asset_card(asset, Some(i), cx);
            let key = thumb_key(&asset.asset);
            cards.push(if self.entering.contains(&key) { enter_card(card, key, ASSET_CARD).into_any_element() } else { card.into_any_element() });
        }
        for exit in &self.exiting {
            if let ExitKind::Thumb { asset, index } = &exit.kind {
                let card = self.render_asset_card(asset, None, cx);
                let key: SharedString = format!("exit-{}", thumb_key(&asset.asset)).into();
                cards.insert((*index).min(cards.len()), exit_card(card, key, ASSET_CARD).into_any_element());
            }
        }
        // A role the frames/references divide has closed off keeps its filled cards but offers no
        // picker: the two can't be sent together, so there is nothing to add to the closed side.
        let mut picker_roles: Vec<AssetRole> = constraints.allowed.keys().copied().collect();
        // The constraint map is keyed by `AssetRole`'s own order; on a video the frames the clip is
        // built between are what the user reaches for first, so they lead the row.
        picker_roles.sort_by_key(|role| role_rank(*role));
        for role in &picker_roles {
            if !self.state.role_is_open(*role) {
                continue;
            }
            let card = self.render_picker_card(*role, true, window, cx);
            let key = picker_key(*role);
            cards.push(if self.entering.contains(&key) { enter_card(card, key, ASSET_CARD).into_any_element() } else { card.into_any_element() });
        }
        for exit in &self.exiting {
            if let ExitKind::Picker(role) = exit.kind {
                let card = self.render_picker_card(role, false, window, cx);
                cards.push(exit_card(card, format!("exit-{}", picker_key(role)), ASSET_CARD).into_any_element());
            }
        }
        let assets_row = h_flex().gap_2().flex_wrap().children(cards);

        // --- prompt + footer ---
        let total = self.total(cx);
        let can = self.can_generate(cx);
        let (verb, glyph) = match tab {
            ComposeTab::Media(_) => ("Generate", "magic-wand"),
            ComposeTab::Tool(ToolId::Upscale) => (ToolId::Upscale.label(), "chevron-up"),
            ComposeTab::Tool(ToolId::RemoveBackground) => (ToolId::RemoveBackground.label(), "scissor"),
        };
        // The output-count stepper (image and video tabs) shares Generate's row (and height: a
        // medium button is `h_8`), spaced off the button it multiplies.
        let count_stepper = match tab {
            ComposeTab::Media(MediaType::Image) => Some(("image", "Number of images")),
            ComposeTab::Media(MediaType::Video) => Some(("video", "Number of videos")),
            ComposeTab::Media(MediaType::Audio) | ComposeTab::Tool(_) => None,
        }
        .map(|(noun, tip)| {
            let count = self.state.count();
            h_flex()
                .id("count")
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .h_8()
                .items_center()
                .rounded_md()
                .border_1()
                .border_color(border)
                .child(button("count-minus").icon(icon("minus")).ghost().small().disabled(count <= 1).on_click(cx.listener(|v, _, _, cx| v.step_count(-1, cx))))
                .child(gpui::div().px_1().text_sm().child(format!("{count} {noun}{}", if count == 1 { "" } else { "s" })))
                .child(button("count-plus").icon(icon("plus")).ghost().small().disabled(count >= MAX_COUNT).on_click(cx.listener(|v, _, _, cx| v.step_count(1, cx))))
        });
        let footer = h_flex()
            .items_center()
            .gap_2()
            .children(count_stepper)
            .when_some(self.album.clone(), |d, album| {
                let name = self.library.read(cx).lib.album(&album).map(|a| a.name.clone()).unwrap_or_default();
                d.child(gpui::div().text_xs().text_color(muted_fg).child(format!("→ {name} album")))
            })
            .child(gpui::div().flex_1())
            .child(
                button("generate")
                    .icon(icon(glyph))
                    .label(if total > 1 { format!("{verb} {total}") } else { verb.to_string() })
                    .primary()
                    .disabled(!can || self.improving.is_some())
                    .tooltip_with_action("Generate", &Generate, Some("Compose"))
                    .on_click(cx.listener(|v, _, window, cx| v.generate(window, cx))),
            );
        // Right-aligned under the button it prices, in the same muted caption style as the album
        // hint beside it.
        let footer = v_flex().gap_1().pb_2().child(footer).when_some(self.cost_caption(cx), |d, caption| {
            d.child(h_flex().justify_end().child(gpui::div().text_xs().text_color(muted_fg).child(caption)))
        });

        let used: usize = constraints.allowed.keys().map(|role| self.state.role_count(*role)).sum();
        let capacity: usize = constraints.allowed.values().map(|range| *range.end()).sum();
        let section = |label: &'static str, note: Option<SharedString>, control: gpui::AnyElement, cx: &App| v_flex().gap_1p5().child(section_label(label, note, cx)).child(control);

        gpui::div()
            .id("compose")
            .key_context("Compose")
            .track_focus(&self.focus)
            .size_full()
            .bg(bg)
            .text_color(fg)
            .flex()
            .flex_col()
            .on_action(cx.listener(|v, _: &Generate, window, cx| v.generate(window, cx)))
            .on_action(cx.listener(|v, _: &ImprovePrompt, window, cx| v.toggle_improve(window, cx)))
            .on_action(cx.listener(Self::clear_prompt))
            .on_action(cx.listener(Self::paste_image))
            // Escape cancels a rewrite first; with none running it means "focus the feed" as ever.
            .on_action(cx.listener(|v, _: &FocusFeed, _window, cx| {
                if v.improving.is_some() {
                    v.cancel_improve(cx);
                } else {
                    cx.propagate();
                }
            }))
            // A grid drag let go anywhere else on the panel still drops (the cards take theirs first).
            .on_drop(cx.listener(|v, dragged: &DraggedAssets, window, cx| v.drop_on_panel(dragged, window, cx)))
            // A toolbar-height header like the feed's, so the two rows line up across the split
            // (without the feed's rule under it). The provider is the outer choice — it decides
            // which tabs there are — so it sits above them.
            .child(h_flex().h(px(44.)).flex_none().px_3().gap_1p5().items_center().child(provider_button))
            // flarly's section titles; References carries its used/capacity count.
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .gap_3()
                    .child(h_flex().child(type_row))
                    .child(section("Model", None, model_button.into_any_element(), cx))
                    .when(!model_desc.is_empty() && model_desc != "TBD", |d| d.child(gpui::div().text_xs().text_color(muted_fg).child(model_desc.to_string())))
                    .child(options)
                    .when(!constraints.allowed.is_empty(), |d| d.child(section("References", Some(format!("{used}/{capacity}").into()), assets_row.into_any_element(), cx)))
                    // Focus ring like gpui-component's own inputs (`appearance(false)` drops it).
                    .when(takes_prompt, |d| {
                        let improving = self.improving.is_some();
                        // Inside the field, under the text: the wand sits where the user's eye ends
                        // up, without an overlay for the text to run beneath.
                        let improve_button = self.can_improve().then(|| {
                            let control = if improving {
                                // The button keeps its id (and so its click target): while a rewrite
                                // runs it says how to stop it.
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .child(spin(icon("loader-circle").size_4().text_color(muted_fg)))
                                    .child(gpui::div().text_xs().text_color(muted_fg).child("Improving…"))
                                    .child(
                                        button("improve")
                                            .ghost()
                                            .small()
                                            .label("Cancel")
                                            .tooltip_with_action("Cancel", &ImprovePrompt, Some("Compose"))
                                            .on_click(cx.listener(|v, _, window, cx| v.toggle_improve(window, cx))),
                                    )
                            } else {
                                h_flex().child(
                                    button("improve")
                                        .ghost()
                                        .small()
                                        .icon(icon("sparkles"))
                                        .disabled(self.prompt_value(cx).is_empty())
                                        .tooltip_with_action("Improve prompt with AI", &ImprovePrompt, Some("Compose"))
                                        .on_click(cx.listener(|v, _, window, cx| v.toggle_improve(window, cx))),
                                )
                            };
                            h_flex().justify_end().items_center().child(control)
                        });
                        let prompt = v_flex()
                            .flex_1()
                            .w_full()
                            .min_h(PROMPT_HEIGHT)
                            .rounded_md()
                            .border_1()
                            .border_color(if prompt_focused { ring } else { border })
                            .p_1()
                            .child(Textarea::new(&self.prompt).appearance(false).readonly(improving).w_full().flex_1().min_h_0())
                            .children(improve_button);
                        let prompt = v_flex().flex_1().min_h_0().gap_2().child(prompt).children(self.render_reference_tags(cx));
                        // The prompt takes whatever height the panel has left over, so the field is
                        // as tall as the window allows and the footer stays at the bottom.
                        d.child(section("Prompt", None, prompt.into_any_element(), cx).flex_1().min_h_0())
                    })
                    .child(footer),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env, seed_request};
    use gpui::{TestAppContext, VisualTestContext};
    use majik_core::model::Status;
    use majik_generation::GenerationType;
    use majik_providers::catalog::{audio, image, tool, video};
    use majik_providers::{AudioGenerationSettings, ImageGenerationSettings, ProviderId, VideoGenerationSettings};

    macro_rules! compose_window {
        ($cx:ident, $provider:expr) => {{
            let e = env($cx, 1, $provider);
            let slot: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<ComposeView>>>> = Default::default();
            let slot2 = slot.clone();
            // Wrap in Root like the real window so notifications / dialog layers work.
            let (_root, vcx) = $cx.add_window_view(move |window, cx| {
                let v = cx.new(|cx| ComposeView::new(window, cx));
                *slot2.borrow_mut() = Some(v.clone());
                gpui_component::Root::new(gpui::AnyView::from(v), window, cx)
            });
            vcx.run_until_parked();
            let view = slot.borrow().clone().unwrap();
            (view, vcx, e)
        }};
    }

    /// [`compose_window`] over a runner that records prompt rewrites instead of running them, so a
    /// test decides what (and when) the model answers. Returns the rewrite log as well.
    macro_rules! compose_window_recording {
        ($cx:ident, $provider:expr) => {{
            let (runner, _jobs, rewrites) = crate::test_support::RecordingRunner::with_rewrites();
            let e = $cx.update(|cx| crate::test_support::setup_with_runner(cx, 1, $provider, Box::new(runner)));
            let slot: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<ComposeView>>>> = Default::default();
            let slot2 = slot.clone();
            let (_root, vcx) = $cx.add_window_view(move |window, cx| {
                let v = cx.new(|cx| ComposeView::new(window, cx));
                *slot2.borrow_mut() = Some(v.clone());
                gpui_component::Root::new(gpui::AnyView::from(v), window, cx)
            });
            vcx.run_until_parked();
            let view = slot.borrow().clone().unwrap();
            (view, vcx, e, rewrites)
        }};
    }

    /// Answer the one rewrite the composer asked for, the way the provider would.
    fn answer_rewrite(rewrites: &crate::test_support::Rewrites, vcx: &mut VisualTestContext, outcome: Result<&str, majik_providers::GenerationError>) {
        let (_, sender) = rewrites.lock().unwrap().pop().expect("a rewrite was asked for");
        sender.send_blocking(outcome.map(str::to_string)).expect("the composer is waiting");
        vcx.run_until_parked();
    }

    /// What the composer asked the model to rewrite, and under what instruction.
    fn asked(rewrites: &crate::test_support::Rewrites) -> majik_generation::TextRequest {
        rewrites.lock().unwrap().last().expect("a rewrite was asked for").0.clone()
    }

    fn set_prompt(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, text: &str) {
        let text = text.to_string();
        view.update_in(vcx, move |v, window, cx| {
            let input = v.prompt.clone();
            input.update(cx, |s, cx| s.set_value(text, window, cx));
        });
    }

    fn switch_to(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, t: MediaType) {
        view.update_in(vcx, move |v, w, cx| v.set_media_type(t, w, cx));
    }

    fn select_image_model(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, id: &str) {
        view.update(vcx, |v, cx| {
            let ix = v.state.provider.supported_image_models.iter().position(|m| m.id == id).expect("model in provider");
            v.select_model(ix, cx);
        });
    }

    /// A library asset named `name`: a small PNG whose colour (and so content, and so identity)
    /// derives from the name — the same name is the same asset, different names are different ones.
    fn import_named(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, name: &str) -> AssetId {
        let seed = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
        let rgb = [(seed % 251) as u8, ((seed >> 8) % 251) as u8, ((seed >> 16) % 251) as u8];
        let bytes = majik_core::images::solid_png(4, 4, rgb);
        let library = view.read_with(vcx, |v, _| v.library.clone());
        library.update(vcx, |m, cx| m.import_asset("image/png", &bytes, cx).unwrap())
    }

    fn add(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, name: &str, role: AssetRole) -> AssetId {
        let asset = import_named(view, vcx, name);
        let id = asset.clone();
        view.update(vcx, move |v, cx| v.add_asset(id, role, cx));
        asset
    }

    /// Pick / drop real files into the active tab's `role`.
    fn add_files(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, paths: Vec<PathBuf>, role: AssetRole) {
        view.update_in(vcx, move |v, window, cx| v.add_paths(role, paths, window, cx));
    }

    /// An image arriving without a role (paste, a drop on the panel).
    fn add_image(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, name: &str) -> AssetId {
        let asset = import_named(view, vcx, name);
        let id = asset.clone();
        view.update_in(vcx, |v, w, cx| {
            v.add_image(id, w, cx);
        });
        asset
    }

    fn roles(assets: &[DraftAsset]) -> Vec<AssetRole> {
        assets.iter().map(|a| a.role).collect()
    }

    /// Recreate a row that stored `req` with inputs `(role, asset name)`; returns the assets it
    /// referenced.
    fn recreate(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, req: Request, inputs: Vec<(&str, &str)>) -> Vec<AssetId> {
        let inputs: Vec<(&str, AssetId)> = inputs.into_iter().map(|(role, name)| (role, import_named(view, vcx, name))).collect();
        let ids: Vec<AssetId> = inputs.iter().map(|(_, id)| id.clone()).collect();
        let library = view.update(vcx, |v, _| v.library.clone());
        let id = seed_request(&library, vcx, &req, &inputs);
        view.update_in(vcx, |v, w, cx| v.apply(PendingCompose { recreate: Some(id) }, w, cx));
        ids
    }

    fn tool_request(model: &majik_providers::ToolModel) -> Request {
        Request::tool(ProviderId::mock(), model, AssetInput::new(AssetRole::ReferenceImage, "image/png", vec![]))
    }

    fn video_request(model: &majik_providers::VideoModel, duration: u32) -> Request {
        Request::new(
            ProviderId::mock(),
            GenerationType::Video(VideoGenerationSettings { model: model.clone(), aspect_ratio: Some(VideoAspectRatio::Landscape), resolution: Some(VideoResolution::Hd), duration, audio_enabled: true }),
            "recreated video",
            vec![],
        )
    }

    fn audio_request(model: &majik_providers::AudioModel) -> Request {
        Request::new(ProviderId::mock(), GenerationType::Audio(AudioGenerationSettings { model: model.clone(), speaker1: AudioVoice::new("Kore", "Kore"), speaker2: None }), "recreated audio", vec![])
    }

    fn toasts(vcx: &mut VisualTestContext) -> u64 {
        vcx.update(|_, cx| crate::ui::toast_generation(cx))
    }

    fn seeded_png(dir: &std::path::Path) -> PathBuf {
        std::fs::read_dir(dir).unwrap().filter_map(Result::ok).map(|d| d.path()).find(|p| p.extension().is_some_and(|x| x == "png")).expect("env seeds a png")
    }

    #[gpui::test]
    fn switching_media_type_coerces_settings(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        view.update(vcx, |v, _| assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image)));
        // fal supports video; switching coerces to a valid video model + its defaults.
        view.update_in(vcx, |v, w, cx| v.set_media_type(MediaType::Video, w, cx));
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Video));
            let caps = v.state.video_caps().expect("a video model is selected");
            assert!(caps.aspect_ratios.contains(&v.state.video.aspect_ratio.unwrap()));
            assert!(caps.duration_range.contains(v.state.video.duration));
        });
    }

    #[gpui::test]
    fn generate_is_gated_on_prompt(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        view.update(vcx, |v, cx| assert!(!v.can_generate(cx), "no prompt → disabled"));
        set_prompt(&view, vcx, "a red apple");
        view.update(vcx, |v, cx| assert!(v.can_generate(cx), "prompt → enabled"));
        // Image models declare no cap: the length is the provider's business, so nothing is limited here.
        view.update(vcx, |v, _| assert_eq!(v.prompt_limit(), None));
        set_prompt(&view, vcx, &"x".repeat(20_000));
        view.update(vcx, |v, cx| assert!(v.can_generate(cx), "no declared cap → still enabled"));
        // Audio models do declare one, and going over it disables again.
        switch_to(&view, vcx, MediaType::Audio);
        let limit = view.update(vcx, |v, _| v.prompt_limit().expect("audio models cap the script"));
        set_prompt(&view, vcx, &"x".repeat(limit));
        view.update(vcx, |v, cx| assert!(v.can_generate(cx), "at the cap → enabled"));
        set_prompt(&view, vcx, &"x".repeat(limit + 1));
        view.update(vcx, |v, cx| assert!(!v.can_generate(cx), "over the cap → disabled"));
    }

    // ----- improve prompt ---------------------------------------------------------

    #[gpui::test]
    fn improve_is_offered_on_image_and_video_but_not_audio_or_a_tool(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        view.update(vcx, |v, _| assert!(v.can_improve(), "the image tab writes a prompt"));
        switch_to(&view, vcx, MediaType::Video);
        view.update(vcx, |v, _| assert!(v.can_improve()));
        switch_to(&view, vcx, MediaType::Audio);
        view.update(vcx, |v, _| assert!(!v.can_improve(), "an audio script is spoken verbatim"));
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        view.update(vcx, |v, _| assert!(!v.can_improve(), "a tool has no prompt"));
    }

    #[gpui::test]
    fn improve_does_nothing_until_something_is_typed(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        assert!(rewrites.lock().unwrap().is_empty(), "a blank prompt asks nothing");
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        assert_eq!(rewrites.lock().unwrap().len(), 1);
    }

    #[gpui::test]
    fn improve_sends_the_prompt_the_model_and_the_reference_roles(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        select_image_model(&view, vcx, "gemini-3-pro");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        set_prompt(&view, vcx, "  a cat  ");
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));

        let request = asked(&rewrites);
        assert_eq!(request.user, "a cat", "the prompt goes as typed, trimmed");
        assert_eq!(request.provider, ProviderId::mock());
        assert!(request.system.contains("Nano Banana Pro"), "{}", request.system);
        assert!(request.system.contains("receive 1 reference image (image)"), "{}", request.system);
    }

    #[gpui::test]
    fn improve_replaces_the_prompt_and_keeps_the_draft(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        answer_rewrite(&rewrites, vcx, Ok("  a tabby cat on a windowsill  "));

        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "a tabby cat on a windowsill", "trimmed into the field"));
        vcx.update(|_, cx| assert_eq!(cx.global::<Config>().draft_prompt, "a tabby cat on a windowsill", "the draft follows the edit"));
    }

    #[gpui::test]
    fn undo_after_improve_restores_what_the_user_wrote(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        // Type into the field so the undo history starts from the user's own text.
        view.update_in(vcx, |v, w, cx| v.focus_prompt(w, cx));
        vcx.simulate_keystrokes("!");
        let typed = view.update(vcx, |v, cx| v.prompt_text(cx));

        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        answer_rewrite(&rewrites, vcx, Ok("a tabby cat on a windowsill"));
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "a tabby cat on a windowsill"));

        vcx.simulate_keystrokes("secondary-z");
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), typed, "one undo gives the user's own words back"));
    }

    #[gpui::test]
    fn the_prompt_and_generate_are_frozen_while_improving(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        view.update(vcx, |v, cx| assert!(v.can_generate(cx), "generate is available before the rewrite"));

        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        view.update_in(vcx, |v, w, cx| v.focus_prompt(w, cx));
        vcx.run_until_parked();
        view.update(vcx, |v, _| assert!(v.improving.is_some(), "a rewrite is in flight"));
        vcx.simulate_keystrokes("x");
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "a cat", "the field refuses edits meanwhile"));

        answer_rewrite(&rewrites, vcx, Ok("a tabby cat"));
        view.update(vcx, |v, cx| {
            assert!(v.improving.is_none());
            assert!(v.can_generate(cx), "generate is available again");
        });
        // `replace_all` leaves the caret at the start, so the keystroke goes there.
        vcx.simulate_keystrokes("!");
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "!a tabby cat", "and the field takes edits again"));
    }

    #[gpui::test]
    fn clicking_again_cancels_the_rewrite_and_leaves_the_prompt_alone(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.toggle_improve(w, cx));
        view.update_in(vcx, |v, w, cx| v.toggle_improve(w, cx));
        view.update(vcx, |v, cx| {
            assert!(v.improving.is_none(), "the second click cancelled it");
            assert_eq!(v.prompt_text(cx), "a cat", "nothing was written");
        });
        // An answer that arrives for the abandoned rewrite changes nothing.
        answer_rewrite(&rewrites, vcx, Ok("a tabby cat"));
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "a cat"));
    }

    #[gpui::test]
    fn escape_cancels_a_rewrite_in_flight(cx: &mut TestAppContext) {
        let (view, vcx, _e, _rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        view.update_in(vcx, |v, w, cx| v.focus_prompt(w, cx));
        vcx.simulate_keystrokes("escape");
        view.update(vcx, |v, cx| {
            assert!(v.improving.is_none(), "escape stopped it");
            assert_eq!(v.prompt_text(cx), "a cat");
        });
    }

    #[gpui::test]
    fn switching_tab_or_model_cancels_a_rewrite(cx: &mut TestAppContext) {
        let (view, vcx, _e, _rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        switch_to(&view, vcx, MediaType::Video);
        view.update(vcx, |v, _| assert!(v.improving.is_none(), "the rewrite was for the image model"));

        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        view.update(vcx, |v, _| assert!(v.improving.is_some()));
        view.update(vcx, |v, cx| v.select_model(1, cx));
        view.update(vcx, |v, _| assert!(v.improving.is_none(), "a different model gets a fresh ask"));
    }

    #[gpui::test]
    fn improve_without_a_key_opens_settings_on_that_provider(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        vcx.update(|_, cx| {
            for provider in ["Mock", "OpenRouter", "Replicate", "fal.ai"] {
                state::keys(cx).delete(provider, cx).detach();
            }
        });
        vcx.run_until_parked();
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        assert!(rewrites.lock().unwrap().is_empty(), "nothing is asked without a key");
        vcx.update(|_, cx| assert!(cx.global::<crate::windows::Windows>().settings.is_some(), "settings opened to fix it"));
    }

    #[gpui::test]
    fn a_failed_rewrite_toasts_and_unfreezes(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        let before = toasts(vcx);
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        answer_rewrite(&rewrites, vcx, Err(majik_providers::GenerationError::RateLimited("slow down".into())));
        assert!(toasts(vcx) > before, "the failure is shown");
        view.update(vcx, |v, cx| {
            assert!(v.improving.is_none());
            assert_eq!(v.prompt_text(cx), "a cat", "the prompt is left as typed");
        });
    }

    #[gpui::test]
    fn an_empty_rewrite_is_not_written_into_the_prompt(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        let before = toasts(vcx);
        view.update_in(vcx, |v, w, cx| v.improve(w, cx));
        answer_rewrite(&rewrites, vcx, Ok("   "));
        assert!(toasts(vcx) > before, "the user is told why nothing changed");
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "a cat"));
    }

    #[gpui::test]
    fn the_improve_action_runs_the_same_path_as_the_button(cx: &mut TestAppContext) {
        let (view, vcx, _e, rewrites) = compose_window_recording!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.focus_prompt(w, cx));
        vcx.dispatch_action(ImprovePrompt);
        vcx.run_until_parked();
        assert_eq!(rewrites.lock().unwrap().len(), 1, "⌘⇧I asks for the rewrite");
        answer_rewrite(&rewrites, vcx, Ok("a tabby cat"));
        view.update(vcx, |v, cx| assert_eq!(v.prompt_text(cx), "a tabby cat"));
    }

    #[gpui::test]
    fn generate_inserts_placeholder_rows(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        let before = e.library.read_with(vcx, |m, _| m.lib.generations().len());
        set_prompt(&view, vcx, "one");
        // Count 2 → two generating rows appear immediately (before the mock finishes).
        view.update(vcx, |v, _| v.state.image.count = 2);
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        let (total, generating) = e.library.read_with(vcx, |m, _| {
            let items = m.lib.generations();
            (items.len(), items.iter().filter(|i| i.status == Status::Generating).count())
        });
        assert_eq!(total, before + 2, "two placeholder rows inserted");
        assert_eq!(generating, 2);
        // The prompt is cleared after dispatch.
        view.update(vcx, |v, cx| assert!(v.prompt_text(cx).is_empty()));
    }

    #[gpui::test]
    fn recreate_loads_stored_settings(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        // A stored request for a specific model/ratio/resolution.
        let req = majik_generation::Request::new(
            majik_providers::ProviderId::mock(),
            GenerationType::Image(ImageGenerationSettings {
                model: majik_providers::catalog::image::ALL[2].clone(),
                aspect_ratio: AspectRatio::Landscape,
                resolution: ImageResolution::Hd,
            }),
            "recreate me",
            vec![],
        );
        recreate(&view, vcx, req, vec![]);
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image));
            assert_eq!(v.state.image_model().unwrap().id, majik_providers::catalog::image::ALL[2].id);
            assert_eq!(v.state.image.aspect_ratio, Some(AspectRatio::Landscape));
            assert_eq!(v.prompt_text(cx), "recreate me");
        });
    }

    #[gpui::test]
    fn image_count_clamps_1_to_8(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        view.update(vcx, |v, _| {
            v.state.image.count = 1;
            v.state.coerce();
            assert_eq!(v.state.image.count, 1);
            v.state.image.count = 99;
            v.state.coerce();
            assert_eq!(v.state.image.count, 8, "count clamped to the 1..8 range");
        });
    }

    #[gpui::test]
    fn asset_add_respects_reference_capacity(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        view.update(vcx, |v, cx| {
            let max = v.state.image_caps().unwrap().max_input_images;
            assert!(max >= 1);
            for i in 0..(max + 3) {
                v.add_asset(AssetId(format!("ref-{i}")), AssetRole::ReferenceImage, cx);
            }
            assert_eq!(v.state.assets.image.len(), max, "capped at the model's max input images");
        });
    }

    #[gpui::test]
    fn removing_asset_queues_exit_until_timer(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        let asset = AssetId("ref-a".into());
        view.update(vcx, |v, cx| {
            v.add_asset(asset.clone(), AssetRole::ReferenceImage, cx);
            assert!(v.entering.contains(&thumb_key(&asset)), "new cards play the enter transition");
            v.remove_asset(0, cx);
            assert!(v.state.active_assets().is_empty());
            assert!(!v.entering.contains(&thumb_key(&asset)));
            assert_eq!(v.exiting.len(), 1);
            assert!(matches!(v.exiting[0].kind, ExitKind::Thumb { index: 0, .. }));
        });
        vcx.background_executor.advance_clock(CARD_EXIT + Duration::from_millis(10));
        vcx.run_until_parked();
        view.update(vcx, |v, _| assert!(v.exiting.is_empty(), "timer pruned the ghost"));
    }

    #[gpui::test]
    fn reduce_motion_removes_immediately(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        vcx.update(|_, cx| cx.set_reduce_motion(true));
        view.update(vcx, |v, cx| {
            v.add_asset(AssetId("ref-a".into()), AssetRole::ReferenceImage, cx);
            v.remove_asset(0, cx);
            assert!(v.state.active_assets().is_empty());
            assert!(v.exiting.is_empty());
        });
    }

    #[gpui::test]
    fn filling_a_role_queues_picker_exit_and_reappears_on_remove(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        view.update(vcx, |v, cx| {
            let max = v.state.image_caps().unwrap().max_input_images;
            for i in 0..max {
                v.add_asset(AssetId(format!("ref-{i}")), AssetRole::ReferenceImage, cx);
            }
            assert!(v.exiting.iter().any(|e| matches!(e.kind, ExitKind::Picker(AssetRole::ReferenceImage))), "full role hides its picker");
        });
        vcx.background_executor.advance_clock(CARD_EXIT + Duration::from_millis(10));
        vcx.run_until_parked();
        view.update(vcx, |v, cx| {
            assert!(v.exiting.is_empty());
            v.remove_asset(0, cx);
            assert!(v.entering.contains(&picker_key(AssetRole::ReferenceImage)), "picker re-enters once the role has room");
        });
    }

    #[gpui::test]
    fn drop_target_follows_drag_position(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        view.update(vcx, |v, cx| {
            v.set_drop_target(AssetRole::ReferenceImage, true, cx);
            assert_eq!(v.drop_target, Some(AssetRole::ReferenceImage));
            // Another card reporting "outside" doesn't clear a different target.
            v.set_drop_target(AssetRole::Audio, false, cx);
            assert_eq!(v.drop_target, Some(AssetRole::ReferenceImage));
            v.set_drop_target(AssetRole::ReferenceImage, false, cx);
            assert_eq!(v.drop_target, None);
        });
    }

    #[gpui::test]
    fn losing_the_picked_providers_key_generates_with_one_that_has_a_key(cx: &mut TestAppContext) {
        // fal's key is gone; the composer moves to the first provider that still has one (Mock).
        let (view, vcx, e) = compose_window!(cx, "fal.ai");
        vcx.update(|_, cx| state::keys(cx).delete("fal.ai", cx).detach());
        vcx.run_until_parked();
        let before = e.library.read_with(vcx, |m, _| m.lib.generations().len());
        set_prompt(&view, vcx, "runs on Mock");
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        vcx.run_until_parked();
        view.read_with(vcx, |v, _| assert_eq!(v.state.provider.display_name, "Mock"));
        e.library.read_with(vcx, |m, _| {
            let items = m.lib.generations();
            assert_eq!(items.len(), before + 1, "one row created with the fallback provider");
            assert_eq!(items.first().and_then(|i| i.provider.as_deref()), Some("Mock"), "newest first");
        });
        assert!(vcx.update(|_, cx| cx.global::<crate::windows::Windows>().settings.is_none()), "no key prompt: a provider was ready");
    }

    // ----- per-tab assets -----

    #[gpui::test]
    fn image_assets_survive_tab_round_trip(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        for t in [MediaType::Video, MediaType::Audio, MediaType::Image] {
            switch_to(&view, vcx, t);
            let active = view.update(vcx, |v, _| v.state.active_assets().len());
            assert_eq!(active, usize::from(t == MediaType::Image), "{t:?}: only the image tab shows the image draft");
        }
        view.update(vcx, |v, _| assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage]));
    }

    #[gpui::test]
    fn video_assets_are_separate_from_image_assets(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        switch_to(&view, vcx, MediaType::Video);
        add(&view, vcx, "/tmp/first.png", AssetRole::FirstFrame);
        view.update(vcx, |v, _| assert_eq!(roles(v.state.active_assets()), vec![AssetRole::FirstFrame]));
        switch_to(&view, vcx, MediaType::Image);
        view.update(vcx, |v, _| {
            assert_eq!(roles(v.state.active_assets()), vec![AssetRole::ReferenceImage]);
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::FirstFrame]);
        });
    }

    #[gpui::test]
    fn select_model_keeps_unaccepted_assets(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        select_image_model(&view, vcx, "gpt-image-2");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        add(&view, vcx, "/tmp/mask.png", AssetRole::MaskImage);
        select_image_model(&view, vcx, "gemini-3-pro");
        view.update(vcx, |v, _| {
            assert_eq!(v.state.assets.image.len(), 2, "nothing is pruned");
            assert_eq!(roles(&v.state.accepted_assets().into_iter().cloned().collect::<Vec<_>>()), vec![AssetRole::ReferenceImage], "the mask is hidden");
        });
        select_image_model(&view, vcx, "gpt-image-2");
        view.update(vcx, |v, _| assert_eq!(v.state.accepted_assets().len(), 2, "and back"));
    }

    #[gpui::test]
    fn generate_clears_only_active_tab(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        let png = seeded_png(e.dir.path());
        add_files(&view, vcx, vec![png.clone()], AssetRole::ReferenceImage);
        switch_to(&view, vcx, MediaType::Video);
        add_files(&view, vcx, vec![png.clone()], AssetRole::FirstFrame);
        switch_to(&view, vcx, MediaType::Image);
        let before = e.library.read_with(vcx, |m, _| m.lib.generations().len());
        set_prompt(&view, vcx, "with a reference");
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.generations().len()), before + 1);
        view.update(vcx, |v, _| {
            assert!(v.state.assets.image.is_empty(), "the sent tab is cleared");
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::FirstFrame], "the other tab keeps its draft");
        });
    }

    #[gpui::test]
    fn provider_switch_keeps_assets(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        view.update_in(vcx, |v, w, cx| v.select_provider(ProviderId::fal(), w, cx));
        view.update(vcx, |v, _| {
            assert_eq!(v.state.provider.id, ProviderId::fal());
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage]);
        });
    }

    #[gpui::test]
    fn selecting_a_provider_persists_it_for_the_whole_app(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        view.update_in(vcx, |v, w, cx| v.select_provider(ProviderId::replicate(), w, cx));
        view.update(vcx, |v, _| assert_eq!(v.state.provider.id, ProviderId::replicate()));
        vcx.update(|_, cx| {
            assert_eq!(cx.global::<Config>().provider, "Replicate");
            assert_eq!(state::selected_provider(cx).id, ProviderId::replicate(), "feed and detail tools follow the composer");
        });
    }

    #[gpui::test]
    fn provider_menu_offers_only_providers_with_a_key(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        vcx.update(|_, cx| {
            state::keys(cx).delete("fal.ai", cx).detach();
            state::keys(cx).delete("OpenRouter", cx).detach();
        });
        vcx.run_until_parked();
        let offered: Vec<&str> = vcx.update(|_, cx| state::available_providers(cx).iter().map(|d| d.display_name).collect());
        assert_eq!(offered, vec!["Mock", "Replicate"]);
        // A render (which builds the menu) keeps the current pick while it has a key.
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        view.update(vcx, |v, _| assert_eq!(v.state.provider.id, ProviderId::mock(), "the current pick stays while it has a key"));
    }

    #[gpui::test]
    fn removing_the_selected_providers_key_moves_the_composer_to_one_that_has_one(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Replicate");
        view.update(vcx, |v, _| assert_eq!(v.state.provider.id, ProviderId::replicate()));
        vcx.update(|_, cx| state::keys(cx).delete("Replicate", cx).detach());
        vcx.run_until_parked();
        view.update_in(vcx, |v, w, cx| v.sync_provider(w, cx));
        view.update(vcx, |v, _| assert_eq!(v.state.provider.display_name, "Mock", "first available provider"));
    }

    #[gpui::test]
    fn generate_without_any_key_opens_settings_on_that_provider(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Replicate");
        vcx.update(|_, cx| {
            for provider in ["Mock", "OpenRouter", "Replicate", "fal.ai"] {
                state::keys(cx).delete(provider, cx).detach();
            }
        });
        vcx.run_until_parked();
        let before = e.library.read_with(vcx, |m, _| m.lib.generations().len());
        set_prompt(&view, vcx, "a cat");
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        vcx.run_until_parked();
        let handle = vcx.update(|_, cx| cx.global::<crate::windows::Windows>().settings).expect("settings opened for the key");
        let target = vcx.update(|_, cx| {
            handle
                .update(cx, |root, _, cx| {
                    let view = root.view().clone().downcast::<crate::views::settings::SettingsWindow>().unwrap();
                    view.read(cx).target()
                })
                .unwrap()
        });
        assert_eq!(target.page, crate::views::settings::SettingsPage::Providers);
        assert_eq!(target.provider, Some(ProviderId::replicate()));
        assert_eq!(target.message.as_deref(), Some("Please configure your Replicate API key to generate images."));
        e.library.read_with(vcx, |m, _| assert_eq!(m.lib.generations().len(), before, "nothing generated"));
    }

    #[gpui::test]
    fn generate_rejects_too_many_assets_for_model(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "fal.ai");
        select_image_model(&view, vcx, "gpt-image-2");
        add(&view, vcx, "/tmp/a.png", AssetRole::ReferenceImage);
        add(&view, vcx, "/tmp/b.png", AssetRole::ReferenceImage);
        select_image_model(&view, vcx, "flux-2-pro");
        set_prompt(&view, vcx, "two refs, one slot");
        let (rows, toasts_before) = (e.library.read_with(vcx, |m, _| m.lib.generations().len()), toasts(vcx));
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.generations().len()), rows, "refused before dispatch");
        assert_eq!(toasts(vcx), toasts_before + 1);
        view.update(vcx, |v, _| assert_eq!(v.state.assets.image.len(), 2, "nothing was cleared"));
    }

    #[gpui::test]
    fn generate_refuses_an_input_whose_file_is_gone(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        // Recreate a row made from an image whose file has since gone: the card is there, its file is not.
        let request = Request::new(ProviderId::mock(), GenerationType::Image(ImageGenerationSettings { model: image::ALL[0].clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd }), "from a reference", vec![]);
        let assets = recreate(&view, vcx, request, vec![("reference_image", "ref.png")]);
        e.library.update(vcx, |m, _| {
            std::fs::remove_file(&m.lib.asset(&assets[0]).unwrap().path).unwrap();
            m.lib.reload().unwrap();
        });
        let (rows, toasts_before) = (e.library.read_with(vcx, |m, _| m.lib.generations().len()), toasts(vcx));
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.generations().len()), rows, "not sent as a text-only request");
        assert_eq!(toasts(vcx), toasts_before + 1, "told which input is unusable");
        view.update(vcx, |v, _| assert_eq!(v.state.accepted_assets().len(), 1, "the card stays for the user to replace"));
    }

    // ----- use image -----

    #[gpui::test]
    fn a_roleless_image_lands_on_current_tab_first_free_role(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        switch_to(&view, vcx, MediaType::Video);
        for name in ["a.png", "b.png"] {
            add_image(&view, vcx, name);
        }
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Video), "no tab switch while the video tab has room");
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::FirstFrame, AssetRole::LastFrame]);
        });
        switch_to(&view, vcx, MediaType::Audio);
        add_image(&view, vcx, "c.png");
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image), "audio has no image slot, so the image tab takes it");
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage]);
        });
    }

    #[gpui::test]
    fn a_roleless_image_toasts_when_nothing_has_room(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        select_image_model(&view, vcx, "flux-2-pro");
        add(&view, vcx, "/tmp/a.png", AssetRole::ReferenceImage);
        let before = toasts(vcx);
        add_image(&view, vcx, "b.png");
        assert_eq!(toasts(vcx), before + 1);
        view.update(vcx, |v, _| assert_eq!(v.state.assets.image.len(), 1));
    }

    // ----- drags from a grid -----

    fn dragged(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, kinds: &[(MediaType, &str)]) -> DraggedAssets {
        let library = view.read_with(vcx, |v, _| v.library.clone());
        let assets = kinds
            .iter()
            .map(|(kind, name)| {
                let seed = name.bytes().fold(3u8, |h, b| h.wrapping_mul(7).wrapping_add(b));
                let id = crate::test_support::seed_asset(&library, vcx, *kind, seed);
                let (kind, path) = library.read_with(vcx, |m, _| {
                    let asset = m.lib.asset(&id).unwrap();
                    (asset.kind, asset.path.clone())
                });
                crate::state::DraggedAsset { id, kind, path, generation: None }
            })
            .collect();
        DraggedAssets { assets }
    }

    #[gpui::test]
    fn a_drop_on_a_role_card_attaches_the_dragged_assets_that_fit(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        let dragged = dragged(&view, vcx, &[(MediaType::Image, "a.png"), (MediaType::Audio, "a.wav"), (MediaType::Image, "b.png")]);
        view.update(vcx, |v, cx| {
            v.drop_target = Some(AssetRole::FirstFrame);
            v.add_dragged(AssetRole::FirstFrame, &dragged, cx);
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::FirstFrame], "one slot: the first image that fits");
            assert_eq!(v.state.assets.video[0].asset, dragged.assets[0].id);
            assert!(v.drop_target.is_none(), "the card stops glowing");
            v.add_dragged(AssetRole::LastFrame, &dragged, cx);
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::FirstFrame, AssetRole::LastFrame]);
            assert_eq!(v.state.assets.video[1].asset, dragged.assets[0].id, "the same asset can fill another role");
            v.add_dragged(AssetRole::FirstFrame, &dragged, cx);
            assert_eq!(v.state.assets.video.len(), 2, "a full role takes nothing more");
        });
    }

    #[gpui::test]
    fn a_drop_on_the_panel_routes_by_kind(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        let image = dragged(&view, vcx, &[(MediaType::Image, "p.png")]);
        view.update_in(vcx, |v, w, cx| v.drop_on_panel(&image, w, cx));
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image));
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage], "an image lands in the first free image role");
        });

        // A video is a reference, on the tab that takes one; the image beside it follows, because a
        // reference request takes no frames.
        select_video_model(&view, vcx, "seedance-2.5");
        switch_to(&view, vcx, MediaType::Image);
        let clip = dragged(&view, vcx, &[(MediaType::Video, "clip.mp4")]);
        view.update_in(vcx, |v, w, cx| v.drop_on_panel(&clip, w, cx));
        let another = dragged(&view, vcx, &[(MediaType::Image, "q.png")]);
        view.update_in(vcx, |v, w, cx| v.drop_on_panel(&another, w, cx));
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Video), "the video moved to the tab that takes one");
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::ReferenceVideo, AssetRole::ReferenceImage]);
        });

        // A model with no reference list has nowhere to put a video.
        select_video_model(&view, vcx, "kling-3-pro");
        let before = toasts(vcx);
        let clip = dragged(&view, vcx, &[(MediaType::Video, "other.mp4")]);
        view.update_in(vcx, |v, w, cx| v.drop_on_panel(&clip, w, cx));
        assert_eq!(toasts(vcx), before + 1, "a video has no role to land in");

        // Audio goes to the video tab, beside the references it is one of.
        select_video_model(&view, vcx, "seedance-2.5");
        let sound = dragged(&view, vcx, &[(MediaType::Audio, "voice.wav")]);
        view.update_in(vcx, |v, w, cx| v.drop_on_panel(&sound, w, cx));
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Video), "audio belongs to a video model");
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::ReferenceVideo, AssetRole::ReferenceImage, AssetRole::Audio]);
        });
    }

    // ----- references and their handles ---------------------------------------------------------

    /// A video asset in the library, attached in `role`.
    fn add_clip(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, role: AssetRole) -> AssetId {
        let library = view.read_with(vcx, |v, _| v.library.clone());
        let asset = library.update(vcx, |m, cx| m.import_asset("video/mp4", crate::test_support::mock_clip(), cx).unwrap());
        let id = asset.clone();
        view.update(vcx, move |v, cx| v.add_asset(id, role, cx));
        asset
    }

    /// The handles are per role and per position, so the second image is `@Image2` whatever else is
    /// attached beside it.
    #[gpui::test]
    fn reference_handles_number_within_their_role(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        select_video_model(&view, vcx, "seedance-2.5");
        view.update(vcx, |v, _| assert!(v.state.reference_handles().is_empty(), "no chips before a reference"));

        add(&view, vcx, "a.png", AssetRole::ReferenceImage);
        add_clip(&view, vcx, AssetRole::ReferenceVideo);
        add(&view, vcx, "b.png", AssetRole::ReferenceImage);
        view.update(vcx, |v, _| {
            let handles: Vec<String> = v.state.reference_handles().into_iter().map(|(role, i)| majik_providers::references::handle(role, i)).collect();
            assert_eq!(handles, vec!["@Image1", "@Image2", "@Video1"]);
        });
    }

    /// Wan 2.7's audio slot is a conditioning track, not a reference: no chip for it.
    #[gpui::test]
    fn a_conditioning_track_gets_no_handle(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "fal.ai");
        select_video_model(&view, vcx, "wan-2.7");
        add(&view, vcx, "voice.wav", AssetRole::Audio);
        view.update(vcx, |v, _| {
            assert!(v.state.asset_constraints().accepts(AssetRole::Audio));
            assert!(v.state.reference_handles().iter().all(|(role, _)| *role != AssetRole::Audio), "the track is not addressable");
        });
    }

    /// Clicking a chip types the handle where the caret is, with the spacing the user would use.
    #[gpui::test]
    fn a_tag_chip_inserts_its_handle_at_the_caret(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        select_video_model(&view, vcx, "seedance-2.5");
        add(&view, vcx, "a.png", AssetRole::ReferenceImage);
        add(&view, vcx, "b.png", AssetRole::ReferenceImage);

        view.update_in(vcx, |v, w, cx| v.insert_handle("@Image1".into(), w, cx));
        assert_eq!(view.read_with(vcx, |v, cx| v.prompt_text(cx)), "@Image1 ", "nothing typed yet: no leading space");
        view.update_in(vcx, |v, w, cx| {
            v.prompt.update(cx, |s, cx| s.insert("waves at", w, cx));
            v.insert_handle("@Image2".into(), w, cx);
        });
        assert_eq!(view.read_with(vcx, |v, cx| v.prompt_text(cx)), "@Image1 waves at @Image2 ");
    }

    /// A clip is built between its frames, so those pickers lead the row — whatever order the
    /// model's constraint map (keyed by `AssetRole`) holds them in.
    #[gpui::test]
    fn the_frame_pickers_come_before_the_reference_pickers(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        select_video_model(&view, vcx, "seedance-2.5");
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        let mut left_of = |role: AssetRole| {
            let selector: &'static str = Box::leak(picker_key(role).to_string().into_boxed_str());
            let bounds = vcx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is on the panel"));
            (bounds.origin.y, bounds.origin.x)
        };
        let (first, last, reference) = (left_of(AssetRole::FirstFrame), left_of(AssetRole::LastFrame), left_of(AssetRole::ReferenceImage));
        assert!(first < last, "first frame before last frame: {first:?} {last:?}");
        assert!(last < reference, "the frames before the references: {last:?} {reference:?}");
    }

    /// References and frames go to different endpoints, so filling one side closes the other.
    #[gpui::test]
    fn references_and_frames_close_each_other(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        select_video_model(&view, vcx, "seedance-2.5");
        view.update(vcx, |v, _| {
            assert!(v.state.role_is_open(AssetRole::FirstFrame) && v.state.role_is_open(AssetRole::ReferenceImage));
        });

        add(&view, vcx, "a.png", AssetRole::ReferenceImage);
        view.update(vcx, |v, _| {
            assert!(!v.state.role_is_open(AssetRole::FirstFrame), "a reference closes the frames");
            assert!(!v.state.role_is_open(AssetRole::LastFrame));
            assert!(v.state.role_is_open(AssetRole::ReferenceImage), "more references still fit");
            assert_eq!(v.state.first_available_image_role(), Some(AssetRole::ReferenceImage));
        });

        view.update(vcx, |v, cx| {
            v.remove_asset(0, cx);
            v.state.clear_active_assets();
        });
        add(&view, vcx, "frame.png", AssetRole::FirstFrame);
        view.update(vcx, |v, _| {
            assert!(!v.state.role_is_open(AssetRole::ReferenceImage), "a frame closes the references");
            assert!(v.state.role_is_open(AssetRole::LastFrame), "the other frame still fits");
        });
    }

    /// The handles only mean something because the row stores role and position.
    #[gpui::test]
    fn generate_stores_every_reference_in_order(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        select_video_model(&view, vcx, "seedance-2.5");
        let first = add(&view, vcx, "a.png", AssetRole::ReferenceImage);
        add_clip(&view, vcx, AssetRole::ReferenceVideo);
        let second = add(&view, vcx, "b.png", AssetRole::ReferenceImage);
        set_prompt(&view, vcx, "@Image2 waves at @Video1");

        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 1);
        let id = rows[0].id.clone();
        let inputs = e.library.read_with(vcx, |m, _| m.lib.inputs(&id));
        let stored: Vec<(String, usize, AssetId)> = inputs.iter().map(|(link, asset)| (link.role.clone(), link.position, asset.id.clone())).collect();
        assert_eq!(
            stored,
            vec![
                ("reference_image".to_string(), 0, first),
                ("reference_image".to_string(), 1, second),
                ("reference_video".to_string(), 0, stored[2].2.clone()),
            ],
            "each role numbered from zero in attach order — @Image2 is the second image"
        );
        assert_eq!(request_of(&rows[0]).prompt, "@Image2 waves at @Video1", "the canonical prompt is what is stored");
    }

    /// Recreate has to hand the same references back in the same order, or every handle in the
    /// prompt it restores would point somewhere else.
    #[gpui::test]
    fn recreate_restores_references_in_handle_order(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        select_video_model(&view, vcx, "seedance-2.5");
        add(&view, vcx, "a.png", AssetRole::ReferenceImage);
        add_clip(&view, vcx, AssetRole::ReferenceVideo);
        add(&view, vcx, "b.png", AssetRole::ReferenceImage);
        set_prompt(&view, vcx, "@Image2 waves at @Video1");
        let before: Vec<AssetId> = view.read_with(vcx, |v, _| v.state.assets.video.iter().map(|a| a.asset.clone()).collect());
        let rows = generate_rows(&view, vcx, &e);
        let id = rows[0].id.clone();

        view.update(vcx, |v, _| v.state.clear_active_assets());
        view.update_in(vcx, |v, w, cx| v.apply(crate::state::PendingCompose { recreate: Some(id) }, w, cx));
        vcx.run_until_parked();
        view.update(vcx, |v, _| {
            assert_eq!(roles(&v.state.assets.video), vec![AssetRole::ReferenceImage, AssetRole::ReferenceImage, AssetRole::ReferenceVideo]);
            let ids: Vec<AssetId> = v.state.assets.video.iter().map(|a| a.asset.clone()).collect();
            assert_eq!(ids[0], before[0], "the first image is still @Image1");
            assert_eq!(ids[1], before[2], "the second image is still @Image2");
            let handles: Vec<String> = v.state.reference_handles().into_iter().map(|(role, i)| majik_providers::references::handle(role, i)).collect();
            assert_eq!(handles, vec!["@Image1", "@Image2", "@Video1"]);
        });
        assert_eq!(view.read_with(vcx, |v, cx| v.prompt_text(cx)), "@Image2 waves at @Video1");
    }

    // ----- recreate (loadRecreateSettings) -----

    #[gpui::test]
    fn recreate_replaces_only_target_tab_and_toasts_once(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        switch_to(&view, vcx, MediaType::Video);
        add(&view, vcx, "/tmp/old-first.png", AssetRole::FirstFrame);
        switch_to(&view, vcx, MediaType::Image);

        let before = toasts(vcx);
        let stored = recreate(&view, vcx, video_request(&video::VEO_31, 8), vec![("first_frame", "stored-first.png")]);
        assert_eq!(toasts(vcx), before, "supported model and settings: no warning");
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Video));
            assert_eq!(v.state.assets.video, vec![DraftAsset { asset: stored[0].clone(), role: AssetRole::FirstFrame }], "replaced, not appended");
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage], "the image tab is untouched");
            assert_eq!(v.prompt_text(cx), "recreated video");
        });

        let before = toasts(vcx);
        recreate(&view, vcx, video_request(&video::SORA_2, 5), vec![]);
        assert_eq!(toasts(vcx), before + 1, "duration 5 isn't a Sora preset: exactly one warning");
        view.update(vcx, |v, _| assert_eq!(v.state.video.duration, 4));
    }

    #[gpui::test]
    fn recreate_unsupported_model_selects_provider_default(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Replicate");
        let before = toasts(vcx);
        recreate(&view, vcx, audio_request(&audio::GEMINI_25_PRO), vec![]);
        assert_eq!(toasts(vcx), before + 1);
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Audio));
            assert_eq!(v.state.audio_model().unwrap().id, "elevenlabs-v3", "the fallback is actually selected");
            assert_eq!(v.state.audio.speaker1.as_ref().unwrap().id, "Rachel");
            assert_eq!(v.prompt_text(cx), "recreated audio");
        });
    }

    #[gpui::test]
    fn recreate_unsupported_modality_keeps_prompt_and_assets(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "OpenRouter");
        set_prompt(&view, vcx, "keep me");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        let before = toasts(vcx);
        recreate(&view, vcx, audio_request(&audio::ELEVEN_LABS_V3), vec![("audio", "x.wav")]);
        assert_eq!(toasts(vcx), before + 1);
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image));
            assert_eq!(v.prompt_text(cx), "keep me");
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage]);
        });
    }

    #[gpui::test]
    fn recreate_keeps_stored_roles_the_model_hides(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        let req = Request::new(
            ProviderId::mock(),
            GenerationType::Image(ImageGenerationSettings { model: image::GEMINI_3_PRO.clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Hd }),
            "p",
            vec![],
        );
        recreate(&view, vcx, req, vec![("reference_image", "r.png"), ("mask_image", "m.png")]);
        view.update(vcx, |v, _| {
            assert_eq!(v.state.assets.image.len(), 2, "stored inputs are kept as they were");
            assert_eq!(roles(&v.state.accepted_assets().into_iter().cloned().collect::<Vec<_>>()), vec![AssetRole::ReferenceImage], "the mask waits for a model with a mask slot");
        });
    }

    #[gpui::test]
    fn recreate_upscale_row_lands_on_upscale_tab_with_its_asset(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        set_prompt(&view, vcx, "keep me");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        let before = toasts(vcx);
        let stored = recreate(&view, vcx, tool_request(&tool::MOCK_UPSCALE), vec![("reference_image", "small.png")]);
        assert_eq!(toasts(vcx), before, "the provider has the model: no warning");
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.tab, ComposeTab::Tool(ToolId::Upscale));
            assert_eq!(v.state.active_tool_model().map(|m| m.id), Some("mock-upscale"));
            assert_eq!(v.state.assets.upscale, vec![DraftAsset { asset: stored[0].clone(), role: AssetRole::ReferenceImage }], "the one image the row was made from");
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage], "the image tab keeps its draft");
            assert_eq!(v.prompt_text(cx), "keep me", "a tool has no prompt to load over what is typed");
        });
    }

    #[gpui::test]
    fn recreate_tool_row_on_provider_without_the_tool_toasts(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "OpenRouter");
        add(&view, vcx, "/tmp/ref.png", AssetRole::ReferenceImage);
        let before = toasts(vcx);
        recreate(&view, vcx, tool_request(&tool::MOCK_REMOVE_BACKGROUND), vec![("reference_image", "small.png")]);
        assert_eq!(toasts(vcx), before + 1, "told the provider has no background remover");
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image), "nothing changed");
            assert_eq!(roles(&v.state.assets.image), vec![AssetRole::ReferenceImage]);
        });
    }

    #[gpui::test]
    fn recreate_of_a_row_that_is_gone_or_unreadable_toasts(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        let before = toasts(vcx);
        view.update_in(vcx, |v, w, cx| v.apply(PendingCompose { recreate: Some(majik_core::model::GenerationId::new()) }, w, cx));
        assert_eq!(toasts(vcx), before + 1, "a row that no longer exists");
        let unreadable = e.library.update(vcx, |m, _| m.lib.add_generating(MediaType::Image, Some("{not json".into()), Some("Mock".into()), Some("Mock".into()), None));
        view.update_in(vcx, |v, w, cx| v.apply(PendingCompose { recreate: Some(unreadable) }, w, cx));
        assert_eq!(toasts(vcx), before + 2, "a request this version can't read");
        view.update(vcx, |v, _| assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image)));
    }

    // ----- tool tabs (Upscale / Remove Background) -----

    fn switch_to_tab(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, tab: ComposeTab) {
        view.update_in(vcx, move |v, w, cx| v.set_tab(tab, w, cx));
    }

    /// `n` copies of the seeded PNG, so tool submits have real image files to read.
    /// `n` distinct PNG files outside the library (distinct content, so distinct assets).
    fn input_pngs(e: &crate::test_support::TestEnv, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let path = e.dir.path().join(format!("input-{i}.png"));
                std::fs::write(&path, majik_core::images::solid_png(6, 6, [i as u8 * 30 + 5, 120, 200])).unwrap();
                path
            })
            .collect()
    }

    fn tool_rows(e: &crate::test_support::TestEnv, vcx: &mut VisualTestContext, tool: ToolId) -> Vec<majik_core::model::Generation> {
        e.library.read_with(vcx, |m, _| m.lib.generations().iter().filter(|i| i.tool == Some(tool)).cloned().collect())
    }

    #[gpui::test]
    fn tool_tabs_listed_for_a_provider_with_tools(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        view.update(vcx, |v, _| {
            let tabs = v.state.supported_tabs();
            assert_eq!(&tabs[..3], &[ComposeTab::Media(MediaType::Image), ComposeTab::Media(MediaType::Video), ComposeTab::Media(MediaType::Audio)]);
            assert_eq!(&tabs[3..], &[ComposeTab::Tool(ToolId::Upscale), ComposeTab::Tool(ToolId::RemoveBackground)]);
        });
    }

    #[gpui::test]
    fn tool_tabs_hidden_for_a_provider_without_tools(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "OpenRouter");
        view.update(vcx, |v, _| assert_eq!(v.state.supported_tabs(), vec![ComposeTab::Media(MediaType::Image)]));
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        view.update(vcx, |v, _| assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image), "an unsupported tab is refused"));
    }

    #[gpui::test]
    fn switching_to_tool_tab_hides_prompt_and_shows_tool_model(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        set_prompt(&view, vcx, "a prompt tools never read");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.tab, ComposeTab::Tool(ToolId::Upscale));
            assert!(!v.takes_prompt(), "the prompt box is gone");
            assert_eq!(v.state.active_tool_model().map(|m| m.name), Some("Mock Upscale"));
            assert_eq!(v.state.asset_constraints().range(AssetRole::ReferenceImage), Some(&(1..=crate::composer_state::TOOL_MAX_IMAGES)));
            assert!(!v.can_generate(cx), "no images → nothing to run, whatever the prompt says");
        });
        switch_to(&view, vcx, MediaType::Image);
        view.update(vcx, |v, cx| {
            assert!(v.takes_prompt());
            assert_eq!(v.prompt_text(cx), "a prompt tools never read", "the prompt survives the round trip");
        });
    }

    #[gpui::test]
    fn tool_generate_is_gated_on_at_least_one_image(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::RemoveBackground));
        let before = e.library.read_with(vcx, |m, _| m.lib.generations().len());
        let toasts_before = toasts(vcx);
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.generations().len()), before, "nothing queued");
        assert_eq!(toasts(vcx), toasts_before + 1, "the user is told to add an image");
    }

    #[gpui::test]
    fn tool_generate_creates_one_row_per_image_with_selected_model(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        add(&view, vcx, "/tmp/image-tab.png", AssetRole::ReferenceImage);
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        add_files(&view, vcx, input_pngs(&e, 3), AssetRole::ReferenceImage);
        view.update(vcx, |v, cx| {
            assert_eq!(v.total(cx), 3);
            assert!(v.can_generate(cx));
        });
        let toasts_before = toasts(vcx);
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        let rows = tool_rows(&e, vcx, ToolId::Upscale);
        assert_eq!(rows.len(), 3, "one row per image");
        for row in &rows {
            assert_eq!(row.status, Status::Generating);
            assert_eq!(row.media_type, MediaType::Image);
            assert_eq!(row.model_name.as_deref(), Some("Mock Upscale"));
            assert_eq!(row.provider.as_deref(), Some("Mock"));
            assert_eq!(e.library.read_with(vcx, |m, _| m.lib.inputs(&row.id).len()), 1, "the input is referenced for retry");
        }
        assert_eq!(toasts(vcx), toasts_before + 1);
        view.update(vcx, |v, cx| {
            assert!(v.state.assets.upscale.is_empty(), "the sent tab is cleared");
            assert_eq!(v.state.assets.image.len(), 1, "the image tab keeps its draft");
            assert!(!v.can_generate(cx));
        });
    }

    #[gpui::test]
    fn tool_generate_with_missing_key_opens_settings_and_keeps_images(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "fal.ai");
        vcx.update(|_, cx| {
            crate::state::keys(cx).delete("fal.ai", cx).detach();
        });
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        add_files(&view, vcx, input_pngs(&e, 1), AssetRole::ReferenceImage);
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert!(tool_rows(&e, vcx, ToolId::Upscale).is_empty(), "no rows without an API key");
        view.update(vcx, |v, _| assert_eq!(v.state.active_assets().len(), 1, "the input stays for after the key is entered"));
    }

    #[gpui::test]
    fn tool_tab_refuses_files_that_are_not_images(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        let bogus = e.dir.path().join("notes.txt");
        std::fs::write(&bogus, b"not an image").unwrap();
        let toasts_before = toasts(vcx);
        add_files(&view, vcx, vec![bogus], AssetRole::ReferenceImage);
        assert_eq!(toasts(vcx), toasts_before + 1, "told the file isn't a supported image");
        view.update(vcx, |v, _| assert!(v.state.active_assets().is_empty(), "nothing was imported"));
        e.library.read_with(vcx, |m, _| assert!(m.lib.assets().iter().all(|a| a.kind == MediaType::Image), "no asset row for it either"));
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert!(tool_rows(&e, vcx, ToolId::Upscale).is_empty());
        assert_eq!(toasts(vcx), toasts_before + 2, "generate asks for an image");
    }

    #[gpui::test]
    fn tool_generate_skips_assets_whose_file_is_gone(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        add_files(&view, vcx, input_pngs(&e, 1), AssetRole::ReferenceImage);
        e.library.update(vcx, |m, _| {
            let path = m.lib.assets()[0].path.clone();
            std::fs::remove_file(path).unwrap();
            m.lib.reload().unwrap();
        });
        let toasts_before = toasts(vcx);
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        assert!(tool_rows(&e, vcx, ToolId::Upscale).is_empty());
        assert_eq!(toasts(vcx), toasts_before + 1, "told nothing was usable");
        view.update(vcx, |v, _| assert_eq!(v.state.active_assets().len(), 1, "kept so the user can swap it out"));
    }

    #[gpui::test]
    fn tool_tab_takes_at_most_ten_images(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        for i in 0..12 {
            add(&view, vcx, &format!("/tmp/{i}.png"), AssetRole::ReferenceImage);
        }
        view.update(vcx, |v, cx| {
            assert_eq!(v.state.active_assets().len(), crate::composer_state::TOOL_MAX_IMAGES);
            assert!(v.state.role_is_full(AssetRole::ReferenceImage));
            assert_eq!(v.total(cx), crate::composer_state::TOOL_MAX_IMAGES);
        });
    }

    #[gpui::test]
    fn a_roleless_image_lands_in_tool_tab_when_it_has_room(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::RemoveBackground));
        add_image(&view, vcx, "a.png");
        view.update(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Tool(ToolId::RemoveBackground), "no tab switch while the tool tab has room");
            assert_eq!(roles(&v.state.assets.remove_background), vec![AssetRole::ReferenceImage]);
            assert!(v.state.assets.image.is_empty());
        });
    }

    #[gpui::test]
    fn tool_tab_and_model_round_trip_through_drafts(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::RemoveBackground));
        let draft = view.update(vcx, |v, _| v.state.to_draft());
        assert_eq!(draft.media_type.as_deref(), Some("removeBackground"));
        assert_eq!(draft.upscale.model_id.as_deref(), Some("mock-upscale"));
        assert_eq!(draft.remove_background.model_id.as_deref(), Some("mock-remove-background"));
        let restored = ComposerState::new(majik_providers::mock::descriptor(), &draft);
        assert_eq!(restored.tab, ComposeTab::Tool(ToolId::RemoveBackground));
        assert_eq!(restored.active_tool_model().map(|m| m.id), Some("mock-remove-background"));
        let restored = ComposerState::new(majik_providers::openrouter::descriptor(), &draft);
        assert_eq!(restored.tab, ComposeTab::Media(MediaType::Image), "a provider without the tool falls back to its first tab");
    }

    #[gpui::test]
    fn generate_action_on_tool_tab_runs_tool(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::RemoveBackground));
        add_files(&view, vcx, input_pngs(&e, 2), AssetRole::ReferenceImage);
        view.update_in(vcx, |v, w, cx| w.focus(&v.focus, cx));
        vcx.dispatch_action(Generate);
        vcx.run_until_parked();
        let rows = tool_rows(&e, vcx, ToolId::RemoveBackground);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.model_name.as_deref() == Some("Mock Remove Background")));
    }

    fn paste(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext) {
        view.update_in(vcx, |v, w, cx| v.paste_image(&PasteImage, w, cx));
        vcx.run_until_parked();
    }

    /// The library file behind the active tab's first draft asset.
    fn first_draft_path(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, e: &crate::test_support::TestEnv) -> PathBuf {
        let asset = view.read_with(vcx, |v, _| v.state.accepted_assets()[0].asset.clone());
        e.library.read_with(vcx, |m, _| m.lib.asset(&asset).expect("draft references a library asset").path.clone())
    }

    #[gpui::test]
    fn paste_image_adds_the_clipboard_png_as_an_asset(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        let png = majik_core::images::solid_png(8, 8, [10, 20, 30]);
        vcx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Png, png.clone())));
        let before = toasts(vcx);
        paste(&view, vcx);
        view.read_with(vcx, |v, _| {
            let assets = v.state.accepted_assets();
            assert_eq!(assets.len(), 1);
            assert_eq!(assets[0].role, AssetRole::ReferenceImage);
        });
        let path = first_draft_path(&view, vcx, &e);
        assert!(path.starts_with(e.dir.path()), "pasted straight into the library, no temp file");
        assert_eq!(std::fs::read(&path).expect("pasted file"), png, "the PNG is stored untouched");
        assert_eq!(toasts(vcx), before + 1, "Pasted image toast");
    }

    #[gpui::test]
    fn paste_image_transcodes_non_png_clipboard_images(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        let png = majik_core::images::solid_png(8, 8, [10, 20, 30]);
        let jpeg = ::image::load_from_memory(&png).expect("png decodes").into_rgb8();
        let mut bytes = Vec::new();
        jpeg.write_to(&mut std::io::Cursor::new(&mut bytes), ::image::ImageFormat::Jpeg).expect("jpeg encodes");
        vcx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes)));
        paste(&view, vcx);
        view.read_with(vcx, |v, _| assert_eq!(v.state.accepted_assets().len(), 1));
        let path = first_draft_path(&view, vcx, &e);
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert!(::image::load_from_memory(&std::fs::read(&path).unwrap()).is_ok(), "stored as a decodable PNG");
    }

    #[gpui::test]
    fn paste_without_an_image_only_toasts(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        vcx.write_to_clipboard(gpui::ClipboardItem::new_string("just text".into()));
        let before = toasts(vcx);
        paste(&view, vcx);
        view.read_with(vcx, |v, _| assert!(v.state.accepted_assets().is_empty()));
        assert_eq!(toasts(vcx), before + 1, "No image on the clipboard toast");
    }

    #[gpui::test]
    fn paste_image_on_the_audio_tab_switches_to_image(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Audio);
        vcx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Png, majik_core::images::solid_png(8, 8, [1, 2, 3]))));
        paste(&view, vcx);
        view.read_with(vcx, |v, _| {
            assert_eq!(v.state.tab, ComposeTab::Media(MediaType::Image), "no image slot on audio: falls back to the image tab");
            assert_eq!(v.state.accepted_assets().len(), 1);
        });
    }

    // ----- request shape: what actually goes to the provider ------------------------------------

    fn select_video_model(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, id: &str) {
        view.update_in(vcx, |v, window, cx| {
            v.set_media_type(MediaType::Video, window, cx);
            let ix = v.state.provider.supported_video_models.iter().position(|m| m.id == id).expect("model in provider");
            v.select_model(ix, cx);
        });
    }

    fn library_ids(e: &crate::test_support::TestEnv, vcx: &mut VisualTestContext) -> Vec<majik_core::model::GenerationId> {
        e.library.read_with(vcx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect())
    }

    /// Generate from the current tab and return the rows it queued (newest first is not guaranteed,
    /// so they are whatever wasn't there before).
    fn generate_rows(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext, e: &crate::test_support::TestEnv) -> Vec<majik_core::model::Generation> {
        let before = library_ids(e, vcx);
        view.update_in(vcx, |v, w, cx| v.generate(w, cx));
        vcx.run_until_parked();
        e.library.read_with(vcx, |m, _| m.lib.generations().iter().filter(|i| !before.contains(&i.id)).cloned().collect())
    }

    fn request_of(item: &majik_core::model::Generation) -> Request {
        Request::from_json(item.request_json.as_deref().expect("row stores its request")).expect("request parses")
    }

    fn stored_roles(e: &crate::test_support::TestEnv, vcx: &mut VisualTestContext, id: &majik_core::model::GenerationId) -> Vec<String> {
        e.library.read_with(vcx, |m, _| m.lib.inputs(id).into_iter().map(|(link, _)| link.role).collect())
    }

    #[gpui::test]
    fn count_stepper_clamps_between_1_and_8(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        view.update(vcx, |v, cx| {
            v.state.image.count = 1;
            v.step_count(-1, cx);
            assert_eq!(v.state.image.count, 1, "minus at 1 stays at 1");
            for _ in 0..10 {
                v.step_count(1, cx);
            }
            assert_eq!(v.state.image.count, 8, "plus never exceeds 8");
            v.step_count(-1, cx);
            assert_eq!(v.state.image.count, 7);
        });
    }

    #[gpui::test]
    fn image_aspect_ratio_resolution_and_count_reach_the_request(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        select_image_model(&view, vcx, "gemini-3.1-flash");
        view.update(vcx, |v, _| {
            v.state.image.aspect_ratio = Some(AspectRatio::Landscape);
            v.state.image.resolution = Some(ImageResolution::Fhd);
            v.state.image.count = 3;
        });
        set_prompt(&view, vcx, "three wide ones");
        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 3, "one row per requested image");
        for row in &rows {
            assert_eq!(row.media_type, MediaType::Image);
            assert_eq!(row.status, Status::Generating);
            let request = request_of(row);
            assert_eq!(request.prompt, "three wide ones");
            match request.generation_type {
                GenerationType::Image(s) => {
                    assert_eq!(s.model.id, "gemini-3.1-flash");
                    assert_eq!(s.aspect_ratio, AspectRatio::Landscape);
                    assert_eq!(s.resolution, ImageResolution::Fhd);
                }
                other => panic!("expected an image request, got {other:?}"),
            }
        }
    }

    #[gpui::test]
    fn first_and_last_frame_inputs_are_both_stored_with_the_request(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        let pngs = input_pngs(&e, 2);
        add_files(&view, vcx, vec![pngs[0].clone()], AssetRole::FirstFrame);
        add_files(&view, vcx, vec![pngs[1].clone()], AssetRole::LastFrame);
        view.update(vcx, |v, _| {
            assert_eq!(roles(&v.state.accepted_assets().into_iter().cloned().collect::<Vec<_>>()), [AssetRole::FirstFrame, AssetRole::LastFrame]);
        });
        set_prompt(&view, vcx, "morph");
        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 1);
        assert_eq!(stored_roles(&e, vcx, &rows[0].id), ["first_frame", "last_frame"], "both role slots travel with the request");
    }

    #[gpui::test]
    fn text_to_video_submits_a_video_request_without_inputs(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        set_prompt(&view, vcx, "a slow pan");
        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].media_type, MediaType::Video);
        assert_eq!(rows[0].model_name.as_deref(), Some("Veo 3.1"));
        let request = request_of(&rows[0]);
        assert_eq!(request.prompt, "a slow pan");
        assert!(matches!(request.generation_type, GenerationType::Video(ref s) if s.model.id == "veo-3.1"));
        assert!(stored_roles(&e, vcx, &rows[0].id).is_empty());
    }

    #[gpui::test]
    fn image_to_video_stores_the_start_frame(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        add_files(&view, vcx, input_pngs(&e, 1), AssetRole::FirstFrame);
        set_prompt(&view, vcx, "animate this");
        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 1);
        assert_eq!(stored_roles(&e, vcx, &rows[0].id), ["first_frame"]);
    }

    #[gpui::test]
    fn video_settings_reach_the_request(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        view.update(vcx, |v, _| {
            v.state.video.aspect_ratio = Some(VideoAspectRatio::Tall);
            v.state.video.resolution = Some(VideoResolution::Fhd);
            v.state.video.duration = 6;
            v.state.video.audio = false;
        });
        set_prompt(&view, vcx, "portrait, silent");
        let rows = generate_rows(&view, vcx, &e);
        match request_of(&rows[0]).generation_type {
            GenerationType::Video(s) => {
                assert_eq!(s.aspect_ratio, Some(VideoAspectRatio::Tall), "9:16");
                assert_eq!(s.resolution, Some(VideoResolution::Fhd));
                assert_eq!(s.duration, 6);
                assert!(!s.audio_enabled);
            }
            other => panic!("expected a video request, got {other:?}"),
        }
    }

    #[gpui::test]
    fn video_count_makes_one_row_per_video(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        view.update(vcx, |v, cx| {
            v.step_count(1, cx);
            v.step_count(1, cx);
            assert_eq!(v.state.video.count, 3, "the stepper edits the video count on the video tab");
            assert_eq!(v.state.image.count, 1, "and leaves the image count alone");
        });
        set_prompt(&view, vcx, "three clips");
        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 3, "one row per requested video");
        for row in &rows {
            assert_eq!(row.media_type, MediaType::Video);
            assert_eq!(request_of(row).prompt, "three clips");
        }
    }

    #[gpui::test]
    fn count_stepper_does_nothing_on_the_audio_tab(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Audio);
        view.update(vcx, |v, cx| {
            v.step_count(1, cx);
            assert_eq!(v.state.count(), 1);
            assert_eq!((v.state.image.count, v.state.video.count), (1, 1));
        });
    }

    #[gpui::test]
    fn video_audio_toggle_reaches_the_request(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        view.update(vcx, |v, _| {
            assert!(v.state.video_caps().unwrap().supports_audio_toggle(), "veo exposes the toggle");
            v.state.video.audio = true;
        });
        set_prompt(&view, vcx, "with sound");
        let rows = generate_rows(&view, vcx, &e);
        assert!(matches!(request_of(&rows[0]).generation_type, GenerationType::Video(ref s) if s.audio_enabled));
    }

    // ----- audio: speakers and the voice picker -----------------------------------------------

    #[gpui::test]
    fn audio_capable_provider_offers_two_speakers(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Audio);
        let (voice, other) = view.read_with(vcx, |v, _| {
            let caps = v.state.audio_caps().expect("audio model has capabilities");
            assert!(caps.supports_two_speakers, "speaker 2 capsule is offered");
            assert!(v.state.audio.speaker1.is_some(), "speaker 1 starts on the default voice");
            assert!(v.state.audio.speaker2.is_none(), "speaker 2 starts on None");
            (caps.supported_voices[0].clone(), caps.supported_voices[1].clone())
        });
        view.update_in(vcx, |v, w, cx| {
            v.set_voice(crate::views::voice_picker::Speaker::Two, Some(voice.clone()), w, cx);
            assert_eq!(v.state.audio.speaker2.as_ref().map(|s| &s.id), Some(&voice.id));
            v.set_voice(crate::views::voice_picker::Speaker::Two, None, w, cx);
            assert!(v.state.audio.speaker2.is_none(), "speaker 2 can be cleared");
            v.set_voice(crate::views::voice_picker::Speaker::One, Some(other.clone()), w, cx);
            v.set_voice(crate::views::voice_picker::Speaker::One, None, w, cx);
            assert_eq!(v.state.audio.speaker1.as_ref().map(|s| &s.id), Some(&other.id), "speaker 1 can't be cleared");
        });
    }

    #[gpui::test]
    fn speakers_reach_the_audio_request(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Audio);
        let second = view.read_with(vcx, |v, _| v.state.audio_caps().unwrap().supported_voices[1].clone());
        view.update_in(vcx, |v, w, cx| v.set_voice(crate::views::voice_picker::Speaker::Two, Some(second.clone()), w, cx));
        set_prompt(&view, vcx, "Speaker 1: hello\nSpeaker 2: hi");
        let rows = generate_rows(&view, vcx, &e);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].media_type, MediaType::Audio);
        match request_of(&rows[0]).generation_type {
            GenerationType::Audio(s) => {
                assert!(!s.speaker1.id.is_empty());
                assert_eq!(s.speaker2.map(|v| v.id), Some(second.id));
            }
            other => panic!("expected an audio request, got {other:?}"),
        }
    }

    #[gpui::test]
    fn voice_picker_opens_as_a_dialog_and_closes_on_selection(cx: &mut TestAppContext) {
        use gpui_component::WindowExt as _;
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Audio);
        let voices = view.read_with(vcx, |v, _| v.state.audio_caps().unwrap().supported_voices.clone());
        let picked = voices[1].clone();
        view.update_in(vcx, |v, window, cx| {
            assert!(!window.has_active_dialog(cx));
            crate::views::voice_picker::open_voice_picker(cx.entity().downgrade(), crate::views::voice_picker::Speaker::One, voices.clone(), v.state.audio.speaker1.clone(), false, window, cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| assert!(window.has_active_dialog(cx), "the picker is a dialog over the composer"));
        // What a voice row's click does: pick, then close.
        view.update_in(vcx, |v, window, cx| {
            v.set_voice(crate::views::voice_picker::Speaker::One, Some(picked.clone()), window, cx);
            window.close_dialog(cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
        view.read_with(vcx, |v, _| assert_eq!(v.state.audio.speaker1.as_ref().map(|s| &s.id), Some(&picked.id)));
    }

    // ----- cost estimate -----
    //
    // Mock prices synthetically ($0.01/image, $0.10/s video, $0.15/s with audio, $0.0001 a spoken
    // character, $0.02 a tool run), so these pin the caption the user reads without depending on a
    // real provider's figures. `cost_caption` returns exactly the string `render` draws.

    fn caption(view: &gpui::Entity<ComposeView>, vcx: &mut VisualTestContext) -> Option<String> {
        view.read_with(vcx, |v, cx| v.cost_caption(cx).map(|c| c.to_string()))
    }

    #[gpui::test]
    fn cost_shows_the_per_output_price_before_anything_is_typed(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.01 each"), "the settings cost this much per image");
    }

    #[gpui::test]
    fn cost_totals_the_batch_once_there_is_a_prompt(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.01 estimated"));
        view.update(vcx, |v, cx| {
            for _ in 0..3 {
                v.step_count(1, cx);
            }
        });
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.04 estimated"), "4 images at $0.01");
    }

    #[gpui::test]
    fn cost_tracks_video_duration_and_the_audio_toggle(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        set_prompt(&view, vcx, "a cat");
        view.update(vcx, |v, _| {
            v.state.video.duration = 8;
            v.state.video.audio = false;
        });
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.80 estimated"), "8 s at $0.10/s");
        view.update(vcx, |v, _| v.state.video.audio = true);
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $1.20 estimated"), "audio costs more per second");
        view.update(vcx, |v, _| v.state.video.duration = 4);
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.60 estimated"), "half the video, half the price");
    }

    #[gpui::test]
    fn cost_multiplies_video_duration_by_the_batch(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Video);
        select_video_model(&view, vcx, "veo-3.1");
        set_prompt(&view, vcx, "a cat");
        view.update(vcx, |v, cx| {
            v.state.video.duration = 8;
            v.state.video.audio = true;
            v.step_count(1, cx);
        });
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $2.40 estimated"), "two 8 s clips with audio");
    }

    #[gpui::test]
    fn cost_says_so_when_the_model_has_no_price(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        set_prompt(&view, vcx, "a cat");
        select_image_model(&view, vcx, majik_providers::mock::pricing::UNPRICED_MODEL_ID);
        assert_eq!(caption(&view, vcx).as_deref(), Some("No estimate available"), "never guess a number");
    }

    #[gpui::test]
    fn cost_scales_a_tool_tab_with_the_number_of_images(cx: &mut TestAppContext) {
        let (view, vcx, e) = compose_window!(cx, "Mock");
        switch_to_tab(&view, vcx, ComposeTab::Tool(ToolId::Upscale));
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.02 each"), "nothing to upscale yet");
        add_files(&view, vcx, input_pngs(&e, 3), AssetRole::ReferenceImage);
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.06 estimated"), "one $0.02 run per image");
    }

    #[gpui::test]
    fn cost_on_the_audio_tab_waits_for_text_to_price(cx: &mut TestAppContext) {
        let (view, vcx, _e) = compose_window!(cx, "Mock");
        switch_to(&view, vcx, MediaType::Audio);
        assert_eq!(caption(&view, vcx), None, "audio bills per character; with no text there is nothing to say");
        set_prompt(&view, vcx, &"a".repeat(1_000));
        assert_eq!(caption(&view, vcx).as_deref(), Some("≈ $0.10 estimated"), "1000 characters at $0.0001");
    }
}
