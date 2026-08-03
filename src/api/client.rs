//! The DSM HTTP client: response envelope, API discovery, sid handling, retry.
//!
//! Three rules from the plan live here and nowhere else:
//!
//! 1. **Discovery is a special case.** `SYNO.API.Info` is served from a fixed
//!    `/webapi/query.cgi`, *not* from the `entry.cgi` that everything else uses.
//!    Every later URL is `{base}/webapi/{discovered path}`.
//! 2. **No hardcoded API versions**, bar the two deliberate pins in
//!    `download_station` and `file_station`. A caller states the version range
//!    *it* understands; [`ApiInfoMap::endpoint`] intersects that with the range
//!    the NAS advertises and picks the highest version in the overlap. A
//!    missing API is reported by DSM package name, not as a bare 102.
//! 3. **One transparent retry.** On DSM 106 / 107 / 119 (see
//!    [`crate::error::is_session_error`]) the client re-authenticates once and
//!    replays the request exactly once. A second failure is returned.
//!
//! Testing note: per the plan there is deliberately no mock HTTP server and no
//! trait over `reqwest`. The tested surface is therefore the pure part —
//! envelope deserialization from JSON strings, the API-info map lookup,
//! `pick_version_in`, and URL/parameter construction. The `async fn`s that
//! actually talk to a NAS are verified by running the binary.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
// Brought in only for `Error::custom`, which is how a protocol violation
// ("success with no data") becomes an `Error::Parse` without inventing a new
// variant. Imported anonymously so it cannot collide with `crate::error::Error`.
use serde::de::Error as _;

use crate::api::auth::{self, Credentials};
use crate::config::ResolvedConfig;
use crate::error::{Error, PERMISSION_DENIED_CODE, Result, may_be_stale_session};

/// The discovery API. Queried once at startup.
pub const API_INFO: &str = "SYNO.API.Info";
/// Discovery is pinned to version 1: it is the endpoint that *tells* us the
/// versions of everything else, so it cannot itself be discovered.
pub const API_INFO_VERSION: u32 = 1;
/// `SYNO.API.Info` lives at this CGI, not at the `entry.cgi` the rest uses.
pub const QUERY_CGI: &str = "query.cgi";
/// Path segment every DSM API URL is rooted at.
pub const WEBAPI_PREFIX: &str = "webapi";

/// How long to wait for the TCP + TLS handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long any single request may take. File deletion deliberately uses
/// `start` + `status` polling so it never has to fit inside this.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// An inclusive `(min, max)` API version range.
pub type VersionRange = (u32, u32);

/// Build `{base}/webapi/{path}`, tolerating stray slashes on either side.
pub fn webapi_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{WEBAPI_PREFIX}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// The `api` / `version` / `method` triple every DSM request carries.
pub fn build_base_params(api: &str, version: u32, method: &str) -> Vec<(&'static str, String)> {
    vec![
        ("api", api.to_string()),
        ("version", version.to_string()),
        ("method", method.to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// The error object in a failed response: `{"error": {"code": 119}}`.
///
/// Some APIs add an `errors` array with per-item detail; it is captured
/// verbatim so a bug report can show it, but nothing depends on its shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DsmError {
    pub code: i32,
    #[serde(default)]
    pub errors: Vec<serde_json::Value>,
}

/// The standard DSM response wrapper.
///
/// `success` defaults to `false` so a body that is valid JSON but not a DSM
/// envelope is treated as a failure rather than silently succeeding with no
/// data.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    #[serde(default)]
    pub success: bool,
    // `Option` fields are implicitly optional to serde, and spelling
    // `#[serde(default)]` here would drag a needless `T: Default` bound into
    // the derived `Deserialize`.
    pub data: Option<T>,
    pub error: Option<DsmError>,
}

impl<T> Envelope<T> {
    /// Turn `success: false` into [`Error::Dsm`], keeping the payload as an
    /// `Option` — several methods (logout, pause, resume) legitimately answer
    /// with no `data` at all.
    pub fn into_result(self, api: &str) -> Result<Option<T>> {
        if self.success {
            return Ok(self.data);
        }
        match self.error {
            Some(err) => {
                // The per-item detail is the only place a File Station failure
                // says *which* path it was about, and `Error::Dsm` carries just
                // the code. Logged rather than folded into the message: it is
                // free-form JSON with no shape this client can rely on, but a
                // bug report needs it.
                if !err.errors.is_empty() {
                    tracing::warn!(api, code = err.code, detail = ?err.errors, "DSM per-item errors");
                }
                Err(Error::dsm(err.code, api))
            }
            // A NAS that reports failure without saying why is a protocol
            // violation, not a DSM error code we can translate.
            None => Err(protocol_error(format!(
                "{api} reported failure with no error code"
            ))),
        }
    }
}

/// Deserialize an envelope and require its payload.
pub fn parse_envelope<T: DeserializeOwned>(body: &str, api: &str) -> Result<T> {
    let envelope: Envelope<T> = serde_json::from_str(body)?;
    envelope
        .into_result(api)?
        .ok_or_else(|| protocol_error(format!("{api} reported success but returned no data")))
}

/// Check only whether a response succeeded, ignoring any payload.
///
/// Used both for no-data methods and to decide whether a request needs the
/// re-login retry, before committing to a concrete payload type.
pub fn check_envelope(body: &str, api: &str) -> Result<()> {
    let envelope: Envelope<serde::de::IgnoredAny> = serde_json::from_str(body)?;
    envelope.into_result(api).map(|_| ())
}

/// A response that broke the protocol rather than reporting a DSM error code.
/// Reuses [`Error::Parse`] — the body was not what this client can work with.
fn protocol_error(message: impl AsRef<str>) -> Error {
    Error::Parse(serde_json::Error::custom(message.as_ref()))
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// One entry of the `SYNO.API.Info` `query` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiInfo {
    /// CGI path relative to `/webapi/`, e.g. `entry.cgi`.
    pub path: String,
    pub min_version: u32,
    pub max_version: u32,
}

/// Everything the NAS said it supports, keyed by API name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiInfoMap {
    apis: BTreeMap<String, ApiInfo>,
}

impl ApiInfoMap {
    /// Parse a full `SYNO.API.Info` `query` response body.
    pub fn from_response(body: &str) -> Result<Self> {
        let apis: BTreeMap<String, ApiInfo> = parse_envelope(body, API_INFO)?;
        Ok(ApiInfoMap { apis })
    }

    /// Build directly from entries. For tests and for `--dump-api-info`.
    pub fn from_entries(entries: impl IntoIterator<Item = (String, ApiInfo)>) -> Self {
        ApiInfoMap {
            apis: entries.into_iter().collect(),
        }
    }

    /// Look up an API, reporting an absent one by DSM package name.
    pub fn get(&self, api: &str) -> Result<&ApiInfo> {
        self.apis.get(api).ok_or_else(|| Error::api_missing(api))
    }

    pub fn len(&self) -> usize {
        self.apis.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apis.is_empty()
    }

    /// Everything needed to issue a request: URL, negotiated version, API name.
    ///
    /// The version is the highest both this client and the NAS understand:
    /// `supported` is the range *this* client implements, and the result is the
    /// top of its overlap with what the NAS advertises. A non-overlapping range
    /// is an error naming both, which is the only actionable thing to say
    /// about it.
    pub fn endpoint(&self, base_url: &str, api: &str, supported: VersionRange) -> Result<Endpoint> {
        let info = self.get(api)?;
        let version = pick_version_in(api, (info.min_version, info.max_version), supported)?;
        Ok(Endpoint {
            api: api.to_string(),
            url: webapi_url(base_url, &info.path),
            version,
        })
    }
}

/// Intersect two inclusive version ranges and take the top of the overlap.
///
/// Split out from [`ApiInfoMap::endpoint`] so the arithmetic is testable
/// without building a map.
pub fn pick_version_in(api: &str, nas: VersionRange, supported: VersionRange) -> Result<u32> {
    let low = nas.0.max(supported.0);
    let high = nas.1.min(supported.1);
    if low > high {
        return Err(Error::api_version_mismatch(api, nas, supported));
    }
    Ok(high)
}

/// A resolved call target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub api: String,
    pub url: String,
    pub version: u32,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// The authenticated DSM client.
///
/// Holds the shared `reqwest::Client`, the discovery map, the current `sid`
/// and — so the transparent retry has something to retry *with* — the
/// credentials used to obtain it.
#[derive(Debug)]
pub struct SynoClient {
    http: reqwest::Client,
    base_url: String,
    apis: ApiInfoMap,
    sid: RwLock<Option<String>>,
    credentials: Option<Credentials>,
    /// Set once a re-login has failed to clear a [`PERMISSION_DENIED_CODE`],
    /// after which 105 stops being treated as a stale session.
    ///
    /// Without this, an account that genuinely may not use the API would
    /// re-authenticate on every request — and the poller makes one every
    /// `refresh_secs`, so a permanently misconfigured account would produce a
    /// login every three seconds for as long as the program is open.
    permission_is_real: AtomicBool,
}

impl SynoClient {
    /// Build a client for a resolved configuration.
    ///
    /// `insecure` maps to `danger_accept_invalid_certs`: a NAS on the LAN
    /// almost always has a self-signed certificate, and the flag is how the
    /// user says so explicitly.
    pub fn new(config: &ResolvedConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(config.insecure)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(SynoClient {
            http,
            base_url: config.base_url(),
            apis: ApiInfoMap::default(),
            sid: RwLock::new(None),
            credentials: None,
            permission_is_real: AtomicBool::new(false),
        })
    }

    /// Attach the credentials the transparent re-login will use.
    pub fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Seed a `sid` cached from a previous run.
    pub fn with_sid(self, sid: impl Into<String>) -> Self {
        self.set_sid(sid);
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The current session ID, if the client has one.
    pub fn sid(&self) -> Option<String> {
        self.read_sid().clone()
    }

    pub fn set_sid(&self, sid: impl Into<String>) {
        *self.write_sid() = Some(sid.into());
    }

    pub fn clear_sid(&self) {
        *self.write_sid() = None;
    }

    fn read_sid(&self) -> std::sync::RwLockReadGuard<'_, Option<String>> {
        self.sid.read().unwrap_or_else(|err| err.into_inner())
    }

    fn write_sid(&self) -> std::sync::RwLockWriteGuard<'_, Option<String>> {
        self.sid.write().unwrap_or_else(|err| err.into_inner())
    }

    /// Query `SYNO.API.Info` and remember what the NAS supports.
    ///
    /// Called once at startup. Everything afterwards resolves its URL and
    /// version out of this map, so no version is ever hardcoded.
    pub async fn discover(&mut self) -> Result<()> {
        self.apis = ApiInfoMap::from_response(&self.discovery_json().await?)?;
        tracing::info!(count = self.apis.len(), "discovered DSM APIs");
        Ok(())
    }

    /// The raw discovery response, for the hidden `--dump-api-info` flag.
    pub async fn discovery_json(&self) -> Result<String> {
        let url = webapi_url(&self.base_url, QUERY_CGI);
        let mut params = build_base_params(API_INFO, API_INFO_VERSION, "query");
        params.push(("query", "all".to_string()));
        self.fetch_text(&url, &params).await
    }

    /// Resolve an API name plus the version range this client implements into
    /// a concrete call target.
    pub fn endpoint(&self, api: &str, supported: VersionRange) -> Result<Endpoint> {
        self.apis.endpoint(&self.base_url, api, supported)
    }

    /// Issue a request and return the decoded payload, retrying once through a
    /// fresh login if the session was rejected.
    pub async fn call<T: DeserializeOwned>(
        &self,
        api: &str,
        method: &str,
        supported: VersionRange,
        params: &[(&str, String)],
    ) -> Result<T> {
        let body = self.call_text(api, method, supported, params).await?;
        parse_envelope(&body, api)
    }

    /// The retry seam: fetch the body, and if DSM rejected the session,
    /// re-authenticate **once** and fetch it again exactly once.
    ///
    /// Returns the raw body so the caller decides how to decode it; any
    /// non-session DSM error short-circuits here.
    pub async fn call_text(
        &self,
        api: &str,
        method: &str,
        supported: VersionRange,
        params: &[(&str, String)],
    ) -> Result<String> {
        let endpoint = self.endpoint(api, supported)?;
        let body = self.send(&endpoint, method, params, self.sid()).await?;

        match check_envelope(&body, api) {
            Ok(()) => Ok(body),
            Err(Error::Dsm { code, .. }) if self.should_retry_session(code) => {
                tracing::info!(
                    api,
                    method,
                    code,
                    "session rejected, re-authenticating once"
                );
                self.clear_sid();
                let sid = self.relogin().await?;
                let retried = self.send(&endpoint, method, params, Some(sid)).await?;

                // 105 is ambiguous (see `error::may_be_stale_session`), and this
                // is where the ambiguity is settled: a fresh session did not
                // clear it, so it is a real permission problem and re-logging in
                // for the next one would achieve nothing but a login per poll.
                if code == PERMISSION_DENIED_CODE
                    && matches!(
                        check_envelope(&retried, api),
                        Err(Error::Dsm {
                            code: PERMISSION_DENIED_CODE,
                            ..
                        })
                    )
                {
                    self.permission_is_real.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        api,
                        method,
                        "a fresh session is still refused; treating this as a real \
                         permission problem and not re-authenticating again"
                    );
                }
                Ok(retried)
            }
            Err(err) => Err(err),
        }
    }

    /// Whether a failing `code` justifies one re-login and replay.
    ///
    /// Needs credentials to be worth attempting at all, and — for
    /// [`PERMISSION_DENIED_CODE`] only — must not already have been shown that a
    /// fresh session makes no difference.
    fn should_retry_session(&self, code: i32) -> bool {
        if !self.can_relogin() || !may_be_stale_session(code) {
            return false;
        }
        code != PERMISSION_DENIED_CODE || !self.permission_is_real.load(Ordering::Relaxed)
    }

    /// One **POST** against an already-resolved endpoint, carrying `params` in
    /// the form body instead of the query string.
    ///
    /// Exists for exactly one caller: [`crate::api::auth::login`]. A DSM query
    /// string is written verbatim to the NAS's nginx access log (and to any
    /// proxy's in between), so a `passwd=` there is the account password
    /// persisted to disk on every login. Only the routing triple —
    /// `api`, `version`, `method` — stays in the query.
    pub async fn post_form(
        &self,
        endpoint: &Endpoint,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<String> {
        let query = build_base_params(&endpoint.api, endpoint.version, method);
        tracing::debug!(url = %endpoint.url, method, "POST");
        let response = self
            .http
            .post(&endpoint.url)
            .query(&query)
            .form(params)
            .send()
            .await?
            .error_for_status()?;
        Ok(response.text().await?)
    }

    /// One request against an already-resolved endpoint. No retry, no envelope
    /// interpretation — used by [`Self::call_text`] and by `auth`, which must
    /// not recurse into the retry that calls it.
    pub async fn send(
        &self,
        endpoint: &Endpoint,
        method: &str,
        params: &[(&str, String)],
        sid: Option<String>,
    ) -> Result<String> {
        let mut query = build_base_params(&endpoint.api, endpoint.version, method);
        query.extend(params.iter().map(|(k, v)| (*k, v.clone())));
        if let Some(sid) = sid {
            query.push(("_sid", sid));
        }
        self.fetch_text(&endpoint.url, &query).await
    }

    /// Whether a rejected session can be repaired without asking the user.
    fn can_relogin(&self) -> bool {
        self.credentials.is_some()
    }

    /// Log in again with the stored credentials and adopt the new `sid`.
    pub async fn relogin(&self) -> Result<String> {
        let credentials = self.credentials.as_ref().ok_or_else(|| {
            Error::Auth("the session expired and no credentials are available to renew it".into())
        })?;
        let sid = auth::login(self, credentials).await?;
        self.set_sid(&sid);
        Ok(sid)
    }

    /// The single place an HTTP request is actually made.
    async fn fetch_text(&self, url: &str, query: &[(&str, String)]) -> Result<String> {
        tracing::debug!(url, "GET");
        let response = self
            .http
            .get(url)
            .query(query)
            .send()
            .await?
            .error_for_status()?;
        Ok(response.text().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::offline_client;

    const DS_TASK: &str = "SYNO.DownloadStation.Task";
    const FS_LIST: &str = "SYNO.FileStation.List";

    fn info(path: &str, min: u32, max: u32) -> ApiInfo {
        ApiInfo {
            path: path.to_string(),
            min_version: min,
            max_version: max,
        }
    }

    /// Negotiate a version through the one production entry point.
    fn sample_endpoint(map: &ApiInfoMap, api: &str, supported: VersionRange) -> Result<Endpoint> {
        map.endpoint("https://nas.local:5001", api, supported)
    }

    fn sample_map() -> ApiInfoMap {
        ApiInfoMap::from_entries([
            (DS_TASK.to_string(), info("DownloadStation/task.cgi", 1, 3)),
            (FS_LIST.to_string(), info("entry.cgi", 1, 2)),
        ])
    }

    // ---- envelope ---------------------------------------------------------

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Payload {
        total: u32,
        name: String,
    }

    #[test]
    fn success_envelope_yields_the_payload() {
        let body = r#"{"success": true, "data": {"total": 2, "name": "ok"}}"#;
        let parsed: Payload = parse_envelope(body, DS_TASK).expect("success payload");
        assert_eq!(
            parsed,
            Payload {
                total: 2,
                name: "ok".into()
            }
        );
    }

    #[test]
    fn error_envelope_becomes_a_dsm_error_carrying_the_code_and_api() {
        let body = r#"{"success": false, "error": {"code": 119}}"#;
        let err = parse_envelope::<Payload>(body, DS_TASK).expect_err("failure envelope");
        match err {
            Error::Dsm { code, ref api } => {
                assert_eq!(code, 119);
                assert_eq!(api, DS_TASK);
            }
            other => panic!("expected Error::Dsm, got {other:?}"),
        }
        assert!(err.to_string().contains("invalid session ID"), "{err}");
    }

    #[test]
    fn error_envelope_keeps_the_per_item_errors_array() {
        let body = r#"{"success": false, "error": {"code": 408, "errors": [{"path": "/a"}]}}"#;
        let envelope: Envelope<Payload> = serde_json::from_str(body).expect("valid JSON");
        let err = envelope.error.expect("error object");
        assert_eq!(err.code, 408);
        assert_eq!(err.errors.len(), 1);
    }

    #[test]
    fn malformed_body_is_a_parse_error_not_a_panic() {
        for body in ["", "not json at all", "{\"success\":", "[1, 2, 3]"] {
            let err = parse_envelope::<Payload>(body, DS_TASK)
                .expect_err("malformed body must not parse");
            assert!(matches!(err, Error::Parse(_)), "{body:?} gave {err:?}");
        }
    }

    #[test]
    fn payload_of_the_wrong_shape_is_a_parse_error() {
        let body = r#"{"success": true, "data": {"total": "two"}}"#;
        let err = parse_envelope::<Payload>(body, DS_TASK).expect_err("wrong payload shape");
        assert!(matches!(err, Error::Parse(_)), "{err:?}");
    }

    #[test]
    fn success_without_data_is_rejected_when_a_payload_is_required() {
        let body = r#"{"success": true}"#;
        let err = parse_envelope::<Payload>(body, DS_TASK).expect_err("no data");
        assert!(matches!(err, Error::Parse(_)), "{err:?}");
        assert!(err.to_string().contains("no data"), "{err}");
    }

    #[test]
    fn success_without_data_is_fine_for_no_data_methods() {
        // logout / pause / resume answer with a bare `{"success": true}`.
        check_envelope(r#"{"success": true}"#, DS_TASK).expect("no-data success");
        check_envelope(r#"{"success": true, "data": null}"#, DS_TASK).expect("explicit null");

        // The envelope itself keeps the absent payload as `None` rather than
        // inventing one — `parse_envelope` is what turns that into an error.
        let envelope: Envelope<Payload> =
            serde_json::from_str(r#"{"success": true}"#).expect("valid JSON");
        assert_eq!(envelope.into_result(DS_TASK).expect("optional"), None);
    }

    #[test]
    fn check_envelope_reports_the_dsm_code() {
        let err = check_envelope(r#"{"success": false, "error": {"code": 105}}"#, DS_TASK)
            .expect_err("failure");
        assert!(matches!(err, Error::Dsm { code: 105, .. }), "{err:?}");
    }

    #[test]
    fn failure_without_an_error_code_is_a_protocol_error() {
        // Not translatable to a DSM message — there is no code to translate.
        let err = check_envelope(r#"{"success": false}"#, DS_TASK).expect_err("no code");
        assert!(matches!(err, Error::Parse(_)), "{err:?}");
    }

    #[test]
    fn a_body_that_is_json_but_not_an_envelope_is_not_a_silent_success() {
        let err =
            check_envelope(r#"{"totally": "unrelated"}"#, DS_TASK).expect_err("not an envelope");
        assert!(matches!(err, Error::Parse(_)), "{err:?}");
    }

    // ---- API info map -----------------------------------------------------

    #[test]
    fn discovery_response_parses_into_the_map() {
        let body = r#"{
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 7},
                "SYNO.DownloadStation.Task": {
                    "path": "DownloadStation/task.cgi", "minVersion": 1, "maxVersion": 3
                }
            }
        }"#;
        let map = ApiInfoMap::from_response(body).expect("valid discovery response");
        assert_eq!(map.len(), 2);
        let info = map.get(DS_TASK).expect("present");
        assert_eq!(info.path, "DownloadStation/task.cgi");
        assert_eq!((info.min_version, info.max_version), (1, 3));
    }

    #[test]
    fn discovery_failure_surfaces_as_a_dsm_error() {
        let body = r#"{"success": false, "error": {"code": 105}}"#;
        let err = ApiInfoMap::from_response(body).expect_err("failed discovery");
        assert!(matches!(err, Error::Dsm { code: 105, .. }), "{err:?}");
    }

    #[test]
    fn looking_up_a_present_api_succeeds() {
        let map = sample_map();
        assert!(map.get(DS_TASK).is_ok());
        assert_eq!(map.get(FS_LIST).expect("present").path, "entry.cgi");
        assert!(!map.is_empty());
    }

    #[test]
    fn looking_up_a_missing_api_names_the_dsm_package() {
        let map = ApiInfoMap::from_entries([(DS_TASK.to_string(), info("task.cgi", 1, 3))]);
        let err = map.get(FS_LIST).expect_err("absent API");
        assert!(matches!(err, Error::ApiUnavailable { .. }), "{err:?}");
        assert!(
            err.to_string().starts_with("File Station is not installed"),
            "{err}"
        );
    }

    #[test]
    fn empty_map_reports_everything_missing() {
        let map = ApiInfoMap::default();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert!(map.get(DS_TASK).is_err());
    }

    // ---- pick_version -----------------------------------------------------

    #[test]
    fn pick_version_clamps_to_the_nas_max() {
        // NAS tops out at 3, we would go to 5 — take 3.
        let map = sample_map();
        assert_eq!(
            sample_endpoint(&map, DS_TASK, (1, 5))
                .expect("overlap")
                .version,
            3
        );
    }

    #[test]
    fn pick_version_clamps_to_the_supported_max() {
        // The NAS offers 3 but this client only implements up to 1.
        let map = sample_map();
        assert_eq!(
            sample_endpoint(&map, DS_TASK, (1, 1))
                .expect("overlap")
                .version,
            1
        );
    }

    #[test]
    fn pick_version_takes_the_top_of_the_overlap() {
        assert_eq!(
            pick_version_in(DS_TASK, (2, 6), (3, 4)).expect("overlap"),
            4
        );
        assert_eq!(
            pick_version_in(DS_TASK, (1, 1), (1, 1)).expect("overlap"),
            1
        );
    }

    #[test]
    fn pick_version_errors_when_the_nas_is_too_old() {
        // DSM 6-era NAS: it offers only v1, this client needs v2+.
        let err = pick_version_in(DS_TASK, (1, 1), (2, 3)).expect_err("no overlap");
        match err {
            Error::ApiUnavailable { ref api, reason } => {
                assert_eq!(api, DS_TASK);
                assert!(
                    matches!(
                        reason,
                        crate::error::ApiUnavailableReason::VersionMismatch {
                            nas: (1, 1),
                            supported: (2, 3)
                        }
                    ),
                    "{reason:?}"
                );
            }
            other => panic!("expected ApiUnavailable, got {other:?}"),
        }
        let rendered = err.to_string();
        assert!(rendered.contains("1-1"), "{rendered}");
        assert!(rendered.contains("2-3"), "{rendered}");
    }

    #[test]
    fn pick_version_errors_when_the_nas_is_too_new() {
        let err = pick_version_in(FS_LIST, (5, 9), (1, 2)).expect_err("no overlap");
        assert!(matches!(err, Error::ApiUnavailable { .. }), "{err:?}");
    }

    #[test]
    fn pick_version_on_a_missing_api_reports_the_api_not_the_range() {
        let err = sample_endpoint(&ApiInfoMap::default(), FS_LIST, (1, 2)).expect_err("absent API");
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    // ---- URL and parameter construction -----------------------------------

    #[test]
    fn webapi_urls_are_built_from_the_discovered_path() {
        assert_eq!(
            webapi_url("https://nas.local:5001", "DownloadStation/task.cgi"),
            "https://nas.local:5001/webapi/DownloadStation/task.cgi"
        );
        // Stray slashes on either side must not double up.
        assert_eq!(
            webapi_url("https://nas.local:5001/", "/entry.cgi"),
            "https://nas.local:5001/webapi/entry.cgi"
        );
    }

    #[test]
    fn discovery_is_not_served_from_entry_cgi() {
        // The whole point of the special case: query.cgi, always.
        assert_eq!(
            webapi_url("http://nas:5000", QUERY_CGI),
            "http://nas:5000/webapi/query.cgi"
        );
        assert_eq!(API_INFO_VERSION, 1);
    }

    #[test]
    fn endpoint_combines_base_url_discovered_path_and_negotiated_version() {
        let endpoint = sample_map()
            .endpoint("https://nas.local:5001", DS_TASK, (1, 1))
            .expect("resolvable");
        assert_eq!(
            endpoint,
            Endpoint {
                api: DS_TASK.to_string(),
                url: "https://nas.local:5001/webapi/DownloadStation/task.cgi".to_string(),
                version: 1,
            }
        );
    }

    #[test]
    fn endpoint_propagates_a_missing_api() {
        let err = sample_map()
            .endpoint("https://nas.local:5001", "SYNO.FileStation.Delete", (1, 2))
            .expect_err("absent API");
        assert!(matches!(err, Error::ApiUnavailable { .. }), "{err:?}");
    }

    // ---- the sid ----------------------------------------------------------

    #[test]
    fn a_fresh_client_carries_no_session_and_cannot_repair_one() {
        let client = offline_client();
        assert_eq!(client.sid(), None);
        // No credentials: a rejected session has to fail rather than loop.
        assert!(!client.can_relogin());
        assert!(client.base_url().starts_with("https://nas.invalid:5001"));
    }

    #[test]
    fn a_sid_can_be_seeded_replaced_and_cleared() {
        let client = offline_client().with_sid("cached");
        assert_eq!(client.sid().as_deref(), Some("cached"));

        // The re-login path replaces rather than appends.
        client.set_sid("renewed");
        assert_eq!(client.sid().as_deref(), Some("renewed"));

        client.clear_sid();
        assert_eq!(client.sid(), None);
    }

    #[test]
    fn credentials_are_what_make_the_transparent_retry_possible() {
        let client = offline_client().with_credentials(Credentials::new("eduard", "hunter2"));
        assert!(client.can_relogin());
        // And the password is not reachable through the derived `Debug`.
        assert!(!format!("{client:?}").contains("hunter2"));
    }

    #[test]
    fn base_params_carry_api_version_and_method() {
        assert_eq!(
            build_base_params(DS_TASK, 3, "list"),
            vec![
                ("api", DS_TASK.to_string()),
                ("version", "3".to_string()),
                ("method", "list".to_string()),
            ]
        );
    }
}
