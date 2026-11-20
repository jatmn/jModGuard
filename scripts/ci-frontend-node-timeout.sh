#!/usr/bin/env bash
# Node 20/22 apply the CLI timeout to the whole file. See docs/CI.md.
# shellcheck disable=SC2034  # Read by preflight and its self-test.
readonly frontend_node_test_timeout_min_major=24
