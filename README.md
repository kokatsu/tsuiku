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
tsuiku              # compare HEAD against the final worktree state
tsuiku show <rev>   # compare a commit against its first parent
```

Main keys: `j`/`k` to move by line, `]`/`[` to jump between hunks, `q` to quit.

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
