---
name: github
category: dev
description: >
  GitHub CLI (gh) for repository management, issues, pull requests, releases,
  workflows, search, gists, and API access. Use when the user asks about any
  GitHub operation — creating repos, forking, cloning, PR workflows, issue
  triage, CI/CD runs, code search, release management, or direct API calls.
  Keywords: github, gh, pull request, issue, repository, release, workflow,
  actions, search, gist, fork, clone, label, milestone, project.
license: MIT
compatibility: Requires gh (GitHub CLI) v2.0+. Install via brew install gh (macOS), winget install GitHub.cli (Windows), or apt install gh (Linux).
metadata:
  author: zeph
  version: "1.0"
---
# GitHub CLI Operations

IMPORTANT: Always use `gh` CLI for ALL GitHub interactions. Never use `curl` with GitHub API
directly — `gh api` handles authentication, pagination, and rate limiting automatically.

## Quick Reference

| Category | Command | Details |
|---|---|---|
| Auth | `gh auth status` | Check login state |
| Auth | `gh auth login` | Interactive login |
| Repos | `gh repo view` | View current repo |
| Repos | `gh repo create NAME` | Create new repo |
| Repos | `gh repo clone OWNER/REPO` | Clone a repo |
| Repos | `gh repo fork OWNER/REPO` | Fork a repo |
| Issues | `gh issue list --limit 10` | [references/issues.md](references/issues.md) |
| Issues | `gh issue create --title T --body B` | Create issue |
| PRs | `gh pr list --limit 10` | [references/pull-requests.md](references/pull-requests.md) |
| PRs | `gh pr create --title T --body B` | Create PR |
| PRs | `gh pr merge N` | Merge PR |
| Search | `gh search repos "QUERY"` | [references/search.md](references/search.md) |
| Releases | `gh release list --limit 5` | [references/releases.md](references/releases.md) |
| Workflows | `gh run list --limit 10` | [references/workflows.md](references/workflows.md) |
| API | `gh api repos/OWNER/REPO` | [references/api.md](references/api.md) |
| Gists | `gh gist list --limit 10` | [references/gists.md](references/gists.md) |
| Labels | `gh label list` | List repo labels |
| Install | `brew install gh` | [references/install.md](references/install.md) |

## Authentication

```bash
# Interactive login (browser or token)
gh auth login

# Login with token (non-interactive, CI)
echo "$TOKEN" | gh auth login --with-token

# Check auth status
gh auth status

# Switch between accounts
gh auth switch

# Refresh token scopes
gh auth refresh --scopes read:project

# Logout
gh auth logout
```

Environment variable auth (overrides stored credentials):

| Variable | Purpose |
|---|---|
| `GH_TOKEN` / `GITHUB_TOKEN` | Auth token for github.com |
| `GH_ENTERPRISE_TOKEN` | Auth token for GitHub Enterprise |

## Repository Workflows

```bash
# Create a new repo (interactive)
gh repo create my-project --public --clone

# Create from template
gh repo create my-project --template OWNER/TEMPLATE --clone --public

# Fork and clone
gh repo fork OWNER/REPO --clone

# Clone
gh repo clone OWNER/REPO

# View repo details
gh repo view OWNER/REPO
gh repo view OWNER/REPO --json name,description,stargazerCount

# Archive / delete
gh repo archive OWNER/REPO
gh repo delete OWNER/REPO --yes

# Set default repo (for commands outside a git directory)
gh repo set-default OWNER/REPO

# Rename
gh repo rename NEW-NAME
```

## Pull Request Workflow

```bash
# Create PR (current branch -> default branch)
gh pr create --title "Title" --body "Description"

# Create PR with specific base
gh pr create --base develop --title "Title" --body "Body"

# Create draft PR
gh pr create --draft --title "WIP: Feature" --body "Body"

# List open PRs
gh pr list --limit 20

# View PR details
gh pr view 123
gh pr view 123 --json state,reviews,checks

# Check PR status (checks, reviews)
gh pr checks 123

# Review a PR
gh pr review 123 --approve
gh pr review 123 --request-changes --body "Fix X"
gh pr review 123 --comment --body "Looks good"

# Merge PR
gh pr merge 123 --squash --delete-branch
gh pr merge 123 --rebase
gh pr merge 123 --merge

# Close without merging
gh pr close 123

# View PR diff
gh pr diff 123

# Checkout a PR locally
gh pr checkout 123

# Edit PR metadata
gh pr edit 123 --add-label "bug" --add-reviewer "user"
gh pr edit 123 --milestone "v1.0"

# View PR comments
gh api repos/OWNER/REPO/pulls/123/comments
```

## Issue Triage

```bash
# List open issues
gh issue list --limit 20
gh issue list --label "bug" --assignee "@me"
gh issue list --state closed --limit 10

# Create issue
gh issue create --title "Bug: X" --body "Steps to reproduce..."
gh issue create --label "bug,critical" --assignee "@me"

# View issue
gh issue view 456
gh issue view 456 --json title,body,labels,comments

# Close / reopen
gh issue close 456 --reason "completed"
gh issue reopen 456

# Edit issue
gh issue edit 456 --add-label "priority:high" --milestone "v2.0"

# Transfer issue to another repo
gh issue transfer 456 OWNER/OTHER-REPO

# Pin / unpin
gh issue pin 456
gh issue unpin 456

# Comment on issue
gh issue comment 456 --body "Working on this"

# List issue comments
gh api repos/OWNER/REPO/issues/456/comments
```

## Search

Never use `curl` or browser for GitHub search. Use `gh search` subcommands:

```bash
# Search repositories
gh search repos "cli language:rust stars:>100" --limit 10 --sort stars

# Search code
gh search code "fn parse language:rust repo:OWNER/REPO" --limit 10
gh search code "filename:Cargo.toml org:ORG" --limit 10

# Search issues
gh search issues "repo:OWNER/REPO is:open label:bug" --limit 10

# Search pull requests
gh search prs "is:merged author:USER" --limit 10 --sort updated

# Search commits
gh search commits "fix memory repo:OWNER/REPO" --limit 10 --sort committer-date
```

Key qualifiers: `repo:`, `org:`, `language:`, `filename:`, `path:`, `extension:`, `is:`,
`label:`, `author:`, `stars:`, `created:`, `in:`.

Full qualifier reference: [references/search.md](references/search.md)

## Output Formatting

### JSON output (`--json`)

Select specific fields to reduce output size:

```bash
gh pr list --json number,title,state
gh issue list --json number,title,labels,assignees
gh repo view --json name,description,stargazerCount
gh pr view 123 --json commits,files,reviews
```

### JQ filtering (`--jq`)

Filter and transform JSON output:

```bash
# Get just PR titles
gh pr list --json title --jq '.[].title'

# Filter issues by label
gh issue list --json number,title,labels \
  --jq '.[] | select(.labels | any(.name == "bug"))'

# Count open PRs by author
gh pr list --json author --jq '[.[].author.login] | group_by(.) | map({(.[0]): length}) | add'

# Extract specific nested fields
gh pr view 123 --json reviews --jq '.reviews[].state'
```

### Go templates (`--template`)

```bash
gh issue list --template '{{range .}}#{{.number}} {{.title}}{{"\n"}}{{end}}'
gh pr list --template '{{range .}}{{.number}}\t{{.state}}\t{{.title}}{{"\n"}}{{end}}'
```

## API Access

Use `gh api` for any GitHub REST or GraphQL endpoint not covered by built-in commands:

```bash
# REST API (automatically authenticated)
gh api repos/OWNER/REPO
gh api repos/OWNER/REPO/contributors --paginate

# POST with JSON body
gh api repos/OWNER/REPO/issues -f title="Bug" -f body="Description"

# GraphQL
gh api graphql -f query='{ viewer { login } }'

# With JQ filtering
gh api repos/OWNER/REPO/releases --jq '.[0].tag_name'

# Paginate automatically
gh api repos/OWNER/REPO/issues --paginate --jq '.[].title'
```

Full API reference: [references/api.md](references/api.md)

## CI/CD and Workflows

```bash
# List workflow runs
gh run list --limit 10
gh run list --workflow build.yml --limit 5

# View run details
gh run view RUN_ID
gh run view RUN_ID --log

# Watch a run in progress
gh run watch RUN_ID

# Rerun a failed run
gh run rerun RUN_ID
gh run rerun RUN_ID --failed

# Trigger a workflow dispatch
gh workflow run build.yml --ref main

# List workflows
gh workflow list

# Manage repo secrets
gh secret set SECRET_NAME --body "value"
gh secret list

# Manage repo variables
gh variable set VAR_NAME --body "value"
gh variable list

# View action cache
gh cache list
gh cache delete KEY
```

Full workflows reference: [references/workflows.md](references/workflows.md)

## Environment Variables

| Variable | Description |
|---|---|
| `GH_TOKEN` | Auth token (overrides stored credentials) |
| `GITHUB_TOKEN` | Alternative to `GH_TOKEN` |
| `GH_ENTERPRISE_TOKEN` | Token for GitHub Enterprise |
| `GH_HOST` | Default GitHub hostname |
| `GH_REPO` | Default repo in `[HOST/]OWNER/REPO` format |
| `GH_EDITOR` | Editor for interactive commands |
| `GH_BROWSER` | Browser for `gh browse` |
| `GH_PAGER` | Pager for output (default: system pager) |
| `GH_DEBUG` | Set to `1` or `api` for debug output |
| `GH_NO_UPDATE_NOTIFIER` | Set to `1` to disable update notifications |
| `GH_CONFIG_DIR` | Config directory (default: `~/.config/gh`) |
| `NO_COLOR` | Disable color output |

## Configuration

```bash
# View current config
gh config list

# Set default editor
gh config set editor vim

# Set default git protocol
gh config set git_protocol ssh

# Set default browser
gh config set browser "firefox"

# Set prompt (disable interactive prompts)
gh config set prompt disabled

# Per-host config
gh config set git_protocol ssh --host github.example.com
```

## Extensions

```bash
# Browse available extensions
gh ext browse

# Install an extension
gh ext install OWNER/gh-EXTENSION

# List installed extensions
gh ext list

# Upgrade extensions
gh ext upgrade --all
gh ext upgrade OWNER/gh-EXTENSION

# Remove an extension
gh ext remove OWNER/gh-EXTENSION
```

## Token-Saving Patterns

- Always use `--limit N` or `| head -N` to cap output
- Use `--json FIELD1,FIELD2` to select only needed fields
- Use `--jq 'EXPRESSION'` to filter JSON responses
- Combine `--json` and `--jq` to get minimal output in one call
- Use `gh api --paginate` instead of manual pagination loops
- Use `--web` to open in browser instead of fetching large content
- Prefer `gh pr checks` over `gh run list` when checking a specific PR
