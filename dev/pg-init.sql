-- Test data: 200k orders, `status` deliberately unindexed so filtering on it is a sequential scan.
CREATE EXTENSION pg_stat_statements;

-- The monitoring account. pg_monitor = pg_read_all_stats + pg_read_all_settings + pg_stat_scan_tables:
-- without pg_read_all_stats other roles' query text shows as "<insufficient privilege>".
CREATE ROLE dbtop LOGIN PASSWORD 'dbtop' IN ROLE pg_monitor;

-- A role that may NOT see other roles' sessions (no pg_read_all_stats): dbtop must say so.
CREATE ROLE plain LOGIN PASSWORD 'plain';

-- A workload account for the tests: it runs the statements dbtop then observes.
CREATE ROLE app LOGIN PASSWORD 'app';

CREATE DATABASE shop OWNER app;
\connect shop

CREATE EXTENSION pg_stat_statements;

SET ROLE app;
CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  customer_id int NOT NULL,
  status      text NOT NULL,
  amount      numeric(10,2) NOT NULL,
  created_at  timestamptz NOT NULL
);
CREATE INDEX idx_orders_customer ON orders (customer_id);
INSERT INTO orders (customer_id, status, amount, created_at)
SELECT n % 5000, (ARRAY['NEW','PAID','SHIPPED'])[1 + n % 3], (n % 997) / 10.0, now() - (n % 90) * interval '1 day'
FROM generate_series(1, 200000) AS n;
ANALYZE orders;
RESET ROLE;

-- EXPLAIN needs the privileges of the statement it explains.
GRANT SELECT ON ALL TABLES IN SCHEMA public TO dbtop;
