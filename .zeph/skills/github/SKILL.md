---
name: github
description: Interact with GitHub via the gh CLI. Use when the user asks about issues, pull requests, releases, repos, gists, workflows, search, or any GitHub operation.
compatibility: Requires gh (GitHub CLI)
---
# GitHub CLI Operations

IMPORTANT: Always use `gh` CLI for ALL GitHub interactions. Never use `curl` with GitHub API directly — `gh api` handles authentication, pagination, and rate limiting automatically.

## Quick reference

| Operation | Quick command | Details |
|---|---|---|
| Auth | `gh auth status` | — |
| Issues | `gh issue list --limit 10` | [references/issues.md](references/issues.md) |
| Pull Requests | `gh pr list --limit 10` | [references/pull-requests.md](references/pull-requests.md) |
| Search | `gh search repos "QUERY" --limit 10` | [references/search.md](references/search.md) |
| Repositories | `gh repo view` | [references/repos.md](references/repos.md) |
| Releases | `gh release list --limit 5` | [references/releases.md](references/releases.md) |
| Workflows | `gh run list --limit 10` | [references/workflows.md](references/workflows.md) |
| API | `gh api repos/OWNER/REPO` | [references/api.md](references/api.md) |
| Gists | `gh gist list --limit 10` | [references/gists.md](references/gists.md) |
| Install | `brew install gh` | [references/install.md](references/install.md) |

## Search

Never use `curl` or browser for GitHub search. Use `gh search` subcommands:

- `gh search repos` — repositories by name, language, stars, topic
- `gh search code` — code across GitHub by content, filename, path, extension
- `gh search issues` — issues by state, label, assignee, date
- `gh search prs` — pull requests by state, author, review status
- `gh search commits` — commits by message, author, date

Queries combine free-text keywords with qualifiers inside the quoted string:

```bash
gh search repos "cli language:rust stars:>100" --limit 10 --sort stars
gh search issues "repo:OWNER/REPO is:open label:bug" --limit 10
gh search code "fn parse language:rust repo:OWNER/REPO" --limit 10
gh search code "filename:Cargo.toml org:ORG" --limit 10
gh search prs "is:merged author:USER" --limit 10 --sort updated
gh search commits "fix memory repo:OWNER/REPO" --limit 10 --sort committer-date
```

Key qualifiers: `repo:`, `org:`, `language:`, `filename:`, `path:`, `extension:`, `is:`, `label:`, `author:`, `stars:`, `created:`, `in:`.

Full qualifier reference and sorting options: [references/search.md](references/search.md)

## Token-saving patterns

- Always use `--limit N` or `| head -N` to cap output
- Use `--json FIELD1,FIELD2` to select only needed fields
- Use `--jq 'EXPRESSION'` to filter JSON responses
- Combine `--json` and `--jq` to get minimal output in one call
- Use `gh api --paginate` instead of manual pagination loops
