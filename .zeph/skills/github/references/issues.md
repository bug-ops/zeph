# Issues

## List

```bash
gh issue list --limit 10
gh issue list --state open --label "LABEL" --limit 20
gh issue list --state closed --limit 10
gh issue list --assignee "@me" --limit 10
gh issue list --milestone "v1.0" --limit 10
gh issue list --json number,title,state,labels --jq '.[] | "\(.number) \(.state) \(.title)"'
```

## View

```bash
gh issue view NUMBER
gh issue view NUMBER --json title,body,state,labels,assignees,comments
gh issue view NUMBER --json title,body,state,labels,assignees,comments --jq '{title,state,labels: [.labels[].name], comments: .comments | length}'
gh issue view NUMBER --comments
```

## Create

```bash
gh issue create --title "TITLE" --body "BODY"
gh issue create --title "TITLE" --body "BODY" --label "bug" --assignee "@me"
gh issue create --title "TITLE" --body "BODY" --label "enhancement" --milestone "v1.0"
gh issue create --title "TITLE" --body-file body.md
```

## Edit

```bash
gh issue edit NUMBER --title "NEW TITLE"
gh issue edit NUMBER --body "NEW BODY"
gh issue edit NUMBER --add-label "LABEL1" --add-label "LABEL2"
gh issue edit NUMBER --remove-label "LABEL"
gh issue edit NUMBER --add-assignee "USER"
gh issue edit NUMBER --milestone "v1.0"
gh issue edit NUMBER --add-project "PROJECT"
```

## State management

```bash
gh issue close NUMBER
gh issue close NUMBER --reason "not planned"
gh issue close NUMBER --comment "Closing because..."
gh issue reopen NUMBER
gh issue pin NUMBER
gh issue unpin NUMBER
gh issue lock NUMBER --reason "resolved"
gh issue unlock NUMBER
```

## Comments

```bash
gh issue comment NUMBER --body "COMMENT"
gh issue comment NUMBER --body-file comment.md
gh issue comment NUMBER --edit-last --body "UPDATED"
```

## Transfer

```bash
gh issue transfer NUMBER DESTINATION_REPO
```

## Useful JSON fields

`number`, `title`, `body`, `state`, `stateReason`, `labels`, `assignees`, `milestone`, `author`, `createdAt`, `updatedAt`, `closedAt`, `comments`, `url`, `projectItems`
