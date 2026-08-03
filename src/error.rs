//! Error type and DSM numeric error-code mapping.
//!
//! Every DSM response carries the same envelope, and a failure is reported as
//! a bare integer (`{"success": false, "error": {"code": 119}}`). Turning that
//! integer into something a user can act on is the whole job of this module.
//!
//! Two code spaces overlap: the *common* codes (100-119) mean the same thing
//! for every API, while the 400-range is **API-specific**. Two tables are
//! implemented — [`auth_message`] for `SYNO.API.Auth`, where transposing 401
//! and 402 turns "your account is disabled" into "permission denied" and sends
//! the user hunting in the wrong place, and [`file_station_message`] for
//! `SYNO.FileStation.*`, which reuses the same numbers for something else
//! entirely (403 is a permission problem there, a 2FA prompt on Auth). Any
//! other API's 400-range code falls through to the common table and then to a
//! message naming the raw number, so a Download Station 400 can never render
//! as "incorrect password".

use std::fmt;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// DSM error codes that mean "your session is no longer usable".
///
/// The client re-authenticates once and retries exactly once when it sees one
/// of these; see `api::client`.
pub const SESSION_ERROR_CODES: [i32; 3] = [106, 107, 119];

/// DSM 105, "insufficient user privilege" — and **ambiguous**.
///
/// The common table calls it a permission problem, and it genuinely is one when
/// the account may not use the API. But a real DSM 7 also answers 105 from
/// `SYNO.DownloadStation.Task` for a `_sid` that has simply expired, where 119
/// ("invalid session") is what the documentation would lead you to expect.
/// Observed directly: a cached sid returned 105 for every request while a fresh
/// login with the same credentials returned the task list immediately.
///
/// Because the client cannot tell the two apart from the code alone, it
/// **disambiguates by trying**: see [`may_be_stale_session`].
pub const PERMISSION_DENIED_CODE: i32 = 105;

/// DSM auth error code asking for a 2-step verification code.
pub const OTP_REQUIRED_CODE: i32 = 403;

/// The `SYNO.API.Auth` API name, used to select the auth error-code table.
pub const AUTH_API: &str = "SYNO.API.Auth";

/// True when `code` means the session must be re-established.
pub fn is_session_error(code: i32) -> bool {
    SESSION_ERROR_CODES.contains(&code)
}

/// True when `code` *might* mean the session is stale, so re-authenticating is
/// worth one attempt.
///
/// Wider than [`is_session_error`] by exactly [`PERMISSION_DENIED_CODE`], which
/// is why the two are separate functions rather than one list: 105 still
/// *renders* as a permission error, because that is what it means when it is
/// not a stale session, and the message a user finally sees should say so.
///
/// The ambiguity is resolved by attempting the re-login rather than by guessing:
/// if a request fails 105, a fresh session fixes it, and it was a stale sid; if
/// it fails 105 again, the account really may not do this. `api::client` stops
/// treating 105 as a session error for the rest of the process the first time
/// the second answer comes back — otherwise an account that genuinely lacks
/// Download Station permission would re-authenticate on every poll, which at the
/// default three-second interval is a login attempt every three seconds for as
/// long as the program is open.
pub fn may_be_stale_session(code: i32) -> bool {
    is_session_error(code) || code == PERMISSION_DENIED_CODE
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level failure: DNS, TLS, connection refused, timeout.
    ///
    /// Carries a **rendered, query-stripped** message rather than deriving one
    /// from the source on demand — see [`http_message`]. The `reqwest::Error`
    /// rides along as the error source for anything that wants to inspect it,
    /// but nothing that displays this variant may reach for it.
    #[error("HTTP request failed: {message}")]
    Http {
        message: String,
        #[source]
        source: reqwest::Error,
    },

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

/// The one place a `reqwest::Error` becomes an [`Error`], so the redaction
/// below cannot be bypassed by a `?`.
impl From<reqwest::Error> for Error {
    fn from(source: reqwest::Error) -> Self {
        Error::Http {
            message: http_message(&source),
            source,
        }
    }
}

/// A transport error's own text, with any **query string removed**.
///
/// reqwest appends `" for url (<the whole url>)"` to every error it can
/// attribute to a request, and every non-login request this client makes puts
/// `_sid=<the session id>` in that query (`api::client::send`). A sid is a
/// bearer credential — the reason `session.json` and the log file are both mode
/// `0600` — and that text travels a long way: into `tracing::warn!(%err, …)`,
/// into a delete's per-item failure reason, into the footer, and onto stderr
/// during the shutdown drain.
///
/// Stripped **here, at the one boundary where a `reqwest::Error` enters this
/// crate**, rather than at each of the places that render one: there is no
/// second copy of the rule to forget. The scheme, host and path survive, which
/// is the half that says which endpoint failed.
fn http_message(err: &reqwest::Error) -> String {
    redact_query(&err.to_string(), err.url())
}

/// [`http_message`] with the URL passed in, so the substitution is testable
/// without a network stack to produce a real `reqwest::Error` from.
fn redact_query(text: &str, url: Option<&reqwest::Url>) -> String {
    match url {
        Some(url) if url.query().is_some() => {
            let mut redacted = url.clone();
            redacted.set_query(None);
            text.replace(url.as_str(), redacted.as_str())
        }
        _ => text.to_string(),
    }
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

    /// An operation that failed with no DSM code behind it.
    ///
    /// Reuses [`Error::Io`] rather than growing the enum, the same way
    /// `api::client` reuses [`Error::Parse`] for protocol violations: a
    /// `path_err_num` with no code, or a per-task result array that never
    /// mentioned the task asked about. One spelling, so the three call sites
    /// cannot drift apart.
    pub fn operation_failed(message: impl Into<String>) -> Self {
        Error::Io(std::io::Error::other(message.into()))
    }

    /// A bounded wait that ran out — same [`Error::Io`] reuse as
    /// [`Error::operation_failed`], with the kind that says which it was.
    pub fn timed_out(message: impl Into<String>) -> Self {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            message.into(),
        ))
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

/// A startup diagnostic for a NAS that could not be reached or logged in to.
///
/// Printed **instead of** entering the TUI: a table that is empty because the
/// program never got a session looks exactly like a NAS with no downloads on
/// it, and the user would have no way to tell which they were looking at. Three
/// lines, in the order the reader needs them:
///
/// 1. what failed, naming the host and port that were tried and the account
/// 2. the underlying error, including the DSM code's meaning via [`Error`]'s
///    own `Display` (so `dsm_message` is reached exactly once, here as
///    everywhere else)
/// 3. one hint, chosen from the failure — see [`connection_hint`]
pub fn connection_diagnostic(err: &Error, target: &str, username: &str) -> String {
    let headline = if is_auth_failure(err) {
        format!("cannot log in to {target} as {username}")
    } else {
        format!("cannot reach {target}")
    };
    format!("{headline}\n  {err}\n  hint: {}", connection_hint(err))
}

/// Whether a failure is about credentials rather than connectivity.
fn is_auth_failure(err: &Error) -> bool {
    match err {
        Error::Auth(_) => true,
        Error::Dsm { api, .. } => api == AUTH_API,
        _ => false,
    }
}

/// The one thing most likely to fix this failure.
///
/// Deliberately a single sentence per case: a wall of possibilities is a wall
/// the user scrolls past. The DSM-code cases lean on the auth table above —
/// "no such account or incorrect password" and "blocked IP source" need very
/// different next steps.
pub fn connection_hint(err: &Error) -> String {
    match err {
        Error::Http { .. } => "check the host, the port and that DSM is reachable — HTTPS is 5001 \
             and HTTP is 5000 by default, and a self-signed certificate needs --insecure"
            .to_string(),
        Error::Dsm { code, api } if api == AUTH_API => auth_hint(*code).to_string(),
        Error::Dsm { code: 105, .. } => {
            "this DSM account lacks permission — grant it Download Station and File Station \
             access in Control Panel > User"
                .to_string()
        }
        Error::Dsm { code, api } => format!(
            "{api} refused the request: {}",
            dsm_message(*code, api.as_str())
        ),
        Error::ApiUnavailable { .. } => {
            "install and start the package in DSM's Package Center, then try again".to_string()
        }
        Error::Auth(_) => "check the account name and SYNO_CLEAN_PASSWORD".to_string(),
        _ => "check the configuration and the log file for details".to_string(),
    }
}

/// Next step for a login DSM rejected.
fn auth_hint(code: i32) -> &'static str {
    match code {
        400 => "check the account name and the password (SYNO_CLEAN_PASSWORD, or the prompt)",
        401 => "this DSM account is disabled — re-enable it in Control Panel > User",
        402 => "this DSM account may not use Download Station — check its permissions in DSM",
        OTP_REQUIRED_CODE | 404 | 406 => {
            "this account uses 2-step verification — set SYNO_CLEAN_OTP or enter the code \
             at the prompt"
        }
        407 => "DSM has blocked this IP — clear it in Control Panel > Security > Auto Block",
        408..=410 => "the DSM password has expired — change it in DSM, then try again",
        _ => "check the account name and the password, then the DSM log",
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
    if api.starts_with(FILE_STATION_PREFIX)
        && let Some(msg) = file_station_message(code)
    {
        return msg.to_string();
    }
    match common_message(code) {
        Some(msg) => msg.to_string(),
        None => format!("unrecognized DSM error code {code}"),
    }
}

/// Every File Station API name starts with this.
const FILE_STATION_PREFIX: &str = "SYNO.FileStation";

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

/// `SYNO.FileStation.*`-specific codes.
///
/// These reuse the 400 range for something completely different from the auth
/// table, and they are what the delete path actually surfaces: without this,
/// a permission-denied existence check reads as "unrecognized DSM error code
/// 403" — a message that tells the user nothing about the one operation in
/// this program that can lose data.
fn file_station_message(code: i32) -> Option<&'static str> {
    Some(match code {
        400 => "invalid parameter for the File Station request",
        401 => "unknown File Station error",
        402 => "the File Station system is too busy",
        403 => "permission denied — this DSM account may not read or write that path",
        404 => "the shared folder is in the recycle bin",
        405 => "cannot accept a request from another user",
        406 => "the shared folder does not have a quota, or the quota is exceeded",
        407 => "the operation failed because the path is in use",
        408 => "no such file or directory",
        409 => "the volume does not support this operation",
        410 => "the operation failed — no such task on the NAS",
        411 => "the destination is read-only",
        412 => "the file already exists at the destination",
        413 => "the destination folder is a subfolder of the source",
        414 => "the destination folder does not exist",
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
    fn a_stale_session_may_also_arrive_as_105() {
        // Observed on a real DSM 7: a cached sid returned 105 from
        // SYNO.DownloadStation.Task for every request, while a fresh login with
        // the same credentials listed the tasks immediately. The documented
        // "invalid session" code, 119, was never sent.
        assert!(may_be_stale_session(105));
        for code in SESSION_ERROR_CODES {
            assert!(may_be_stale_session(code));
        }
        // Everything else stays outside: a re-login cannot fix a bad password
        // or a missing API, and retrying would only double the traffic.
        for code in [100, 101, 102, 103, 104, 400, 402, 403, 408] {
            assert!(!may_be_stale_session(code), "{code}");
        }
    }

    #[test]
    fn code_105_still_reads_as_a_permission_problem() {
        // The retry widening must not change what the user is finally told:
        // when a fresh session does *not* clear it, 105 means exactly what the
        // common table says.
        let rendered = dsm_message(PERMISSION_DENIED_CODE, OTHER_API);
        assert!(rendered.contains("permission"), "{rendered}");
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

    // ---- the startup connection diagnostic ---------------------------------

    const TARGET: &str = "https://nas.local:5001";

    #[test]
    fn a_login_failure_names_the_account_and_what_the_code_means() {
        let err = Error::dsm(400, AUTH_API);
        let diagnostic = connection_diagnostic(&err, TARGET, "eduard");
        // Where it tried, as whom...
        assert!(diagnostic.contains("nas.local"), "{diagnostic}");
        assert!(diagnostic.contains("5001"), "{diagnostic}");
        assert!(diagnostic.contains("eduard"), "{diagnostic}");
        assert!(diagnostic.starts_with("cannot log in"), "{diagnostic}");
        // ...what DSM said, in words rather than as a bare 400...
        assert!(
            diagnostic.contains("no such account or incorrect password"),
            "{diagnostic}"
        );
        // ...and what to do about it.
        assert!(diagnostic.contains("SYNO_CLEAN_PASSWORD"), "{diagnostic}");
    }

    #[test]
    fn each_auth_failure_points_somewhere_different() {
        // These are the codes whose fixes are in completely different places;
        // rendering them all as "check your password" is the failure mode.
        let hint = |code| connection_hint(&Error::dsm(code, AUTH_API));
        assert!(hint(401).contains("disabled"), "{}", hint(401));
        assert!(hint(402).contains("permissions"), "{}", hint(402));
        assert!(
            hint(OTP_REQUIRED_CODE).contains("SYNO_CLEAN_OTP"),
            "{}",
            hint(OTP_REQUIRED_CODE)
        );
        assert!(hint(407).contains("Auto Block"), "{}", hint(407));
        assert!(hint(409).contains("expired"), "{}", hint(409));
        // An auth code nobody has documented still gets a usable next step.
        assert!(!hint(499).is_empty());
    }

    #[test]
    fn a_non_auth_failure_reads_as_a_connection_problem() {
        let err = Error::dsm(105, OTHER_API);
        let diagnostic = connection_diagnostic(&err, TARGET, "eduard");
        assert!(diagnostic.starts_with("cannot reach"), "{diagnostic}");
        assert!(diagnostic.contains("nas.local:5001"), "{diagnostic}");
        assert!(diagnostic.contains("Download Station"), "{diagnostic}");
    }

    #[test]
    fn a_missing_package_says_to_install_it() {
        let err = Error::api_missing("SYNO.FileStation.List");
        let diagnostic = connection_diagnostic(&err, TARGET, "eduard");
        assert!(
            diagnostic.contains("File Station is not installed"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("Package Center"), "{diagnostic}");
    }

    // ---- the sid must not ride out on a transport error ---------------------

    /// The text reqwest produces: its own description, then the whole URL.
    fn reqwest_style(url: &str) -> String {
        format!("error sending request for url ({url})")
    }

    #[test]
    fn a_transport_errors_url_is_rendered_without_its_query() {
        // Every non-login request carries `_sid=<bearer credential>` in the
        // query, and reqwest appends the whole URL to the error it hands back.
        // That text reaches the log, the footer and stderr.
        let url = reqwest::Url::parse(
            "https://nas.local:5001/webapi/entry.cgi\
             ?api=SYNO.FileStation.List&method=list&_sid=SECRET-SESSION-ID",
        )
        .expect("a valid url");
        let redacted = redact_query(&reqwest_style(url.as_str()), Some(&url));

        assert!(!redacted.contains("_sid"), "{redacted}");
        assert!(!redacted.contains("SECRET-SESSION-ID"), "{redacted}");
        // The half that says which endpoint failed survives.
        assert!(
            redacted.contains("https://nas.local:5001/webapi/entry.cgi"),
            "{redacted}"
        );
        assert!(redacted.starts_with("error sending request"), "{redacted}");
    }

    #[test]
    fn an_error_with_no_query_or_no_url_is_left_exactly_as_it_was() {
        let bare = reqwest::Url::parse("https://nas.local:5001/webapi/entry.cgi").expect("url");
        let text = reqwest_style(bare.as_str());
        assert_eq!(redact_query(&text, Some(&bare)), text);
        assert_eq!(redact_query("connection closed", None), "connection closed");
    }

    #[tokio::test]
    async fn a_real_transport_failure_reaches_display_with_no_sid_in_it() {
        // The end-to-end path: a `?` on a reqwest call, through `From`, out of
        // `Display`. Port 1 on the loopback refuses immediately, so this is a
        // genuine `reqwest::Error` carrying a genuine URL and no network.
        let result: Result<String> = async {
            let text = reqwest::Client::new()
                .get("http://127.0.0.1:1/webapi/entry.cgi")
                .query(&[("api", "SYNO.FileStation.List"), ("_sid", "SECRET")])
                .send()
                .await?
                .text()
                .await?;
            Ok(text)
        }
        .await;

        let err = result.expect_err("nothing listens on port 1");
        let rendered = err.to_string();
        assert!(matches!(err, Error::Http { .. }), "{rendered}");
        assert!(!rendered.contains("_sid"), "{rendered}");
        assert!(!rendered.contains("SECRET"), "{rendered}");
        assert!(rendered.contains("127.0.0.1:1"), "{rendered}");
    }

    #[test]
    fn every_diagnostic_is_headline_error_and_hint() {
        for err in [
            Error::dsm(400, AUTH_API),
            Error::dsm(101, OTHER_API),
            Error::api_missing("SYNO.DownloadStation.Task"),
            Error::Auth("no sid in the login response".into()),
            Error::config("nothing configured"),
        ] {
            let diagnostic = connection_diagnostic(&err, TARGET, "eduard");
            let lines: Vec<&str> = diagnostic.lines().collect();
            assert_eq!(lines.len(), 3, "{diagnostic}");
            assert!(lines[1].trim() == err.to_string(), "{diagnostic}");
            assert!(lines[2].starts_with("  hint: "), "{diagnostic}");
            assert!(lines[2].len() > "  hint: ".len(), "{diagnostic}");
        }
    }
}
