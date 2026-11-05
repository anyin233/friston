//! Domain model shared by every Meeting Master crate.
//!
//! The types here are deliberately free of I/O so that the GUI, the store,
//! the indexer and the native-engine client can all agree on one vocabulary.

pub mod capture;
pub mod ids;
pub mod media;
pub mod meeting;
pub mod speaker;
pub mod transcript;

pub use capture::*;
pub use ids::*;
pub use media::*;
pub use meeting::*;
pub use speaker::*;
pub use transcript::*;
