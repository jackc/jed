FROM mcr.microsoft.com/devcontainers/base:ubuntu24.04

# Get the PostgreSQL 18 client from the PostgreSQL repo, plus the build
# dependencies Ruby needs.
RUN apt-get update \
    && apt-get install -y postgresql-common \
    && yes | /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh \
    && apt-get install -y postgresql-client-18 \
        build-essential libssl-dev libreadline-dev zlib1g-dev libffi-dev \
        ncurses-dev libyaml-dev libxml2 libxml2-dev libxslt1-dev \
    && rm -rf /var/lib/apt/lists/*
