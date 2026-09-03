# CLAUDE.md

Majik's architecture and house style, and the guidance Claude Code (claude.ai/code) works from.
Read it before your first change, human or otherwise.

Majik: a cross-platform desktop app for generating images, video and audio through hosted model
providers (fal / Replicate / OpenRouter), built in Rust on GPUI.

## Cross-platform first

This app ships on macOS, Windows, and Linux. **Check every decision against all three platforms
first.** Before choosing an approach, ask whether it works, or has a clear route to working, on all
three. Prefer something GPUI (or another portable crate) already provides everywhere over a
macOS-only version we maintain ourselves, even when the macOS-only one is nicer today: we pay for
platform-specific code forever, and it blocks porting. When platform-native code is unavoidable, put
it behind a trait with a `#[cfg(target_os)]` backend and a stub for the others, as `majik-platform`
and `majik-dragout` already do, and say in the PR what happens on Windows and Linux. Use a macOS-only
convenience only as a deliberate stopgap you note; never as the default. For example, drag-out uses
GPUI's native drag, which covers macOS and Wayland now and gets Windows/X11 upstream for free,
rather than our own macOS-only `NSDraggingSession`.

## Commands

```sh
cargo run -p majik-app                          # Dev channel: its own library/config (or MAJIK_LIBRARY=… / first CLI arg / config.json)
cargo test --workspace                          # all tests (~1140; includes 30+ headless GPUI view tests)
cargo test -p majik-providers --test fal        # one integration-test file
cargo test -p majik-app views::feed::tests::    # one view's tests; add a name to run a single test
cargo test -p majik-providers --test e2e -- --ignored smoke   # live providers: the cheap tier (real money)
cargo clippy --workspace --all-targets -- -D warnings
./script/bundle-mac [target-triple]             # → a signed, notarized DMG (ad-hoc + unnotarized without the secrets)
script/bump-version minor                       # bump, commit, tag; pushing the tag runs .github/workflows/release.yml
cargo run --release -p majik-generation --example seed_library -- ~/majik-perf \
    --images 9000 --videos 800 --audio 200 --thumbnails   # a library to measure against (`--help`)
gh workflow run platforms.yml                   # on-demand 3-OS matrix + full macOS app build (billed 10x; not on push)
```

- **Build channels** (`config.rs`): `script/lib/release.sh` is the only place that exports
  `MAJIK_CHANNEL=stable`, so the bundles the release scripts produce are the only `Stable` builds;
  everything else — `cargo run`, `cargo run --release`, `cargo test` — is `Dev`. The channel names the
  app-data folder (macOS `com.app.majik[-dev]`, Windows `Majik[ Dev]`, Linux `majik[-dev]`), so the
  two are independent installs with their own library, `config.json`, `drafts.json`, window frames
  and bundle id, and you can wipe the dev one without touching the app you use. A test pins Stable's
  paths — never rename them. API keys are the one deliberate exception
  (`config::credentials_dir` is always the stable folder, `KEYCHAIN_URL` carries no channel), so you
  enter a key once; the cost is that removing one removes it for both. Channel and build profile are
  independent: `cargo run --release` is Dev-channel but uses the keychain and has no Mock provider,
  because `credentials.rs`, `mock/mod.rs` and the Settings Debug section still switch on
  `cfg!(debug_assertions)`. Anything that builds a shippable binary without going through a bundle
  script silently ships a Dev-channel app, so each script calls `require_channel_marker` on the
  binary before packaging it, and `majik --channel` prints the stamp.
- Video is in-process on every platform (`majik_core::video`: `re_mp4` demux + openh264 decode/encode,
  built from vendored source — no ffmpeg anywhere, also not in tests). H.264 in MP4 only.
- Dev switches (env): `MAJIK_MOCK_KEYS=1` (in-memory key for Mock — use this with the Mock provider),
  `MAJIK_COMPOSE=1` (start with the composer panel open), `MAJIK_OPEN=<index|video|audio>`,
  `MAJIK_AUTOPLAY=1`, `MAJIK_GENERATE="prompt"`, `MAJIK_USE_KEYCHAIN=1` (debug builds otherwise keep
  API keys in `development_credentials.json` — unsigned/ad-hoc binaries change identity
  every build, and a login-keychain item's ACL is bound to the creating binary's signature).
  `MAJIK_CHANNEL=stable` is a *build-time* stamp set by `script/lib/release.sh`, not a runtime switch;
  an unknown value fails the build.
  Mock prompt directives: `#delay:5`, `#fail:rateLimited`.
- Performance work runs against a seeded library (`majik_generation::seed`): rows, attempts, assets
  and files written the way the library writes them, with locally rendered media, every status the
  feed shows, favorites, albums, tool rows and imports. 10k generations take ~23 s and 13.4 GB at the
  default 1024 px (`--long-edge 512` is ~3.5 GB); `--pool 0` renders a distinct image per row.
- **Live-API tests** (`crates/majik-providers/tests/e2e.rs`) call the real providers, so every one is
  `#[ignore]`d: `cargo test` skips them and says how many. Run them with `--ignored`, filtered by
  module — `smoke` (one model per provider per media type), `fal::video`, `errors` (invalid keys;
  needs no key of your own). Keys come from `FAL_API_KEY` / `OPENROUTER_API_KEY` /
  `REPLICATE_API_KEY`; a missing key skips that provider loudly, so run with only the keys you hold.
  An unfiltered run is ~170 paid calls including 60 video renders — pass `--test-threads=2` and
  expect to wait. `majik-generation` has its own live suite (`--test e2e`) for prompt improvement as
  the app does it, using the shipped instruction and the engine rather than the raw client. Both
  binaries share one tokio runtime (`rt()` / a static `Engine`) because `http::client()` is a
  process-wide `reqwest::Client` bound to whichever runtime first uses it, and a runtime per test
  poisons it for the next one. These suites detect provider changes rather than regressions: a
  failure may mean the provider changed. Neither is ever run in CI.
- CI on push (`ci.yml`) only builds/tests/clippies the portable crates on Linux
  (`core storage providers generation audio`); `majik-app`, `majik-video`, `majik-platform`,
  `majik-dragout` are only checked by the manual `platforms.yml` run. Run the workspace clippy locally.

## Architecture

Dependency direction (never reversed): `app → {generation, video, audio, platform, dragout} → providers → core → storage`.

**Vocabulary** (flarly's, kept the same in code, docs and tests):
- **Asset** — a file the library holds (an output, an input, an import), content-addressed; carries no role.
- **Generation** — one row the app made from a request: the `Request`, its input assets with roles,
  and a pointer to its active attempt. This is what the Library and album feeds list.
- **Attempt** (`GenerationJob`) — one provider run of a generation; a retry is the next attempt. The
  row mirrors the active one; every HTTP exchange of it is a **trace**.
- **Request** — everything needed to (re)run a generation: provider, `GenerationType` (a media
  kind's settings, or a tool over one input), prompt. Stored on the row; Recreate loads it back.
- **Tool** — Upscale / Remove Background: a generation whose request is `Upscale` /
  `RemoveBackground(ToolSettings)`; shown as its own composer tab. A tool model declares the media it
  works on (`ToolModel::media`), so the one Upscale tab takes an image or a clip depending on the
  model picked, and the row's media type is the model's.
- **Draft** — the composer's per-provider state (`ComposerState`, persisted in `drafts.json`); a
  **`DraftAsset`** is an asset attached in a role, sent as an `AssetInput` when generating.
- **Entry** — what a grid or the detail shows: a generation or an asset (`EntryId`).

- **No GPUI below `majik-app`.** `core`/`providers`/`generation` are plain Rust so they're testable and
  reusable. Platform code (objc2 / AppKit) lives only in `majik-platform` and `majik-dragout`, each with a
  `#[cfg(target_os)]` backend and a stub for other OSes. `majik-video` is plain Rust (it depends on
  `majik-audio` for the sound track): a `Player` state machine the app drives with decode jobs on its
  background executor, so playback is testable headlessly.
- **`majik-core::Library`** is the synchronous domain model: SQLite (`<library>/.majik/library.db`) is
  the source of truth, with flarly's shape. An **`Asset`** is a file the library holds — a generation's
  output (`<uuid>.<ext>` in the root), an input, or an import (`.majik/assets/<sha256>.<ext>`,
  content-addressed so the same bytes are one asset); assets carry no role. A **`Generation`** is a
  generation: it stores no file, it references its output (`output_asset_id`) and its inputs
  (`generation_inputs(generation_id, asset_id, role, position)`), so using an output as an input again shares
  the row — never copy bytes. Files merely dropped into the folder are not assets. On open every
  asset is checked against the folder: one whose file is gone is `Asset::missing` and its generation
  `Status::Missing` (shown with an error, never dropped, retry regenerates in place; derived, never
  stored). Deleting a generation soft-deletes it (`deleted_at`) and leaves its assets alone; an asset
  can only be trashed (`.majik/trash/`, nothing is ever hard-deleted) once no live generation
  references it (`Library::is_referenced`). Thumbnails (`.majik/thumbs/`) belong to assets. All file
  content goes through `majik-storage::BlobStore` (relative keys; local backend today, an S3 one
  later) — don't touch library files with `std::fs` directly. A generation's provider runs are
  **`generation_jobs`** rows (flarly's `generationJobs`): one per attempt (`attempt`, `status`
  `queued | running | completed | failed | canceled`, the provider's `external_id` / `poll_url`, the
  output asset, the error, timestamps, and the provider request / create response / final response
  bodies), with every HTTP exchange in `generation_job_traces`; `generations.active_job_id` points at the
  current one and the row's `status` / `error` / `job_id` mirror it (`Library` writes both in one
  transaction; `load_generations` joins the rest back). Retry = `start_attempt` (a new job, refused while
  one is in flight); engine `Event`s name their attempt and a stale one's are dropped. Jobs and
  traces are read on demand (`Library::active_job / jobs / traces`), never cached. **Schema:** one
  DDL in `db.rs` (`SCHEMA`, `SCHEMA_VERSION`); pre-release there are no migrations — change the DDL
  in place, bump the version, and an older `library.db` is recreated on open.
- **`majik-providers`** is the provider layer: descriptors, model catalogs, capability tables,
  price tables, error mapping, HTTP clients (fal / Replicate / OpenRouter / Mock). **Pricing**
  (`pricing.rs` + a `pricing.rs` per provider) answers "what will this job cost?" for the composer's
  live estimate. Prices are per provider *per model* — the same catalog model costs a different
  amount on each — so they live on `ProviderDescriptor::pricing`, never on the id-only catalog
  structs. Amounts are `Usd` micro-dollars (integers, so the arithmetic and the rendered string
  can't disagree).
  A model with no figure is `Estimate::Unknown` and the UI says so rather than guessing; the
  `every_supported_model_is_priced_or_listed_as_unpriced` guard means leaving a model unpriced has
  to be deliberate. Prices change, so every table entry carries the date it was checked.
  Tests are wiremock request-shape tests in `crates/majik-providers/tests/`; `shared.rs` holds the
  cross-provider suite and `e2e.rs` the opt-in live-API suite, whose matrix is generated from the
  catalogs and checked against each descriptor by a guard test that does run on every push.
- **`majik-generation::Engine`** owns a tokio runtime on a background thread (reqwest needs it; GPUI's
  executor doesn't). One `Request` type covers every operation — `GenerationType::Image / Video /
  Audio` and the tools, `Upscale / RemoveBackground(ToolSettings { model, upscale_factor, variant })`
  over one input image or clip —
  and every row stores its request (`generations.request_json`; the `tool` column is derived from it), so
  Recreate and Retry work on tool rows exactly as on generations. Requests go in; `Event`s
  (`Accepted / Trace / Completed / Failed / Cancelled`, each naming its attempt) come out on
  an `async-channel` receiver. Retry-once, per-media-type stale deadlines, `CancellationToken` per job.
- **`majik-app` state flow.** `AppState` (GPUI `Global`) holds `Entity<LibraryModel>` + the
  `ApiKeyStore`. `LibraryModel` wraps `Library` + `Engine`, pumps engine events in a detached
  `cx.spawn` loop, runs thumbnails/probes via `cx.background_spawn`, and funnels every mutation through
  `changed()` (`cx.notify()` + `LibraryEvent::Changed`). Views `cx.observe` the library and rebuild
  their id lists; they never treat a copy of a `Generation` as the source of truth.
- **Windows.** Two singletons in `windows.rs`, frames persisted in `Config`: the `Library` window and
  the `Settings` window (`views/settings.rs`, modelled on Zed's `settings_ui`: a nav pane of pages —
  General, Providers, Storage, Shortcuts, About — beside the page's `title + description | control`
  rows; `windows::open_settings(SettingsTarget)` opens or re-targets it from ⌘, / the menu / the
  sidebar / the composer, and `SettingsTarget::missing_key` is the error-recovery mode). The sidebar
  (left, ⌘⌥S) and the composer (right) are collapsible panels around the feed — one `Side` /
  `SidePanel` mechanism in `LibraryWindow`, open state + width in `Config` — present on every library
  screen (Library, Favorites, Assets, albums) and hidden only while a detail covers the window. The
  grid and the detail are keyed by `EntryId` (a generation or an asset): the Library / Favorites /
  album feeds list generations, the Assets feed lists assets, and the detail shows an asset through a
  generation-shaped `Subject` (an output opened from Assets is shown as its generation). Assets reach
  the composer by reference only: drag cells from any grid onto a role card (`DraggedAssets`, the same
  payload that becomes the native file drag outside the window), or Recreate on a generation — there
  is deliberately no Use Image / "Use as…" menu, the drag says where; files dropped, picked or
  pasted into the composer are imported as assets first, so `DraftAsset` holds an `AssetId`.
  Recreate hands the composer only a `GenerationId` (`PendingCompose { recreate }`); the composer reads
  the row's request and inputs itself and becomes the state that made it: the media tab with model,
  settings, inputs and prompt, or — for an upscale / background removal — that tool's tab with its
  model and the one input image (the prompt stays as typed). Batch count is composer-only, never
  stored or restored.
  Feed/detail reach the composer by emitting
  `FeedEvent::Compose` / `DetailEvent::Compose` (a `WindowHandle` can't re-enter the window that is
  dispatching the action). ⌘N cycles closed → open + prompt focused → closed again once you're already
  typing (`ToggleComposer` is the plain toggle behind the toolbar button / View menu); Escape in the
  prompt returns focus to the feed. **Actions** are declared once in `actions.rs` (`actions!(majik, […])`);
  `actions::shortcuts()` is the single keymap table (bound at init and listed on Settings → Shortcuts)
  with the key contexts `"Library"` (window root), `"Feed"`, `"Detail"`, `"Compose"` (the panel),
  `"Settings"` / `"SettingsNav"`, and the actions are mirrored in the native menu bar. Views set `key_context` and handle actions with
  `.on_action(cx.listener(...))`; bindings resolve along the focus path, so feed shortcuts don't fire
  while the prompt is focused. The composer's own actions (Generate, Improve Prompt, Paste Image) are
  the exception: `LibraryWindow` installs them too while the panel is open and hands them to the
  panel, so ⌘⏎ generates from the feed or a detail.
- **A menu item that can't act is greyed, on every platform.** macOS asks gpui whether the action
  reaches a handler (`validateMenuItem:` → `is_action_available`), so installing an action only where
  it can act — `LibraryWindow` gates its handlers on `.when(…)` — is what greys it. The menu bar we
  draw on Windows and Linux (gpui-component's `AppMenuBar`, since gpui only *stores* the menus off
  macOS) has no such hook and reads a `disabled` flag per item, so `actions::MenuState` mirrors those
  same conditions and `LibraryWindow::sync_menus` rebuilds the drawn menus when they change. Adding a
  menu item means giving it both: a handler where it belongs, and a condition in `MenuState`.
  `every_menu_action_reaches_a_handler` fails on an item that can act in no state at all.
- **Never write `cmd-` in a keystroke — always `secondary-`.** This holds everywhere, in production
  bindings *and* in test keystrokes (`simulate_keystrokes`). `secondary-` is ⌘ on macOS and Ctrl
  elsewhere; a literal `cmd-` is the Windows / Super key off macOS, bound to nothing, so the
  keystroke silently does nothing and the test passes on macOS while failing only on Windows and
  Linux — a bug you cannot see locally. gpui-component's own text-input bindings are split the same
  way (`cmd-z` on macOS, `ctrl-z` / `ctrl-y` elsewhere), so a test that presses `cmd-z` to undo
  exercises nothing off macOS. `actions::tests::no_binding_uses_the_cmd_modifier` scans every source
  file in the crate and fails with the offending file and line; keep it that way rather than
  narrowing it. The same trap applies to anything else that differs per platform behind a
  `#[cfg(target_os)]` in a dependency — prefer a runtime `cfg!(...)` branch in our own code, which
  is type-checked on every platform, over `#[cfg]`, which is only compiled on one.
- **Preferences vs library state.** `Config` (`config.json` in the channel's app-data dir, a `Global`)
  holds app preferences: provider, appearance, columns, draft prompt. Everything about media lives in
  the library DB. In tests no config dir is set, so `Config::save` is a no-op.
- **View tests** (`crates/majik-app/src/test_support.rs`): `env(cx, n_images, "Mock")` seeds a temp
  library with solid-colour PNGs, sets globals, and returns the library entity; then
  `cx.add_window_view(|window, cx| SomeView::new(window, cx))` and drive the real view methods. They
  run headless with `gpui`'s `test-support` feature and `#[gpui::test]`.

## Conventions (Zed style)

Follow how the Zed team writes GPUI/Rust; the items below are adapted from Zed's own agent rules.

**General Rust**
- Correctness and clarity first; performance second unless asked.
- Comments explain *why* when it's non-obvious. No section banners or comments that restate the code.
- Avoid panics: no `unwrap()`/`expect()`/unchecked indexing outside tests and process startup in
  `main.rs`. Propagate with `?`; use `let … else { return }` for early exits.
- Never silently discard a fallible result with `let _ =`. Either `?`, handle it, or log it:
  `tracing::warn!(target: "majik", "context: {e:#}")`. Errors from async work must reach the UI
  (toast / failed row), not vanish.
- Libraries define `thiserror` enums (`GenerationError`, `ValidationError`); the app uses `anyhow` with
  `.context(...)`. Match on structured error variants, never on message strings.
- Full-word identifiers (`request`, not `req`; `library`, not `lib`) in new code.
- Prefer extending an existing file over adding small new ones. New modules are `src/foo.rs`, not
  `src/foo/mod.rs` (existing `mod.rs` files stay as they are).
- Shared dependencies go in `[workspace.dependencies]` and are referenced with `.workspace = true`.
  Don't add a dependency without a reason you can state in the PR.
- Keep `cargo clippy --workspace --all-targets -- -D warnings` clean.

**GPUI**
- Parameter order: `window: &mut Window` before `cx`; callbacks after `cx`. Names are always `window`
  and `cx`.
- Inside `entity.update(cx, |this, cx| …)` use the inner `cx`, never the outer one. Never update an
  entity that is already being updated (panic).
- Async: `cx.spawn(async move |this, cx| …)` (`this: WeakEntity<T>`) for foreground work,
  `cx.background_spawn(async move { … })` for CPU/IO, then `this.update(cx, …)` to apply. Every `Task`
  is awaited, stored in a field (cancel-on-drop), or `.detach()`ed — never dropped implicitly.
  Use variable shadowing to scope clones for async closures.
- State changes that affect rendering call `cx.notify()`. Cross-entity communication is
  `EventEmitter` + `cx.emit` + `cx.subscribe`; keep `Subscription`s in a `_subscriptions` field or
  `.detach()` them deliberately.
- Render must be cheap and pure (no IO, no DB queries): precompute in `refresh` / `update`, render
  from fields. Conditional trees use `.when(cond, |this| …)` / `.when_some(opt, |this, v| …)`.
- Text args are `impl Into<SharedString>`. Stateful views are `Entity<T: Render>`; a new reusable
  stateless widget should be a `RenderOnce` + `#[derive(IntoElement)]` type rather than a bare
  `fn -> Div` (today's small helpers live as functions in `ui.rs`).
- Icons come from `ui::icon("name")` (HugeIcons — flarly's set — as SVGs in `assets/icons/`, generated by
  `node packaging/icons.mjs` from the name → export manifest `packaging/icons.json`; never hand-edit them);
  colours from `cx.theme()` (gpui-component), never hard-coded. Manufacturer and provider logos come
  from `ui::logo(name)` / `ui::logo_tile`: one monochrome SVG per logo in `assets/logos/`
  (`majik_providers::logo` holds the names), drawn like an icon — a mask filled with the theme
  foreground — so the same file serves the light and the dark theme and there are no `-dark`
  variants or PNGs. A logo carries no colour (a white cut-out must be a real hole, one evenodd path)
  and its viewBox is never taller than wide, because gpui fits an SVG by width; `assets.rs` tests
  both and that every catalog model and provider names an embedded file. Buttons come from `ui::button(id)`
  (a `Button` with the pointing-hand cursor), never `Button::new`; other clickable controls set
  `.cursor_pointer()` themselves.
- **Corner radius is a four-step scale, chosen by what the thing is, not by how it looks in place.**
  Controls (buttons, pickers, steppers, the prompt box, list rows, chips of information, drop
  targets, floating bars over the detail stage) use the theme radius: `rounded_md()`
  on a div, the default rounding on a `Button` — both 6 px, the same figure gpui-component's inputs,
  menus and tooltips use, so a control looks the same wherever it sits. Media cards (the composer's
  asset cards and role targets, the detail's input thumbnails, the feed's drag preview) and surfaces
  (dialogs, notifications, the toast) are one step larger, `rounded_lg()` / `theme.radius_lg` (8 px); an image inside a card gets the same
  radius itself because the parent's clip alone doesn't round it. Captions laid over a thumbnail are
  one step smaller, `rounded_sm()` (4 px). `rounded_full()` is reserved for things that are pills or
  circles by nature — count and duration badges, filter chips, the play button,
  the stage's page arrows, progress tracks — never for a bar of controls. Don't pick a bespoke
  pixel radius; the one exception today is the onboarding screen's 80 px provider tiles.

**Testing — every feature ships with a full end-to-end suite**

There are no manual test docs; the tests are the spec. A feature is not done until its tests cover the
whole behaviour end to end, the way Zed tests its editor, and `cargo test --workspace` passes.

- Every feature (new view, action, keybinding, menu item, state transition, generation flow) gets a
  `#[cfg(test)] mod tests` next to the code with a suite that walks every user-visible flow: the happy
  path, every branch and edge case, cancellation, and every failure mode (provider error, missing key,
  stale/timed-out job, invalid input). A change to behaviour changes or adds a test in the same commit.
- Tests are end to end through the real stack, headless: `#[gpui::test] fn name(cx: &mut TestAppContext)`,
  `env(cx, n_images, "Mock")` from `test_support.rs` for a real temp library + globals, then
  `cx.add_window_view(|window, cx| View::new(window, cx))` to get a `VisualTestContext`. Drive the view
  the way a user would — `cx.dispatch_action(Action)`, `cx.simulate_keystrokes("cmd-enter")`, the real
  public view methods — not private helpers. Assert on what the user sees and on the library model
  (`library.read(cx)`), never on render internals.
- Everything is deterministic: the Mock provider with an in-memory `MemoryStore` key, the
  `#delay:`/`#fail:` prompt directives, and the test executor. Wait with `cx.run_until_parked()`; use
  `cx.background_executor().timer(d).await` for time, never `smol::Timer` or real sleeps. Use
  `#[gpui::test(iterations = N)]` to shake out ordering bugs in async/concurrent flows.
- The non-UI crates keep their own suites: wiremock request-shape tests for every provider endpoint in
  `crates/majik-providers/tests/` (`shared.rs` for the cross-provider suite), plain unit tests for
  `core`/`storage`/`generation`. A bug fix starts with a failing test that reproduces it.
- Test code follows the same conventions as production code except that `unwrap()`/`expect()` are fine.
  Keep tests small and named after the behaviour (`compose_submit_with_missing_key_shows_toast`); one
  assertion topic per test, shared setup in `test_support.rs`, no sleeps, and no `#[ignore]` except
  the live-API suite (`majik-providers/tests/e2e.rs`), which is ignored because it costs money and
  needs provider keys.

**Process**
- Non-trivial changes come with their tests in the same commit (see above).
- Commit finished work on `main` without being asked; never `git push` unless the user asks.
- **Several agents often build features in this tree at the same time.** Other modified files in
  `git status` are someone else's work in progress: leave them alone, and commit only the files you
  changed — stage them by path, never `git add -A` / `git commit -a`. If their half-done change
  stops the workspace from building or testing, wait for it to land and try again rather than
  fixing or reverting it.
- Commit / PR titles: imperative, capitalized, no conventional-commit prefix, optionally
  `crate: Summary` (e.g. `composer: Always show remove badge on asset cards`).
