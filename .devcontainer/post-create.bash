#!/bin/bash
set -e

mise trust
mise install
eval "$(mise env -s bash)"
go install golang.org/x/tools/cmd/goimports@latest

# Run any additional setup scripts included on the shared volume. This is to allow for per developer or
# per-environment customizations. These scripts are not checked into source control.
if [ -x "/persist/shared/devcontainer/install" ]; then
  /persist/shared/devcontainer/install
fi

# Create a symlink to the shared scratch directory (on the persistent shared volume) for temporary files.
if [ ! -e .scratch ] && [ ! -L .scratch ]; then
  ln -s /persist/shared/scratch .scratch
fi
