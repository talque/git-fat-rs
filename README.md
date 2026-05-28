# git-fat-rs

A Rust reimplementation of [git-fat](https://github.com/jedbrown/git-fat), a
tool for managing large binary files in Git repositories.

## What it does

git-fat works by replacing large files in your repository with small text
placeholders, and storing the actual file content separately, either locally
in `.git/fat/objects/` or on a remote store. Git only ever sees the
placeholders, keeping your repository history lean. The real files are synced
separately via `git fat push` and `git fat pull`.

It integrates with Git's filter driver mechanism, so once configured it's
largely transparent: `git add`, `git checkout`, etc. all do the right thing.

## Why this exists

The [original git-fat](https://github.com/jedbrown/git-fat) is a Python
implementation. This version attempts to be a faithful drop-in replacement,
implementing the same commands and placeholder format, but written in Rust and
using [libgit2](https://libgit2.org/) (via the
[git2](https://crates.io/crates/git2) crate) rather than shelling out to git
for most operations. In practice this means noticeably better performance,
especially on repositories with large histories.

It may not be 100% identical to the original in every edge case, but the
intent is full compatibility: same `.gitfat` config format, same placeholder
format, same remote backends.

Caveat: The HTTP backend is not yet implemented, only rsync and copy.

## Setup

Add a `.gitattributes` file to your repository telling git which files to
manage:

```
*.bin filter=fat
```

Then run `git-fat init` to register the filter driver in your local git
config. After that, `git add` on matching files will run them through the
filter automatically.

## Commands

```
git-fat init                    # configure filter driver in .git/config
git-fat filter-clean [file]     # (called by git) clean filter
git-fat filter-smudge [file]    # (called by git) smudge filter
git-fat push [backend]          # push cached objects to remote
git-fat pull [backend]          # pull missing objects from remote
git-fat checkout                # restore placeholder files that have objects cached locally
git-fat status                  # show orphan and stale objects
git-fat list                    # list all git-fat managed files
git-fat find <size>             # find files in history over a size threshold
git-fat index-filter <filelist> # convert files to fat placeholders (for use with git filter-branch)
```

## Configuration

The `.gitfat` file in your repository root configures the remote backend.
Currently supported backends are `rsync` and `copy` (local directory mirror):

```ini
[rsync]
remote = user@example.com:/srv/git-fat
```

```ini
[copy]
remote = /mnt/shared/git-fat-objects
```

## Building

```
cargo build --release
```
