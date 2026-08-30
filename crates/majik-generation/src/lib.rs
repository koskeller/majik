//! The generation pipeline: requests → jobs → provider calls → results, with retry, timeouts,
//! cancellation and an event stream the UI applies to the library.

pub mod engine;
pub mod improve;
pub mod recovery;
pub mod seed;
pub mod request;
pub mod validation;

pub use engine::{Engine, Event, Job};
pub use improve::{improve_channel, ImproveReceiver, ImproveSender, TextRequest};
pub use recovery::RecoveryAction;
pub use seed::{seed_library, SeedOptions, SeedReport};
pub use request::{build_requests, AssetInput, GenerationType, Request, ToolSettings};
pub use validation::{validate_request, validate_requests, ValidationError};
