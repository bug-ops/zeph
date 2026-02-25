# Search

Use `gh search` for all GitHub search operations. Never use `curl` or browser.

## Query syntax

Queries combine free-text keywords with qualifiers. Qualifiers go inside the quoted string.

| Qualifier | Applies to | Example |
|---|---|---|
| `repo:OWNER/REPO` | all | scope to a single repo |
| `org:ORG` | repos, code | scope to an organization |
| `user:USER` | repos, issues | scope to a user |
| `language:LANG` | repos, code | `language:rust`, `language:python` |
| `filename:NAME` | code | `filename:Cargo.toml`, `filename:.env` |
| `path:DIR` | code | `path:src/`, `path:crates/core` |
| `extension:EXT` | code | `extension:rs`, `extension:toml` |
| `is:STATE` | issues, prs | `is:open`, `is:closed`, `is:merged` |
| `label:NAME` | issues, prs | `label:bug`, `label:"help wanted"` |
| `author:USER` | issues, prs, commits | `author:octocat` |
| `assignee:USER` | issues | `assignee:@me` |
| `milestone:NAME` | issues, prs | `milestone:v1.0` |
| `created:DATE` | issues, prs | `created:>2025-01-01`, `created:2025-01-01..2025-06-01` |
| `updated:DATE` | issues, prs | `updated:>2025-06-01` |
| `stars:N` | repos | `stars:>1000`, `stars:100..500` |
| `topic:NAME` | repos | `topic:cli` |
| `in:SCOPE` | repos, issues | `in:name`, `in:description`, `in:readme`, `in:title,body` |
| `size:N` | repos | `size:>1000` (KB) |
| `license:KEY` | repos | `license:mit`, `license:apache-2.0` |
| `archived:BOOL` | repos | `archived:false` |
| `is:public/private` | repos | `is:public` |
| `involves:USER` | issues, prs | author, assignee, commenter, or mentioned |
| `commenter:USER` | issues, prs | filter by commenter |
| `mentions:USER` | issues, prs | filter by @mention |
| `reviewed-by:USER` | prs | `reviewed-by:octocat` |
| `review:STATE` | prs | `review:approved`, `review:changes_requested`, `review:required` |
| `draft:BOOL` | prs | `draft:true`, `draft:false` |
| `merged:DATE` | prs | `merged:>2025-01-01` |
| `closed:DATE` | issues, prs | `closed:>2025-01-01` |
| `comments:N` | issues, prs | `comments:>10` |
| `reactions:N` | issues, prs | `reactions:>5` |
| `no:THING` | issues, prs | `no:label`, `no:milestone`, `no:assignee` |
| `linked:pr/issue` | issues, prs | `linked:pr` (issue with linked PR) |

Multiple qualifiers combine with spaces (AND logic). Use quotes around values with spaces.

## Search repositories

```bash
gh search repos "QUERY" --limit 10
gh search repos "cli language:rust stars:>100" --limit 10 --sort stars
gh search repos "org:tokio-rs topic:async" --limit 10
gh search repos "QUERY" --json fullName,description,stargazersCount
gh search repos "QUERY license:mit language:rust" --limit 10
```

## Search issues

```bash
gh search issues "QUERY" --limit 10
gh search issues "repo:OWNER/REPO QUERY" --limit 10
gh search issues "QUERY is:open label:bug" --limit 10
gh search issues "QUERY assignee:@me is:open" --limit 10
gh search issues "memory leak created:>2025-01-01 is:open" --limit 10
gh search issues "QUERY no:assignee is:open label:\"help wanted\"" --limit 10
gh search issues "QUERY comments:>5 reactions:>3" --limit 10
```

## Search pull requests

```bash
gh search prs "QUERY" --limit 10
gh search prs "QUERY is:merged" --limit 10
gh search prs "repo:OWNER/REPO is:merged author:USER" --limit 10
gh search prs "QUERY review:approved is:open" --limit 10
gh search prs "QUERY draft:false is:open" --limit 10
```

## Search code

```bash
gh search code "QUERY" --limit 10
gh search code "QUERY language:rust" --limit 10
gh search code "QUERY repo:OWNER/REPO" --limit 10
gh search code "QUERY filename:Cargo.toml" --limit 10
gh search code "QUERY path:src/filter extension:rs" --limit 10
gh search code "QUERY org:ORG" --limit 10
gh search code "serde(rename_all) extension:rs org:bug-ops" --limit 10
```

## Search commits

```bash
gh search commits "QUERY" --limit 10
gh search commits "QUERY repo:OWNER/REPO" --limit 10 --sort committer-date
gh search commits "fix author:USER" --limit 10 --sort author-date
```

## Sorting

| Command | `--sort` values | `--order` |
|---|---|---|
| `gh search repos` | `stars`, `forks`, `help-wanted-issues`, `updated` | `asc`, `desc` |
| `gh search issues/prs` | `created`, `updated`, `comments` | `asc`, `desc` |
| `gh search commits` | `author-date`, `committer-date` | `asc`, `desc` |
| `gh search code` | `indexed` | `asc`, `desc` |

Default order is `desc` (best match first when no `--sort` specified).
