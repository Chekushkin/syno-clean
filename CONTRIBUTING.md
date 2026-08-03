# Contributing to syno-clean

Thanks for taking a look. This is a small, deliberately conservative program: it
deletes people's data, so changes are judged first on whether they can be reasoned
about, and only then on what they add.

`CLAUDE.md` in the repository root is the working notes — module layout, configuration
precedence, the delete ordering and the path-safety invariants. Read it before changing
anything under `src/delete.rs` or `src/api/`.

## Toolchain

The toolchain is pinned in `rust-toolchain.toml` to an **explicit version** (currently
`1.97.1`, edition 2024), not `stable`, so builds are reproducible. With
[rustup](https://rustup.rs) installed, the right toolchain and the `rustfmt` and
`clippy` components are fetched automatically on the first cargo command:

```sh
git clone https://github.com/emacarov/syno-clean
cd syno-clean
cargo build
```

## The validation gate

Every change must leave the repository in this state, and CI runs exactly these three
commands on Linux and macOS:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

Note the `-- --check`: CI *verifies* formatting rather than applying it. Run `cargo fmt
--all` locally to fix what the check reports.

**Warnings are errors.** Do not silence a lint with `#[allow]` without saying why in a
comment right above it.

## Dependency rules

- **Never add `crossterm` as a direct dependency.** It is consumed through
  `ratatui::crossterm`, so there is exactly one crossterm in the tree and no
  version-skew type errors.
- `reqwest` is `default-features = false` with `rustls`. No OpenSSL, no system TLS.
- `tracing` writes to a **file**, never stdout — the TUI owns the terminal.

## Testing philosophy

Coverage here is **intentionally narrow, and that is a decision rather than an
oversight.** Pure logic where a bug is silent and expensive is tested thoroughly:

- `delete.rs` — path resolution, the safety guards and the operation ordering. This is
  the highest-value test in the project; new behaviour here needs new tests, including
  the refusal cases.
- `format.rs` — sizes, speeds, ETAs and display-width truncation.
- `model.rs` — DSM JSON → `Task`, driven by the checked-in fixture.
- `view.rs` — sort comparators, status filters, search.
- `error.rs` — DSM numeric code → message.
- `api::client` — response envelope deserialization and parameter construction.
- `app.rs` — the key state machine, selection, and cursor/selection stability across a
  refresh.

Not tested, and verified by running the binary instead: the terminal lifecycle (raw
mode, alternate screen, the panic hook) and live HTTP against a real DSM.

Two rules that keep the suite honest:

- **No test may touch the network, a real timer, or a TTY.** Anything long-running is
  split into a pure function plus a thin I/O wrapper — `build_*_params`,
  `plan_delete_ops`, `classify_*` — and the pure half is what gets tested. There is no
  mocking framework and no trait abstraction over the HTTP client; one implementation
  does not warrant one.
- **No test may read or write process-global state.** Environment reads go through an
  injected `EnvLookup` closure and filesystem paths through `config::Paths`
  (`Paths::with_base(tempdir)` in tests), so the suite stays parallel-safe and never
  goes near the real `~/.config/syno-clean` or `~/.cache/syno-clean`.

Frame rendering *can* be asserted without a terminal: `ratatui::backend::TestBackend`
draws into an in-memory buffer, and layout regressions (column shearing on CJK titles,
rows overflowing the width) are caught that way.

## Running it without a NAS

The hidden `--fixture <path>` flag runs the whole TUI over a captured `list` response
with no network call and **no configuration at all** — no config file, no host, no
password:

```sh
cargo run -- --fixture tests/fixtures/task_list.json
```

The file is a full DSM response envelope, read through the same parser the live path
uses. `tests/fixtures/task_list.json` covers every task status, missing and partial
`additional` blocks, a zero-size task, CJK and emoji titles, and a file list with no
common root (the refusal case). Extend it rather than writing a second, laxer fixture.

Offline mode forces `--dry-run` semantics: there is no client, so the confirmation
dialog never promises a delete that could not happen.

Two more hidden flags exist for capturing real responses:

```sh
syno-clean --dump-api-info                                  # raw SYNO.API.Info discovery
syno-clean --dump-tasks-json > tests/fixtures/task_list.json  # raw list response
```

`--dump-api-info` deliberately does not log in — discovery needs no session, which is
exactly the case where the login is what is broken.

## Testing against a real NAS

Use a DSM 7 machine with Download Station and File Station installed, and **test
deletes on something disposable first**. `--dry-run` runs the full flow and issues no
destructive call, so start there. Confirm afterwards in File Station that both the task
and the directory are actually gone — and check whether the share's Recycle Bin is
enabled, since that changes whether space is really reclaimed.

## Pull requests

- One logical change per PR, with a clear description of what and why.
- Update `CHANGELOG.md` under `Unreleased`.
- Update `README.md` when a flag, config key, environment variable or keybinding
  changes. The keybinding overlay (`dialog::HELP_SECTIONS`) is data, and a test asserts
  that every key in a **hand-maintained list** appears in it. That list is not derived
  from `App`'s match arms — it mirrors `handle_normal_key`, `handle_search_key` and
  `handle_confirm_key` by hand, so adding a binding means adding it in three places: the
  handler, `HELP_SECTIONS`, and the list in that test. Miss the overlay and the test
  names the key; miss the test's list and nothing catches you.
- Update `CLAUDE.md` when a convention or an invariant changes.

## Reporting bugs

Open an issue with the DSM version, the `syno-clean --version`, what you did, what
happened, and the relevant lines from `~/.cache/syno-clean/syno-clean.log`. The log is
written at `INFO` and above and that level is fixed — there is no `--verbose` flag and
`RUST_LOG` is ignored — so `tracing::debug!` lines never reach it.

Redact the host name if you would rather not share it. The **password** cannot reach the
log: `Credentials` has a hand-written redacting `Debug`. Neither can the session `sid`:
it travels in the request query string, and `error::Error` strips the query out of every
transport error before it is rendered. Neither guarantee survives a `{:?}` of a
`SynoClient`, which derives `Debug` and holds both — so scan for a `_sid=` or a `sid:`
before pasting anyway, and treat one as a live credential if you find it.
