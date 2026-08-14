#!/bin/sh
set -eu

payload=$(cat <<EOF
{
  "connector.class": "io.debezium.connector.postgresql.PostgresConnector",
    "database.hostname": "${DEBEZIUM_DATABASE_HOSTNAME:-postgres}",
    "database.port": "${DEBEZIUM_DATABASE_PORT:-5432}",
    "database.user": "${DEBEZIUM_DATABASE_USER:-prompt}",
    "database.password": "${DEBEZIUM_DATABASE_PASSWORD:-prompt}",
    "database.dbname": "${DEBEZIUM_DATABASE_DBNAME:-prompt_analyzer}",
    "topic.prefix": "${DEBEZIUM_TOPIC_PREFIX:-promptdb}",
    "plugin.name": "pgoutput",
    "slot.name": "prompt_analyzer_slot",
    "publication.autocreate.mode": "filtered",
    "table.include.list": "public.prompt_runs,public.evaluations",
    "snapshot.mode": "initial",
  "tombstones.on.delete": "false"
}
EOF
)

code=$(curl -sS -o /tmp/connector-response -w '%{http_code}' \
  -X PUT -H 'Content-Type: application/json' \
  --data "$payload" \
  http://connect:8083/connectors/prompt-postgres-cdc/config)

test "$code" -ge 200 -a "$code" -lt 300 || { cat /tmp/connector-response; exit 1; }
cat /tmp/connector-response
