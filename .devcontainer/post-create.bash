#!/bin/bash
set -e

mise trust
mise install
eval "$(mise env -s bash)"
go install golang.org/x/tools/cmd/goimports@latest

# Browser e2e (Playwright): two suites need npm dev deps (vite + @playwright/test), a Chromium build,
# and the OS libraries Chromium links (NSS / X / fonts):
#   - impl/ts — the TS core's Browser/OPFS host e2e (spec/design/hosts.md §5), `npm run test:browser`.
#   - web     — the jed website's Playwright suite (CLAUDE.md §10), also `npm run test:browser`; it runs
#               the TS core in a browser Web Worker, so its browser e2e is the interactive-feature
#               contract. Install both so each works out of the box.
# The browser binary lands under PLAYWRIGHT_BROWSERS_PATH (devcontainer.json) on the shared volume, so it
# persists across rebuilds and re-downloads only when missing — the second `playwright install` is
# normally a redundant no-op; --with-deps adds the system libs via apt (passwordless sudo here) into the
# container filesystem, which does NOT persist, so it reinstalls each create. Best-effort: a network
# hiccup must not block engine-only work (the engine itself runs on bare Node with no install), so a
# failure warns rather than aborting the create.
for dir in impl/ts web; do
  (
    cd "$dir" \
      && npm install \
      && npx playwright install --with-deps chromium
  ) || echo "WARNING: browser e2e tooling not fully installed for $dir; run 'cd $dir && npm install && npx playwright install --with-deps chromium' before 'npm run test:browser'."
done

# Run any additional setup scripts included on the shared volume. This is to allow for per developer or
# per-environment customizations. These scripts are not checked into source control.
if [ -x "/persist/shared/devcontainer/install" ]; then
  /persist/shared/devcontainer/install
fi

# Create a symlink to the shared scratch directory (on the persistent shared volume) for temporary files.
if [ ! -e .scratch ] && [ ! -L .scratch ]; then
  ln -s /persist/shared/scratch .scratch
fi
