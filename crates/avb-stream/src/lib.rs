//! AVTP audio streams for talker and listener entities: AAF and
//! AM824 frames built and read (IEEE 1722-2016, 7 and 6.4), sent paced on
//! gPTP time and received into a buffer the audio device plays, with
//! audio from and to the computer's devices through cpal, a test tone or
//! nothing; and media clock streams, CRF (10), on the same time.
//!
//! The media clock is gPTP's: a talker sends its samples at the nominal
//! rate of gPTP time, as gptpd gives it for the interface, and
//! the audio device, on its own clock, is kept in step by resampling
//! between the two, the ratio steered by the fill of a buffer between
//! them.
//!
//! With `AVB_STREAM_TIMING` naming a file, each talker's thread writes
//! there a line a second: how late it woke for its frames, how often it
//! was already behind, how long building and sending them took, how late
//! each was handed over against its departure, and how many it left out
//! as too late, counted by the thread itself and written by one of
//! ordinary priority. Without it, the thread keeps no time of its own.

pub mod aaf;
pub mod am824;
pub mod audio;
pub mod clock;
pub mod crf;
pub mod formats;
pub mod listener;
mod lock;
pub mod media;
mod meter;
pub mod realtime;
pub mod recovered;
pub mod talker;
mod timing;

pub use clock::MediaClock;
pub use listener::{GroupListener, Listener, ListenerConfig, ListenerGroup, ListenerStats};
pub use recovered::RecoveredClock;
pub use talker::{
    GroupConfig, GroupStream, GroupTalker, Talker, TalkerConfig, TalkerGroup, TalkerStats,
};
