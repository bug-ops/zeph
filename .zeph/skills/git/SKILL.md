---
name: git
category: dev
description: >
  Run git version control commands for repository management. Use when the user
  asks to check status, view history or diffs, stage and commit changes, manage
  branches, merge or rebase, work with remotes, stash changes, tag releases,
  cherry-pick commits, bisect regressions, resolve conflicts, manage worktrees
  or submodules, or perform any source control operation. Keywords: git, commit,
  branch, merge, rebase, diff, log, push, pull, stash, tag, remote, conflict.
license: MIT
compatibility: Requires git (v2.30+)
metadata:
  author: zeph
  version: "1.0"
---

# Git Operations

## Status and Information

```bash
git status            # working tree status
git status -s         # short format
git branch -vv        # branches with tracking info
```

## Viewing Changes

```bash
# Unstaged / staged / between branches
git diff
git diff --cached
git diff main..feature

# Changes since common ancestor (useful for PRs)
git diff main...feature

# Diff specific file
git diff -- PATH

# Word-level diff
git diff --word-diff

# Stat summary
git diff --stat

# Show a specific commit
git show COMMIT

# Show file at specific commit
git show COMMIT:PATH
```

## Commit History

```bash
# Recent commits (compact)
git log --oneline -20

# With diffs
git log -p -5

# Graph view
git log --oneline --graph --all

# With file stats
git log --stat

# Filter by author / date / message / file
git log --author="NAME"
git log --since="2025-01-01" --until="2025-06-01"
git log --grep="fix"
git log -- PATH

# Commits that added/removed a string (pickaxe)
git log -S "function_name"

# Custom format
git log --pretty=format:"%h %ad %s (%an)" --date=short

# Who changed each line
git blame FILE
git blame -L 10,20 FILE
```

## Staging

```bash
# Stage specific files
git add FILE1 FILE2

# Stage all tracked file changes
git add -u

# Stage everything (tracked + untracked)
git add -A

# Stage hunks interactively
git add -p

# Unstage file (keep changes)
git reset HEAD FILE

# Discard unstaged changes
git restore FILE

# Restore file from specific commit
git restore --source=COMMIT FILE
```

## Committing

```bash
# Commit with message
git commit -m "message"

# Multi-line message
git commit -m "subject" -m "body"

# Stage tracked files and commit
git commit -a -m "message"

# Amend last commit
git commit --amend

# Amend without changing message
git commit --amend --no-edit

# Empty commit (trigger CI)
git commit --allow-empty -m "trigger build"
```

## Undoing Commits

```bash
# Revert a commit (creates new inverse commit)
git revert COMMIT

# Revert without auto-commit
git revert --no-commit COMMIT

# Revert a merge commit (keep first parent)
git revert -m 1 MERGE_COMMIT

# Reset to commit (keep changes staged)
git reset --soft COMMIT

# Reset to commit (keep changes unstaged)
git reset --mixed COMMIT

# Reset to commit (discard all changes)
git reset --hard COMMIT
```

## Branch Management

```bash
# List local / all / remote branches
git branch
git branch -a
git branch -r

# Create branch
git branch BRANCH

# Create and switch
git switch -c BRANCH

# Switch branch
git switch BRANCH

# Rename current branch
git branch -m NEW_NAME

# Delete merged / force delete unmerged
git branch -d BRANCH
git branch -D BRANCH

# Delete remote branch
git push origin --delete BRANCH

# List merged / unmerged branches
git branch --merged
git branch --no-merged
```

## Merging

```bash
# Merge branch into current
git merge BRANCH

# Always create merge commit
git merge --no-ff BRANCH

# Squash merge (no merge commit, changes staged)
git merge --squash BRANCH

# Abort / continue
git merge --abort
git add . && git merge --continue
```

## Rebasing

```bash
# Rebase current branch onto target
git rebase TARGET

# Rebase onto specific base
git rebase --onto NEW_BASE OLD_BASE BRANCH

# Interactive rebase (last N commits)
git rebase -i HEAD~N

# Abort / continue / skip
git rebase --abort
git rebase --continue
git rebase --skip

# Autosquash fixup!/squash! commits
git rebase -i --autosquash TARGET
```

Interactive rebase commands: `pick`, `reword`, `edit`, `squash`, `fixup`, `drop`.

## Remote Operations

```bash
# List remotes
git remote -v

# Add / remove / rename remote
git remote add NAME URL
git remote remove NAME
git remote rename OLD NEW

# Change remote URL
git remote set-url origin NEW_URL

# Fetch (does not merge)
git fetch origin
git fetch --all
git fetch --prune          # prune deleted remote branches

# Pull (fetch + merge)
git pull
git pull --rebase          # rebase instead of merge

# Push
git push origin BRANCH
git push -u origin BRANCH  # set upstream
git push --tags             # push all tags

# Safer force push (rejects if remote has unknown commits)
git push --force-with-lease origin BRANCH
```

## Stash

```bash
# Stash working changes
git stash
git stash push -m "description"

# Stash specific files
git stash push -m "msg" -- FILE1 FILE2

# Include untracked files
git stash -u

# List / apply / pop
git stash list
git stash apply stash@{0}
git stash pop

# Show stash diff
git stash show -p stash@{0}

# Drop / clear
git stash drop stash@{1}
git stash clear

# Create branch from stash
git stash branch BRANCH stash@{0}
```

## Tags

```bash
# List tags
git tag
git tag -l "v1.*"

# Create annotated tag (recommended)
git tag -a TAG -m "message"

# Tag specific commit
git tag -a TAG COMMIT -m "message"

# Push tag / all tags
git push origin TAG
git push --tags

# Delete local / remote tag
git tag -d TAG
git push origin --delete TAG
```

## Cherry-Pick

```bash
# Apply a commit to current branch
git cherry-pick COMMIT

# Without committing
git cherry-pick --no-commit COMMIT

# Range of commits (exclusive start)
git cherry-pick START..END

# Abort / continue
git cherry-pick --abort
git cherry-pick --continue
```

## Bisect (Finding Regressions)

```bash
# Manual bisect
git bisect start
git bisect bad              # current commit is bad
git bisect good COMMIT      # known good commit
# Git checks out middle commit — test and mark good/bad, repeat
git bisect reset            # finish

# Automated bisect with test script
git bisect start HEAD GOOD_COMMIT
git bisect run ./test-script.sh
```

Test script exits: 0 = good, 1-124/126-127 = bad, 125 = skip.

## Reflog (Recovery)

```bash
# Show HEAD movement history
git reflog

# Recover lost commit
git reflog
git switch -c recovery-branch LOST_HASH

# Undo accidental reset
git reset --hard HEAD@{N}
```

Entries expire after 90 days (30 days for unreachable commits).

## Worktree

```bash
# Add worktree for existing branch
git worktree add ../path BRANCH

# Add worktree with new branch
git worktree add -b NEW_BRANCH ../path BASE

# List / remove / prune
git worktree list
git worktree remove ../path
git worktree prune
```

## Submodules

```bash
# Add submodule
git submodule add URL PATH

# Initialize after clone
git submodule init && git submodule update

# Clone with submodules
git clone --recurse-submodules URL

# Update to latest remote
git submodule update --remote

# Remove submodule
git submodule deinit PATH && git rm PATH
```

## Conflict Resolution

When a merge, rebase, or cherry-pick encounters conflicts:

```bash
git status                    # see conflicted files
# Edit files, remove conflict markers (<<<<<<< / ======= / >>>>>>>)
git add FILE                  # mark resolved
git merge --continue          # or rebase --continue / cherry-pick --continue
git merge --abort             # or rebase --abort to cancel
```

## Patch Creation

```bash
# Create patches from commits
git format-patch -N                          # last N commits
git format-patch main..feature --stdout > changes.patch

# Apply patch
git am < changes.patch
git apply changes.patch

# Check if patch applies cleanly
git apply --check changes.patch
```

## Cleanup

```bash
# Remove untracked files (dry run first)
git clean -n
git clean -fd                # files + directories
git clean -fdx               # including ignored files

# Garbage collect
git gc

# Verify integrity
git fsck
```

## Common Workflows

### Feature branch

```bash
git switch main && git pull
git switch -c feat/my-feature
# ... make changes ...
git add -A && git commit -m "add feature"
git push -u origin feat/my-feature
# After PR merge:
git switch main && git pull && git branch -d feat/my-feature
```

### Sync feature branch with main

```bash
git fetch origin main
git rebase origin/main       # or: git merge origin/main
```

### Squash commits before PR

```bash
git rebase -i HEAD~N
# Mark commits as squash/fixup, keep first as pick
```
