---
name: Bug report
about: Something behaved differently from how it is documented
title: ''
labels: bug
assignees: ''
---

## What happened

<!-- What you did, what you expected, what happened instead. -->

## Steps to reproduce

1.
2.
3.

## Was data affected?

<!-- If a delete removed the wrong thing, left files behind, or skipped a task you
     expected it to remove, say so here — that is the highest-priority kind of bug in
     this project. Include the task's destination and, if you can, whether its file
     list had a single common root. -->

- [ ] This involves a delete that did not do what the confirmation dialog said

## Environment

- `syno-clean --version`:
- Installed from: <!-- release binary / built from source at commit ... -->
- OS and terminal: <!-- e.g. macOS 15.3, Ghostty 1.1 -->
- DSM version:
- Download Station version:
- Reproducible with `--fixture tests/fixtures/task_list.json`? <!-- yes / no / n/a -->

## Log output

<!-- The relevant lines from ~/.cache/syno-clean/syno-clean.log (or --log-file).
     Your DSM password is never written to the log. The session id (sid) can be —
     it is a bearer credential, so search the excerpt for `_sid=` and replace any
     match before pasting. Redact the host name too if you would rather not share
     it. Note that only INFO and above is recorded; there is no verbose flag. -->

```
paste here
```

## Anything else

<!-- Screenshots of a mangled layout are useful; so is the raw output of
     `syno-clean --dump-tasks-json` (redacted) when a task parses wrongly. -->
