//! The DSM HTTP API client.
//!
//! Everything the NAS is asked to do goes through [`client::SynoClient`]:
//!
//! * [`client`] — the `reqwest` client, `SYNO.API.Info` discovery, the response
//!   envelope, sid handling and the re-login-once retry.
//! * [`auth`] — `SYNO.API.Auth` login and logout.
//!
//! Later tasks add `download_station` and `file_station` on top; they never
//! build a URL or a version number themselves, they ask the client.

pub mod auth;
pub mod client;

pub use auth::Credentials;
pub use client::{ApiInfo, ApiInfoMap, DsmError, Endpoint, Envelope, SynoClient, parse_envelope};
