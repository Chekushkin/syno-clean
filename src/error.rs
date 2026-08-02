//! Error type and DSM numeric error-code mapping.
//!
//! Every DSM response carries the same envelope, and a failure is reported as
//! a bare integer (`{"success": false, "error": {"code": 119}}`). Turning that
//! integer into something a user can act on is the whole job of this module.
//!
//! Two code spaces overlap: the *common* codes (100-119) mean the same thing
//! for every API, while the 400-range is API-specific — the table here covers
//! `SYNO.API.Auth`, where transposing 401 and 402 turns "your account is
//! disabled" into "permission denied" and sends the user hunting in the wrong
//! place.

use std::fmt;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// DSM error codes that mean "your session is no longer usable".
///
/// The client re-authenticates once and retries exactly once when it sees one
/// of these; see `api::client`.
pub const SESSION_ERROR_CODES: [i32; 3] = [106, 107, 119];

/// DSM auth error code asking for a 2-step verification code.
pub const OTP_REQUIRED_CODE: i32 = 403;

/// The `SYNO.API.Auth` API name, used to select the auth error-code table.
pub const AUTH_API: &str = "SYNO.API.Auth";

/// True when `code` means the session must be re-established.
pub fn is_session_error(code: i32) -> bool {
    SESSION_ERROR_CODES.contains(&code)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level failure: DNS, TLS, connection refused, timeout.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The NAS answered, but with `success: false` and a numeric code.
    #[error("{api} failed: {} (DSM error {code})", dsm_message(*code, api))]
    Dsm { code: i32, api: String },

    /// Configuration is missing, malformed, or internally inconsistent.
    #[error("configuration error: {0}")]
    Config(String),

    /// Local filesystem failure (config file, session cache, log file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A response body did not match the shape this client expects.
    #[error("failed to parse response: {0}")]
    Parse(#[from] serde_json::Error),

    /// Login could not be completed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// A resolved delete path failed a safety guard. Never proceed past this.
    #[error("refusing unsafe path {path:?}: {reason}")]
    UnsafePath { path: String, reason: String },

    /// A required API is absent from `SYNO.API.Info`, or its version range
    /// does not overlap what this client understands.
    #[error("{}", api_unavailable_message(api, *reason))]
    ApiUnavailable {
        api: String,
        reason: ApiUnavailableReason,
    },
}

impl Error {
    /// Build a [`Error::Dsm`] from a code and the API that produced it.
    pub fn dsm(code: i32, api: impl Into<String>) -> Self {
        Error::Dsm {
            code,
            api: api.into(),
        }
    }

    /// Build a [`Error::Config`] from anything displayable.
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }

    /// Build a [`Error::UnsafePath`].
    pub fn unsafe_path(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::UnsafePath {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// The API is not listed by `SYNO.API.Info` at all.
    pub fn api_missing(api: impl Into<String>) -> Self {
        Error::ApiUnavailable {
            api: api.into(),
            reason: ApiUnavailableReason::NotInstalled,
        }
    }

    /// The API exists but this client and the NAS share no supported version.
    pub fn api_version_mismatch(
        api: impl Into<String>,
        nas: (u32, u32),
        supported: (u32, u32),
    ) -> Self {
        Error::ApiUnavailable {
            api: api.into(),
            reason: ApiUnavailableReason::VersionMismatch { nas, supported },
        }
    }
}

/// Why a required API could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiUnavailableReason {
    /// Absent from the discovery response.
    NotInstalled,
    /// Present, but `minVersion..maxVersion` does not overlap our range.
    VersionMismatch {
        nas: (u32, u32),
        supported: (u32, u32),
    },
}

impl fmt::Display for ApiUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiUnavailableReason::NotInstalled => f.write_str("not installed"),
            ApiUnavailableReason::VersionMismatch { nas, supported } => write!(
                f,
                "NAS supports versions {}-{}, this client supports {}-{}",
                nas.0, nas.1, supported.0, supported.1
            ),
        }
    }
}

/// The DSM package that owns an API name, for messages a user can act on.
fn package_for(api: &str) -> Option<&'static str> {
    if api.starts_with("SYNO.DownloadStation") {
        Some("Download Station")
    } else if api.starts_with("SYNO.FileStation") {
        Some("File Station")
    } else {
        None
    }
}

/// Message for a required API that cannot be used.
pub fn api_unavailable_message(api: &str, reason: ApiUnavailableReason) -> String {
    match (reason, package_for(api)) {
        (ApiUnavailableReason::NotInstalled, Some(pkg)) => {
            format!("{pkg} is not installed on this NAS (missing API {api})")
        }
        (ApiUnavailableReason::NotInstalled, None) => {
            format!("this NAS does not provide the API {api}")
        }
        (reason @ ApiUnavailableReason::VersionMismatch { .. }, _) => {
            format!("the API {api} is unusable: {reason}")
        }
    }
}

/// Human-readable text for a DSM numeric error code.
///
/// `api` selects the code table: the 400-range is API-specific, so
/// `SYNO.API.Auth` gets the login table while everything else falls through to
/// the common codes. Unknown codes produce a generic message that still
/// includes the number, so a bug report is actionable.
///
/// Returns an owned `String` (rather than the `&'static str` originally
/// sketched) precisely so the unknown-code fallback can name the code.
pub fn dsm_message(code: i32, api: &str) -> String {
    if api == AUTH_API
        && let Some(msg) = auth_message(code)
    {
        return msg.to_string();
    }
    match common_message(code) {
        Some(msg) => msg.to_string(),
        None => format!("unrecognized DSM error code {code}"),
    }
}

/// Codes that mean the same thing for every DSM API.
fn common_message(code: i32) -> Option<&'static str> {
    Some(match code {
        100 => "unknown error",
        101 => "invalid parameter",
        102 => "the requested API does not exist",
        103 => "the requested method does not exist",
        104 => "the requested version does not support this functionality",
        105 => "the logged-in session does not have permission for this request",
        106 => "session timeout",
        107 => "session interrupted by duplicate login",
        119 => "invalid session ID (SID)",
        _ => return None,
    })
}

/// `SYNO.API.Auth`-specific codes. Order matters less than accuracy here:
/// 400/401/402 are trivially transposed and each sends the user somewhere
/// different.
fn auth_message(code: i32) -> Option<&'static str> {
    Some(match code {
        400 => "no such account or incorrect password",
        401 => "account disabled",
        402 => "permission denied",
        403 => "2-step verification code required",
        404 => "failed to authenticate 2-step verification code",
        406 => "2-step verification is enforced for this account",
        407 => "blocked IP source",
        408 => "expired password (cannot be changed)",
        409 => "expired password",
        410 => "password must be changed",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHER_API: &str = "SYNO.DownloadStation.Task";

    #[test]
    fn common_codes_map_to_specific_text() {
        assert_eq!(dsm_message(100, OTHER_API), "unknown error");
        assert_eq!(dsm_message(101, OTHER_API), "invalid parameter");
        assert_eq!(
            dsm_message(102, OTHER_API),
            "the requested API does not exist"
        );
        assert_eq!(
            dsm_message(103, OTHER_API),
            "the requested method does not exist"
        );
        assert_eq!(
            dsm_message(105, OTHER_API),
            "the logged-in session does not have permission for this request"
        );
        assert_eq!(dsm_message(106, OTHER_API), "session timeout");
        assert_eq!(
            dsm_message(107, OTHER_API),
            "session interrupted by duplicate login"
        );
        assert_eq!(dsm_message(119, OTHER_API), "invalid session ID (SID)");
    }

    #[test]
    fn common_codes_apply_to_the_auth_api_too() {
        assert_eq!(dsm_message(119, AUTH_API), "invalid session ID (SID)");
        assert_eq!(dsm_message(106, AUTH_API), "session timeout");
    }

    // 400/401/402 are the three most easily transposed codes in the whole
    // table, so each is asserted on its own.
    #[test]
    fn auth_400_is_bad_credentials() {
        assert_eq!(
            dsm_message(400, AUTH_API),
            "no such account or incorrect password"
        );
    }

    #[test]
    fn auth_401_is_account_disabled() {
        assert_eq!(dsm_message(401, AUTH_API), "account disabled");
    }

    #[test]
    fn auth_402_is_permission_denied() {
        assert_eq!(dsm_message(402, AUTH_API), "permission denied");
    }

    #[test]
    fn remaining_auth_codes_map_to_specific_text() {
        assert_eq!(
            dsm_message(403, AUTH_API),
            "2-step verification code required"
        );
        assert_eq!(
            dsm_message(404, AUTH_API),
            "failed to authenticate 2-step verification code"
        );
        assert_eq!(
            dsm_message(406, AUTH_API),
            "2-step verification is enforced for this account"
        );
        assert_eq!(dsm_message(407, AUTH_API), "blocked IP source");
        assert_eq!(
            dsm_message(408, AUTH_API),
            "expired password (cannot be changed)"
        );
        assert_eq!(dsm_message(409, AUTH_API), "expired password");
        assert_eq!(dsm_message(410, AUTH_API), "password must be changed");
    }

    #[test]
    fn auth_table_does_not_leak_into_other_apis() {
        // 400 on Download Station is *not* "incorrect password"; without an
        // API-specific table it must fall through to the generic message.
        let msg = dsm_message(400, OTHER_API);
        assert!(msg.contains("400"), "{msg}");
        assert!(!msg.contains("password"), "{msg}");
    }

    #[test]
    fn unknown_code_falls_back_to_a_message_naming_the_code() {
        let msg = dsm_message(9999, OTHER_API);
        assert!(msg.contains("9999"), "{msg}");
        let msg = dsm_message(-1, AUTH_API);
        assert!(msg.contains("-1"), "{msg}");
    }

    #[test]
    fn dsm_error_display_includes_api_code_and_meaning() {
        let err = Error::dsm(401, AUTH_API);
        let rendered = err.to_string();
        assert!(rendered.contains(AUTH_API), "{rendered}");
        assert!(rendered.contains("401"), "{rendered}");
        assert!(rendered.contains("account disabled"), "{rendered}");
    }

    #[test]
    fn session_errors_are_exactly_106_107_119() {
        assert!(is_session_error(106));
        assert!(is_session_error(107));
        assert!(is_session_error(119));
        assert!(!is_session_error(105));
        assert!(!is_session_error(400));
    }

    #[test]
    fn missing_api_names_the_dsm_package() {
        let err = Error::api_missing("SYNO.FileStation.List");
        assert_eq!(
            err.to_string(),
            "File Station is not installed on this NAS (missing API SYNO.FileStation.List)"
        );

        let err = Error::api_missing("SYNO.DownloadStation.Task");
        assert!(
            err.to_string()
                .starts_with("Download Station is not installed")
        );

        let err = Error::api_missing("SYNO.Weird.Thing");
        assert!(err.to_string().contains("SYNO.Weird.Thing"));
    }

    #[test]
    fn version_mismatch_reports_both_ranges() {
        let err = Error::api_version_mismatch("SYNO.DownloadStation.Task", (2, 3), (1, 1));
        let rendered = err.to_string();
        assert!(rendered.contains("2-3"), "{rendered}");
        assert!(rendered.contains("1-1"), "{rendered}");
    }

    #[test]
    fn unsafe_path_error_quotes_the_path_and_reason() {
        let err = Error::unsafe_path("/downloads", "fewer than two path components");
        let rendered = err.to_string();
        assert!(rendered.contains("/downloads"), "{rendered}");
        assert!(rendered.contains("fewer than two"), "{rendered}");
    }
}
