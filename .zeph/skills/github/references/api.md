# API

Use `gh api` instead of `curl`. It handles authentication, pagination, and rate limiting automatically.

## GET requests

```bash
gh api repos/OWNER/REPO --jq '.full_name'
gh api repos/OWNER/REPO/contributors --jq '.[].login' | head -20
gh api repos/OWNER/REPO/pulls/NUMBER/comments --jq '.[].body' | head -50
gh api repos/OWNER/REPO/actions/runs --jq '.workflow_runs[:5] | .[].conclusion'
gh api repos/OWNER/REPO/topics --jq '.names'
```

## Pagination

```bash
gh api repos/OWNER/REPO/issues --paginate --jq '.[].title' | head -30
gh api repos/OWNER/REPO/stargazers --paginate --jq '.[].login' | wc -l
```

Use `--paginate` to automatically follow `Link` headers. Always pipe through `head` or `wc` to limit output.

## POST / PATCH / DELETE

```bash
# Add label
gh api repos/OWNER/REPO/issues/NUMBER/labels -f "labels[]=bug"

# Close issue
gh api -X PATCH repos/OWNER/REPO/issues/NUMBER -f state=closed

# Create comment
gh api repos/OWNER/REPO/issues/NUMBER/comments -f body="COMMENT"

# Delete branch
gh api -X DELETE repos/OWNER/REPO/git/refs/heads/BRANCH

# Create reaction
gh api repos/OWNER/REPO/issues/NUMBER/reactions -f content="+1"
```

## GraphQL

```bash
# Single field
gh api graphql -f query='{ repository(owner:"OWNER", name:"REPO") { stargazerCount } }'

# Complex query
gh api graphql -f query='
  query {
    repository(owner:"OWNER", name:"REPO") {
      issues(first:5, states:OPEN, orderBy:{field:UPDATED_AT, direction:DESC}) {
        nodes { number title updatedAt }
      }
    }
  }
'

# With variables
gh api graphql -F owner='OWNER' -F repo='REPO' -f query='
  query($owner:String!, $repo:String!) {
    repository(owner:$owner, name:$repo) { defaultBranchRef { name } }
  }
'
```

## Rate limit

```bash
gh api rate_limit --jq '{core: .resources.core, search: .resources.search}'
```

## Headers and options

```bash
gh api -H "Accept: application/vnd.github.v3.raw" repos/OWNER/REPO/readme
gh api -H "Accept: application/vnd.github.v3.diff" repos/OWNER/REPO/pulls/NUMBER
gh api --hostname github.example.com repos/OWNER/REPO  # GitHub Enterprise
```
