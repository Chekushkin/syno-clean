//! The DSM HTTP API client.
//!
//! Everything the NAS is asked to do goes through [`client::SynoClient`]:
//!
//! * [`client`] — the `reqwest` client, `SYNO.API.Info` discovery, the response
//!   envelope, sid handling and the re-login-once retry.
//! * [`auth`] — `SYNO.API.Auth` login and logout.
//! * [`download_station`] — `SYNO.DownloadStation.Task`, the v1 task list plus
//!   the per-task `getinfo` / `pause` / `resume` / `delete` methods.
//! * [`file_station`] — `SYNO.FileStation.List` `getinfo` (the pre-delete
//!   existence check) and `SYNO.FileStation.Delete` (`start` + `status`
//!   polling), which is what actually reclaims the space.
//!
//! None of these modules ever builds a URL or picks a version itself; they ask
//! the client.

pub mod auth;
pub mod client;
pub mod download_station;
pub mod file_station;

pub use auth::Credentials;
pub use client::{ApiInfo, ApiInfoMap, DsmError, Endpoint, Envelope, SynoClient, parse_envelope};
