# Worktree Isolation for Sub-Agents

Zeph can automatically create isolated git worktrees for background sub-agents to prevent file system conflicts and mutations when multiple agents work on the same repository simultaneously.

## Why Worktree Isolation?

When you spawn multiple sub-agents in parallel (e.g., one fixing bugs while another adds features), they both need to work on the codebase without interfering with each other. Without isolation:

- Both agents modify the same working directory
- Changes from one agent can be overwritten by another
- File locks and git state conflicts cause failures
- Test results are unpredictable

Worktree isolation solves this by giving each sub-agent its own independent checkout via git worktrees. Each worktree can have its own branch, uncommitted changes, and build artifacts.

## Setup

### 1. Enable in Configuration

Add the `[worktree]` section to your config:

```toml
[worktree]
enabled = true                    # Enable worktree isolation
bg_isolation = "worktree"         # Isolation mode: "none" or "worktree"
base_ref = "head"                 # Worktree base ref: "head" or "fresh"
git_timeout_secs = 30             # Git operation timeout
```

Or during `zeph init` wizard:

```bash
zeph init
# ... follow the prompts until "Worktree Isolation" step
```

### 2. Understand the Modes

**`bg_isolation = "none"` (default):** Sub-agents operate in the same directory as the main agent. No isolation.

**`bg_isolation = "worktree"`:** Each sub-agent gets its own git worktree cloned from the repository. This requires:
- A git repository at the current working directory (or ancestor)
- Git version 2.7.0 or later
- Sufficient disk space for shallow clones

### 3. Choose a Base Reference

When creating worktrees, Zeph uses one of two strategies:

**`base_ref = "head"` (default):** Clone from the current working tree's HEAD commit. Sub-agents see the exact state of your repository at spawn time.
- **Pros**: Includes all your uncommitted changes and local branches
- **Cons**: If your working tree has broken tests or build errors, sub-agents inherit them

**`base_ref = "fresh"`:** Clone from the main/master branch on the remote. Sub-agents start with a clean, build-ready state.
- **Pros**: Sub-agents always start from a known-good state
- **Cons**: Local changes are not visible to sub-agents; they may be out-of-sync with your working tree

Choose based on your workflow:
- **Development flow**: Use `head` so sub-agents can see your in-progress work
- **CI/release flow**: Use `fresh` so sub-agents always test against the main branch

### 4. Manage Worktrees

List active worktrees:

```bash
zeph worktree list
```

Output example:

```
Active Worktrees:
/home/user/repo/.git/worktrees/zeph-agent-a1b2c3d4 (created 2 minutes ago)
/home/user/repo/.git/worktrees/zeph-agent-e5f6g7h8 (created 1 minute ago)
```

Clean up stale worktrees (those no longer tracked by active sub-agents):

```bash
zeph worktree clean
```

This is safe to run anytime — it only removes worktrees that are no longer in use.

## Spawning Sub-Agents with Isolation

Once worktree isolation is enabled, background sub-agents automatically use isolated worktrees:

```
> /agent spawn code-reviewer Review the auth module
Code reviewer is working in isolated worktree at .git/worktrees/zeph-a1b2c3d4
```

Sub-agents can:
- Read and modify files without affecting your main working tree
- Create branches and commits
- Run tests and builds
- Check out different commits

## Override Base Reference

At runtime, override the base ref for a specific sub-agent spawn:

```bash
zeph --worktree-base-ref fresh
> /agent spawn validator Check main branch
```

This is useful for one-off runs where you want a different base than your config default.

## Performance Considerations

Worktree creation incurs a git clone operation:

- **Shallow clones**: Zeph uses `--depth=1` to minimize download time
- **Time**: Typically 1-10 seconds depending on repository size
- **Disk space**: Shallow clones use ~20-30% of a full clone

Example timings:

| Repository Size | Time | Disk Space |
|-----------------|------|-----------|
| Small (< 100 MB) | 1-2s | 20-30 MB |
| Medium (100-500 MB) | 3-5s | 60-150 MB |
| Large (> 500 MB) | 10-30s | 200+ MB |

If worktree creation is slow on your system:

1. **Increase timeout**: `git_timeout_secs = 60` (default: 30)
2. **Verify network**: Shallow clones fetch from the remote; slow network slows clones
3. **Check disk**: Low disk space can slow git operations
4. **Use `head` mode**: Avoids a remote fetch; only copies the local worktree

## Disk Quota Management

When running many concurrent sub-agents or long-running sessions, worktree storage can accumulate. Zeph provides automatic disk quota management to keep worktree directories under control:

### Configuration

Add the following to your `[worktree]` section:

```toml
[worktree]
enabled = true
max_worktrees = 10                      # Hard limit on active worktrees (default: 10)
disk_quota_mb = 2048                    # Max total disk space for all worktrees in MB (default: 2048, ~2 GiB)
auto_reconcile_secs = 3600              # Periodic sweep interval in seconds (default: 3600, 1 hour)
reconcile_on_startup = true             # Run a cleanup sweep on agent startup (default: true)
```

### How It Works

1. **On startup**: Zeph runs a sweep to clean up orphaned worktrees from previous sessions
2. **On worktree creation**: Checks if adding a new worktree would exceed `disk_quota_mb` or `max_worktrees`. If so, automatically prunes the oldest unused worktrees until there's room
3. **Periodic reconciliation**: Every `auto_reconcile_secs`, the background supervisor runs a cleanup sweep to remove stale worktrees not linked to any active sub-agent
4. **Pruning strategy**: Only removes worktrees that have been pruned via git (git has marked them as prunable); never force-deletes intact checkouts

### Quotas in Action

Example: if `disk_quota_mb = 2048` and `max_worktrees = 10`:

- When you spawn the 11th concurrent sub-agent, the oldest idle worktree is pruned to make room
- If total worktree disk usage would exceed 2 GiB, older worktrees are pruned before creating a new one
- A warning is logged when quota is tight or sweeps are frequent (may indicate undersized quotas)

### Manual Cleanup

Even with automatic quotas, you can manually clean up anytime:

```bash
zeph worktree clean              # Remove stale/pruned worktrees
zeph worktree list               # See current usage and breakdown
```

To temporarily disable periodic reconciliation (e.g., for performance testing):

```bash
auto_reconcile_secs = 0          # Disable periodic sweep; only clean on startup and admission
```

## Troubleshooting

### "Git operation timed out"

The git command to create or remove the worktree exceeded `git_timeout_secs`. Solutions:

1. Increase timeout in config: `git_timeout_secs = 60`
2. Check your network connection
3. Verify the repository is healthy: `git gc`, `git fsck`

### "Worktree already exists"

A previous sub-agent's worktree wasn't cleaned up. Run:

```bash
zeph worktree clean
```

If the directory still exists on disk but git doesn't know about it, remove it manually:

```bash
rm -rf .git/worktrees/zeph-*
```

### Sub-agent fails in worktree

Sub-agents run inside the worktree. If a sub-agent fails:

1. Check the logs: `zeph --debug-dump`
2. Inspect the worktree: `cd .git/worktrees/zeph-<id> && git log --oneline`
3. File an issue with the worktree path and sub-agent logs

### No worktrees listed, but sub-agents are spawning

Worktree listing shows only **created** worktrees. Worktrees are removed automatically when:
- The sub-agent completes and is not being resumed
- You manually run `zeph worktree clean`

If you want to inspect a completed sub-agent's changes before cleanup, resume the sub-agent or manually check the worktree directory before cleaning.

## Next Steps

- [Sub-Agent Orchestration](../advanced/sub-agents.md) — full sub-agent system documentation
- [Configuration Reference](../reference/configuration.md) — all worktree config options
