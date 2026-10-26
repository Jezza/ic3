//! `ic3-irc`: an IRCv3 server that presents Microsoft Teams chat (through
//! `ic3-teams`) to ordinary IRC clients. Each Teams conversation is a channel or a
//! private query; Teams is the source of truth for history and read state.
#![forbid(unsafe_code)]

pub mod backend;
pub mod caps;
pub mod config;
pub mod conn;
pub mod fuzzy;
pub mod gateway;
pub mod http;
pub mod irc;
pub mod media;
pub mod names;
pub mod render;
pub mod service;
pub mod settings;
