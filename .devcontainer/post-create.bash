#!/bin/bash
set -e

mise trust
mise install
eval "$(mise env -s bash)"
go install golang.org/x/tools/cmd/goimports@latest

# Browser/OPFS host e2e (spec/design/hosts.md §5): the TS core's `npm run test:browser` (Vite +
# Playwright) needs its dev deps (vite + @playwright/test), a Chromium build, and the OS libraries
# Chromium links (NSS / X / fonts). Install all three so the browser e2e works out of the box. The
# browser binary lands under PLAYWRIGHT_BROWSERS_PATH (devcontainer.json) on the shared volume, so it
# persists across rebuilds and re-downloads only when missing; --with-deps adds the system libs via apt
# (passwordless sudo here) into the container filesystem, which does NOT persist, so it reinstalls each
# create. Best-effort: a network hiccup must not block engine-only work (the engine itself runs on bare
# Node with no install), so a failure warns rather than aborting the create.
(
  cd impl/ts \
    && npm install \
    && npx playwright install --with-deps chromium
) || echo "WARNING: browser/OPFS e2e tooling not fully installed (spec/design/hosts.md §5); run 'cd impl/ts && npm install && npx playwright install --with-deps chromium' before 'npm run test:browser'."

# Run any additional setup scripts included on the shared volume. This is to allow for per developer or
# per-environment customizations. These scripts are not checked into source control.
if [ -x "/persist/shared/devcontainer/install" ]; then
  /persist/shared/devcontainer/install
fi

# Create a symlink to the shared scratch directory (on the persistent shared volume) for temporary files.
if [ ! -e .scratch ] && [ ! -L .scratch ]; then
  ln -s /persist/shared/scratch .scratch
fi
