FROM mcr.microsoft.com/devcontainers/base:ubuntu24.04

# PostgreSQL 18 CLIENT and SERVER from the PostgreSQL repo, plus the build
# dependencies Ruby needs. sqlite3 is the CLI for ad-hoc inspection of the
# benchmark databases (spec/design/benchmarks.md — the harnesses themselves
# use drivers, never the CLI).
#
# The SERVER package (initdb / postgres / pg_ctl) is here so the container can
# run a per-checkout cluster under .dev/postgres, the same way a native macOS
# worktree does — the convergence the native migration is built on. It does NOT
# start a service: Debian's postgresql-18 package auto-creates a "main" cluster
# and an init script, both of which we suppress, because the cluster this
# project uses is created by `mise run db:init` inside the checkout and supervised
# by process-compose. The `db` compose service remains the oracle for now.
RUN apt-get update \
    && apt-get install -y postgresql-common \
    && yes | /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh \
    && sed -i 's/^#*create_main_cluster.*/create_main_cluster = false/' \
        /etc/postgresql-common/createcluster.conf \
    && apt-get install -y postgresql-client-18 postgresql-18 sqlite3 \
        build-essential libssl-dev libreadline-dev zlib1g-dev libffi-dev \
        ncurses-dev libyaml-dev libxml2 libxml2-dev libxslt1-dev \
    && rm -rf /var/lib/apt/lists/*
