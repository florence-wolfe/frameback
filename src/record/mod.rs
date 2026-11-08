//! Recording and DVR: one always-on encoder pipeline whose packets feed both
//! the rewind buffer and, while recording, a file.

mod clip;
mod codec;
mod import;
mod muxer;
mod pipeline;
mod player;
mod store;

pub use clip::save_clip;
pub use codec::{Codec, effective_fps};
pub use import::{Import, ImportEvent, Source};
pub use pipeline::{Pipeline, PipelineEvent};
pub use player::{Player, PlayerEvent};
pub use store::Store;
