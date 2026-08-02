//! Command-line interface definitions.
//!
//! The CLI is the *highest-precedence* configuration layer: every flag here
//! overrides the matching `SYNO_CLEAN_*` environment variable, which in turn
//! overrides the config file. See [`crate::config::merge`].
//!
//! Boolean flags are one-way switches — they can turn a setting on
//! (`--insecure`, `--dry-run`) or off (`--no-delete-files`), but their absence
//! means "not specified", never "false". That keeps `insecure = true` in the
//! config file working without a matching `--no-insecure` flag.

use std::path::PathBuf;

use clap::Parser;

/// A terminal UI for reviewing and cleaning up Synology Download Station
/// tasks — removing both the DSM task and the files it left on the volume.
#[derive(Debug, Clone, Default, Parser)]
#[command(name = "syno-clean", version, about, long_about = None)]
pub struct Cli {
    /// Path to the config file (default: `~/.config/syno-clean/config.toml`).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// DSM hostname or IP address.
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// DSM account name.
    #[arg(long = "user", value_name = "NAME")]
    pub username: Option<String>,

    /// DSM port (default: 5001 for HTTPS, 5000 for HTTP).
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Accept a self-signed or otherwise invalid TLS certificate.
    #[arg(long)]
    pub insecure: bool,

    /// Seconds between automatic task-list refreshes.
    #[arg(long = "refresh-secs", value_name = "SECS")]
    pub refresh_secs: Option<u64>,

    /// Remove the DSM task only, leaving the downloaded files in place.
    #[arg(long = "no-delete-files")]
    pub no_delete_files: bool,

    /// Write logs here instead of `~/.cache/syno-clean/syno-clean.log`.
    #[arg(long = "log-file", value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Report what would be deleted without issuing any destructive call.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Invalidate the cached session and exit. Normal quit never logs out.
    #[arg(long)]
    pub logout: bool,

    /// Print the raw `SYNO.API.Info` discovery response and exit.
    ///
    /// Hidden: a debugging aid, not part of the advertised interface. It needs
    /// no session, so it works before any credentials exist.
    #[arg(long = "dump-api-info", hide = true)]
    pub dump_api_info: bool,

    /// Print the raw `SYNO.DownloadStation.Task` list response and exit.
    ///
    /// Hidden. This is how `tests/fixtures/task_list.json` is captured from a
    /// real NAS:
    /// `syno-clean --dump-tasks-json > tests/fixtures/task_list.json`.
    #[arg(long = "dump-tasks-json", hide = true)]
    pub dump_tasks_json: bool,

    /// Run the TUI against a captured `list` response instead of a NAS.
    ///
    /// Hidden. The file is a full DSM response envelope — exactly what
    /// `--dump-tasks-json` prints and what `tests/fixtures/task_list.json`
    /// holds. No network call is made and **no configuration is required**:
    /// the point is to exercise the table, selection and the sort/filter keys
    /// with no NAS in reach, so demanding a host and a password would defeat
    /// it.
    #[arg(long = "fixture", value_name = "PATH", hide = true)]
    pub fixture: Option<PathBuf>,
}

impl Cli {
    /// True when the invocation is one of the hidden dump modes, which print
    /// raw JSON and exit instead of entering the TUI.
    pub fn is_dump(&self) -> bool {
        self.dump_api_info || self.dump_tasks_json
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches duplicate flags, bad arg specs and the like at test time
        // rather than on first run.
        Cli::command().debug_assert();
    }

    #[test]
    fn flags_parse_into_the_expected_fields() {
        let cli = Cli::try_parse_from([
            "syno-clean",
            "--config",
            "/tmp/c.toml",
            "--host",
            "nas.local",
            "--user",
            "eduard",
            "--port",
            "5011",
            "--insecure",
            "--refresh-secs",
            "7",
            "--no-delete-files",
            "--log-file",
            "/tmp/s.log",
            "--dry-run",
            "--logout",
            "--dump-api-info",
            "--dump-tasks-json",
            "--fixture",
            "/tmp/tasks.json",
        ])
        .expect("valid arguments");

        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/c.toml"))
        );
        assert_eq!(cli.host.as_deref(), Some("nas.local"));
        assert_eq!(cli.username.as_deref(), Some("eduard"));
        assert_eq!(cli.port, Some(5011));
        assert!(cli.insecure);
        assert_eq!(cli.refresh_secs, Some(7));
        assert!(cli.no_delete_files);
        assert_eq!(
            cli.log_file.as_deref(),
            Some(std::path::Path::new("/tmp/s.log"))
        );
        assert!(cli.dry_run);
        assert!(cli.logout);
        assert!(cli.dump_api_info);
        assert!(cli.dump_tasks_json);
        assert!(cli.is_dump());
        assert_eq!(
            cli.fixture.as_deref(),
            Some(std::path::Path::new("/tmp/tasks.json"))
        );
    }

    #[test]
    fn the_fixture_flag_is_hidden_and_takes_a_path() {
        let cli = Cli::try_parse_from(["syno-clean", "--fixture", "tests/fixtures/task_list.json"])
            .expect("valid");
        assert_eq!(
            cli.fixture.as_deref(),
            Some(std::path::Path::new("tests/fixtures/task_list.json"))
        );
        // Offline mode is not one of the dump modes: it enters the TUI.
        assert!(!cli.is_dump());
        assert!(
            !Cli::command()
                .render_long_help()
                .to_string()
                .contains("--fixture")
        );
        // It requires a value rather than defaulting to some path.
        assert!(Cli::try_parse_from(["syno-clean", "--fixture"]).is_err());
    }

    #[test]
    fn the_dump_flags_are_hidden_from_help() {
        // They are debugging aids for capturing real responses, not part of
        // the advertised interface.
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("--dump-api-info"), "{help}");
        assert!(!help.contains("--dump-tasks-json"), "{help}");
        // ...but the documented flags are all there.
        assert!(help.contains("--dry-run"), "{help}");
    }

    #[test]
    fn each_dump_flag_works_on_its_own() {
        let cli = Cli::try_parse_from(["syno-clean", "--dump-api-info"]).expect("valid");
        assert!(cli.dump_api_info);
        assert!(!cli.dump_tasks_json);
        assert!(cli.is_dump());

        let cli = Cli::try_parse_from(["syno-clean", "--dump-tasks-json"]).expect("valid");
        assert!(!cli.dump_api_info);
        assert!(cli.dump_tasks_json);
        assert!(cli.is_dump());
    }

    #[test]
    fn bare_invocation_specifies_nothing() {
        let cli = Cli::try_parse_from(["syno-clean"]).expect("no arguments is valid");
        assert!(cli.host.is_none());
        assert!(cli.username.is_none());
        assert!(cli.port.is_none());
        assert!(!cli.insecure);
        assert!(!cli.no_delete_files);
        assert!(!cli.dry_run);
        assert!(!cli.logout);
        assert!(!cli.is_dump());
        assert!(cli.fixture.is_none());
    }
}
