#!/bin/sh
# Starts a deliberately runaway process named "runaway-job" that pins one CPU core, so there is
# something for the assistant to find and stop. Stop it yourself with: pkill -f runaway-job
set -eu
nohup node -e 'process.title = "runaway-job"; for (;;) {}' > /dev/null 2>&1 &
echo "runaway-job started (pid $!)"
