//! The daemon's contracts with what is outside it, through its public API:
//! how it reads local API requests, its files on disk (credentials, records,
//! snapshots, the Machine lock), the service files it installs and the
//! environment git gets.

mod api;
mod auth;
mod config;
mod machine_lock;
mod service;
mod state;
