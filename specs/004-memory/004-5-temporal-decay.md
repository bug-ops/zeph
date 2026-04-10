---
aliases:
  - Temporal Decay
  - Forgetting Curve
  - Retention Scoring
  - Ebbinghaus Decay
tags:
  - sdd
  - spec
  - memory
  - temporal
created: 2026-04-10
status: complete
related:
  - "[[004-memory/spec]]"
  - "[[004-1-architecture]]"
  - "[[004-2-compaction]]"
  - "[[004-3-admission-control]]"
---

# Spec: Temporal Decay & Forgetting

Ebbinghaus-inspired forgetting curve, retention scoring, access frequency tracking, SleepGate forgetting pass.

## Overview

Human memory doesn't retain all information equally. The Ebbinghaus forgetting curve models how memory strength decays over time without reinforcement. Zeph implements this principle to deprioritize stale information during compaction and memory search, making room for fresh context while preserving truly important facts.

## Key Invariants

**Always:**
- Access timestamp updated on every successful recall (relevance + graph lookup)
- Retention score ranges [0.0, 1.0]; decays exponentially toward 0 with age
- Memory decisions (keep vs. compact, rank vs. demote) weighted by retention score
- SleepGate forgetting pass runs at configurable intervals (e.g., daily)

**Never:**
- Forget critical facts (flagged `importance=critical`) regardless of age
- Reset access timestamp on passive reads (only on agent-triggered recall)
- Use raw age as decay metric (always apply Ebbinghaus curve)

## Ebbinghaus Forgetting Curve

The standard model:

```
R(t) = e^(-t/S)

where:
  R(t) = retention strength at time t
  t = time elapsed since last access (seconds)
  S = strength constant (decay timescale)
```

Zeph parameterization:

```rust
fn retention_score(
    current_time: i64,
    last_access: i64,
    access_count: u32,
    importance: Importance,
) -> f32 {
    let elapsed = (current_time - last_access) as f32;
    
    // Half-life: how long until retention = 0.5?
    // Default: 7 days for regular facts, 30 days for important
    let half_life = match importance {
        Importance::Critical => f32::INFINITY,  // never decay
        Importance::Important => 30.0 * 86400.0,
        Importance::Normal => 7.0 * 86400.0,
        Importance::Low => 2.0 * 86400.0,
    };
    
    if importance == Importance::Critical {
        return 1.0;  // always retain
    }
    
    // Ebbinghaus decay: R(t) = 2^(-t / half_life)
    // More intuitive than e^(-t/S) for humans
    let decay = 2.0_f32.powf(-elapsed / half_life);
    
    // Boost for frequently accessed items (reinforcement)
    let access_boost = (access_count as f32 / 10.0).min(1.0);
    
    // Combined: decay * (1 + boost)
    let score = decay * (1.0 + access_boost * 0.5);
    
    score.min(1.0).max(0.0)
}
```

## Retention Tracking

Update metadata on every memory recall:

```rust
async fn recall_message(
    &self,
    message_id: &str,
) -> Result<Message> {
    let msg = self.db.get_message(message_id).await?;
    
    // Update access metadata
    self.db.update_message_access(
        message_id,
        AccessUpdate {
            last_access_at: now(),
            access_count: msg.access_count + 1,
        }
    ).await?;
    
    // Update retention score (used in ranking)
    let retention = retention_score(
        now(),
        msg.last_access_at,
        msg.access_count + 1,
        msg.importance,
    );
    self.db.update_message_retention(message_id, retention).await?;
    
    Ok(msg)
}
```

## SleepGate Forgetting Pass

Periodic compaction of low-retention items:

```rust
struct SleepGate {
    enabled: bool,
    interval: Duration,          // e.g., 24 hours
    retention_threshold: f32,    // e.g., 0.15
    max_forget_per_run: u32,     // e.g., 100 items
}

impl SleepGate {
    async fn run_forgetting_pass(
        &self,
        memory: &SemanticMemory,
    ) -> Result<u32> {
        if !self.enabled {
            return Ok(0);
        }
        
        // Find low-retention items
        let candidates = memory.db.query(
            "SELECT id FROM messages 
             WHERE importance != 'critical' 
             AND retention_score < ? 
             ORDER BY retention_score ASC 
             LIMIT ?",
            (self.retention_threshold, self.max_forget_per_run),
        ).await?;
        
        let mut forgotten = 0;
        for item_id in candidates {
            // Soft delete: mark for eventual purge
            memory.db.soft_delete_message(&item_id).await?;
            forgotten += 1;
        }
        
        log::info!("SleepGate: forgot {} low-retention items", forgotten);
        Ok(forgotten)
    }
}
```

## Configuration

```toml
[memory.temporal_decay]
enabled = true

# Ebbinghaus half-life constants (seconds)
half_life_normal = 604800      # 7 days
half_life_important = 2592000  # 30 days

# Access tracking
track_access_count = true
update_on_relevance_score = true

# SleepGate forgetting pass
[memory.temporal_decay.sleepgate]
enabled = true
interval_hours = 24
retention_threshold = 0.15     # forget when retention < 15%
max_forget_per_run = 100
```

## Integration Points

- [[004-1-architecture]] — retention scores used during compaction ranking
- [[004-2-compaction]] — SleepGate invoked as part of compaction cycle
- [[004-3-admission-control]] — recency factor in A-MAC importance scoring
- [[017-index]] — AST-indexed code snippets decay over time without access

## See Also

- [[004-memory/spec]] — Parent
- [[004-1-architecture]] — Core memory pipeline where retention drives decisions
- [[004-2-compaction]] — Compaction threshold uses retention scores
- Ebbinghaus, H. (1885) — *Memory: A Contribution to Experimental Psychology*
