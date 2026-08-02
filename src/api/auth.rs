//! `SYNO.API.Auth` — login and logout.
//!
//! Two rules from the plan are enforced here:
//!
//! * **Logout is invoked only by `--logout`.** A normal quit leaves the session
//!   alive; logging out would invalidate the cached `sid` and defeat the whole
//!   point of caching it.
//! * **2FA is demand-driven.** DSM answers a login that needs a one-time code
//!   with error 403 ([`crate::error::OTP_REQUIRED_CODE`]), so an absent code is
//!   the normal case. [`is_otp_required`] classifies that answer for the caller
//!   that owns the prompt.
//!
//! Parameter construction is a pure function ([`build_login_params`]) so the
//! encoding is unit-tested without a network, matching the `build_*_params`
//! convention used by the Download Station and File Station modules.

use std::fmt;

use serde::Deserialize;

use crate::api::client::{SynoClient, VersionRange};
use crate::error::{AUTH_API, Error, OTP_REQUIRED_CODE, Result};

/// DSM session name. Download Station privileges are scoped to it, and logout
/// must name the same session that login did.
pub const AUTH_SESSION: &str = "DownloadStation";

/// Version range this client implements for `SYNO.API.Auth`.
///
/// The floor is 3 because that is where `otp_code` appears — below it there is
/// no way to complete a 2FA login. The ceiling is 6; DSM 7 advertises 7, but
/// nothing in this client needs anything above 6, and the negotiation in
/// [`crate::api::client::pick_version_in`] clamps to the overlap anyway.
pub const AUTH_SUPPORTED: VersionRange = (3, 6);

/// What is needed to obtain a session — and to obtain another one silently
/// when DSM decides the first has expired.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    /// A 2FA code, when one is already known. DSM only asks for it after a
    /// login attempt returns 403, so this is usually `None`.
    pub otp: Option<String>,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Credentials {
            username: username.into(),
            password: password.into(),
            otp: None,
        }
    }

    /// Attach a one-time code, typically after a 403 sent the caller to prompt.
    pub fn with_otp(mut self, otp: impl Into<String>) -> Self {
        let otp = otp.into();
        self.otp = (!otp.is_empty()).then_some(otp);
        self
    }
}

/// Hand-written so a password can never reach the log file through a
/// `{:?}` on this struct or on anything containing it.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("otp", &self.otp.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The `data` object of a successful login.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginData {
    pub sid: String,
    #[serde(default)]
    pub did: Option<String>,
    #[serde(default)]
    pub is_portal_port: Option<bool>,
}

/// Query parameters for `method=login`.
///
/// `format=sid` asks DSM for a session id in the response body rather than a
/// cookie, which is what lets the sid be cached to disk and reused.
pub fn build_login_params(credentials: &Credentials, session: &str) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("account", credentials.username.clone()),
        ("passwd", credentials.password.clone()),
        ("session", session.to_string()),
        ("format", "sid".to_string()),
    ];
    if let Some(otp) = credentials.otp.as_deref().filter(|c| !c.is_empty()) {
        params.push(("otp_code", otp.to_string()));
    }
    params
}

/// Query parameters for `method=logout`.
pub fn build_logout_params(session: &str) -> Vec<(&'static str, String)> {
    vec![("session", session.to_string())]
}

/// Log in and return the new `sid`.
///
/// Deliberately bypasses [`SynoClient::call_text`]: that path re-logs-in on a
/// session error, and a login must not recurse into itself.
pub async fn login(client: &SynoClient, credentials: &Credentials) -> Result<String> {
    let endpoint = client.endpoint(AUTH_API, AUTH_SUPPORTED)?;
    let params = build_login_params(credentials, AUTH_SESSION);
    let body = client.send(&endpoint, "login", &params, None).await?;
    let data: LoginData = crate::api::client::parse_envelope(&body, AUTH_API)?;
    tracing::info!(user = %credentials.username, "logged in to DSM");
    Ok(data.sid)
}

/// Invalidate the current session on the NAS.
///
/// Only `--logout` calls this. Quitting normally keeps the session so the next
/// start is instant.
pub async fn logout(client: &SynoClient) -> Result<()> {
    let endpoint = client.endpoint(AUTH_API, AUTH_SUPPORTED)?;
    let params = build_logout_params(AUTH_SESSION);
    let body = client
        .send(&endpoint, "logout", &params, client.sid())
        .await?;
    crate::api::client::check_envelope(&body, AUTH_API)?;
    client.clear_sid();
    tracing::info!("logged out of DSM");
    Ok(())
}

/// True when DSM refused a login because it wants a 2-step verification code.
///
/// The caller owns the prompt (it has to happen before the alternate screen is
/// entered), so classification lives here and the interaction does not.
pub fn is_otp_required(error: &Error) -> bool {
    matches!(error, Error::Dsm { code, api } if *code == OTP_REQUIRED_CODE && api == AUTH_API)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;

    #[test]
    fn login_params_request_a_sid_for_the_download_station_session() {
        let credentials = Credentials::new("eduard", "hunter2");
        assert_eq!(
            build_login_params(&credentials, AUTH_SESSION),
            vec![
                ("account", "eduard".to_string()),
                ("passwd", "hunter2".to_string()),
                ("session", "DownloadStation".to_string()),
                ("format", "sid".to_string()),
            ]
        );
    }

    #[test]
    fn otp_code_is_sent_only_when_one_is_known() {
        let credentials = Credentials::new("eduard", "hunter2").with_otp("123456");
        let params = build_login_params(&credentials, AUTH_SESSION);
        assert_eq!(params.last(), Some(&("otp_code", "123456".to_string())));

        // An empty code is the same as no code — never send `otp_code=`.
        let credentials = Credentials::new("eduard", "hunter2").with_otp("");
        let params = build_login_params(&credentials, AUTH_SESSION);
        assert!(params.iter().all(|(k, _)| *k != "otp_code"), "{params:?}");
    }

    #[test]
    fn logout_names_the_same_session_login_used() {
        assert_eq!(
            build_logout_params(AUTH_SESSION),
            vec![("session", "DownloadStation".to_string())]
        );
    }

    #[test]
    fn credentials_never_render_the_password() {
        let rendered = format!(
            "{:?}",
            Credentials::new("eduard", "hunter2").with_otp("999")
        );
        assert!(rendered.contains("eduard"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("999"), "{rendered}");
    }

    #[test]
    fn login_response_parses_out_the_sid() {
        let body = r#"{"success": true, "data": {"sid": "abc123", "did": "d1"}}"#;
        let data: LoginData = parse_envelope(body, AUTH_API).expect("login payload");
        assert_eq!(data.sid, "abc123");
        assert_eq!(data.did.as_deref(), Some("d1"));
        assert_eq!(data.is_portal_port, None);

        // DSM 7 answers with just the sid on some versions.
        let data: LoginData =
            parse_envelope(r#"{"success": true, "data": {"sid": "x"}}"#, AUTH_API)
                .expect("minimal payload");
        assert_eq!(data.sid, "x");
    }

    #[test]
    fn a_failed_login_renders_the_auth_specific_message() {
        let err =
            parse_envelope::<LoginData>(r#"{"success": false, "error": {"code": 400}}"#, AUTH_API)
                .expect_err("bad credentials");
        assert!(
            err.to_string()
                .contains("no such account or incorrect password"),
            "{err}"
        );
    }

    #[test]
    fn otp_required_is_recognized_only_for_403_on_the_auth_api() {
        assert!(is_otp_required(&Error::dsm(OTP_REQUIRED_CODE, AUTH_API)));
        assert!(!is_otp_required(&Error::dsm(400, AUTH_API)));
        // 403 means something else entirely on another API.
        assert!(!is_otp_required(&Error::dsm(
            OTP_REQUIRED_CODE,
            "SYNO.DownloadStation.Task"
        )));
        assert!(!is_otp_required(&Error::Auth("nope".into())));
    }

    #[test]
    fn the_supported_range_can_carry_an_otp_code() {
        // Below version 3 there is no `otp_code` parameter at all, so a 2FA
        // account could never log in.
        assert!(AUTH_SUPPORTED.0 >= 3, "{AUTH_SUPPORTED:?}");
        assert!(AUTH_SUPPORTED.0 <= AUTH_SUPPORTED.1);
    }
}
