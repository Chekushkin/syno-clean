<!-- claudish-to-english:rewritten -->

# syno-clean

A terminal tool to view and clean up **Synology Download Station** tasks. It deletes the downloaded **files** and the DSM task at the same time.

In DSM's web UI, deleting a task leaves the files on your drive. You have to go to File Station to delete them manually. `syno-clean` shows all tasks in a table. You can sort, filter, and select many tasks at once. It removes both the files and the task in one step. It does not install anything on your NAS. It just talks to the DSM API using HTTP.

```text
 syno-clean 0.1.0 · demo@nas.local:5001                                                                 14 / 14 tasks
  Name▲        Status            Size  Progress      ↓ Speed      ↑ Speed  Ratio Seeds/Peers      ETA Destination
✓ Absolute.De… finished     256.0 MiB    100.0%            —            —   1.00           —        ∞ /volume1/downlo…
✓ archlinux-2… finished       1.1 GiB    100.0%            —            —   0.50           —        ∞ downloads
  Big.Buck.Bu… seeding        1.8 GiB    100.0%            —    1.2 MiB/s   2.14         5/9        ∞ video/movies
  Broken.Rele… error        700.0 MiB      0.0%            —            —   0.00           —        ∞ downloads
  empty-place… waiting            0 B      0.0%            —            —   0.00           —        ∞ downloads
  Hosted.Arch… hosting      500.0 MiB      0.0%            —            —   0.00           —        ∞ —
  LibreOffice… paused         3.0 GiB     30.0%            —            —   0.00           —        ∞ software
  Mixed.Root.… seeding        3.0 MiB    100.0%            —     64 KiB/s   0.33         2/1        ∞ video/tv
  Mystery.Tas… captcha_ne…    1.0 GiB      0.0%            —            —   0.00           —        ∞ —
  Sintel.2010… finishing     12.0 GiB    100.0%    1.0 MiB/s    256 KiB/s   0.17         6/2       1s downloads/incom…
  Some.Show.S… checking      20.0 GiB     50.0%            —            —   0.00           —        ∞ video/tv
  syno-clean-… waiting        4.0 MiB      0.0%            —            —   0.00           —        ∞ downloads
  Ubuntu.24.0… downloading    5.8 GiB     39.0%    8.5 MiB/s    512 KiB/s   0.05        12/4    7m 7s downloads
  千と千尋の…  extracting     8.0 GiB    100.0%            —            —   0.05         3/0        ∞ video/movies
 2 selected · 1.3 GiB · sort Name▲ · d delete · p/u pause/resume · r refresh · q quit · ? help
```

> This is not a screenshot. The program draws this frame itself using a test file (`tests/fixtures/task_list.json`). Colors and the cursor highlight are not shown in plain text, but everything else is exactly what the terminal sees.

## Features

- Shows every task in one table: name, status, size, progress, speeds, ratio, seeds/peers, ETA, and destination. It aligns columns correctly for CJK characters and emojis.
- Use arrow keys or Vim keys to move. Use `Space` to select many tasks. Use `a` to select all visible tasks. There are eight sort keys, status filters, and a search tool.
- Press `d` to see a list of tasks and file paths that will be deleted. It shows how much space you will save. Then it deletes the files and the task.
- Pause (`p`) and resume (`u`) tasks. The UI updates in the background. It shows a storage bar for each volume so you can see saved space.

## ⚠️ `d` deletes files, and you cannot undo it

When you confirm a delete, the program removes the task's folder and everything inside it via File Station. Then it removes the Download Station task. You cannot undo this.

- **Try `--dry-run` first.** It shows the same dialog (labeled `DRY RUN`) but does not actually delete anything.
- If your DSM share has the **DSM Recycle Bin** turned on, deleted files might go to the `#recycle` folder. This means they won't free up space. This is a DSM setting that this program cannot change.

## Requirements

**DSM 7** is required (DSM 6 does not work). You must have **Download Station** and **File Station** installed. Your DSM account must be allowed to use both. If you don't have File Station, you can see the tasks, but deleting will fail. The program will leave the task alone instead of leaving the files behind. It checks the API version when it starts.

## Install

You can find prebuilt files for macOS and Linux (x86_64 and aarch64) in the [releases](https://github.com/Chekushkin/syno-clean/releases). Use the `SHA256SUMS` file to check them. Unpack the file and put `syno-clean` in your `PATH`. Or build it yourself:

```sh
git clone https://github.com/Chekushkin/syno-clean
cd syno-clean
cargo build --release
install -m 0755 target/release/syno-clean ~/.local/bin/
```

## Quick start

```sh
syno-clean --host nas.local --user demo            # no config file needed
syno-clean --host nas.local --user demo --dry-run  # check before you delete
```

The password is **never** saved in the config file. It comes from `SYNO_CLEAN_PASSWORD` or a prompt in the terminal. If you use 2FA, use `SYNO_CLEAN_OTP` or a prompt. On the first run, it creates a template at `~/.config/syno-clean/config.toml` and tells you what is missing.

## Configuration

The order of priority is: **CLI flags > `SYNO_CLEAN_*` env vars > config file > defaults**. You only need `host` and `username`. Paths follow XDG rules on all platforms, including macOS. Config is at `~/.config/syno-clean/config.toml`. Logs and cache are at `~/.cache/syno-clean/`.

```toml
host         = "nas.local"
port         = 5001      # default: 5001 with https, 5000 without
https        = true      # default: true
insecure     = false     # accept a self-signed / invalid TLS certificate
username     = "demo"
refresh_secs = 3         # must be > 0
delete_files = true      # false = remove the DSM task only, leave the files
```

Each setting has a matching `SYNO_CLEAN_*` env var and a CLI flag (see `--help`). The `delete_files` setting has **no** env var to prevent accidental mistakes. Use `--no-delete-files` or the config file instead. Quitting normally does not log you out. Use `--logout` to clear the session.

## Keys

Press `?` to see the full list of keys. Short version: `↑`/`↓` or `j`/`k` to move, `Space` to select, `a` to select all visible rows, `d` to delete, `p`/`u` to pause/resume, `s`/`S` to sort, `f` to filter by status, `/` to search, `r` to refresh, `v` to see the last results (and why some failed), `q` to quit. In the delete window, **Cancel has the focus**. Press `Enter` to cancel or `y` to confirm.

## Delete safety model

The tool must find the right folder to delete. To avoid deleting the wrong data, it follows these rules:

- It finds the path by looking at the destination and the task's file list.
- It refuses the path if it looks unsafe (like `..` or control characters) or if the task has no destination or file list.
- Before deleting, it checks if the path exists on the volume. If the path is the wrong type, has a permission error, or is missing, the program skips it and keeps the task.
- It pauses active tasks first and waits for the NAS to confirm the pause.
- **If any step fails, the rest of the steps are skipped.** If files cannot be deleted, the task is not deleted either.
- The confirmation window shows exactly what will happen. It shows a snapshot of all paths. If a row is skipped, it shows `SKIPPED` and the reason. These skipped rows are not included in the total space saved. Use `--no-delete-files` to remove only the tasks.

## Troubleshooting

If a connection or login fails, the program stops and shows an error message. It will not open the UI. If a failure happens while using the program, a red banner appears at the bottom. It will go away on the next refresh. You can find more details in the log file (`~/.cache/syno-clean/syno-clean.log`). Please include this log if you report a bug.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). You must run `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` successfully. You can test the UI offline using `--fixture`.

## License

MIT — see [LICENSE](LICENSE).
