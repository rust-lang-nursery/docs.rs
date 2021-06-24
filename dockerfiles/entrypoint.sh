#!/usr/bin/env bash

set -euv

export DOCSRS_PREFIX=/opt/docsrs/prefix
export DOCSRS_DOCKER=true
export DOCSRS_LOG=${DOCSRS_LOG-"docs-rs,rustwide=info"}
export PATH="$PATH:/build/target/release"

# Try migrating the database multiple times if it fails
# This avoids the docker container crashing the first time it's started with
# docker-compose, as PostgreSQL needs some time to initialize.
set +e
failed=0
while true; do
    if ! docsrs database migrate; then
        ((failed=failed + 1))
        if [ "${failed}" -eq 5 ]; then
            exit 1
        fi
        echo "failed to migrate the database"
        echo "waiting 1 second..."
        sleep 1
    else
        break
    fi
done
set -e

if ! [ -d "${DOCSRS_PREFIX}/crates.io-index/.git" ]; then
    git clone https://github.com/rust-lang/crates.io-index "${DOCSRS_PREFIX}/crates.io-index"
    # Prevent new crates built before the container creation to be built
    git --git-dir="$DOCSRS_PREFIX/crates.io-index/.git" branch crates-index-diff_last-seen
fi

docsrs build update-toolchain --only-first-time

docsrs "$@"
