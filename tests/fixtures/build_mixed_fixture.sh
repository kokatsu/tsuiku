#!/usr/bin/env bash
# Build the mixed fixture repository used for local performance measurement.
#
# Usage: build_mixed_fixture.sh <target-dir>
#
# Creates one repository whose worktree differs from HEAD in 191 files:
#   180 small files  (~300 lines each, rotating .rs / .ts / .py / .go)
#    10 medium files (~1,700 lines each, .rs)
#     1 very large file (~20,000 lines, .rs)
# The size tiers match the whole-file timings recorded for highlight_bench
# (300 / 1.7k / 20k lines), so highlight cost per tier is already known.
#
# CJK coverage: every 20th small file carries CJK comments and string
# literals, and one of them lives at a CJK path.
#
# Generation is deterministic: content depends only on the file index, so
# two runs produce the same commit and identical worktree content
# (.git/index still differs, since it records stat metadata).
#
# The global git config is neutralised so the fixture does not inherit the
# developer's settings (autocrlf, diff.external, and friends).

set -euo pipefail

target=${1:?usage: build_mixed_fixture.sh <target-dir>}

# The target is recreated from scratch, and this command is meant to be typed
# by hand, so refuse to delete anything that is not a previous build of this
# fixture: only a missing directory, an empty one, or one carrying the marker
# written at the end of a build may be removed.
# Inside .git/ so the fixture's 191-file change set stays unpolluted.
marker=.git/tsuiku-mixed-fixture
if [ -e "$target" ]; then
  if [ ! -d "$target" ]; then
    echo "error: $target exists and is not a directory" >&2
    exit 1
  fi
  if [ ! -e "$target/$marker" ] && [ -n "$(ls -A "$target")" ]; then
    echo "error: $target is not empty and has no $marker marker; refusing to delete it" >&2
    exit 1
  fi
fi

export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=tsuiku
export GIT_AUTHOR_EMAIL=tsuiku@example.invalid
export GIT_COMMITTER_NAME=tsuiku
export GIT_COMMITTER_EMAIL=tsuiku@example.invalid
export GIT_AUTHOR_DATE='2026-01-01T00:00:00+00:00'
export GIT_COMMITTER_DATE='2026-01-01T00:00:00+00:00'

rm -rf "$target"
mkdir -p "$target"
target=$(cd "$target" && pwd)

git init --quiet --initial-branch=main "$target"
git -C "$target" config core.autocrlf false
cd "$target"

# Emit roughly $2 lines of synthetic code for extension $1, seeded by $3 so
# every file is unique. Each block is 5 lines (4 of code + 1 blank), the same
# shape highlight_bench synthesises.
synth() {
  local ext=$1 lines=$2 seed=$3
  awk -v ext="$ext" -v lines="$lines" -v seed="$seed" 'BEGIN {
    blocks = int((lines + 4) / 5)
    for (n = 0; n < blocks; n++) {
      id = seed "_" n
      if (ext == "rs")
        printf "fn item_%s() -> usize {\n    let value = \"v%s\"; // block %s\n    value.len() + %d\n}\n\n", id, id, id, n
      else if (ext == "ts")
        printf "export function item_%s(): number {\n    const value = \"v%s\"; // block %s\n    return value.length + %d;\n}\n\n", id, id, id, n
      else if (ext == "py")
        printf "def item_%s():\n    value = \"v%s\"  # block %s\n    return len(value) + %d\n\n\n", id, id, id, n
      else
        printf "func item_%s() int {\n    value := \"v%s\" // block %s\n    return len(value) + %d\n}\n\n", id, id, id, n
    }
  }'
}

# Same shape, with CJK comments and string literals.
synth_cjk() {
  local lines=$1 seed=$2
  awk -v lines="$lines" -v seed="$seed" 'BEGIN {
    blocks = int((lines + 4) / 5)
    for (n = 0; n < blocks; n++) {
      id = seed "_" n
      printf "fn item_%s() -> usize {\n    let value = \"値%s 🎉\"; // 日本語のコメント %s\n    value.chars().count() + %d\n}\n\n", id, id, id, n
    }
  }'
}

exts=(rs ts py go)

# --- base commit ----------------------------------------------------------
mkdir -p small medium large 混在

for i in $(seq -w 1 180); do
  if (( 10#$i % 20 == 0 )); then
    synth_cjk 300 "s$i" > "混在/small_$i.rs"
  else
    ext=${exts[$(( 10#$i % 4 ))]}
    synth "$ext" 300 "s$i" > "small/small_$i.$ext"
  fi
done

for i in $(seq -w 1 10); do
  synth rs 1700 "m$i" > "medium/medium_$i.rs"
done

synth rs 20000 xl > large/very_large.rs

git add -A
git commit --quiet -m 'mixed fixture base'

# --- worktree edits: every file differs from HEAD -------------------------
# Small files gain one appended block (a single add hunk). Medium files also
# get a one-line change in the middle, and the very large file is edited at
# several scattered points, so hunk shapes vary across tiers.
for i in $(seq -w 1 180); do
  if (( 10#$i % 20 == 0 )); then
    synth_cjk 5 "w$i" >> "混在/small_$i.rs"
  else
    ext=${exts[$(( 10#$i % 4 ))]}
    synth "$ext" 5 "w$i" >> "small/small_$i.$ext"
  fi
done

for i in $(seq -w 1 10); do
  f=medium/medium_$i.rs
  # -i.bak + rm, because in-place editing without a suffix is not portable
  # between BSD and GNU sed.
  sed -i.bak -E "s|// block (m${i}_100)\$|// edited \\1|" "$f"
  rm "$f.bak"
  synth rs 5 "w$i" >> "$f"
done

# The 20k-line file holds 4,000 five-line blocks; touch five spread across it.
sed -i.bak -E 's#// block (xl_(400|1200|2000|2800|3600))$#// edited \1#' large/very_large.rs
rm large/very_large.rs.bak
synth rs 5 wxl >> large/very_large.rs

touch "$marker"

printf 'mixed fixture built in %s (%s files changed)\n' \
  "$target" "$(git status --porcelain | wc -l | tr -d ' ')"
