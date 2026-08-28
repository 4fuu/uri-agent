//! Terminal conversation interface.
//!
//! The implementation is split by responsibility below `app`; these exports
//! preserve the established crate-level TUI API.

mod app;

pub(crate) use app::rate::visible_units;
pub use app::{TuiInfo, TuiOutcome, TuiServices, TuiTerminal};
