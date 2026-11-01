#!/usr/bin/env bash
# Undo-stack memory on a 5 GB file (a virtual file: rows are computed on read,
# so no disk is needed). Prints a table of undo-stack and process memory per
# operation for a 1M-row and a 62.5M-row file, and checks they agree.
#
#   scripts/big.sh             the 5 GB test
#   scripts/big.sh --pending   also run the tests marked "pending"
#
# Needs about 1.5 GB of RAM; about a minute after the release build (indexing
# 5 GB and synthesising its bytes is most of it).
set -u
cd "$(dirname "$0")/.."
filter=undo_stack_memory_on_a_five_gigabyte_file
[ "${1:-}" = "--pending" ] && filter=""
cargo test --release --bin jonathan "${filter}" -- --include-ignored --nocapture --test-threads=1 2>&1 \
  | grep -E "UNDO-MEM|^test |test result|panicked|assertion"
