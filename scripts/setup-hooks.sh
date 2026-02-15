#!/bin/sh
# Configure git to use .githooks for pre-push checks
git config core.hooksPath .githooks
echo "Git hooks configured. Pre-push will run: fmt, clippy, test."
