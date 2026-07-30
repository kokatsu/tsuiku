#!/usr/bin/env bash
# Build the fixture repositories used by ChangeDiscoverer tests.
#
# Usage: build_fixture_repos.sh <target-dir>
#
# Creates nine repositories, because some states are mutually exclusive:
#   main/     — a normal repository carrying every ordinary change kind plus
#               the composite staged/worktree states
#   unborn/   — `git init` only, with untracked files (no HEAD yet)
#   conflict/ — stopped mid-merge with unmerged index entries
#   bare/     — no worktree, so only commit-to-commit diffs are possible
#   super/    — a superproject whose checked-out submodule is left dirty
#   super-ignore/ — the same shape, but configured to ignore a dirty checkout
#   filemode/ — core.fileMode is off, so the executable bit is not tracked
#   sparse/   — a cone sparse checkout with an updated skip-worktree entry
#   symlinks-off/ — core.symlinks is off, so links are plain files on disk
#
# main/ carries tags (fixture-root, fixture-annotated-root, fixture-merge,
# fixture-gitlink, fixture-rename) so tests name commits instead of counting
# back from HEAD.
#
# The global git config is neutralised so the fixtures do not inherit the
# developer's settings (autocrlf, diff.external, and friends).

set -euo pipefail

target=${1:?usage: build_fixture_repos.sh <target-dir>}

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

init_repo() {
  git init --quiet --initial-branch=main "$1"
  git -C "$1" config core.autocrlf false
  git -C "$1" config core.symlinks true
}

# --------------------------------------------------------------------------
# main repository
# --------------------------------------------------------------------------
main=$target/main
init_repo "$main"
cd "$main"

# --- root commit ----------------------------------------------------------
# Files that later change in the worktree, the index, or both.
printf 'tracked modify: base\n'   > tracked_modify.txt
printf 'staged only: base\n'      > staged_only.txt
printf 'unstaged only: base\n'    > unstaged_only.txt
printf 'both: base\n'             > both.txt
printf 'to delete: base\n'        > to_delete.txt
printf 'to rename: base\n'        > to_rename.txt
printf 'unchanged: base\n'        > unchanged.txt
: > empty.txt

# Line-model edge cases, so the discoverer is exercised on the same shapes
# the coordinate model was fixed against.
printf 'crlf one\r\ncrlf two\r\n'  > crlf.txt
printf 'no trailing newline'       > no_eof_newline.txt
printf '// 日本語のコメント\n絵文字 \U0001f389\n' > cjk.txt

# Binary-classified content: invalid UTF-8, and valid UTF-8 containing NUL.
printf 'head \377\376 tail\n'          > invalid_utf8.bin
printf 'valid utf8 \000 with nul\n'    > nul_valid_utf8.txt

# Executable-bit cases.
printf '#!/bin/sh\necho mode\n'        > mode_change.sh
printf '#!/bin/sh\necho both\n'        > mode_and_content.sh

# Symlinks: one deleted, one retargeted, one left alone.
ln -s tracked_modify.txt link_removed
ln -s tracked_modify.txt link_retarget
ln -s tracked_modify.txt link_unchanged

# Composite-state subjects; see the composite states section below.
printf 'compose revert: base\n'   > compose_revert.txt
printf 'compose recreate: base\n' > compose_recreate.txt
printf 'compose rename: base\n'   > compose_rename.txt
printf 'rename recreate: base\n'  > rename_recreate.txt
printf 'rename undo: base\n'      > rename_undo.txt

# Renamed in a commit of its own, so tree-to-tree rename handling has a
# subject that does not depend on the index.
printf 'committed rename: base\n' > committed_rename_src.txt

# Non-ASCII path. Kept separate from the invalid-byte path below, which not
# every filesystem accepts.
mkdir -p '対句'
printf 'kanshi\n' > '対句/漢詩.txt'

printf 'ignored/\n*.log\n' > .gitignore

git add -A
git commit --quiet -m 'root commit'
root_commit=$(git rev-parse HEAD)
git tag fixture-root
git tag --annotate --message 'annotated root tag' fixture-annotated-root

# --- a merge commit, for CommitVsParent first-parent handling -------------
git checkout --quiet -b side
printf 'side branch\n' > side_only.txt
git add side_only.txt
git commit --quiet -m 'side commit'

git checkout --quiet main
printf 'main branch\n' > main_only.txt
git add main_only.txt
git commit --quiet -m 'main commit'

git merge --quiet --no-ff -m 'merge side into main' side
merge_commit=$(git rev-parse HEAD)
git tag fixture-merge

# --- a gitlink entry, standing in for a submodule -------------------------
# A real submodule would need a second repository and network-ish plumbing;
# the discoverer only ever sees the gitlink, so we write one directly.
git update-index --add --cacheinfo "160000,$root_commit,submodule_like"
git commit --quiet -m 'add gitlink entry'
git tag fixture-gitlink

# A file that only exists to be deleted-and-staged later.
printf 'staged delete: base\n' > staged_delete.txt
git add staged_delete.txt
git commit --quiet -m 'add staged_delete.txt'

git mv committed_rename_src.txt committed_rename_dst.txt
git commit --quiet -m 'rename committed_rename_src.txt'
git tag fixture-rename

head_commit=$(git rev-parse HEAD)

# --- working state: ordinary changes --------------------------------------
printf 'tracked modify: worktree\n' > tracked_modify.txt   # unstaged modify

printf 'staged only: staged\n' > staged_only.txt
git add staged_only.txt                                     # staged modify

printf 'unstaged only: worktree\n' > unstaged_only.txt      # unstaged modify

printf 'both: staged\n' > both.txt
git add both.txt
printf 'both: worktree\n' > both.txt                        # staged + unstaged

rm to_delete.txt                                            # unstaged delete
git rm --quiet --cached staged_delete.txt
rm -f staged_delete.txt                                     # staged delete

git mv to_rename.txt renamed.txt                            # staged rename

chmod +x mode_change.sh
git add mode_change.sh                                      # mode-only change

chmod +x mode_and_content.sh
printf '#!/bin/sh\necho both changed\n' > mode_and_content.sh
git add mode_and_content.sh                                 # mode + content

rm link_removed                                             # symlink delete
rm link_retarget && ln -s unchanged.txt link_retarget       # symlink retarget
ln -s unchanged.txt link_added                              # symlink add

printf 'invalid \377 changed\n' > invalid_utf8.bin
printf 'valid utf8 \000 changed\n' > nul_valid_utf8.txt

printf 'untracked\n' > untracked.txt                        # untracked
mkdir -p ignored && printf 'ignored\n' > ignored/ignored.txt
printf 'ignored\n' > debug.log                              # ignored

# A path with a byte that is not valid UTF-8. APFS and HFS+ reject these, so
# this is best-effort and the tests must tolerate its absence.
{ printf 'invalid path\n' > $'invalid\xff_path.txt'; } 2>/dev/null || true

# --- working state: composite states --------------------------------------
# Each of these is wrong if HEAD→index and index→worktree are simply unioned.

# staged modify + worktree revert to HEAD → no difference against HEAD
printf 'compose revert: staged\n' > compose_revert.txt
git add compose_revert.txt
printf 'compose revert: base\n' > compose_revert.txt

# staged delete + worktree recreate → Modify against HEAD
git rm --quiet --cached compose_recreate.txt
printf 'compose recreate: recreated\n' > compose_recreate.txt

# staged rename + worktree further modify → Rename plus a content difference
git mv compose_rename.txt compose_renamed.txt
printf 'compose rename: modified after rename\n' > compose_renamed.txt

# staged add + worktree delete → no difference against HEAD
printf 'compose add\n' > compose_add_delete.txt
git add compose_add_delete.txt
rm compose_add_delete.txt

# staged rename + a different file written back at the source path
# → Rename plus an untracked addition at the old path
git mv rename_recreate.txt rename_recreated.txt
printf 'a different file at the old path\n' > rename_recreate.txt

# staged rename undone in the worktree: the destination is gone and the
# original file is back, byte for byte. Against HEAD nothing changed.
git mv rename_undo.txt rename_undone.txt
mv rename_undone.txt rename_undo.txt

# --------------------------------------------------------------------------
# unborn repository
# --------------------------------------------------------------------------
unborn=$target/unborn
init_repo "$unborn"
cd "$unborn"
printf 'unborn untracked\n' > untracked.txt
printf 'unborn staged\n' > staged.txt
git add staged.txt
printf 'ignored/\n' > .gitignore
mkdir -p ignored && printf 'ignored\n' > ignored/ignored.txt

# --------------------------------------------------------------------------
# conflict repository
# --------------------------------------------------------------------------
conflict=$target/conflict
init_repo "$conflict"
cd "$conflict"
printf 'base\n' > conflicted.txt
printf 'untouched\n' > clean.txt
git add -A
git commit --quiet -m 'base'

git checkout --quiet -b theirs
printf 'theirs\n' > conflicted.txt
git commit --quiet -am 'theirs'

git checkout --quiet main
printf 'ours\n' > conflicted.txt
git commit --quiet -am 'ours'

# Leave the merge stopped, so the index keeps stages 1/2/3.
git merge theirs --quiet 2>/dev/null || true

# --------------------------------------------------------------------------
# bare repository: no worktree, but the object database answers commit diffs
# --------------------------------------------------------------------------
git clone --quiet --bare "$main" "$target/bare"

# --------------------------------------------------------------------------
# superproject with a checked-out submodule left dirty
# --------------------------------------------------------------------------
subrepo=$target/submodule-origin
init_repo "$subrepo"
cd "$subrepo"
printf 'sub v1\n' > s.txt
git add -A
git commit --quiet -m 'submodule base'

super=$target/super
init_repo "$super"
cd "$super"
printf 'top\n' > top.txt
git add -A
git commit --quiet -m base
# The file transport is refused by default for submodules.
git -c protocol.file.allow=always submodule add --quiet "$subrepo" sub
git -c protocol.file.allow=always submodule add --quiet "$subrepo" sub_untracked
git commit --quiet -m 'add submodules'
# Same commit, modified tracked file: git renders this as "<oid>-dirty".
printf 'dirty edit\n' >> sub/s.txt
# Same commit, only an untracked file: git reports it in status but emits no
# diff at all, so this must not become a change.
printf 'untracked only\n' > sub_untracked/extra.txt

# --------------------------------------------------------------------------
# superproject configured to ignore a dirty submodule checkout
# --------------------------------------------------------------------------
super_ignore=$target/super-ignore
init_repo "$super_ignore"
cd "$super_ignore"
printf 'top\n' > top.txt
git add -A
git commit --quiet -m base
git -c protocol.file.allow=always submodule add --quiet "$subrepo" sub
git commit --quiet -m 'add submodule'

# Advance the submodule and stage the new commit, then dirty the checkout.
# With diff.ignoreSubmodules=dirty git shows the new commit without `-dirty`.
( cd sub && printf 'sub v2\n' > s2.txt && git add -A && git commit --quiet -m 'submodule v2' )
git add sub
printf 'dirty edit\n' >> sub/s.txt
git config diff.ignoreSubmodules dirty

# --------------------------------------------------------------------------
# repository that does not track the executable bit
# --------------------------------------------------------------------------
filemode=$target/filemode
init_repo "$filemode"
cd "$filemode"
git config core.fileMode false
printf 'body\n' > s.sh
git add -A
git commit --quiet -m base
# Stage a content change, put the original content back, then set the
# executable bit. Git reports nothing at all: the content matches HEAD and the
# bit is not tracked.
printf 'staged body\n' > s.sh
git add s.sh
printf 'body\n' > s.sh
chmod +x s.sh

# --------------------------------------------------------------------------
# sparse checkout: entries outside the cone are absent from disk, and git
# compares the index in their place
# --------------------------------------------------------------------------
sparse=$target/sparse
init_repo "$sparse"
cd "$sparse"
mkdir -p kept omitted
printf 'kept\n' > kept/in.txt
printf 'omitted v1\n' > omitted/out.txt
git add -A
git commit --quiet -m base
git sparse-checkout set --cone kept
# Update the index for the omitted path. `update-index --cacheinfo` clears the
# skip-worktree bit, so put it back.
omitted_blob=$(printf 'omitted v2\n' | git hash-object -w --stdin)
git update-index --cacheinfo "100644,$omitted_blob,omitted/out.txt"
git update-index --skip-worktree omitted/out.txt

# --------------------------------------------------------------------------
# core.symlinks off: symlinks are checked out as regular files holding their
# target, while the index keeps recording them as links
# --------------------------------------------------------------------------
symlinks_off=$target/symlinks-off
init_repo "$symlinks_off"
cd "$symlinks_off"
printf 'target\n' > target.txt
ln -s target.txt link
git add -A
git commit --quiet -m base
git config core.symlinks false
rm link
git checkout --force -- link
# Stage a different target, then put the original back. Git sees no change.
printf 'other.txt' > link
git add link
printf 'target.txt' > link

cd "$target"
printf 'fixtures built in %s\n' "$target"
printf '  main:     head=%s merge=%s root=%s\n' "$head_commit" "$merge_commit" "$root_commit"
