#!/usr/bin/env bash
# GPU-free tests for scripts/create-dmg.sh retry/verify helpers (issue #547).
# No signed .app and no notarization: truncated/non-image files must still
# fail after the bounded retry, and a tiny valid image must still verify.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"

command -v hdiutil >/dev/null 2>&1 || {
    echo "FAIL: hdiutil not found (these tests run on macOS)" >&2
    exit 1
}

CREATE_DMG_LIB=1
# shellcheck source=create-dmg.sh
. "$SCRIPT_DIR/create-dmg.sh"

HDIUTIL_RETRY_ATTEMPTS=3
HDIUTIL_RETRY_SLEEP_SEC=0

TMP="$(mktemp -d "${TMPDIR:-/tmp}/create-dmg-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

VERIFY_CALLS=0
hdiutil() {
    if [ "${1:-}" = "verify" ]; then
        VERIFY_CALLS=$((VERIFY_CALLS + 1))
    fi
    command hdiutil "$@"
}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

pass() {
    echo "ok - $*"
}

run_logged() {
    local log="$1"
    shift
    rc=0
    "$@" >"$log" 2>&1 || rc=$?
}

# --- non-image: verify still fails after exhausting retries ---------------
GARBAGE="$TMP/not-an-image.dmg"
printf 'this is not a disk image\n' > "$GARBAGE"
VERIFY_CALLS=0
run_logged "$TMP/garbage.out" hdiutil_verify_with_retry "$GARBAGE"
[ "$rc" -ne 0 ] || fail "verify of a non-image file exited 0"
[ "$VERIFY_CALLS" -eq "$HDIUTIL_RETRY_ATTEMPTS" ] \
    || fail "non-image verify: expected $HDIUTIL_RETRY_ATTEMPTS attempts, got $VERIFY_CALLS"
grep -q "attempt 1/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/garbage.out" \
    || fail "non-image verify missing attempt 1 log"
grep -q "attempt ${HDIUTIL_RETRY_ATTEMPTS}/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/garbage.out" \
    || fail "non-image verify missing final-attempt log"
pass "non-image verify exhausts retries and fails (calls=$VERIFY_CALLS)"

# --- truncated UDZO: same contract ----------------------------------------
STAGE="$TMP/stage"
mkdir -p "$STAGE"
printf 'hello\n' > "$STAGE/README"
VALID="$TMP/valid.dmg"
create_udzo_dmg "$STAGE" "PaneFlowRetryTest" "$VALID"
[ -s "$VALID" ] || fail "tiny UDZO image was not created"

VERIFY_CALLS=0
verify_dmg "$VALID" >/dev/null 2>&1 || fail "valid tiny DMG failed hdiutil verify"
[ "$VERIFY_CALLS" -eq 1 ] || fail "valid verify should be one call, got $VERIFY_CALLS"

TRUNC="$TMP/truncated.dmg"
head -c 64 "$VALID" > "$TRUNC"
VERIFY_CALLS=0
run_logged "$TMP/trunc.out" hdiutil_verify_with_retry "$TRUNC"
[ "$rc" -ne 0 ] || fail "verify of a truncated image exited 0"
[ "$VERIFY_CALLS" -eq "$HDIUTIL_RETRY_ATTEMPTS" ] \
    || fail "truncated verify: expected $HDIUTIL_RETRY_ATTEMPTS attempts, got $VERIFY_CALLS"
grep -q "attempt ${HDIUTIL_RETRY_ATTEMPTS}/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/trunc.out" \
    || fail "truncated verify did not log exhausting retries"
pass "truncated image verify exhausts retries and fails (calls=$VERIFY_CALLS)"

# --- valid image: first-try success, no retry -----------------------------
VERIFY_CALLS=0
run_logged "$TMP/valid.out" hdiutil_verify_with_retry "$VALID"
[ "$rc" -eq 0 ] || fail "valid image verify failed: $(cat "$TMP/valid.out")"
[ "$VERIFY_CALLS" -eq 1 ] || fail "valid image should verify once, got $VERIFY_CALLS"
grep -q "attempt " "$TMP/valid.out" && fail "valid image should not log retries: $(cat "$TMP/valid.out")"
pass "valid image verifies on the first attempt"

# --- create+verify pair, happy path (no signed .app) ----------------------
PAIR_DEST="$TMP/pair-valid.dmg"
VERIFY_CALLS=0
run_logged "$TMP/pair-ok.out" create_and_verify_dmg "$STAGE" "PaneFlowRetryTest" "$PAIR_DEST"
[ "$rc" -eq 0 ] || fail "create+verify of a tiny folder failed: $(cat "$TMP/pair-ok.out")"
[ -s "$PAIR_DEST" ] || fail "create+verify did not leave a verified image"
[ "$VERIFY_CALLS" -eq 1 ] || fail "create+verify happy path should verify once, got $VERIFY_CALLS"
pass "create+verify pair succeeds on a tiny unsigned folder"

# --- create+verify pair: planted non-image still fails, dest removed ------
PLANTED="$TMP/planted.dmg"
printf 'stale\n' > "$PLANTED"
create_udzo_dmg() { printf 'not a disk image\n' > "$3"; }
VERIFY_CALLS=0
run_logged "$TMP/planted.out" create_and_verify_dmg "$STAGE" "PaneFlowRetryTest" "$PLANTED"
[ "$rc" -ne 0 ] || fail "create+verify pair succeeded on a planted non-image"
[ ! -e "$PLANTED" ] || fail "partial DMG left at $PLANTED after failed retries"
[ "$VERIFY_CALLS" -eq "$HDIUTIL_RETRY_ATTEMPTS" ] \
    || fail "planted non-image: expected $HDIUTIL_RETRY_ATTEMPTS verify attempts, got $VERIFY_CALLS"
grep -q "attempt ${HDIUTIL_RETRY_ATTEMPTS}/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/planted.out" \
    || fail "create+verify pair did not log exhausting retries"
pass "create+verify pair refuses a corrupt image and removes the partial"

# --- attach of a non-image: same retry budget, still fails ----------------
# attach's hdiutil invocation is inside $(...), so ATTACH_CALLS in this
# shell stays 0; the attempt log is the retry contract.
run_logged "$TMP/attach.out" hdiutil_attach_with_retry "$GARBAGE"
[ "$rc" -ne 0 ] || fail "attach of a non-image file exited 0"
attach_attempts="$(grep -c "hdiutil attach failed (attempt " "$TMP/attach.out" || true)"
[ "$attach_attempts" -eq "$HDIUTIL_RETRY_ATTEMPTS" ] \
    || fail "non-image attach: expected $HDIUTIL_RETRY_ATTEMPTS attempts, got $attach_attempts"
grep -q "attempt ${HDIUTIL_RETRY_ATTEMPTS}/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/attach.out" \
    || fail "non-image attach missing final-attempt log"
pass "non-image attach exhausts retries and fails (attempts=$attach_attempts)"

echo "All tests passed."
