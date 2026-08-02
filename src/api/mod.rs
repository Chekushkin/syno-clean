//! The DSM HTTP API client.
//!
//! Everything the NAS is asked to do goes through [`client::SynoClient`]:
//!
//! * [`client`] — the `reqwest` client, `SYNO.API.Info` discovery, the response
//!   envelope, sid handling and the re-login-once retry.
//! * [`auth`] — `SYNO.API.Auth` login and logout.
//! * [`download_station`] — `SYNO.DownloadStation.Task`, the v1 task list.
//!
//! A later task adds `file_station` on top; none of these modules ever build a
//! URL or a version number themselves, they ask the client.

pub mod auth;
pub mod client;
pub mod download_station;

pub use auth::Credentials;
pub use client::{ApiInfo, ApiInfoMap, DsmError, Endpoint, Envelope, SynoClient, parse_envelope};
