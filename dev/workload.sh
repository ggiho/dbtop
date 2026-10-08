#!/bin/sh
# Keeps statements running on the dev databases so dbtop has something to show: slow scans,
# a lock wait, and repeated statements for the slow list. Ctrl-C stops it.
#
#   docker compose -f dev/compose.yaml up -d --wait
#   dev/workload.sh                                  # in one terminal
#   cargo run -- --config dev/config.toml local-mysql   # in another (or local-pg)
cd "$(dirname "$0")" || exit 1
mysql_app() { docker compose -f compose.yaml exec -T mysql mysql -uapp -papp "$@" >/dev/null 2>&1; }
pg_app() { docker compose -f compose.yaml exec -T -e PGPASSWORD=app postgres psql -h 127.0.0.1 -U app -d shop -X -q "$@" >/dev/null 2>&1; }
# Stop our own jobs and leave. (A bare `kill 0` would also hit whatever started this script, and
# without `exit` the loop would carry on.)
trap 'kill $(jobs -p) 2>/dev/null; exit 0' INT TERM

while true; do
  mysql_app -e "SELECT SLEEP(12) FROM shop.orders WHERE status = 'NEW' LIMIT 1" &
  mysql_app -e "SELECT COUNT(*) FROM shop.orders WHERE status = 'PAID' AND amount > 1.5" &
  # a lock wait: one transaction holds a row, another waits for it
  mysql_app -e "BEGIN; UPDATE shop.orders SET amount = amount WHERE id = 1; SELECT SLEEP(10); ROLLBACK" &
  (sleep 1; mysql_app -e "UPDATE shop.orders SET amount = amount + 1 WHERE id = 1") &

  pg_app -c "SELECT pg_sleep(12) FROM orders WHERE status = 'NEW' LIMIT 1" &
  pg_app -c "SELECT count(*) FROM orders WHERE status = 'PAID' AND amount > 1.5" &
  pg_app -c "BEGIN; UPDATE orders SET amount = amount WHERE id = 1; SELECT pg_sleep(10); ROLLBACK" &
  (sleep 1; pg_app -c "UPDATE orders SET amount = amount + 1 WHERE id = 1") &
  # `wait` returns at once when a signal arrives; a foreground sleep would delay the trap.
  sleep 14 &
  wait $!
done
