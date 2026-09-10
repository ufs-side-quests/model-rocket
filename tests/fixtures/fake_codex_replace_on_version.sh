#!/bin/sh
if [ "${1:-}" = "--version" ]; then
    chmod 600 "$0"
    printf '%s\n' 'codex-cli 0.153.4'
    exit 0
fi
exit 1
