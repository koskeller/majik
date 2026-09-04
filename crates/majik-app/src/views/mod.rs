//! The app's views, one module per screen or panel.
//!
//! [`library_window`] is the root: the sidebar and composer panels around a [`feed`], with a
//! [`detail`] covering the window when an entry is opened. [`compose`] is the generation panel,
//! [`settings`] the separate window (with [`telemetry_log`] on its Telemetry page), [`onboarding`]
//! the first-launch flow, and the pickers are the popovers they open. Each is an `Entity<T: Render>` that observes the library and rebuilds its own
//! id lists; they talk to each other with events rather than by reaching across the window.

pub mod compose;
pub mod detail;
pub mod feed;
pub mod library_window;
pub mod settings;
pub mod telemetry_log;
pub mod sidebar;
pub mod album_picker;
pub mod model_picker;
pub mod voice_picker;
pub mod onboarding;
