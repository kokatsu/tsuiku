# tsuiku

A TUI diff viewer that overlays difftastic's structural diffs on line diffs.

Line-based diffing decides the layout and line backgrounds; difftastic's
structural diff is composed on top as in-line highlights. This lets you
visually distinguish formatting-only changes from essential ones.

## Requirements

- macOS or Linux (including WSL)
- [difftastic](https://github.com/Wilfred/difftastic) (the `difft` command;
  without it, tsuiku still works as a line diff viewer without structural
  highlights)

## Usage

```
tsuiku                      # compare HEAD against the final worktree state
tsuiku show <rev>           # compare a commit against its first parent
tsuiku pr [<selector>]      # view a GitHub pull request (needs the gh CLI)
tsuiku diff <rev1> <rev2>   # compare two revisions directly
tsuiku diff <rev1>..<rev2>  # the same, as one range argument
tsuiku diff <rev1>...<rev2> # merge base of the two against rev2
```

Main keys: `j`/`k` to move by line, `]`/`[` to jump between hunks, `n`/`p` to
switch files, `s` to toggle the side-by-side split view, `q` to quit.

The worktree view refreshes automatically: edits, staging, commits, branch
switches and ignore-rule changes are picked up while tsuiku is running.

`tsuiku pr` resolves the pull request through an authenticated
[GitHub CLI](https://cli.github.com) (`gh`); the selector is a PR number, URL,
or branch, and without one gh picks the current branch's PR. The diff is shown
the way GitHub shows it — the merge base of the base branch against the PR
head — and is computed locally. When the PR commits are not available locally
tsuiku fetches them once into the object database without creating any local
ref.

## Configuration

Optional. Everything works without a file; settings only override defaults.

Location: `$XDG_CONFIG_HOME/tsuiku/config.toml` (XDG-first even on macOS;
`~/.config/tsuiku/config.toml` when `XDG_CONFIG_HOME` is unset). Only
absolute `XDG_CONFIG_HOME`/`HOME` values are honored.

```toml
theme = "dark"                    # "dark" (default) or "light"
view = "unified"                  # initial layout: "unified" (default) or "split"
sidebar_min_width = 72            # hide the file sidebar below this terminal width
split_min_width = 120             # fall back to unified below this diff-area width
difft_timeout_seconds = 5         # difftastic subprocess timeout
structural_max_bytes = 2097152    # skip structural diffs for larger pairs
structural_max_lines = 5000       # skip structural diffs for longer files
```

Numeric settings are clamped so a config cannot break responsiveness:
`sidebar_min_width` 48–300, `split_min_width` 60–400,
`difft_timeout_seconds` 1–30, `structural_max_bytes` 524288–8388608,
`structural_max_lines` 1250–20000.

Invalid input never prevents startup: a TOML syntax error rejects the whole
file (warning with position, all defaults), out-of-range numbers are clamped
with a warning, an unknown theme falls back to the default theme, and unknown
keys are ignored. Except for the syntax error, each problem is per-setting
and keeps the other settings effective. Every case reports itself in the
title bar (`config: N warning(s)`) with the full text printed after exit.

## Limitations

- **Rename detection is limited to what Git reports.** Unstaged renames are
  shown as Delete + Add; tsuiku does not do its own rename inference.
- **Binary detection is a conservative built-in heuristic** (NUL bytes and
  UTF-8 validity). It does not follow `.gitattributes` diff/text attributes
  or textconv.
- `tsuiku show` is not strictly compatible with `git show`. Merge commits are
  always compared against the first parent; combined diffs are not shown.
- Windows native is not supported.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work shall be dual licensed as above, without
any additional terms or conditions.
