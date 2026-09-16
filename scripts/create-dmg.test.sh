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
# A failed assertion must not strand a volume mounted under $TMP.
cleanup_test() {
    local mp
    for mp in "$TMP"/mnt-*; do
        [ -d "$mp" ] || continue
        command hdiutil detach "$mp" -force >/dev/null 2>&1 || true
    done
    rm -rf "$TMP"
}
trap cleanup_test EXIT

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

# --- verify: first call fails (EAGAIN), second succeeds -------------------
VERIFY_CALLS=0
hdiutil() {
    if [ "${1:-}" = "verify" ]; then
        VERIFY_CALLS=$((VERIFY_CALLS + 1))
        if [ "$VERIFY_CALLS" -eq 1 ]; then
            echo "hdiutil: verify failed - Resource temporarily unavailable" >&2
            return 1
        fi
    fi
    command hdiutil "$@"
}
run_logged "$TMP/retry-ok.out" hdiutil_verify_with_retry "$VALID"
[ "$rc" -eq 0 ] || fail "verify did not recover after a first-attempt EAGAIN: $(cat "$TMP/retry-ok.out")"
[ "$VERIFY_CALLS" -eq 2 ] || fail "EAGAIN-then-success verify: expected 2 calls, got $VERIFY_CALLS"
grep -q "attempt 1/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/retry-ok.out" \
    || fail "EAGAIN-then-success verify missing attempt 1 log"
pass "verify recovers when the first call fails and the second succeeds"

# Restore the counting wrapper for later tests.
hdiutil() {
    if [ "${1:-}" = "verify" ]; then
        VERIFY_CALLS=$((VERIFY_CALLS + 1))
    fi
    command hdiutil "$@"
}

# --- create+verify pair, happy path (no signed .app) ----------------------
PAIR_DEST="$TMP/pair-valid.dmg"
VERIFY_CALLS=0
run_logged "$TMP/pair-ok.out" create_and_verify_dmg "$STAGE" "PaneFlowRetryTest" "$PAIR_DEST"
[ "$rc" -eq 0 ] || fail "create+verify of a tiny folder failed: $(cat "$TMP/pair-ok.out")"
[ -s "$PAIR_DEST" ] || fail "create+verify did not leave a verified image"
[ "$VERIFY_CALLS" -eq 1 ] || fail "create+verify happy path should verify once, got $VERIFY_CALLS"
pass "create+verify pair succeeds on a tiny unsigned folder"

# --- create+verify pair: first verify fails (EAGAIN), second succeeds -----
# Production uses create_and_verify_dmg, not the standalone verify helper.
# A first-attempt helper failure must recreate the image, not only retry
# verify against the leftover dest.
PAIR_RETRY="$TMP/pair-retry.dmg"
CREATE_CALLS=0
VERIFY_CALLS=0
hdiutil() {
    if [ "${1:-}" = "create" ]; then
        CREATE_CALLS=$((CREATE_CALLS + 1))
    fi
    if [ "${1:-}" = "verify" ]; then
        VERIFY_CALLS=$((VERIFY_CALLS + 1))
        if [ "$VERIFY_CALLS" -eq 1 ]; then
            echo "hdiutil: verify failed - Resource temporarily unavailable" >&2
            return 1
        fi
    fi
    command hdiutil "$@"
}
run_logged "$TMP/pair-retry.out" create_and_verify_dmg "$STAGE" "PaneFlowRetryTest" "$PAIR_RETRY"
[ "$rc" -eq 0 ] || fail "create+verify did not recover after a first-attempt EAGAIN: $(cat "$TMP/pair-retry.out")"
[ -s "$PAIR_RETRY" ] || fail "create+verify retry did not leave a verified image"
[ "$CREATE_CALLS" -eq 2 ] || fail "create+verify EAGAIN-then-success: expected 2 creates, got $CREATE_CALLS"
[ "$VERIFY_CALLS" -eq 2 ] || fail "create+verify EAGAIN-then-success: expected 2 verifies, got $VERIFY_CALLS"
grep -q "attempt 1/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/pair-retry.out" \
    || fail "create+verify EAGAIN-then-success missing attempt 1 log"
pass "create+verify pair recovers when the first verify fails and the second succeeds"

# Restore the counting wrapper for later tests.
hdiutil() {
    if [ "${1:-}" = "verify" ]; then
        VERIFY_CALLS=$((VERIFY_CALLS + 1))
    fi
    command hdiutil "$@"
}

# --- create+verify pair: planted non-image still fails, dest removed ------
PLANTED="$TMP/planted.dmg"
printf 'stale\n' > "$PLANTED"
ORIG_CREATE_UDZO_DMG="$(declare -f create_udzo_dmg)"
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

# Restore the real create_udzo_dmg for every later test, and prove it.
eval "$ORIG_CREATE_UDZO_DMG"
RESTORED="$TMP/restored.dmg"
create_udzo_dmg "$STAGE" "PaneFlowRetryTest" "$RESTORED"
command hdiutil verify "$RESTORED" >/dev/null 2>&1 \
    || fail "create_udzo_dmg was not restored after the planted-non-image block"
pass "create_udzo_dmg restored after the planted-non-image block"

# --- attach of a non-image: same retry budget, still fails ----------------
# attach's hdiutil invocation is inside $(...), so ATTACH_CALLS in this
# shell stays 0; the attempt log is the retry contract.
MP_GARBAGE="$TMP/mnt-garbage"
run_logged "$TMP/attach.out" hdiutil_attach_with_retry "$GARBAGE" "$MP_GARBAGE"
[ "$rc" -ne 0 ] || fail "attach of a non-image file exited 0"
attach_attempts="$(grep -c "hdiutil attach failed (attempt " "$TMP/attach.out" || true)"
[ "$attach_attempts" -eq "$HDIUTIL_RETRY_ATTEMPTS" ] \
    || fail "non-image attach: expected $HDIUTIL_RETRY_ATTEMPTS attempts, got $attach_attempts"
grep -q "attempt ${HDIUTIL_RETRY_ATTEMPTS}/${HDIUTIL_RETRY_ATTEMPTS}" "$TMP/attach.out" \
    || fail "non-image attach missing final-attempt log"
pass "non-image attach exhausts retries and fails (attempts=$attach_attempts)"

# --- attach: first call fails (EAGAIN), second succeeds -------------------
# attach captures stdout in a subshell, so the attempt counter must be a file.
ATTACH_N="$TMP/attach-n"
echo 0 > "$ATTACH_N"
hdiutil() {
    if [ "${1:-}" = "attach" ]; then
        n="$(cat "$ATTACH_N")"
        n=$((n + 1))
        echo "$n" > "$ATTACH_N"
        if [ "$n" -eq 1 ]; then
            echo "hdiutil: attach failed - Resource temporarily unavailable" >&2
            return 1
        fi
    fi
    command hdiutil "$@"
}
MP_VALID="$TMP/mnt-valid"
run_logged "$TMP/attach-retry.out" hdiutil_attach_with_retry "$VALID" "$MP_VALID"
[ "$rc" -eq 0 ] || fail "attach did not recover after a first-attempt EAGAIN: $(cat "$TMP/attach-retry.out")"
attach_calls="$(cat "$ATTACH_N")"
[ "$attach_calls" -eq 2 ] || fail "EAGAIN-then-success attach: expected 2 calls, got $attach_calls"
# The real attach must honour the explicit mountpoint, not /Volumes.
[ -e "$MP_VALID/README" ] || fail "image was not mounted at the explicit mountpoint $MP_VALID"
# hdiutil reports the resolved path (/private/var/...), so compare resolved.
echo "$_HDIUTIL_ATTACH_OUTPUT" | grep -qF "$(cd "$MP_VALID" && pwd -P)" \
    || fail "attach output does not name the explicit mountpoint: $_HDIUTIL_ATTACH_OUTPUT"
attach_dev="$(attach_output_device "$_HDIUTIL_ATTACH_OUTPUT")"
case "$attach_dev" in
    /dev/disk*) ;;
    *) fail "attach_output_device did not find a device in: $_HDIUTIL_ATTACH_OUTPUT" ;;
esac
command hdiutil detach "$attach_dev" -force >/dev/null 2>&1 || fail "detach of $attach_dev failed"
[ ! -e "$MP_VALID/README" ] || fail "image still mounted at $MP_VALID after detach"
pass "attach recovers when the first call fails and the second succeeds, at the explicit mountpoint"

# --- attach: a half-finished first attempt is detached before the retry ---
# Fully stubbed hdiutil: the first attach prints a device line and exits
# non-zero (attached but not mounted); the second succeeds. Every call is
# appended to a log so ordering can be asserted from the parent shell.
HDIUTIL_LOG="$TMP/hdiutil.log"
: > "$HDIUTIL_LOG"
echo 0 > "$ATTACH_N"
hdiutil() {
    echo "$*" >> "$HDIUTIL_LOG"
    case "${1:-}" in
        attach)
            n="$(cat "$ATTACH_N")"
            n=$((n + 1))
            echo "$n" > "$ATTACH_N"
            mp=""
            while [ "$#" -gt 0 ]; do
                if [ "$1" = "-mountpoint" ]; then mp="$2"; fi
                shift
            done
            if [ "$n" -eq 1 ]; then
                echo "/dev/disk98            GUID_partition_scheme"
                echo "hdiutil: attach failed - Resource temporarily unavailable" >&2
                return 1
            fi
            echo "/dev/disk99            GUID_partition_scheme"
            echo "/dev/disk99s1          Apple_HFS                      $mp"
            return 0
            ;;
        detach) return 0 ;;
        info) return 0 ;;
    esac
    return 0
}
MP_STUB="$TMP/mnt-stub"
run_logged "$TMP/attach-leftover.out" hdiutil_attach_with_retry "$VALID" "$MP_STUB"
[ "$rc" -eq 0 ] || fail "stubbed attach did not recover: $(cat "$TMP/attach-leftover.out")"
[ "$(cat "$ATTACH_N")" -eq 2 ] || fail "stubbed attach: expected 2 attach calls, got $(cat "$ATTACH_N")"
[ -d "$MP_STUB" ] || fail "attach did not create the explicit mountpoint $MP_STUB"
first_attach="$(grep -n '^attach ' "$HDIUTIL_LOG" | sed -n '1p' | cut -d: -f1)"
second_attach="$(grep -n '^attach ' "$HDIUTIL_LOG" | sed -n '2p' | cut -d: -f1)"
detach_line="$(grep -n '^detach /dev/disk98 -force' "$HDIUTIL_LOG" | sed -n '1p' | cut -d: -f1)"
[ -n "$detach_line" ] || fail "device from the failed attach was never detached: $(cat "$HDIUTIL_LOG")"
[ "$first_attach" -lt "$detach_line" ] && [ "$detach_line" -lt "$second_attach" ] \
    || fail "detach of /dev/disk98 did not happen between the two attach attempts: $(cat "$HDIUTIL_LOG")"
grep -q "^attach .*-mountpoint $MP_STUB " "$HDIUTIL_LOG" \
    || fail "attach was not given the temp mountpoint: $(cat "$HDIUTIL_LOG")"
grep -q "^attach .*-mountpoint /Volumes" "$HDIUTIL_LOG" \
    && fail "attach used a /Volumes mountpoint: $(cat "$HDIUTIL_LOG")"
echo "$_HDIUTIL_ATTACH_OUTPUT" | grep -qF "/dev/disk99s1 " \
    || fail "attach output is not the second attempt's: $_HDIUTIL_ATTACH_OUTPUT"
echo "$_HDIUTIL_ATTACH_OUTPUT" | grep -qF "$MP_STUB" \
    || fail "attach output does not carry the temp mountpoint: $_HDIUTIL_ATTACH_OUTPUT"
grep -q "Detaching leftover attachment /dev/disk98" "$TMP/attach-leftover.out" \
    || fail "leftover detach was not logged: $(cat "$TMP/attach-leftover.out")"
pass "a device left by a failed attach is detached before the retry, at the temp mountpoint"

# --- cleanup trap: detaches the mounted verification volume ---------------
# shellcheck disable=SC2034  # read by cleanup_create_dmg
VERIFY_PT="$MP_STUB"
MOUNT_ROOT="$TMP/mount-root"
mkdir -p "$MOUNT_ROOT"
STAGING="$TMP/staging-root"
mkdir -p "$STAGING"
: > "$HDIUTIL_LOG"
cleanup_create_dmg
grep -q "^detach /dev/disk99 -force" "$HDIUTIL_LOG" \
    || fail "cleanup did not detach the attached device: $(cat "$HDIUTIL_LOG")"
[ ! -e "$MOUNT_ROOT" ] || fail "cleanup left the temp mount root $MOUNT_ROOT"
[ ! -e "$STAGING" ] || fail "cleanup left the staging dir $STAGING"
: > "$HDIUTIL_LOG"
_HDIUTIL_ATTACH_OUTPUT=""
cleanup_create_dmg
grep -q "^detach" "$HDIUTIL_LOG" && fail "cleanup detached with no attach output: $(cat "$HDIUTIL_LOG")"
unset VERIFY_PT MOUNT_ROOT STAGING
pass "cleanup trap detaches the verification volume and removes temp dirs"

echo "All tests passed."
