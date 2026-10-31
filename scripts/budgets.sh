#!/usr/bin/env bash
# Run the large-file budget tests in release mode and print measured values
# against their limits (docs/design/large-file-engine.md, section 3).
#
#   scripts/budgets.sh            report; exits 0 (red "pending" budgets are expected)
#   scripts/budgets.sh --strict   exit 1 if any budget test fails
#
# Tests marked `ignore = "pending: ..."` are budgets the design requires and
# the code does not meet yet. When the work lands, remove that `ignore` in
# the same commit so the budget becomes a regression guard.
set -u
cd "$(dirname "$0")/.."

out=$(cargo test -p csv_model --release --test budgets -- --include-ignored --nocapture --test-threads=1 2>&1)
status=$?

# With --nocapture a test's first output shares a line with "test NAME ...",
# so extract from anywhere in the line.
echo "$out" | grep -oE 'BUDGET .*$' | sed 's/^BUDGET //'
echo

all=$(echo "$out" | grep -oE '^test [A-Za-z0-9_]+ \.\.\.' | awk '{print $2}' | sort -u)
failed=$(echo "$out" | awk '/^failures:$/ {f=1; next} /^test result/ {f=0} f && /^    [A-Za-z0-9_]+$/ {print $1}' | sort -u)
for t in $all; do
  if echo "$failed" | grep -qx "$t"; then echo "FAIL    $t"; else echo "PASS    $t"; fi
done
echo
echo "$out" | grep -E '^test result' | tail -1

if [ "${1:-}" = "--strict" ] && [ $status -ne 0 ]; then exit 1; fi
exit 0
