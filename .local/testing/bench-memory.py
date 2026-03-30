#!/usr/bin/env python3
"""
Memory benchmarking harness for Zeph.

Seeds N facts into the SQLite memory DB, runs M recall queries via FTS5,
and reports hit_rate, avg_recall_latency_ms, p50_ms, p99_ms,
compression_ratio, and interference_rate as JSON.
Results are also appended to a CSV for longitudinal tracking.

Usage:
    python3 .local/testing/bench-memory.py [--db PATH] [--facts N] [--queries M] [--out CSV]
"""

import argparse
import csv
import json
import os
import random
import sqlite3
import statistics
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent
DEFAULT_DB = SCRIPT_DIR / "data" / "bench-memory.db"
DEFAULT_CSV = SCRIPT_DIR / "results" / "bench-memory-results.csv"

# Fact templates: (fact_text, query_keyword)
FACT_TEMPLATES = [
    ("The capital of {country} is {city}.", "{city}"),
    ("The boiling point of {substance} is {temp} degrees Celsius.", "{substance}"),
    ("The author of '{book}' is {author}.", "{author}"),
    ("The {animal} is native to {region}.", "{animal}"),
    ("The speed of light is approximately {value} km/s.", "speed of light"),
    ("{person} was born in {year} in {place}.", "{person}"),
    ("The chemical symbol for {element} is {symbol}.", "{symbol}"),
    ("The {river} river flows through {country2}.", "{river}"),
    ("The {lang} programming language was created in {year2}.", "{lang}"),
    ("The {planet} has {moons} known moons.", "{planet}"),
]

FILL_DATA = {
    "country": ["France", "Japan", "Brazil", "Germany", "Australia", "Canada", "Egypt", "India"],
    "city": ["Paris", "Tokyo", "Brasilia", "Berlin", "Canberra", "Ottawa", "Cairo", "New Delhi"],
    "substance": ["water", "ethanol", "acetone", "methanol", "benzene", "chloroform"],
    "temp": ["100", "78.4", "56.1", "64.7", "80.1", "61.2"],
    "book": ["1984", "Dune", "Foundation", "Neuromancer", "Snow Crash", "Hyperion"],
    "author": ["Orwell", "Herbert", "Asimov", "Gibson", "Stephenson", "Simmons"],
    "animal": ["kangaroo", "panda", "jaguar", "polar bear", "komodo dragon", "snow leopard"],
    "region": ["Australia", "China", "Amazon", "Arctic", "Indonesia", "Himalayas"],
    "value": ["299792", "299,792", "~3×10^5", "300000", "2.998×10^5"],
    "person": ["Newton", "Curie", "Tesla", "Lovelace", "Turing", "Euler"],
    "year": ["1643", "1867", "1856", "1815", "1912", "1707"],
    "place": ["Woolsthorpe", "Warsaw", "Smiljan", "London", "London", "Basel"],
    "element": ["gold", "silver", "iron", "copper", "helium", "neon"],
    "symbol": ["Au", "Ag", "Fe", "Cu", "He", "Ne"],
    "river": ["Amazon", "Nile", "Yangtze", "Mississippi", "Danube", "Rhine"],
    "country2": ["Brazil", "Africa", "China", "USA", "Europe", "Germany"],
    "lang": ["Python", "Rust", "Go", "Ruby", "Kotlin", "Swift"],
    "year2": ["1991", "2010", "2009", "1995", "2011", "2014"],
    "planet": ["Jupiter", "Saturn", "Uranus", "Neptune", "Mars", "Earth"],
    "moons": ["95", "146", "28", "16", "2", "1"],
}


def _random_fill(template: str, query_tpl: str) -> tuple[str, str]:
    """Fill template slots with random consistent values."""
    slots = {}
    for key, vals in FILL_DATA.items():
        if "{" + key + "}" in template:
            idx = random.randrange(len(vals))
            slots[key] = vals[idx]
            # keep index consistent for paired fields
            for paired, pvals in FILL_DATA.items():
                if paired != key and len(pvals) == len(vals) and "{" + paired + "}" in template:
                    slots[paired] = pvals[idx]
    fact = template.format(**slots)
    query = query_tpl.format(**slots)
    return fact, query


def generate_facts(n: int) -> list[tuple[str, str]]:
    """Generate N (fact, query) pairs."""
    facts = []
    for i in range(n):
        tpl_idx = i % len(FACT_TEMPLATES)
        fact_tpl, query_tpl = FACT_TEMPLATES[tpl_idx]
        fact, query = _random_fill(fact_tpl, query_tpl)
        facts.append((fact, query))
    return facts


def init_db(conn: sqlite3.Connection) -> None:
    conn.executescript("""
        PRAGMA journal_mode = WAL;

        CREATE TABLE IF NOT EXISTS conversations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            importance_score REAL NOT NULL DEFAULT 0.5,
            tier TEXT NOT NULL DEFAULT 'episodic',
            deleted_at TEXT DEFAULT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
            USING fts5(content, content=messages, content_rowid=id);

        CREATE TRIGGER IF NOT EXISTS messages_fts_insert
            AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
            END;

        CREATE TRIGGER IF NOT EXISTS messages_fts_delete
            AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
            END;

        CREATE TABLE IF NOT EXISTS summaries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            content TEXT NOT NULL,
            token_estimate INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
    """)
    conn.commit()


def seed_facts(conn: sqlite3.Connection, facts: list[tuple[str, str]]) -> int:
    """Insert facts as assistant messages in a fresh conversation. Returns conversation_id."""
    cur = conn.execute("INSERT INTO conversations DEFAULT VALUES")
    conv_id = cur.lastrowid
    for fact, _ in facts:
        conn.execute(
            "INSERT INTO messages (conversation_id, role, content, tier) VALUES (?, 'assistant', ?, 'semantic')",
            (conv_id, fact),
        )
    conn.commit()
    return conv_id


def recall_query(conn: sqlite3.Connection, query: str, conv_id: int) -> list[str]:
    """FTS5 recall: return matching message contents."""
    # Escape FTS5 special chars
    safe_query = query.replace('"', '""').replace("'", "''")
    rows = conn.execute(
        """
        SELECT m.content FROM messages_fts f
        JOIN messages m ON m.id = f.rowid
        WHERE f.messages_fts MATCH ? AND m.conversation_id = ? AND m.deleted_at IS NULL
        LIMIT 10
        """,
        (safe_query, conv_id),
    ).fetchall()
    return [r[0] for r in rows]


def keyword_fallback(conn: sqlite3.Connection, query: str, conv_id: int) -> list[str]:
    """LIKE-based fallback when FTS5 returns empty."""
    rows = conn.execute(
        "SELECT content FROM messages WHERE conversation_id = ? AND deleted_at IS NULL AND content LIKE ? LIMIT 10",
        (conv_id, f"%{query}%"),
    ).fetchall()
    return [r[0] for r in rows]


def measure_compression(conn: sqlite3.Connection, conv_id: int, facts: list[tuple[str, str]]) -> float:
    """
    compression_ratio = bytes(summaries) / bytes(original facts).
    Returns 0.0 if no summaries exist (no compression applied).
    """
    summary_rows = conn.execute(
        "SELECT content FROM summaries WHERE conversation_id = ?", (conv_id,)
    ).fetchall()
    if not summary_rows:
        return 0.0
    original_bytes = sum(len(f.encode()) for f, _ in facts)
    summary_bytes = sum(len(r[0].encode()) for r in summary_rows)
    if original_bytes == 0:
        return 0.0
    return summary_bytes / original_bytes


def measure_interference(
    conn: sqlite3.Connection,
    conv_id: int,
    facts: list[tuple[str, str]],
    n_noise: int = 20,
) -> float:
    """
    interference_rate = fraction of noise queries that accidentally match a seeded fact.
    Noise queries are random keywords unlikely to appear in seeded facts.
    """
    noise_words = [
        "xyzzy", "quux", "frobnicator", "blargflob", "zyphon",
        "wibble", "kablammo", "snorkel", "flibbertigibbet", "gazorpazorp",
        "thingamajig", "doohickey", "whatchamacallit", "doodad", "thingamabob",
        "gobbledygook", "mumbo jumbo", "flibbertigibbet", "codswallop", "balderdash",
    ]
    random.shuffle(noise_words)
    hits = 0
    for word in noise_words[:n_noise]:
        results = recall_query(conn, word, conv_id)
        if not results:
            results = keyword_fallback(conn, word, conv_id)
        if results:
            hits += 1
    return hits / n_noise if n_noise > 0 else 0.0


def run_benchmark(
    db_path: Path,
    n_facts: int,
    n_queries: int,
) -> dict:
    conn = sqlite3.connect(str(db_path))
    try:
        init_db(conn)
        facts = generate_facts(n_facts)
        conv_id = seed_facts(conn, facts)

        latencies_ms: list[float] = []
        hits = 0

        for i in range(n_queries):
            fact_idx = i % len(facts)
            _, query = facts[fact_idx]
            expected_fact = facts[fact_idx][0]

            t0 = time.perf_counter()
            results = recall_query(conn, query, conv_id)
            if not results:
                results = keyword_fallback(conn, query, conv_id)
            elapsed_ms = (time.perf_counter() - t0) * 1000
            latencies_ms.append(elapsed_ms)

            # hit = at least one result contains the expected fact content
            if any(expected_fact in r or query.lower() in r.lower() for r in results):
                hits += 1

        latencies_ms_sorted = sorted(latencies_ms)
        avg_latency = statistics.mean(latencies_ms) if latencies_ms else 0.0
        p50 = statistics.median(latencies_ms_sorted) if latencies_ms_sorted else 0.0
        n = len(latencies_ms_sorted)
        p99_idx = max(0, int(0.99 * n) - 1) if n > 0 else 0
        p99 = latencies_ms_sorted[p99_idx] if latencies_ms_sorted else 0.0

        compression_ratio = measure_compression(conn, conv_id, facts)
        interference_rate = measure_interference(conn, conv_id, facts)

        return {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "n_facts": n_facts,
            "n_queries": n_queries,
            "hit_rate": hits / n_queries if n_queries > 0 else 0.0,
            "avg_recall_latency_ms": round(avg_latency, 3),
            "p50_ms": round(p50, 3),
            "p99_ms": round(p99, 3),
            "compression_ratio": round(compression_ratio, 4),
            "interference_rate": round(interference_rate, 4),
        }
    finally:
        conn.close()


def append_csv(csv_path: Path, result: dict) -> None:
    csv_path.parent.mkdir(parents=True, exist_ok=True)
    fieldnames = [
        "timestamp", "n_facts", "n_queries",
        "hit_rate", "avg_recall_latency_ms", "p50_ms", "p99_ms",
        "compression_ratio", "interference_rate",
    ]
    write_header = not csv_path.exists()
    with open(csv_path, "a", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        if write_header:
            writer.writeheader()
        writer.writerow({k: result[k] for k in fieldnames})


def main() -> None:
    parser = argparse.ArgumentParser(description="Zeph memory benchmarking harness")
    parser.add_argument(
        "--db",
        type=Path,
        default=DEFAULT_DB,
        help=f"Path to SQLite DB (default: {DEFAULT_DB})",
    )
    parser.add_argument(
        "--facts",
        type=int,
        default=100,
        metavar="N",
        help="Number of facts to seed (default: 100)",
    )
    parser.add_argument(
        "--queries",
        type=int,
        default=200,
        metavar="M",
        help="Number of recall queries to run (default: 200)",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_CSV,
        help=f"CSV output path for longitudinal tracking (default: {DEFAULT_CSV})",
    )
    parser.add_argument(
        "--json-only",
        action="store_true",
        help="Print JSON result only, skip CSV append",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=None,
        help="Random seed for reproducible runs",
    )
    args = parser.parse_args()

    if args.seed is not None:
        random.seed(args.seed)

    args.db.parent.mkdir(parents=True, exist_ok=True)

    result = run_benchmark(args.db, args.facts, args.queries)

    print(json.dumps(result, indent=2))

    if not args.json_only:
        append_csv(args.out, result)
        print(f"\nResults appended to {args.out}", flush=True)


if __name__ == "__main__":
    main()
