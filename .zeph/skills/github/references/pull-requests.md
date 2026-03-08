# Pull Requests

## List

```bash
gh pr list --limit 10
gh pr list --state merged --limit 10
gh pr list --state closed --limit 10
gh pr list --author "@me" --limit 10
gh pr list --label "LABEL" --limit 10
gh pr list --base main --limit 10
gh pr list --json number,title,state,author --jq '.[] | "\(.number) \(.state) \(.title)"'
```

## View

```bash
gh pr view NUMBER
gh pr view NUMBER --json title,state,mergeable,reviews,statusCheckRollup
gh pr view NUMBER --json title,state,mergeable,reviews,statusCheckRollup --jq '{title,state,mergeable,checks: [.statusCheckRollup[].conclusion]}'
gh pr view NUMBER --comments
```

## Create

```bash
gh pr create --title "TITLE" --body "BODY" --base main
gh pr create --title "TITLE" --body "BODY" --base main --draft
gh pr create --title "TITLE" --body "BODY" --reviewer "USER1,USER2"
gh pr create --title "TITLE" --body "BODY" --label "LABEL" --assignee "@me"
gh pr create --title "TITLE" --body-file body.md --base main
gh pr create --fill  # auto-fill title and body from commits
```

## Edit

```bash
gh pr edit NUMBER --title "NEW TITLE"
gh pr edit NUMBER --body "NEW BODY"
gh pr edit NUMBER --add-label "LABEL"
gh pr edit NUMBER --remove-label "LABEL"
gh pr edit NUMBER --add-reviewer "USER"
gh pr edit NUMBER --base BRANCH
gh pr edit NUMBER --add-assignee "@me"
```

## Review

```bash
gh pr review NUMBER --approve
gh pr review NUMBER --approve --body "LGTM"
gh pr review NUMBER --request-changes --body "REASON"
gh pr review NUMBER --comment --body "COMMENT"
```

## Diff and checks

```bash
gh pr diff NUMBER
gh pr diff NUMBER | head -200
gh pr diff NUMBER --patch
gh pr checks NUMBER
gh pr checks NUMBER --json name,state,conclusion --jq '.[] | "\(.name): \(.conclusion)"'
gh pr checks NUMBER --watch
```

## Merge and state

```bash
gh pr merge NUMBER --squash --delete-branch
gh pr merge NUMBER --rebase --delete-branch
gh pr merge NUMBER --merge
gh pr merge NUMBER --auto --squash  # auto-merge when checks pass
gh pr ready NUMBER   # mark draft as ready
gh pr close NUMBER
gh pr reopen NUMBER
```

## Comments

```bash
gh pr comment NUMBER --body "COMMENT"
gh pr comment NUMBER --body-file comment.md
```

## Useful JSON fields

`number`, `title`, `body`, `state`, `author`, `baseRefName`, `headRefName`, `mergeable`, `isDraft`, `reviewDecision`, `reviews`, `statusCheckRollup`, `labels`, `assignees`, `createdAt`, `mergedAt`, `url`, `additions`, `deletions`, `changedFiles`
