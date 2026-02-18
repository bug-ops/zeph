---
name: github
description: Interact with GitHub via the gh CLI. Use when the user asks about issues, pull requests, releases, repos, gists, workflows, or any GitHub operation.
compatibility: Requires gh (GitHub CLI)
---
# GitHub CLI Operations

All commands use `--json` fields and pipe through `head` to limit output and save tokens.

## Authentication
```bash
gh auth status
```

## Issues
```bash
gh issue list --limit 10
gh issue view NUMBER
gh issue create --title "TITLE" --body "BODY"
gh issue close NUMBER
gh issue list --state open --label "LABEL" --limit 20
gh issue comment NUMBER --body "COMMENT"
```

## Pull Requests
```bash
gh pr list --limit 10
gh pr view NUMBER
gh pr create --title "TITLE" --body "BODY" --base main
gh pr merge NUMBER --squash --delete-branch
gh pr checks NUMBER
gh pr diff NUMBER | head -200
gh pr review NUMBER --approve
gh pr comment NUMBER --body "COMMENT"
```

## Repositories
```bash
gh repo view --json name,description,defaultBranchRef
gh repo clone OWNER/REPO
gh repo create NAME --public --source .
gh repo list OWNER --limit 10
```

## Releases
```bash
gh release list --limit 5
gh release view TAG
gh release create TAG --title "TITLE" --notes "NOTES"
```

## Workflows (CI/CD)
```bash
gh run list --limit 10
gh run view RUN_ID
gh run watch RUN_ID
gh workflow list
gh workflow run WORKFLOW
```

## Search
```bash
gh search repos "QUERY" --limit 5
gh search issues "QUERY" --limit 10
gh search prs "QUERY" --limit 10
```

## API (raw)
```bash
gh api repos/OWNER/REPO --jq '.full_name'
gh api repos/OWNER/REPO/pulls/NUMBER/comments --jq '.[].body' | head -50
```

## Token-saving patterns

- Always use `--limit N` or `| head -N` to cap output
- Use `--json FIELD1,FIELD2` to select only needed fields
- Use `--jq 'EXPRESSION'` to filter JSON responses
- Prefer `gh pr view NUMBER --json title,state,mergeable` over full view

## Installation

If `gh` is not installed, detect OS and follow `references/install.md`.
