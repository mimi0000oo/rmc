//! rmc: remote media control as a library (modules) + single binary.
//! Module ownership: each module owns its area; `main.rs` wires everything.

pub mod backend;
pub mod mpris;
pub mod protocol;
pub mod settings;
pub mod spotify;
pub mod sysaudio;
pub mod webserver;
pub mod ytmusic;
