# Repositories

## View

```bash
gh repo view
gh repo view --json name,description,defaultBranchRef
gh repo view OWNER/REPO --json name,description,stargazerCount,forkCount,licenseInfo
gh repo view OWNER/REPO --web  # open in browser
```

## List

```bash
gh repo list OWNER --limit 10
gh repo list OWNER --limit 10 --json name,description,isPrivate
gh repo list OWNER --source --limit 10  # exclude forks
gh repo list OWNER --language rust --limit 10
```

## Clone

```bash
gh repo clone OWNER/REPO
gh repo clone OWNER/REPO -- --depth 1  # shallow clone
```

## Create

```bash
gh repo create NAME --public --source .
gh repo create NAME --private --source . --push
gh repo create NAME --public --clone --license mit --gitignore Rust
gh repo create ORG/NAME --public --source .
```

## Fork

```bash
gh repo fork OWNER/REPO
gh repo fork OWNER/REPO --clone
gh repo fork OWNER/REPO --org ORG
```

## Settings

```bash
gh repo edit --description "DESCRIPTION"
gh repo edit --default-branch BRANCH
gh repo edit --visibility public
gh repo edit --enable-issues=false
gh repo archive OWNER/REPO
gh repo unarchive OWNER/REPO
gh repo delete OWNER/REPO --yes
gh repo rename NEW_NAME
```

## Useful JSON fields

`name`, `description`, `defaultBranchRef`, `stargazerCount`, `forkCount`, `isPrivate`, `isArchived`, `licenseInfo`, `url`, `sshUrl`, `createdAt`, `pushedAt`, `diskUsage`, `languages`
