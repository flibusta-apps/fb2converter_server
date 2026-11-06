#! /usr/bin/env sh

# Docker's tini entrypoint reaps orphaned converter helper processes.
exec /usr/local/bin/fb2converter_server
