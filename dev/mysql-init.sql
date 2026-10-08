-- Test data: 200k orders, `status` deliberately unindexed so filtering on it is a full scan.
CREATE DATABASE shop;
USE shop;

CREATE TABLE orders (
  id          BIGINT AUTO_INCREMENT PRIMARY KEY,
  customer_id INT NOT NULL,
  status      VARCHAR(16) NOT NULL,
  amount      DECIMAL(10,2) NOT NULL,
  created_at  DATETIME NOT NULL,
  KEY idx_customer (customer_id)
);

SET SESSION cte_max_recursion_depth = 200000;
INSERT INTO orders (customer_id, status, amount, created_at)
WITH RECURSIVE seq (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 200000)
SELECT n % 5000, ELT(1 + n % 3, 'NEW', 'PAID', 'SHIPPED'), (n % 997) / 10, NOW() - INTERVAL (n % 90) DAY
FROM seq;

-- The monitoring account, with only what dbtop uses:
--   PROCESS                        SHOW FULL PROCESSLIST and INNODB_TRX for every user's threads
--   SELECT on performance_schema   statement digests, per-statement counters, lock waits
--   SELECT on shop.*               so EXPLAIN of a statement's text is allowed (EXPLAIN needs the
--                                  privileges of the statement it explains)
-- Deliberately NOT granted: any write privilege. That means EXPLAIN of INSERT/UPDATE/DELETE text
-- is refused, and EXPLAIN ... FOR CONNECTION on another account's thread is refused too (MySQL
-- bug #95850: it needs the explained connection's privileges, not only PROCESS), so dbtop falls
-- back to explaining the statement's text and labels that plan as an estimate.
CREATE USER 'dbtop'@'%' IDENTIFIED BY 'dbtop';
GRANT PROCESS ON *.* TO 'dbtop'@'%';
GRANT SELECT ON performance_schema.* TO 'dbtop'@'%';
GRANT SELECT ON shop.* TO 'dbtop'@'%';

-- A workload account for the tests: it runs the statements dbtop then observes.
CREATE USER 'app'@'%' IDENTIFIED BY 'app';
GRANT ALL ON shop.* TO 'app'@'%';

-- An account nobody else uses and that holds no PROCESS privilege, so it sees only its own thread:
-- dbtop must say that instead of showing what looks like an idle server.
CREATE USER 'blind'@'%' IDENTIFIED BY 'blind';
GRANT SELECT ON shop.* TO 'blind'@'%';
