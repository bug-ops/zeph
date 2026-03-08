---
name: git
description: Run git version control commands. Use when the user asks to check repository status, view commit history, show diffs, stage and commit changes, or manage branches.
compatibility: Requires git
---
# Git Operations

## Check status
```bash
git status
```

## View recent history
```bash
git log --oneline -10
```

## Show changes
```bash
git diff
```

## Stage and commit
```bash
git add FILE && git commit -m "MESSAGE"
```

## Branch operations
```bash
git branch -a
git checkout -b BRANCH
git merge BRANCH
```
