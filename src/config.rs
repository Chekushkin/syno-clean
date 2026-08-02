//! Configuration: TOML file, environment overrides, CLI merge and validation,
//! plus the XDG paths, the session-ID cache and logging setup that hang off it.
//!
//! **Precedence is CLI flags > `SYNO_CLEAN_*` env vars > config file >
//! defaults**, resolved once in [`merge`], which also enforces that `host` and
//! `username` are present. Every later module takes a [`ResolvedConfig`] and
//! may assume those two are non-empty.
//!
//! Two deliberate testability seams:
//!
//! * Filesystem locations come from a [`Paths`] value. Production builds it
//!   with [`Paths::discover`] (XDG on every platform, via `etcetera`); tests
//!   build it with [`Paths::with_base`] against a temporary directory, so no
//!   test ever reads or writes the real `~/.config` or `~/.cache`.
//! * Environment reads go through a `&dyn Fn(&str) -> Option<String>` lookup
//!   rather than `std::env` directly, so env-precedence tests are pure and
//!   safe to run in parallel.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::WorkerGuard;

use crate::cli::Cli;
use crate::error::{Error, Result};

/// Directory name used under both the XDG config and cache roots.
pub const APP_DIR: &str = "syno-clean";
/// Config file name inside the config directory.
pub const CONFIG_FILE: &str = "config.toml";
/// Session cache file name inside the cache directory.
pub const SESSION_FILE: &str = "session.json";
/// Log file name inside the cache directory.
pub const LOG_FILE: &str = "syno-clean.log";

/// Default to HTTPS: DSM's HTTP port usually redirects anyway.
pub const DEFAULT_HTTPS: bool = true;
/// DSM's default HTTPS management port.
pub const DEFAULT_HTTPS_PORT: u16 = 5001;
/// DSM's default HTTP management port.
pub const DEFAULT_HTTP_PORT: u16 = 5000;
/// Default seconds between automatic refreshes.
pub const DEFAULT_REFRESH_SECS: u64 = 3;
/// Deleting the files as well as the task is the entire point of the tool.
pub const DEFAULT_DELETE_FILES: bool = true;
/// Certificate validation is on unless explicitly disabled.
pub const DEFAULT_INSECURE: bool = false;

/// Environment variable names, all of them.
pub mod env_vars {
    pub const HOST: &str = "SYNO_CLEAN_HOST";
    pub const PORT: &str = "SYNO_CLEAN_PORT";
    pub const HTTPS: &str = "SYNO_CLEAN_HTTPS";
    pub const INSECURE: &str = "SYNO_CLEAN_INSECURE";
    pub const USERNAME: &str = "SYNO_CLEAN_USERNAME";
    pub const PASSWORD: &str = "SYNO_CLEAN_PASSWORD";
    pub const OTP: &str = "SYNO_CLEAN_OTP";
    pub const REFRESH_SECS: &str = "SYNO_CLEAN_REFRESH_SECS";
}

/// How the config layers read the environment.
///
/// Taking a lookup function instead of calling `std::env::var` keeps the
/// precedence logic pure and lets tests run in parallel without racing on
/// process-global state.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The real environment. Pass this in `main`.
pub fn system_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// One configuration layer: every field optional, because "absent" and
/// "explicitly set to the default value" have to stay distinguishable for
/// precedence to work.
///
/// Deserialized without `deny_unknown_fields` on purpose — an older binary
/// must tolerate a newer config file. Unknown keys are reported by
/// [`parse_config`] and logged as warnings by [`Config::load`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub https: Option<bool>,
    pub insecure: Option<bool>,
    pub username: Option<String>,
    pub refresh_secs: Option<u64>,
    pub delete_files: Option<bool>,
}

/// Keys this version understands; anything else in the file is ignored.
pub const KNOWN_KEYS: [&str; 7] = [
    "host",
    "port",
    "https",
    "insecure",
    "username",
    "refresh_secs",
    "delete_files",
];

impl Config {
    /// Read and parse a config file.
    ///
    /// A missing file is **not** an error — `syno-clean --host nas --user me`
    /// must work on a clean machine — it yields an empty layer. Unknown keys
    /// are logged and ignored.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no config file, using defaults");
                return Ok(Config::default());
            }
            Err(err) => {
                return Err(Error::config(format!(
                    "cannot read config file {}: {err}",
                    path.display()
                )));
            }
        };

        let (config, unknown) = parse_config(&text)
            .map_err(|err| Error::config(format!("{}: {err}", path.display())))?;
        for key in &unknown {
            tracing::warn!(
                path = %path.display(),
                key = %key,
                "unknown config key ignored"
            );
        }
        Ok(config)
    }

    /// Build a configuration layer from the environment.
    pub fn from_env(get: EnvLookup<'_>) -> Result<Self> {
        Ok(Config {
            host: get(env_vars::HOST).filter(|v| !v.is_empty()),
            port: parse_env(get, env_vars::PORT, |v| v.parse::<u16>().ok())?,
            https: parse_env(get, env_vars::HTTPS, parse_bool)?,
            insecure: parse_env(get, env_vars::INSECURE, parse_bool)?,
            username: get(env_vars::USERNAME).filter(|v| !v.is_empty()),
            refresh_secs: parse_env(get, env_vars::REFRESH_SECS, |v| v.parse::<u64>().ok())?,
            // No `SYNO_CLEAN_DELETE_FILES`: `--no-delete-files` and the config
            // key cover it, and an env var that silently disables the tool's
            // main function is a footgun.
            delete_files: None,
        })
    }
}

/// Parse a config file body, returning the config and any unrecognized keys.
///
/// Split out from [`Config::load`] so the unknown-key behaviour is testable
/// without touching the filesystem.
pub fn parse_config(text: &str) -> std::result::Result<(Config, Vec<String>), toml::de::Error> {
    let table: toml::Table = toml::from_str(text)?;
    let unknown: Vec<String> = table
        .keys()
        .filter(|k| !KNOWN_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();
    let config: Config = toml::from_str(text)?;
    Ok((config, unknown))
}

/// Read one environment variable and parse it, turning a bad value into an
/// actionable error rather than silently ignoring it.
fn parse_env<T>(
    get: EnvLookup<'_>,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>> {
    match get(key) {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => parse(raw.trim())
            .map(Some)
            .ok_or_else(|| Error::config(format!("{key}: cannot parse {raw:?}"))),
    }
}

/// Accept the spellings people actually type in a shell.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// What to say when `host` could not be resolved from any layer.
///
/// One definition, used both by [`merge`] (which refuses to produce a
/// [`ResolvedConfig`] without it) and by the first-run path in `main`, which
/// prints the same sentence beside the config template it just wrote.
pub const MISSING_HOST_HELP: &str = "no NAS host configured — pass --host <HOST>, \
     set SYNO_CLEAN_HOST, or add `host = \"nas.local\"` to the config file";

/// What to say when `username` could not be resolved from any layer.
pub const MISSING_USERNAME_HELP: &str = "no DSM username configured — pass --user <NAME>, \
     set SYNO_CLEAN_USERNAME, or add `username = \"admin\"` to the config file";

/// Which required values are still unresolved after the CLI/env/file merge.
///
/// A merely *missing config file* is not an error — `syno-clean --host nas
/// --user me` must work on a clean machine — so "should we write a template and
/// stop?" is a question about the **merged** values, never about the file. This
/// is that question, asked without building (and failing to build) a
/// [`ResolvedConfig`]; [`merge`] enforces exactly the same rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MissingRequired {
    pub host: bool,
    pub username: bool,
}

impl MissingRequired {
    /// Whether anything required is missing.
    pub fn any(self) -> bool {
        self.host || self.username
    }

    /// The actionable sentence for each missing value, in flag order.
    pub fn help_lines(self) -> Vec<&'static str> {
        let mut lines = Vec::new();
        if self.host {
            lines.push(MISSING_HOST_HELP);
        }
        if self.username {
            lines.push(MISSING_USERNAME_HELP);
        }
        lines
    }
}

/// Ask whether the three layers resolve the required values, without merging.
///
/// Uses the very same resolution [`merge`] does — see [`resolved_host`] and
/// [`resolved_username`] — so the two can never disagree about what counts as
/// configured.
pub fn missing_required(file: &Config, env: &Config, cli: &Cli) -> MissingRequired {
    MissingRequired {
        host: resolved_host(file, env, cli).is_none(),
        username: resolved_username(file, env, cli).is_none(),
    }
}

/// The `host` the three layers resolve to, trimmed, if any layer supplies one.
fn resolved_host(file: &Config, env: &Config, cli: &Cli) -> Option<String> {
    first_set(
        cli.host.as_deref(),
        env.host.as_deref(),
        file.host.as_deref(),
    )
}

/// The `username` the three layers resolve to, trimmed, if any layer supplies
/// one.
fn resolved_username(file: &Config, env: &Config, cli: &Cli) -> Option<String> {
    first_set(
        cli.username.as_deref(),
        env.username.as_deref(),
        file.username.as_deref(),
    )
}

/// CLI beats env beats file, and a value that is only whitespace counts as
/// **unset at the layer that supplied it** — it does not fall through, because
/// `--host "  "` is a mistake to report, not a reason to silently use the
/// config file's host.
fn first_set(cli: Option<&str>, env: Option<&str>, file: Option<&str>) -> Option<String> {
    cli.or(env)
        .or(file)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A commented starter config, written on a first run that has nothing to
/// connect to.
///
/// **Every key is commented out.** A template that shipped a live
/// `host = "nas.local"` would send the next invocation off to a host the user
/// never named; leaving the examples inert means the file documents the format
/// and changes nothing until it is edited. The password is deliberately absent:
/// it is never stored here.
pub const CONFIG_TEMPLATE: &str = r#"# syno-clean configuration
#
# Precedence: command-line flags beat SYNO_CLEAN_* environment variables,
# which beat this file, which beats the built-in defaults.
#
# `host` and `username` are required. Uncomment and fill them in below, or
# pass --host and --user on the command line.
#
# The password is never stored here: set SYNO_CLEAN_PASSWORD or type it at
# the prompt. A 2-step verification code comes from SYNO_CLEAN_OTP or the
# prompt DSM asks for.

# DSM hostname or IP address (required).
# host = "nas.local"

# DSM account name (required).
# username = "admin"

# DSM management port. Defaults to 5001 with https, 5000 without.
# port = 5001

# Talk to DSM over HTTPS.
# https = true

# Accept a self-signed or otherwise invalid TLS certificate.
# insecure = false

# Seconds between automatic task-list refreshes.
# refresh_secs = 3

# Delete the downloaded files as well as the Download Station task.
# Set to false to remove the task only and leave the files on the volume.
# delete_files = true
"#;

/// Write [`CONFIG_TEMPLATE`] to `path`, creating the directory if needed.
///
/// Returns whether it wrote anything: an existing file is **never** overwritten,
/// however incomplete it is. Losing a user's settings while explaining that
/// their settings are incomplete would be an unusually poor trade.
pub fn write_config_template(path: &Path) -> Result<bool> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // `create_new` rather than a prior `exists()` check: the check-then-write
    // race is small but the cost of losing the file is not.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(CONFIG_TEMPLATE.as_bytes())?;
            file.flush()?;
            tracing::info!(path = %path.display(), "wrote a starter config");
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(Error::Io(err)),
    }
}

/// Everything the rest of the program needs, with every value resolved and
/// validated. `host` and `username` are guaranteed non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub host: String,
    pub port: u16,
    pub https: bool,
    pub insecure: bool,
    pub username: String,
    pub refresh_secs: u64,
    pub delete_files: bool,
    pub dry_run: bool,
    pub logout: bool,
}

impl ResolvedConfig {
    /// `https://host:port` — the root every API URL is built from.
    pub fn base_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// Session-cache key: `{host}:{port}/{username}`, so two NASes or two
    /// accounts on one NAS never evict each other.
    pub fn session_key(&self) -> String {
        session_key(&self.host, self.port, &self.username)
    }
}

/// Session-cache key for a host/port/user triple.
pub fn session_key(host: &str, port: u16, username: &str) -> String {
    format!("{host}:{port}/{username}")
}

/// Collapse the three layers into a validated [`ResolvedConfig`].
///
/// Precedence is CLI > env > file > default. Boolean CLI flags are one-way:
/// `--insecure` and `--dry-run` can only turn something on, `--no-delete-files`
/// can only turn something off, so an unset flag falls through to the lower
/// layers instead of overriding them with `false`.
pub fn merge(file: Config, env: Config, cli: &Cli) -> Result<ResolvedConfig> {
    let host = resolved_host(&file, &env, cli).ok_or_else(|| Error::config(MISSING_HOST_HELP))?;
    let username =
        resolved_username(&file, &env, cli).ok_or_else(|| Error::config(MISSING_USERNAME_HELP))?;

    // There is no `--https` / `--no-https` flag — HTTPS is chosen by config or
    // env only — so the CLI layer contributes nothing to this one.
    let https = env.https.or(file.https).unwrap_or(DEFAULT_HTTPS);

    let port = cli.port.or(env.port).or(file.port).unwrap_or(if https {
        DEFAULT_HTTPS_PORT
    } else {
        DEFAULT_HTTP_PORT
    });

    let insecure = if cli.insecure {
        true
    } else {
        env.insecure.or(file.insecure).unwrap_or(DEFAULT_INSECURE)
    };

    let refresh_secs = cli
        .refresh_secs
        .or(env.refresh_secs)
        .or(file.refresh_secs)
        .unwrap_or(DEFAULT_REFRESH_SECS);
    if refresh_secs == 0 {
        return Err(Error::config(
            "refresh_secs must be at least 1 — a zero-second refresh would hammer the NAS",
        ));
    }

    let delete_files = if cli.no_delete_files {
        false
    } else {
        env.delete_files
            .or(file.delete_files)
            .unwrap_or(DEFAULT_DELETE_FILES)
    };

    Ok(ResolvedConfig {
        host,
        port,
        https,
        insecure,
        username,
        refresh_secs,
        delete_files,
        dry_run: cli.dry_run,
        logout: cli.logout,
    })
}

/// The DSM password, from the environment or an interactive prompt.
///
/// It is never read from, or written to, the config file. The prompt must
/// happen **before** the alternate screen is entered.
pub fn resolve_password(get: EnvLookup<'_>, username: &str, host: &str) -> Result<String> {
    if let Some(password) = get(env_vars::PASSWORD).filter(|p| !p.is_empty()) {
        return Ok(password);
    }
    let prompt = format!("DSM password for {username}@{host}: ");
    rpassword::prompt_password(prompt).map_err(|err| {
        Error::config(format!(
            "no password available: {} is unset and the prompt failed: {err}",
            env_vars::PASSWORD
        ))
    })
}

/// A 2FA code, if one was supplied up front. DSM only asks for it after a
/// login attempt returns 403, so an absent code is normal.
pub fn otp_from_env(get: EnvLookup<'_>) -> Option<String> {
    get(env_vars::OTP).filter(|c| !c.is_empty())
}

/// Where syno-clean keeps its files.
///
/// XDG semantics on *all* platforms so the documented paths are the real ones
/// on macOS too. [`Paths::with_base`] is the test seam — it never consults the
/// environment or the home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Paths {
    /// Resolve from the XDG base directories (`~/.config`, `~/.cache`).
    pub fn discover() -> Result<Self> {
        use etcetera::base_strategy::{BaseStrategy, Xdg};

        let strategy = Xdg::new()
            .map_err(|err| Error::config(format!("cannot locate the home directory: {err}")))?;
        Ok(Paths {
            config_dir: strategy.config_dir().join(APP_DIR),
            cache_dir: strategy.cache_dir().join(APP_DIR),
        })
    }

    /// Root everything at `base`, mirroring the XDG layout underneath it.
    /// Used by tests so the real user's files are never touched.
    pub fn with_base(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        Paths {
            config_dir: base.join("config").join(APP_DIR),
            cache_dir: base.join("cache").join(APP_DIR),
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }

    pub fn session_file(&self) -> PathBuf {
        self.cache_dir.join(SESSION_FILE)
    }

    pub fn log_file(&self) -> PathBuf {
        self.cache_dir.join(LOG_FILE)
    }
}

/// Cached DSM session IDs, keyed by `{host}:{port}/{username}`.
///
/// Reusing a `sid` is what makes a second invocation start instantly. The file
/// is mode `0600` because a sid is a bearer credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCache {
    #[serde(default)]
    sessions: BTreeMap<String, String>,
}

impl SessionCache {
    /// Load the cache, tolerating both absence and corruption.
    ///
    /// A cache is an optimization; a damaged one must never stop the program
    /// from starting, so it is logged and treated as empty.
    pub fn load(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %err, "cannot read session cache");
                }
                return SessionCache::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(cache) => cache,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "discarding corrupt session cache");
                SessionCache::default()
            }
        }
    }

    /// Write the cache with `0600` permissions, creating the directory if
    /// needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        write_private(path, text.as_bytes())
    }

    /// The cached sid for a key, if any.
    pub fn sid(&self, key: &str) -> Option<&str> {
        self.sessions.get(key).map(String::as_str)
    }

    /// Store a sid, replacing any previous one for the same key.
    pub fn set_sid(&mut self, key: impl Into<String>, sid: impl Into<String>) {
        self.sessions.insert(key.into(), sid.into());
    }

    /// Forget a sid (after a logout, or when DSM rejects it).
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.sessions.remove(key)
    }

    /// Number of cached sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// Write a file only the owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()?;

    // `mode` on OpenOptions only applies at creation time; an existing file
    // keeps whatever mode it had, so tighten it explicitly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Send `tracing` output to a file — never to stdout, which the TUI owns.
///
/// The returned [`WorkerGuard`] must be held for the lifetime of the process:
/// dropping it early silently discards everything still buffered in the
/// background writer.
#[must_use = "dropping the WorkerGuard discards buffered log lines"]
pub fn init_logging(log_file: &Path) -> Result<WorkerGuard> {
    if let Some(parent) = log_file.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)?;
    let (writer, guard) = tracing_appender::non_blocking(file);

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|err| Error::config(format!("cannot initialize logging: {err}")))?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An environment built from a literal list of pairs. Nothing here touches
    /// the process environment, so these tests are parallel-safe.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn empty_env() -> impl Fn(&str) -> Option<String> {
        |_: &str| None
    }

    fn cli() -> Cli {
        Cli::default()
    }

    // ---- TOML parsing -----------------------------------------------------

    #[test]
    fn full_config_file_parses() {
        let text = r#"
host         = "nas.local"
port         = 5001
https        = true
insecure     = false
username     = "eduard"
refresh_secs = 3
delete_files = true
"#;
        let (config, unknown) = parse_config(text).expect("valid TOML");
        assert!(unknown.is_empty(), "{unknown:?}");
        assert_eq!(config.host.as_deref(), Some("nas.local"));
        assert_eq!(config.port, Some(5001));
        assert_eq!(config.https, Some(true));
        assert_eq!(config.insecure, Some(false));
        assert_eq!(config.username.as_deref(), Some("eduard"));
        assert_eq!(config.refresh_secs, Some(3));
        assert_eq!(config.delete_files, Some(true));
    }

    #[test]
    fn minimal_config_file_leaves_everything_else_unset() {
        let (config, unknown) = parse_config("host = \"nas\"\n").expect("valid TOML");
        assert!(unknown.is_empty());
        assert_eq!(config.host.as_deref(), Some("nas"));
        assert_eq!(
            config,
            Config {
                host: Some("nas".into()),
                ..Config::default()
            }
        );
    }

    #[test]
    fn empty_config_file_is_an_empty_layer() {
        let (config, unknown) = parse_config("").expect("valid TOML");
        assert!(unknown.is_empty());
        assert_eq!(config, Config::default());
    }

    #[test]
    fn unknown_keys_are_reported_and_ignored_not_rejected() {
        // An older binary must tolerate a config written by a newer one.
        let text = "host = \"nas\"\nfuture_feature = 42\ncolour_scheme = \"dark\"\n";
        let (config, mut unknown) = parse_config(text).expect("unknown keys must not be an error");
        unknown.sort();
        assert_eq!(unknown, vec!["colour_scheme", "future_feature"]);
        assert_eq!(config.host.as_deref(), Some("nas"));
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse_config("host = ").is_err());
        // Right key, wrong type.
        assert!(parse_config("port = \"not a number\"").is_err());
    }

    #[test]
    fn missing_config_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_base(dir.path());
        let config = Config::load(&paths.config_file()).expect("absent file is fine");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn config_file_round_trips_through_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_base(dir.path());
        fs::create_dir_all(paths.config_dir()).expect("mkdir");
        fs::write(
            paths.config_file(),
            "host = \"nas.local\"\nusername = \"eduard\"\nport = 5011\n",
        )
        .expect("write");

        let config = Config::load(&paths.config_file()).expect("valid file");
        assert_eq!(config.host.as_deref(), Some("nas.local"));
        assert_eq!(config.username.as_deref(), Some("eduard"));
        assert_eq!(config.port, Some(5011));
    }

    // ---- environment layer ------------------------------------------------

    #[test]
    fn env_layer_reads_every_supported_variable() {
        let env = env_of(&[
            (env_vars::HOST, "envhost"),
            (env_vars::PORT, "5555"),
            (env_vars::HTTPS, "false"),
            (env_vars::INSECURE, "yes"),
            (env_vars::USERNAME, "envuser"),
            (env_vars::REFRESH_SECS, "9"),
        ]);
        let config = Config::from_env(&env).expect("valid env");
        assert_eq!(config.host.as_deref(), Some("envhost"));
        assert_eq!(config.port, Some(5555));
        assert_eq!(config.https, Some(false));
        assert_eq!(config.insecure, Some(true));
        assert_eq!(config.username.as_deref(), Some("envuser"));
        assert_eq!(config.refresh_secs, Some(9));
    }

    #[test]
    fn empty_env_produces_an_empty_layer() {
        let config = Config::from_env(&empty_env()).expect("empty env");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn blank_env_values_are_treated_as_unset() {
        let env = env_of(&[
            (env_vars::HOST, ""),
            (env_vars::PORT, "  "),
            (env_vars::USERNAME, ""),
        ]);
        let config = Config::from_env(&env).expect("blank values are not errors");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn unparseable_env_value_is_an_error_not_a_silent_default() {
        let env = env_of(&[(env_vars::PORT, "not-a-port")]);
        let err = Config::from_env(&env).expect_err("bad port must fail");
        assert!(err.to_string().contains(env_vars::PORT), "{err}");

        let env = env_of(&[(env_vars::HTTPS, "maybe")]);
        assert!(Config::from_env(&env).is_err());
    }

    #[test]
    fn bool_env_values_accept_common_spellings() {
        for truthy in ["1", "true", "TRUE", "yes", "On"] {
            let env = env_of(&[(env_vars::INSECURE, truthy)]);
            assert_eq!(
                Config::from_env(&env).expect(truthy).insecure,
                Some(true),
                "{truthy}"
            );
        }
        for falsy in ["0", "false", "No", "OFF"] {
            let env = env_of(&[(env_vars::INSECURE, falsy)]);
            assert_eq!(
                Config::from_env(&env).expect(falsy).insecure,
                Some(false),
                "{falsy}"
            );
        }
    }

    // ---- precedence -------------------------------------------------------

    fn file_layer() -> Config {
        Config {
            host: Some("filehost".into()),
            port: Some(5001),
            https: Some(true),
            insecure: Some(false),
            username: Some("fileuser".into()),
            refresh_secs: Some(3),
            delete_files: Some(true),
        }
    }

    #[test]
    fn cli_beats_env_beats_file() {
        let env = Config::from_env(&env_of(&[
            (env_vars::HOST, "envhost"),
            (env_vars::PORT, "5002"),
            (env_vars::USERNAME, "envuser"),
            (env_vars::REFRESH_SECS, "5"),
        ]))
        .expect("valid env");

        let cli = Cli {
            host: Some("clihost".into()),
            port: Some(5003),
            ..cli()
        };

        let resolved = merge(file_layer(), env, &cli).expect("host and user present");
        assert_eq!(resolved.host, "clihost", "CLI beats env");
        assert_eq!(resolved.port, 5003, "CLI beats env");
        assert_eq!(resolved.username, "envuser", "env beats file");
        assert_eq!(resolved.refresh_secs, 5, "env beats file");
    }

    #[test]
    fn env_beats_file_when_the_cli_is_silent() {
        let env = Config::from_env(&env_of(&[
            (env_vars::HOST, "envhost"),
            (env_vars::HTTPS, "false"),
            (env_vars::INSECURE, "true"),
        ]))
        .expect("valid env");
        let resolved = merge(file_layer(), env, &cli()).expect("valid");
        assert_eq!(resolved.host, "envhost");
        assert!(!resolved.https);
        assert!(resolved.insecure);
        assert_eq!(resolved.username, "fileuser");
    }

    #[test]
    fn file_beats_defaults() {
        let file = Config {
            port: Some(9999),
            https: Some(false),
            insecure: Some(true),
            refresh_secs: Some(30),
            delete_files: Some(false),
            ..file_layer()
        };
        let resolved = merge(file, Config::default(), &cli()).expect("valid");
        assert_eq!(resolved.port, 9999);
        assert!(!resolved.https);
        assert!(resolved.insecure);
        assert_eq!(resolved.refresh_secs, 30);
        assert!(!resolved.delete_files);
    }

    #[test]
    fn defaults_apply_when_nothing_else_says_otherwise() {
        let cli = Cli {
            host: Some("nas".into()),
            username: Some("me".into()),
            ..cli()
        };
        let resolved = merge(Config::default(), Config::default(), &cli).expect("valid");
        assert_eq!(resolved.port, DEFAULT_HTTPS_PORT);
        assert_eq!(resolved.https, DEFAULT_HTTPS);
        assert_eq!(resolved.insecure, DEFAULT_INSECURE);
        assert_eq!(resolved.refresh_secs, DEFAULT_REFRESH_SECS);
        assert_eq!(resolved.delete_files, DEFAULT_DELETE_FILES);
        assert!(!resolved.dry_run);
        assert!(!resolved.logout);
    }

    #[test]
    fn default_port_follows_the_scheme() {
        let cli = Cli {
            host: Some("nas".into()),
            username: Some("me".into()),
            ..cli()
        };
        let env = Config::from_env(&env_of(&[(env_vars::HTTPS, "false")])).expect("valid env");
        let resolved = merge(Config::default(), env, &cli).expect("valid");
        assert!(!resolved.https);
        assert_eq!(resolved.port, DEFAULT_HTTP_PORT);
    }

    #[test]
    fn boolean_flags_are_one_way_switches() {
        // `--insecure` turns it on...
        let flags = Cli {
            insecure: true,
            no_delete_files: true,
            dry_run: true,
            ..cli()
        };
        let resolved = merge(file_layer(), Config::default(), &flags).expect("valid");
        assert!(resolved.insecure);
        assert!(!resolved.delete_files);
        assert!(resolved.dry_run);

        // ...and its absence does not turn a config-file `true` back off.
        let file = Config {
            insecure: Some(true),
            ..file_layer()
        };
        let resolved = merge(file, Config::default(), &cli()).expect("valid");
        assert!(resolved.insecure);
        assert!(resolved.delete_files);
    }

    // ---- validation -------------------------------------------------------

    #[test]
    fn missing_host_is_an_actionable_error() {
        let file = Config {
            username: Some("me".into()),
            ..Config::default()
        };
        let err = merge(file, Config::default(), &cli()).expect_err("host is required");
        let msg = err.to_string();
        assert!(msg.contains("--host"), "{msg}");
        assert!(msg.contains(env_vars::HOST), "{msg}");
    }

    #[test]
    fn missing_username_is_an_actionable_error() {
        let file = Config {
            host: Some("nas".into()),
            ..Config::default()
        };
        let err = merge(file, Config::default(), &cli()).expect_err("username is required");
        let msg = err.to_string();
        assert!(msg.contains("--user"), "{msg}");
        assert!(msg.contains(env_vars::USERNAME), "{msg}");
    }

    #[test]
    fn whitespace_only_values_do_not_satisfy_validation() {
        let cli = Cli {
            host: Some("   ".into()),
            username: Some("me".into()),
            ..cli()
        };
        assert!(merge(Config::default(), Config::default(), &cli).is_err());
    }

    #[test]
    fn host_and_username_are_trimmed() {
        let cli = Cli {
            host: Some("  nas.local ".into()),
            username: Some(" eduard\t".into()),
            ..cli()
        };
        let resolved = merge(Config::default(), Config::default(), &cli).expect("valid");
        assert_eq!(resolved.host, "nas.local");
        assert_eq!(resolved.username, "eduard");
    }

    #[test]
    fn zero_refresh_interval_is_rejected() {
        let cli = Cli {
            host: Some("nas".into()),
            username: Some("me".into()),
            refresh_secs: Some(0),
            ..cli()
        };
        let err = merge(Config::default(), Config::default(), &cli).expect_err("0 is invalid");
        assert!(err.to_string().contains("refresh_secs"), "{err}");
    }

    // ---- the first-run template -------------------------------------------

    /// Uncomment the example assignments in a template, leaving the prose
    /// comments alone: `# host = "nas.local"` becomes `host = "nas.local"`.
    fn uncomment(template: &str) -> String {
        template
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest) if rest.split_once(" = ").is_some() => rest,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_template_parses_as_written_and_changes_nothing() {
        // Every key is commented out, so a freshly written template is a valid
        // config file that supplies no values at all — it must not send the
        // next run off to an example host.
        let (config, unknown) = parse_config(CONFIG_TEMPLATE).expect("the template must parse");
        assert!(unknown.is_empty(), "{unknown:?}");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn the_template_documents_every_key_and_round_trips_uncommented() {
        for key in KNOWN_KEYS {
            assert!(
                CONFIG_TEMPLATE.contains(&format!("# {key} = ")),
                "the template does not document {key}"
            );
        }

        // Uncommented, the examples must still be a config this binary
        // understands — a typo in a key name would otherwise ship silently and
        // be "ignored unknown key" the moment a user followed the template.
        let (config, unknown) = parse_config(&uncomment(CONFIG_TEMPLATE)).expect("valid TOML");
        assert!(unknown.is_empty(), "{unknown:?}");
        assert_eq!(config.host.as_deref(), Some("nas.local"));
        assert_eq!(config.username.as_deref(), Some("admin"));
        assert_eq!(config.port, Some(DEFAULT_HTTPS_PORT));
        assert_eq!(config.https, Some(DEFAULT_HTTPS));
        assert_eq!(config.insecure, Some(DEFAULT_INSECURE));
        assert_eq!(config.refresh_secs, Some(DEFAULT_REFRESH_SECS));
        assert_eq!(config.delete_files, Some(DEFAULT_DELETE_FILES));
    }

    #[test]
    fn writing_the_template_creates_the_config_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_base(dir.path());
        let path = paths.config_file();
        assert!(!paths.config_dir().exists());

        assert!(write_config_template(&path).expect("write"));
        assert_eq!(fs::read_to_string(&path).expect("read"), CONFIG_TEMPLATE);
        // ...and what was written loads as an empty layer, not an error.
        assert_eq!(Config::load(&path).expect("load"), Config::default());
    }

    #[test]
    fn writing_the_template_never_clobbers_an_existing_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Paths::with_base(dir.path()).config_file();
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "port = 5011\n").expect("write");

        assert!(
            !write_config_template(&path).expect("no error"),
            "an existing file must be reported as not written"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), "port = 5011\n");
    }

    // ---- missing required values ------------------------------------------

    #[test]
    fn nothing_is_missing_once_the_layers_supply_host_and_username() {
        let both_on_the_cli = Cli {
            host: Some("nas".into()),
            username: Some("me".into()),
            ..cli()
        };
        assert!(!missing_required(&Config::default(), &Config::default(), &both_on_the_cli).any());

        // ...from any mix of layers.
        let env = Config::from_env(&env_of(&[(env_vars::USERNAME, "envuser")])).expect("valid");
        let file = Config {
            host: Some("filehost".into()),
            ..Config::default()
        };
        assert!(!missing_required(&file, &env, &cli()).any());
    }

    #[test]
    fn a_missing_config_file_alone_is_not_a_first_run() {
        // The whole point: `syno-clean --host nas --user me` on a clean machine
        // must run, with no config file anywhere.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_base(dir.path());
        let file = Config::load(&paths.config_file()).expect("absent file is fine");
        let cli = Cli {
            host: Some("nas.local".into()),
            username: Some("eduard".into()),
            ..cli()
        };

        assert!(!missing_required(&file, &Config::default(), &cli).any());
        assert!(merge(file, Config::default(), &cli).is_ok());
        assert!(!paths.config_file().exists(), "nothing was written");
    }

    #[test]
    fn missing_required_agrees_with_what_merge_refuses() {
        let cases = [
            (None, None, (true, true)),
            (Some("nas"), None, (false, true)),
            (None, Some("me"), (true, false)),
            (Some("  "), Some("me"), (true, false)),
        ];
        for (host, username, (host_missing, user_missing)) in cases {
            let cli = Cli {
                host: host.map(str::to_string),
                username: username.map(str::to_string),
                ..cli()
            };
            let missing = missing_required(&Config::default(), &Config::default(), &cli);
            assert_eq!(missing.host, host_missing, "{host:?}/{username:?}");
            assert_eq!(missing.username, user_missing, "{host:?}/{username:?}");
            assert_eq!(
                missing.any(),
                merge(Config::default(), Config::default(), &cli).is_err(),
                "{host:?}/{username:?}: the two must never disagree"
            );
        }
    }

    #[test]
    fn the_first_run_message_names_the_values_that_are_missing() {
        let both = MissingRequired {
            host: true,
            username: true,
        };
        assert_eq!(both.help_lines().len(), 2);
        assert!(both.help_lines()[0].contains("--host"));
        assert!(both.help_lines()[1].contains("--user"));

        let host_only = MissingRequired {
            host: true,
            username: false,
        };
        assert_eq!(host_only.help_lines(), vec![MISSING_HOST_HELP]);
        assert!(MissingRequired::default().help_lines().is_empty());
        assert!(!MissingRequired::default().any());
    }

    // ---- resolved helpers -------------------------------------------------

    #[test]
    fn base_url_and_session_key_are_built_from_the_resolved_values() {
        let cli = Cli {
            host: Some("nas.local".into()),
            username: Some("eduard".into()),
            ..cli()
        };
        let resolved = merge(Config::default(), Config::default(), &cli).expect("valid");
        assert_eq!(resolved.base_url(), "https://nas.local:5001");
        assert_eq!(resolved.session_key(), "nas.local:5001/eduard");

        let cli = Cli {
            port: Some(5000),
            ..cli
        };
        let env = Config::from_env(&env_of(&[(env_vars::HTTPS, "off")])).expect("valid env");
        let resolved = merge(Config::default(), env, &cli).expect("valid");
        assert_eq!(resolved.base_url(), "http://nas.local:5000");
    }

    #[test]
    fn password_comes_from_the_environment_when_set() {
        let env = env_of(&[(env_vars::PASSWORD, "hunter2")]);
        assert_eq!(
            resolve_password(&env, "eduard", "nas.local").expect("env password"),
            "hunter2"
        );
    }

    #[test]
    fn otp_is_optional() {
        assert_eq!(otp_from_env(&empty_env()), None);
        assert_eq!(otp_from_env(&env_of(&[(env_vars::OTP, "")])), None);
        assert_eq!(
            otp_from_env(&env_of(&[(env_vars::OTP, "123456")])),
            Some("123456".to_string())
        );
    }

    // ---- paths ------------------------------------------------------------

    #[test]
    fn with_base_keeps_everything_under_the_given_root() {
        let paths = Paths::with_base("/tmp/xyz");
        assert!(paths.config_file().starts_with("/tmp/xyz"));
        assert!(paths.session_file().starts_with("/tmp/xyz"));
        assert!(paths.log_file().starts_with("/tmp/xyz"));
        assert!(paths.config_file().ends_with("syno-clean/config.toml"));
        assert!(paths.session_file().ends_with("syno-clean/session.json"));
    }

    // ---- session cache ----------------------------------------------------

    #[test]
    fn absent_session_cache_loads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_base(dir.path());
        assert!(SessionCache::load(&paths.session_file()).is_empty());
    }

    #[test]
    fn session_cache_round_trips_and_keys_do_not_evict_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_base(dir.path());
        let path = paths.session_file();

        let mut cache = SessionCache::default();
        cache.set_sid(session_key("nas-a", 5001, "eduard"), "sid-a");
        cache.set_sid(session_key("nas-b", 5001, "eduard"), "sid-b");
        cache.set_sid(session_key("nas-a", 5001, "admin"), "sid-c");
        cache.save(&path).expect("save");

        let loaded = SessionCache::load(&path);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.sid("nas-a:5001/eduard"), Some("sid-a"));
        assert_eq!(loaded.sid("nas-b:5001/eduard"), Some("sid-b"));
        assert_eq!(loaded.sid("nas-a:5001/admin"), Some("sid-c"));
        assert_eq!(loaded.sid("nas-c:5001/eduard"), None);
    }

    #[test]
    fn storing_a_sid_replaces_only_its_own_key() {
        let mut cache = SessionCache::default();
        cache.set_sid("a", "1");
        cache.set_sid("b", "2");
        cache.set_sid("a", "3");
        assert_eq!(cache.sid("a"), Some("3"));
        assert_eq!(cache.sid("b"), Some("2"));
        assert_eq!(cache.remove("a").as_deref(), Some("3"));
        assert_eq!(cache.sid("a"), None);
        assert_eq!(cache.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn session_cache_is_written_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = Paths::with_base(dir.path()).session_file();

        let mut cache = SessionCache::default();
        cache.set_sid("nas:5001/me", "sid");
        cache.save(&path).expect("save");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");

        // A rewrite over a world-readable file must tighten it back down.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        cache.save(&path).expect("save again");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    #[test]
    fn corrupt_session_cache_is_discarded_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Paths::with_base(dir.path()).session_file();
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "{ this is not json").expect("write");
        assert!(SessionCache::load(&path).is_empty());
    }

    #[test]
    fn saving_creates_the_cache_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Paths::with_base(dir.path()).session_file();
        assert!(!path.parent().expect("parent").exists());
        SessionCache::default().save(&path).expect("save");
        assert!(path.exists());
    }
}
