//! Application state for Text³, independent of the GUI toolkit.

pub mod benchprobe;
pub mod bookmarks;
pub mod cli;

/// The Mac App Store build (`--features store`, #79): updates come from
/// the store, and the sandbox does not allow changing system defaults.
pub const STORE: bool = cfg!(feature = "store");
pub mod counts;
pub mod defaults;
pub mod editor;
pub mod editorconfig;
pub mod filesearch;
pub mod fonts;
pub mod fuzzy;
pub mod goto;
pub mod images;
pub mod keymap;
pub mod macros;
pub mod migrate;
pub mod npp_shortcuts;
pub mod panes;
pub mod recent;
pub mod recovery;
pub mod replacefiles;
pub mod session;
pub mod settings;
pub mod shellreg;
pub mod startprobe;
pub mod syntax;
pub mod update;
pub mod workspace;

pub use editor::{Command, Editor, SaveOutcome};
