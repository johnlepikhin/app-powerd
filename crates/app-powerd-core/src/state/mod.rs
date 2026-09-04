pub(crate) mod app_entry;
/// On-disk record of what is currently suspended, used to restore state after
/// an abrupt daemon exit.
pub mod journal;
pub(crate) mod machine;
pub(crate) mod process_set;
pub(crate) mod registry;

pub use app_entry::{AppEntry, AppId};
pub use journal::{FreezeJournal, FreezeMethod, RecoveryReport};
pub use machine::{AppState, SuspendMode, TransitionAction};
pub use process_set::ProcessSet;
pub use registry::AppRegistry;
