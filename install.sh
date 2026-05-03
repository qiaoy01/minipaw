#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${MINIPAW_HOME:-"$HOME/.minipaw"}"
MEMORY_DIR="$INSTALL_DIR/memory"
USER_WORKSPACE_DIR="$INSTALL_DIR/workspace"
BIN_PATH="$INSTALL_DIR/minipaw"
BASHRC="$HOME/.bashrc"
FEATURES="${MINIPAW_FEATURES-sqlite}"

mkdir -p "$INSTALL_DIR" "$MEMORY_DIR" "$USER_WORKSPACE_DIR" "$INSTALL_DIR/skills"

cd "$ROOT_DIR"
if [ -n "$FEATURES" ]; then
    if cargo build --release --features "$FEATURES"; then
        :
    else
        echo "warning: sqlite-enabled build failed; retrying default release build" >&2
        cargo build --release
    fi
else
    cargo build --release
fi

cp "$ROOT_DIR/target/release/minipaw" "$BIN_PATH"
chmod 755 "$BIN_PATH"

cp "$ROOT_DIR/SOUL.md" "$INSTALL_DIR/SOUL.md"
if [ -d "$ROOT_DIR/skills" ]; then
    cp -R "$ROOT_DIR/skills/." "$INSTALL_DIR/skills/"
fi

if [ ! -f "$INSTALL_DIR/minipaw.json" ]; then
    printf '%s\n' \
        '{' \
        '  "agents": {' \
        '    "primary": {' \
        '      "provider": "llamacpp",' \
        '      "url": "http://127.0.0.1:8080/v1",' \
        '      "model": "local-model"' \
        '    }' \
        '  }' \
        '}' > "$INSTALL_DIR/minipaw.json"
fi

SQLITE_PATH="$MEMORY_DIR/minipaw.sqlite3"
if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$SQLITE_PATH" '
CREATE TABLE IF NOT EXISTS tasks(
    id INTEGER PRIMARY KEY,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    title TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS messages(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    role TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tool_runs(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    tool TEXT NOT NULL,
    ok INTEGER NOT NULL,
    output TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS facts(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);'
else
    MINIPAW_HOME="$INSTALL_DIR" MINIPAW_WORKSPACE="$INSTALL_DIR" MINIPAW_SQLITE_PATH="$SQLITE_PATH" "$BIN_PATH" config check >/dev/null || true
    if [ ! -f "$SQLITE_PATH" ]; then
        : > "$SQLITE_PATH"
        echo "warning: sqlite3 command not found; created placeholder $SQLITE_PATH" >&2
    fi
fi

mkdir -p "$(dirname "$BASHRC")"
touch "$BASHRC"
if ! grep -Fq "# minipaw managed block" "$BASHRC"; then
    {
        printf '\n# minipaw managed block\n'
        printf 'export MINIPAW_HOME="%s"\n' "$INSTALL_DIR"
        printf 'export MINIPAW_WORKSPACE="$MINIPAW_HOME"\n'
        printf 'export MINIPAW_SQLITE_PATH="$MINIPAW_HOME/memory/minipaw.sqlite3"\n'
        printf 'case ":$PATH:" in *":$MINIPAW_HOME:"*) ;; *) export PATH="$MINIPAW_HOME:$PATH" ;; esac\n'
        printf '# end minipaw managed block\n'
    } >> "$BASHRC"
fi

# Source for the rest of this installer process. If the script was sourced by
# the caller, these exports also remain in that shell.
# shellcheck disable=SC1090
set +e +u
source "$BASHRC"
SOURCE_STATUS=$?
set -euo pipefail
if [ "$SOURCE_STATUS" -ne 0 ]; then
    echo "warning: sourcing $BASHRC returned status $SOURCE_STATUS" >&2
fi

echo "minipaw installed at $INSTALL_DIR"
echo "binary: $BIN_PATH"
echo "memory: $SQLITE_PATH"
echo "workspace: $USER_WORKSPACE_DIR"

if [ -t 0 ]; then
    MINIPAW_HOME="$INSTALL_DIR" MINIPAW_WORKSPACE="$INSTALL_DIR" MINIPAW_SQLITE_PATH="$SQLITE_PATH" "$BIN_PATH" onboarding
else
    echo "stdin is not interactive; run 'minipaw onboarding' to configure model and channel"
fi
