//! MPRIS playback state for the Touch Bar.
//!
//! [`Players`] holds what every player reported and decides which one is
//! "now playing"; [`MprisWatcher`] keeps it current from a D-Bus connection
//! and forwards transport commands. The session daemon uses the choice for
//! profile rules and the plugin supervisor for the media capability, so both
//! apply the same rule, and [`session_file`] lets the supervisors follow the
//! daemon's choice where their own history is shorter.

mod model;
pub mod session_file;
mod watch;

pub use model::{
    BUS_NAME_PREFIX, Capabilities, Metadata, NowPlaying, PlaybackStatus, Players, Property,
    is_player_bus_name, player_name,
};
pub use watch::{Command, CommandError, METHOD_TIMEOUT, MprisWatcher};
