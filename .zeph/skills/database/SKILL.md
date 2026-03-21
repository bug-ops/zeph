---
name: database
description: >
  Query and manage SQLite, PostgreSQL, and MySQL databases from the command
  line. Use when the user asks to run SQL queries, inspect database schemas,
  create or alter tables, import or export data, manage indexes, analyze query
  performance with EXPLAIN, back up or restore databases, or perform CRUD
  operations via sqlite3, psql, or mysql CLI tools.
license: MIT
compatibility: Requires sqlite3, psql, or mysql CLI tools depending on the database engine
metadata:
  author: zeph
  version: "1.0"
---

# Database CLI Operations

## Quick Reference

| Action | SQLite | PostgreSQL | MySQL |
|--------|--------|------------|-------|
| Connect | `sqlite3 db.sqlite` | `psql -U user -d dbname` | `mysql -u user -p dbname` |
| List databases | `.databases` | `\l` | `SHOW DATABASES;` |
| List tables | `.tables` | `\dt` | `SHOW TABLES;` |
| Describe table | `.schema tablename` | `\d tablename` | `DESCRIBE tablename;` |
| Quit | `.quit` | `\q` | `\q` or `exit` |
| Run file | `.read file.sql` | `\i file.sql` | `source file.sql` |

## SQLite (sqlite3)

### Connection and Configuration

```bash
# Open or create a database
sqlite3 mydb.sqlite

# Open read-only
sqlite3 -readonly mydb.sqlite

# Execute SQL directly (non-interactive)
sqlite3 mydb.sqlite "SELECT * FROM users;"

# Execute SQL from file
sqlite3 mydb.sqlite < queries.sql
sqlite3 mydb.sqlite ".read queries.sql"

# Output modes
sqlite3 mydb.sqlite -header -column "SELECT * FROM users;"
sqlite3 mydb.sqlite -json "SELECT * FROM users;"
sqlite3 mydb.sqlite -csv "SELECT * FROM users;"
sqlite3 mydb.sqlite -markdown "SELECT * FROM users;"
```

### Dot Commands

```bash
.help                    # List all dot commands
.tables                  # List all tables
.tables %user%           # List tables matching pattern
.schema                  # Show CREATE statements for all tables
.schema users            # Show CREATE statement for specific table
.indexes                 # List all indexes
.indexes users           # List indexes for specific table
.headers on              # Show column headers
.mode column             # Columnar output (also: csv, json, markdown, table, line)
.width 20 10 30          # Set column widths
.timer on                # Show query execution time
.dbinfo                  # Show database metadata
.dump                    # Dump entire database as SQL
.dump users              # Dump specific table
.import file.csv users   # Import CSV into table
.output result.txt       # Redirect output to file
.output stdout           # Reset output to terminal
.changes on              # Show number of rows changed
.eqp on                  # Show query plan automatically
```

### Schema Operations

```sql
-- Create table
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT UNIQUE,
    created_at TEXT DEFAULT (datetime('now'))
);

-- Add column
ALTER TABLE users ADD COLUMN role TEXT DEFAULT 'user';

-- Rename table
ALTER TABLE users RENAME TO app_users;

-- Create index
CREATE INDEX idx_users_email ON users(email);
CREATE UNIQUE INDEX idx_users_name ON users(name);

-- Drop index
DROP INDEX idx_users_email;

-- Analyze (update query planner statistics)
ANALYZE;
```

### CRUD Operations

```sql
-- Insert
INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
INSERT INTO users (name, email) VALUES ('Bob', 'bob@example.com'), ('Carol', 'carol@example.com');

-- Select
SELECT * FROM users WHERE role = 'admin' ORDER BY name LIMIT 10;
SELECT name, COUNT(*) as cnt FROM orders GROUP BY name HAVING cnt > 5;

-- Update
UPDATE users SET role = 'admin' WHERE email = 'alice@example.com';

-- Delete
DELETE FROM users WHERE created_at < datetime('now', '-1 year');

-- Upsert (insert or replace)
INSERT OR REPLACE INTO users (id, name, email) VALUES (1, 'Alice', 'alice@new.com');
INSERT INTO users (name, email) VALUES ('Alice', 'alice@new.com')
  ON CONFLICT(email) DO UPDATE SET name = excluded.name;
```

### Import and Export

```bash
# Export to CSV
sqlite3 -header -csv mydb.sqlite "SELECT * FROM users;" > users.csv

# Export to JSON
sqlite3 -json mydb.sqlite "SELECT * FROM users;" > users.json

# Import CSV
sqlite3 mydb.sqlite <<'EOF'
.mode csv
.import users.csv users
EOF

# Backup (SQL dump)
sqlite3 mydb.sqlite .dump > backup.sql

# Restore from dump
sqlite3 newdb.sqlite < backup.sql

# Binary backup (online, safe while database is in use)
sqlite3 mydb.sqlite ".backup backup.sqlite"
```

### Query Analysis

```sql
-- Query plan
EXPLAIN QUERY PLAN SELECT * FROM users WHERE email = 'alice@example.com';

-- Full explain
EXPLAIN SELECT * FROM users WHERE email = 'alice@example.com';

-- Integrity check
PRAGMA integrity_check;

-- Database size info
PRAGMA page_count;
PRAGMA page_size;

-- Table info
PRAGMA table_info(users);

-- Foreign key check
PRAGMA foreign_key_check;

-- WAL mode (recommended for concurrent access)
PRAGMA journal_mode=WAL;

-- Optimize after bulk operations
VACUUM;
```

## PostgreSQL (psql)

### Connection

```bash
# Connect to local database
psql -U postgres -d mydb

# Connect with host and port
psql -h localhost -p 5432 -U myuser -d mydb

# Connection string (URI)
psql "postgresql://user:password@host:5432/dbname?sslmode=require"

# Execute SQL directly
psql -U postgres -d mydb -c "SELECT * FROM users;"

# Execute SQL file
psql -U postgres -d mydb -f queries.sql

# Output formatting
psql -U postgres -d mydb --csv -c "SELECT * FROM users;"
psql -U postgres -d mydb -t -A -c "SELECT count(*) FROM users;"  # tuples only, unaligned
```

### Meta-Commands

```
\l                       -- List all databases
\c dbname                -- Connect to database
\dt                      -- List tables in current schema
\dt public.*             -- List tables in public schema
\dt+ users               -- Table details with size
\d users                 -- Describe table (columns, types, constraints)
\d+ users                -- Extended description (storage, stats)
\di                      -- List indexes
\di+ idx_users_email     -- Index details
\dn                      -- List schemas
\df                      -- List functions
\dv                      -- List views
\du                      -- List roles/users
\dp users                -- Show table privileges
\x                       -- Toggle expanded display (vertical rows)
\timing                  -- Toggle query timing display
\i file.sql              -- Execute SQL file
\o output.txt            -- Send output to file
\o                       -- Reset output to terminal
\! command               -- Execute shell command
\e                       -- Edit query in $EDITOR
\g                       -- Execute last query again
\s                       -- Show command history
\pset format csv         -- Set output format (csv, html, latex, wrapped)
```

### Schema Inspection

```sql
-- List all tables with sizes
SELECT schemaname, tablename,
       pg_size_pretty(pg_total_relation_size(schemaname || '.' || tablename))
FROM pg_tables
WHERE schemaname = 'public'
ORDER BY pg_total_relation_size(schemaname || '.' || tablename) DESC;

-- List columns for a table
SELECT column_name, data_type, is_nullable, column_default
FROM information_schema.columns
WHERE table_name = 'users'
ORDER BY ordinal_position;

-- List indexes
SELECT indexname, indexdef
FROM pg_indexes
WHERE tablename = 'users';

-- List foreign keys
SELECT conname, conrelid::regclass, confrelid::regclass
FROM pg_constraint
WHERE contype = 'f' AND conrelid = 'users'::regclass;

-- Active connections
SELECT pid, usename, datname, state, query
FROM pg_stat_activity
WHERE state = 'active';
```

### Backup and Restore

```bash
# Dump single database (SQL)
pg_dump -U postgres mydb > backup.sql

# Dump with compression
pg_dump -U postgres -Fc mydb > backup.dump

# Dump schema only
pg_dump -U postgres --schema-only mydb > schema.sql

# Dump data only
pg_dump -U postgres --data-only mydb > data.sql

# Dump single table
pg_dump -U postgres -t users mydb > users.sql

# Dump all databases
pg_dumpall -U postgres > all_databases.sql

# Restore from SQL dump
psql -U postgres mydb < backup.sql

# Restore from custom format
pg_restore -U postgres -d mydb backup.dump

# Restore single table
pg_restore -U postgres -d mydb -t users backup.dump
```

### Query Analysis

```sql
-- Execution plan
EXPLAIN SELECT * FROM users WHERE email = 'alice@example.com';

-- Execution plan with actual runtime stats
EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT)
  SELECT * FROM users WHERE email = 'alice@example.com';

-- Table statistics
SELECT relname, n_live_tup, n_dead_tup, last_vacuum, last_autovacuum
FROM pg_stat_user_tables;

-- Index usage statistics
SELECT indexrelname, idx_scan, idx_tup_read, idx_tup_fetch
FROM pg_stat_user_indexes
WHERE schemaname = 'public';

-- Update statistics
ANALYZE users;
ANALYZE;  -- all tables

-- Slow queries (requires pg_stat_statements extension)
SELECT query, calls, mean_exec_time, total_exec_time
FROM pg_stat_statements
ORDER BY mean_exec_time DESC
LIMIT 10;
```

## MySQL (mysql)

### Connection

```bash
# Connect
mysql -u root -p
mysql -u myuser -p mydb
mysql -h hostname -P 3306 -u myuser -p mydb

# Execute SQL directly
mysql -u root -p -e "SELECT * FROM users;" mydb

# Execute SQL file
mysql -u root -p mydb < queries.sql

# Output formatting
mysql -u root -p -N -B -e "SELECT count(*) FROM users;" mydb  # raw value
```

### Meta-Commands

```sql
SHOW DATABASES;
USE mydb;
SHOW TABLES;
SHOW TABLE STATUS;
DESCRIBE users;              -- column details
SHOW CREATE TABLE users;     -- full CREATE statement
SHOW INDEX FROM users;
SHOW PROCESSLIST;            -- active connections
SHOW VARIABLES LIKE '%max%'; -- server variables
SHOW STATUS LIKE 'Threads%'; -- server status
```

### Backup and Restore

```bash
# Dump single database
mysqldump -u root -p mydb > backup.sql

# Dump with compression
mysqldump -u root -p mydb | gzip > backup.sql.gz

# Dump schema only
mysqldump -u root -p --no-data mydb > schema.sql

# Dump specific tables
mysqldump -u root -p mydb users orders > tables.sql

# Dump all databases
mysqldump -u root -p --all-databases > all.sql

# Restore
mysql -u root -p mydb < backup.sql

# Restore compressed
gunzip < backup.sql.gz | mysql -u root -p mydb
```

### Query Analysis

```sql
-- Execution plan
EXPLAIN SELECT * FROM users WHERE email = 'alice@example.com';

-- Extended explain (JSON format, MySQL 5.7+)
EXPLAIN FORMAT=JSON SELECT * FROM users WHERE email = 'alice@example.com';

-- Analyze (update index statistics)
ANALYZE TABLE users;

-- Check table integrity
CHECK TABLE users;

-- Optimize table (reclaim space)
OPTIMIZE TABLE users;

-- Show index cardinality
SHOW INDEX FROM users;
```

## Common SQL Patterns

```sql
-- Pagination
SELECT * FROM items ORDER BY id LIMIT 20 OFFSET 40;

-- Count with grouping
SELECT status, COUNT(*) as cnt FROM orders GROUP BY status ORDER BY cnt DESC;

-- Join
SELECT u.name, o.total
FROM users u
JOIN orders o ON u.id = o.user_id
WHERE o.created_at > '2024-01-01';

-- Subquery
SELECT * FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 100);

-- CTE (Common Table Expression)
WITH active_users AS (
    SELECT * FROM users WHERE last_login > '2024-01-01'
)
SELECT * FROM active_users WHERE role = 'admin';

-- Window functions
SELECT name, department, salary,
       RANK() OVER (PARTITION BY department ORDER BY salary DESC) as rank
FROM employees;

-- Conditional aggregation
SELECT
    COUNT(*) as total,
    SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END) as active,
    SUM(CASE WHEN status = 'inactive' THEN 1 ELSE 0 END) as inactive
FROM users;
```

## Important Notes

- Always back up before destructive operations (DROP, DELETE, TRUNCATE, ALTER)
- Use transactions for multi-statement changes: `BEGIN; ... COMMIT;` (or `ROLLBACK;`)
- Prefer `EXPLAIN ANALYZE` over `EXPLAIN` to see actual vs estimated row counts
- For SQLite, use WAL mode (`PRAGMA journal_mode=WAL`) for concurrent read/write
- For PostgreSQL, use `\x` for wide tables to get vertical output
- For MySQL, add `\G` at the end of a query for vertical output
- Use `-t` (tuples only) and `-A` (unaligned) in psql for scriptable output
- Never store passwords in command-line arguments; use `.pgpass` (psql) or `.my.cnf` (mysql)
- Use parameterized queries in scripts to prevent SQL injection
