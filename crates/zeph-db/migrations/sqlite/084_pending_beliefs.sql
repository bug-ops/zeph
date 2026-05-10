-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Pre-commitment probabilistic edge layer for BeliefMem (issue #3706).
-- Candidate facts accumulate evidence via Noisy-OR before promotion to committed graph_edges.

CREATE TABLE IF NOT EXISTS pending_beliefs (
    id                  INTEGER PRIMARY KEY,
    source_entity_id    INTEGER NOT NULL REFERENCES graph_entities(id),
    target_entity_id    INTEGER NOT NULL REFERENCES graph_entities(id),
    relation            TEXT NOT NULL,
    canonical_relation  TEXT NOT NULL,
    fact                TEXT NOT NULL,
    edge_type           TEXT NOT NULL DEFAULT 'semantic',
    prob                REAL NOT NULL CHECK (prob > 0.0 AND prob < 1.0),
    episode_id          TEXT,
    created_at          INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at          INTEGER NOT NULL DEFAULT (unixepoch()),
    promoted_at         INTEGER,
    promoted_edge_id    INTEGER REFERENCES graph_edges(id)
);

-- Primary lookup: find existing belief for the same (source, canonical_relation, target, edge_type)
CREATE INDEX IF NOT EXISTS idx_pending_beliefs_lookup
    ON pending_beliefs(source_entity_id, canonical_relation, target_entity_id, edge_type)
    WHERE promoted_at IS NULL;

-- Retrieval: top-K candidates by probability for a (source, canonical_relation) query
CREATE INDEX IF NOT EXISTS idx_pending_beliefs_retrieval
    ON pending_beliefs(source_entity_id, canonical_relation, prob)
    WHERE promoted_at IS NULL;

-- Each observation that contributed to a belief's cumulative probability via Noisy-OR
CREATE TABLE IF NOT EXISTS belief_evidence (
    id              INTEGER PRIMARY KEY,
    belief_id       INTEGER NOT NULL REFERENCES pending_beliefs(id) ON DELETE CASCADE,
    prior_prob      REAL NOT NULL,
    evidence_prob   REAL NOT NULL,
    posterior_prob  REAL NOT NULL,
    episode_id      TEXT,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_belief_evidence_belief
    ON belief_evidence(belief_id, created_at);
