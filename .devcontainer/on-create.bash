#!/bin/bash
set -e

sudo chown vscode:vscode /persist/local /persist/shared
mkdir -p /persist/shared/{claude,codex,atuin/{config,data},mise/{data,cache},psql,devcontainer-downloads}
