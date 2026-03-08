# Workflows (CI/CD)

## List workflows

```bash
gh workflow list
gh workflow list --json name,state
gh workflow view WORKFLOW
```

## Trigger

```bash
gh workflow run WORKFLOW
gh workflow run WORKFLOW --ref BRANCH
gh workflow run WORKFLOW -f param1=value1 -f param2=value2
gh workflow enable WORKFLOW
gh workflow disable WORKFLOW
```

## List runs

```bash
gh run list --limit 10
gh run list --workflow WORKFLOW --limit 5
gh run list --branch BRANCH --limit 10
gh run list --status failure --limit 10
gh run list --user USER --limit 10
gh run list --json databaseId,status,conclusion,headBranch --jq '.[] | "\(.databaseId) \(.status) \(.conclusion) \(.headBranch)"'
```

## View run details

```bash
gh run view RUN_ID
gh run view RUN_ID --json status,conclusion,jobs
gh run view RUN_ID --log | tail -100
gh run view RUN_ID --log-failed | tail -50
gh run view RUN_ID --job JOB_ID
```

## Manage runs

```bash
gh run watch RUN_ID              # live status updates
gh run watch RUN_ID --exit-status  # exit non-zero on failure
gh run rerun RUN_ID
gh run rerun RUN_ID --failed     # rerun only failed jobs
gh run rerun RUN_ID --debug      # rerun with debug logging
gh run cancel RUN_ID
gh run delete RUN_ID
```

## Download artifacts

```bash
gh run download RUN_ID
gh run download RUN_ID --name ARTIFACT_NAME
gh run download RUN_ID --dir ./artifacts
```

## Cache management

```bash
gh cache list --limit 10
gh cache delete KEY
gh cache delete --all
```
