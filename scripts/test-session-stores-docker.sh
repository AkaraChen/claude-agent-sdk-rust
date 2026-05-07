#!/usr/bin/env bash
# Run Redis/Postgres session store live tests against disposable Docker containers.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_DIR"

usage() {
    cat <<'USAGE'
Usage: scripts/test-session-stores-docker.sh [redis|postgres|all]

Commands:
  redis     Start Redis in Docker and run tests/redis_session_store.rs
  postgres  Start Postgres in Docker and run tests/postgres_session_store.rs
  all       Start both stores and run both live test files

Optional image overrides:
  SESSION_STORE_REDIS_IMAGE=redis:7-alpine
  SESSION_STORE_POSTGRES_IMAGE=postgres:16-alpine
USAGE
}

TARGET="${1:-all}"
case "$TARGET" in
    redis | postgres | all) ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 1
        ;;
esac

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required to run live session store tests" >&2
    exit 1
fi

if ! docker info >/dev/null 2>&1; then
    echo "docker is not running or is not reachable" >&2
    exit 1
fi

RUN_ID="cas-rust-session-store-$(date +%s)-$$"
LABEL_KEY="com.claude-agent-sdk-rust.session-store-test"
LABEL="${LABEL_KEY}=${RUN_ID}"

REDIS_IMAGE="${SESSION_STORE_REDIS_IMAGE:-redis:7-alpine}"
POSTGRES_IMAGE="${SESSION_STORE_POSTGRES_IMAGE:-postgres:16-alpine}"
POSTGRES_USER="claude"
POSTGRES_PASSWORD="claude"
POSTGRES_DB="claude_sdk"

REDIS_CONTAINER=""
POSTGRES_CONTAINER=""

cleanup() {
    local status=$?
    set +e

    local ids
    ids="$(docker ps -aq --filter "label=${LABEL}" 2>/dev/null || true)"
    if [ -n "$ids" ]; then
        echo ""
        echo "Cleaning up Docker containers..."
        docker rm -f $ids >/dev/null 2>&1 || true
    fi

    exit "$status"
}

trap cleanup EXIT INT TERM

host_port() {
    local container="$1"
    local private_port="$2"

    docker port "$container" "$private_port/tcp" \
        | tail -n 1 \
        | sed -E 's/.*:([0-9]+)$/\1/'
}

wait_for_redis() {
    local attempt
    for attempt in $(seq 1 60); do
        if docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q '^PONG$'; then
            return 0
        fi
        sleep 1
    done

    echo "Redis did not become ready" >&2
    docker logs "$REDIS_CONTAINER" >&2 || true
    return 1
}

wait_for_postgres() {
    local attempt
    for attempt in $(seq 1 60); do
        if docker exec "$POSTGRES_CONTAINER" \
            pg_isready -U "$POSTGRES_USER" -d "$POSTGRES_DB" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done

    echo "Postgres did not become ready" >&2
    docker logs "$POSTGRES_CONTAINER" >&2 || true
    return 1
}

start_redis() {
    REDIS_CONTAINER="${RUN_ID}-redis"
    echo "Starting Redis container ${REDIS_CONTAINER}..."
    docker run -d --rm \
        --name "$REDIS_CONTAINER" \
        --label "$LABEL" \
        -p "127.0.0.1::6379" \
        "$REDIS_IMAGE" >/dev/null

    wait_for_redis

    local port
    port="$(host_port "$REDIS_CONTAINER" 6379)"
    if [ -z "$port" ]; then
        echo "Could not determine Redis host port" >&2
        return 1
    fi

    export SESSION_STORE_REDIS_URL="redis://127.0.0.1:${port}/"
    echo "Redis ready at ${SESSION_STORE_REDIS_URL}"
}

start_postgres() {
    POSTGRES_CONTAINER="${RUN_ID}-postgres"
    echo "Starting Postgres container ${POSTGRES_CONTAINER}..."
    docker run -d --rm \
        --name "$POSTGRES_CONTAINER" \
        --label "$LABEL" \
        -e "POSTGRES_USER=${POSTGRES_USER}" \
        -e "POSTGRES_PASSWORD=${POSTGRES_PASSWORD}" \
        -e "POSTGRES_DB=${POSTGRES_DB}" \
        -p "127.0.0.1::5432" \
        "$POSTGRES_IMAGE" >/dev/null

    wait_for_postgres

    local port
    port="$(host_port "$POSTGRES_CONTAINER" 5432)"
    if [ -z "$port" ]; then
        echo "Could not determine Postgres host port" >&2
        return 1
    fi

    export SESSION_STORE_POSTGRES_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@127.0.0.1:${port}/${POSTGRES_DB}"
    echo "Postgres ready at ${SESSION_STORE_POSTGRES_URL}"
}

run_redis_tests() {
    echo ""
    echo "Running Redis session store tests..."
    cargo test --test redis_session_store -- --nocapture
}

run_postgres_tests() {
    echo ""
    echo "Running Postgres session store tests..."
    cargo test --test postgres_session_store -- --nocapture
}

case "$TARGET" in
    redis)
        start_redis
        run_redis_tests
        ;;
    postgres)
        start_postgres
        run_postgres_tests
        ;;
    all)
        start_redis
        start_postgres
        run_redis_tests
        run_postgres_tests
        ;;
esac

echo ""
echo "Session store Docker tests passed."
