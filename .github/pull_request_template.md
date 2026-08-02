## What and why

<!-- What this changes, and the problem it solves. Link the issue if there is one. -->

## How it was verified

<!-- Which of these applies. Be specific: "ran against a real NAS with three seeding
     torrents and deleted one" is worth more than "tested". -->

- [ ] `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
      are all clean
- [ ] Exercised offline with `cargo run -- --fixture tests/fixtures/task_list.json`
- [ ] Exercised against a real DSM 7 NAS
- [ ] Not applicable (documentation or CI only)

## Does this touch the delete path?

<!-- src/delete.rs, src/api/file_station.rs, the confirmation dialog, or the op
     ordering in src/event.rs. If yes, this section is the point of the PR. -->

- [ ] No
- [ ] Yes — and new tests cover the resolution, the guards or the ordering it changes,
      including the cases where the answer is to *refuse*

## Checklist

- [ ] `CHANGELOG.md` updated under `Unreleased`
- [ ] `README.md` updated if a flag, config key, environment variable or keybinding
      changed
- [ ] `CLAUDE.md` updated if a convention or an invariant changed
- [ ] No test added that touches the network, a real timer, a TTY, or process-global
      state (environment variables, the real config or cache directories)
