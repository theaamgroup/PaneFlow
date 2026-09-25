#!/usr/bin/env bash
# GPU-free tests for scripts/create-dmg.sh (issue #547): the retry/verify
# helpers against the real hdiutil, then create_dmg_main end-to-end against
# a fake .app with stubbed hdiutil/codesign/xcrun/spctl on PATH.
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
# Tiny base so the exponential backoff (0.01, 0.02, ...) keeps the suite fast.
HDIUTIL_RETRY_SLEEP_SEC=0.01
HDIUTIL_RETRY_SLEEP_MAX_SEC=60

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

# --- backoff schedule: base doubles per attempt, capped ---------------------
# The pure helper with the production defaults (base 2, cap 60).
sched=""
for a in 1 2 3 4 5 6; do
    sched="$sched$(HDIUTIL_RETRY_SLEEP_SEC=2 HDIUTIL_RETRY_SLEEP_MAX_SEC=60 retry_sleep_seconds "$a") "
done
[ "$sched" = "2 4 8 16 32 60 " ] || fail "backoff schedule with base 2 / cap 60 is '$sched', expected '2 4 8 16 32 60 '"
frac="$(HDIUTIL_RETRY_SLEEP_SEC=0.25 retry_sleep_seconds 3)"
[ "$frac" = "1" ] || fail "fractional base 0.25 at attempt 3 gave '$frac', expected 1"
# The retry loop sleeps that schedule between attempts: record the sleeps of
# an exhausting verify with base 2 and a stubbed sleep, expect 2 then 4.
SLEEPS=""
sleep() { SLEEPS="$SLEEPS$1 "; }
hdiutil() {
    case "${1:-}" in
        verify) return 1 ;;
        info) return 0 ;;
    esac
    return 0
}
SLEEPS=""
HDIUTIL_RETRY_SLEEP_SEC=2 hdiutil_verify_with_retry "$TMP/no-such.dmg" >"$TMP/backoff.out" 2>&1 || true
[ "$SLEEPS" = "2 4 " ] || fail "verify retry slept '$SLEEPS', expected '2 4 ' (exponential backoff)"
grep -q "after attempt 1 (sleeping 2s)" "$TMP/backoff.out" \
    || fail "backoff log missing the attempt-1 wait: $(cat "$TMP/backoff.out")"
grep -q "after attempt 2 (sleeping 4s)" "$TMP/backoff.out" \
    || fail "backoff log missing the attempt-2 wait: $(cat "$TMP/backoff.out")"
unset -f sleep
pass "retry wait is exponential: base doubles per attempt (2, 4, 8, ...) and is capped"

# Counting wrapper over the real hdiutil for the verify tests below.
hdiutil() {
    if [ "${1:-}" = "verify" ]; then
        VERIFY_CALLS=$((VERIFY_CALLS + 1))
    fi
    command hdiutil "$@"
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
# The proof is the function body itself, not an hdiutil exit status (#580):
# the old proof was a single bare `command hdiutil verify` outside the retry
# wrapper, so one transient runner-side hdiutil failure failed the lane.
eval "$ORIG_CREATE_UDZO_DMG"
[ "$(declare -f create_udzo_dmg)" = "$ORIG_CREATE_UDZO_DMG" ] \
    || fail "create_udzo_dmg was not restored after the planted-non-image block"
# The restored helper still has to build a real image, through the same
# create+verify retry budget production uses, never a bare hdiutil call.
RESTORED="$TMP/restored.dmg"
VERIFY_CALLS=0
run_logged "$TMP/restored.out" create_and_verify_dmg "$STAGE" "PaneFlowRetryTest" "$RESTORED"
[ "$rc" -eq 0 ] || fail "restored create_udzo_dmg did not build a verifiable image: $(cat "$TMP/restored.out")"
[ -s "$RESTORED" ] || fail "restored create_udzo_dmg left no image at $RESTORED"
[ "$VERIFY_CALLS" -ge 1 ] || fail "restored create+verify never called hdiutil verify"
pass "create_udzo_dmg restored after the planted-non-image block (verify calls=$VERIFY_CALLS)"

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

# === create_dmg_main end-to-end =============================================
# The script derives dist/ from its own location, `die` calls `exit`, and the
# EXIT trap must fire, so main runs as a real process from a copy of the
# script inside a fake repo root. hdiutil, codesign, xcrun (stapler) and
# spctl are executables on a PATH prefix that append every call to
# $TOOL_LOG. Nothing is ever really attached: the stub attach only creates
# <mountpoint>/PaneFlow.app. macOS `mktemp -d` ignores TMPDIR, so the
# mountpoint main creates lands in the per-user temp dir ($SYS_TMP), which
# the happy-path test measures the same way rather than assuming.
FAKE_REPO="$TMP/fake-repo"
mkdir -p "$FAKE_REPO/scripts" "$FAKE_REPO/dist"
cp "$SCRIPT_DIR/create-dmg.sh" "$FAKE_REPO/scripts/create-dmg.sh"
SCRIPT_COPY="$FAKE_REPO/scripts/create-dmg.sh"
FAKE_APP="$TMP/PaneFlow.app"
mkdir -p "$FAKE_APP/Contents/MacOS"
printf 'not a real binary\n' > "$FAKE_APP/Contents/MacOS/paneflow"
printf '<plist/>\n' > "$FAKE_APP/Contents/Info.plist"

STUB_BIN="$TMP/bin"
mkdir -p "$STUB_BIN"
TOOL_LOG="$TMP/tools.log"
export TOOL_LOG
cat > "$STUB_BIN/hdiutil" <<'EOF'
#!/usr/bin/env bash
# Stub hdiutil: logs every call; create touches the dest; attach fakes a
# mount by creating <mountpoint>/PaneFlow.app and printing a device table.
echo "hdiutil $*" >> "$TOOL_LOG"
case "${1:-}" in
    create)
        dest=""
        while [ "$#" -gt 0 ]; do dest="$1"; shift; done
        printf 'fake udzo\n' > "$dest"
        ;;
    verify) ;;
    attach)
        mp=""
        while [ "$#" -gt 0 ]; do
            if [ "$1" = "-mountpoint" ]; then mp="$2"; fi
            shift
        done
        mkdir -p "$mp/PaneFlow.app/Contents"
        echo "expected   CRC32 \$DEADBEEF"
        echo "/dev/disk99            GUID_partition_scheme"
        echo "/dev/disk99s1          Apple_HFS                      $mp"
        ;;
    detach) ;;
    info) ;;
esac
exit 0
EOF
cat > "$STUB_BIN/codesign" <<'EOF'
#!/usr/bin/env bash
echo "codesign $*" >> "$TOOL_LOG"
if [ "${STUB_CODESIGN_FAIL:-}" = "1" ]; then
    echo "codesign: invalid signature (stub)" >&2
    exit 1
fi
exit 0
EOF
cat > "$STUB_BIN/xcrun" <<'EOF'
#!/usr/bin/env bash
echo "xcrun $*" >> "$TOOL_LOG"
exit 0
EOF
cat > "$STUB_BIN/spctl" <<'EOF'
#!/usr/bin/env bash
echo "spctl $*" >> "$TOOL_LOG"
exit 0
EOF
chmod +x "$STUB_BIN"/hdiutil "$STUB_BIN"/codesign "$STUB_BIN"/xcrun "$STUB_BIN"/spctl

# Run the copied script as a process with the stubs first on PATH.
run_main() {
    local log="$1"
    shift
    : > "$TOOL_LOG"
    rc=0
    env PATH="$STUB_BIN:$PATH" \
        HDIUTIL_RETRY_ATTEMPTS=3 HDIUTIL_RETRY_SLEEP_SEC=0.01 \
        bash "$SCRIPT_COPY" "$@" >"$log" 2>&1 || rc=$?
}
# Where the script's bare `mktemp -d` will put MOUNT_ROOT and STAGING.
SYS_TMP_PROBE="$(mktemp -d)"
SYS_TMP="$(dirname "$SYS_TMP_PROBE")"
SYS_TMP_REAL="$(cd "$SYS_TMP" && pwd -P)"
rmdir "$SYS_TMP_PROBE"

# --- sourcing guard: no args prints usage, exits non-zero, no hdiutil ------
run_main "$TMP/main-noargs.out"
[ "$rc" -ne 0 ] || fail "create-dmg.sh with no arguments exited 0"
grep -q "^Usage: " "$TMP/main-noargs.out" || fail "no-args run did not print usage: $(cat "$TMP/main-noargs.out")"
grep -q "error: --version is required" "$TMP/main-noargs.out" \
    || fail "no-args run did not name the missing --version: $(cat "$TMP/main-noargs.out")"
[ ! -s "$TOOL_LOG" ] || fail "no-args run invoked a tool: $(cat "$TOOL_LOG")"
pass "bash create-dmg.sh with no args prints usage, exits $rc, and runs no hdiutil"

# CREATE_DMG_LIB=1 (the test-sourcing guard) makes the script inert even
# when executed directly with no arguments.
: > "$TOOL_LOG"
rc=0
env PATH="$STUB_BIN:$PATH" CREATE_DMG_LIB=1 bash "$SCRIPT_COPY" >"$TMP/main-lib.out" 2>&1 || rc=$?
[ "$rc" -eq 0 ] || fail "CREATE_DMG_LIB=1 run exited $rc: $(cat "$TMP/main-lib.out")"
[ ! -s "$TMP/main-lib.out" ] || fail "CREATE_DMG_LIB=1 run printed output: $(cat "$TMP/main-lib.out")"
[ ! -s "$TOOL_LOG" ] || fail "CREATE_DMG_LIB=1 run invoked a tool: $(cat "$TOOL_LOG")"
# And this very file sourced the script without main running: dist/ untouched.
[ -z "$(ls -A "$FAKE_REPO/dist")" ] || fail "sourcing the script produced output in dist/"
pass "CREATE_DMG_LIB=1 / BASH_SOURCE guard leaves main unrun"

# --- argv parsing errors ---------------------------------------------------
run_main "$TMP/main-noarch.out" --version 9.9.9 --app "$FAKE_APP"
[ "$rc" -ne 0 ] || fail "missing --arch exited 0"
grep -q "error: --arch is required" "$TMP/main-noarch.out" || fail "missing --arch not reported: $(cat "$TMP/main-noarch.out")"
[ ! -s "$TOOL_LOG" ] || fail "missing --arch still invoked a tool: $(cat "$TOOL_LOG")"

run_main "$TMP/main-nover.out" --arch aarch64 --app "$FAKE_APP"
[ "$rc" -ne 0 ] || fail "missing --version exited 0"
grep -q "error: --version is required" "$TMP/main-nover.out" || fail "missing --version not reported: $(cat "$TMP/main-nover.out")"

run_main "$TMP/main-noapp.out" --version 9.9.9 --arch aarch64 --app "$TMP/does-not-exist.app"
[ "$rc" -ne 0 ] || fail "missing --app bundle exited 0"
grep -q "error: bundle not found: $TMP/does-not-exist.app" "$TMP/main-noapp.out" \
    || fail "missing --app bundle not reported: $(cat "$TMP/main-noapp.out")"
[ ! -s "$TOOL_LOG" ] || fail "missing --app still invoked a tool: $(cat "$TOOL_LOG")"

run_main "$TMP/main-badarch.out" --version 9.9.9 --arch armv7 --app "$FAKE_APP"
[ "$rc" -ne 0 ] || fail "bad --arch exited 0"
grep -q "error: --arch must be 'aarch64' (got 'armv7'); this fork is Apple Silicon only" "$TMP/main-badarch.out" \
    || fail "bad --arch not reported: $(cat "$TMP/main-badarch.out")"

# Apple Silicon only: x86_64 is rejected like any other arch, before any tool.
run_main "$TMP/main-x86.out" --version 9.9.9 --arch x86_64 --app "$FAKE_APP"
[ "$rc" -ne 0 ] || fail "--arch x86_64 exited 0"
grep -q "error: --arch must be 'aarch64' (got 'x86_64')" "$TMP/main-x86.out" \
    || fail "--arch x86_64 not reported: $(cat "$TMP/main-x86.out")"
[ ! -s "$TOOL_LOG" ] || fail "--arch x86_64 still invoked a tool: $(cat "$TOOL_LOG")"

run_main "$TMP/main-dangling.out" --version
[ "$rc" -ne 0 ] || fail "dangling --version exited 0"
grep -q "error: --version requires an argument" "$TMP/main-dangling.out" \
    || fail "dangling --version not reported: $(cat "$TMP/main-dangling.out")"

run_main "$TMP/main-unknown.out" --version 9.9.9 --arch aarch64 --bogus
[ "$rc" -ne 0 ] || fail "unknown argument exited 0"
grep -q "error: unknown argument: --bogus" "$TMP/main-unknown.out" \
    || fail "unknown argument not reported: $(cat "$TMP/main-unknown.out")"

run_main "$TMP/main-help.out" --help
[ "$rc" -eq 0 ] || fail "--help exited $rc"
grep -q "^Usage: " "$TMP/main-help.out" || fail "--help did not print usage"
[ ! -s "$TOOL_LOG" ] || fail "--help invoked a tool: $(cat "$TOOL_LOG")"
pass "argv parsing rejects missing --version/--arch/--app, bad arch (incl. x86_64), dangling and unknown flags; --help exits 0"

# --- happy path: tool order, temp mountpoint, advertised output ------------
# The script resolves its repo root with `pwd -P`, so the advertised path is
# the symlink-free form (/private/var/... on macOS), not $TMP as spelled.
FAKE_REPO_REAL="$(cd "$FAKE_REPO" && pwd -P)"
EXPECTED_DMG="$FAKE_REPO_REAL/dist/paneflow-9.9.9-aarch64-apple-darwin.dmg"
rm -f "$EXPECTED_DMG"
run_main "$TMP/main-ok.out" --version 9.9.9 --arch aarch64 --app "$FAKE_APP"
[ "$rc" -eq 0 ] || fail "happy path exited $rc: $(cat "$TMP/main-ok.out")
--- tools ---
$(cat "$TOOL_LOG")"
[ -s "$EXPECTED_DMG" ] || fail "happy path did not produce $EXPECTED_DMG"
grep -qF "Created: $EXPECTED_DMG (" "$TMP/main-ok.out" \
    || fail "happy path did not advertise the output path: $(cat "$TMP/main-ok.out")"
# Exact call sequence (tool + subcommand), in order.
seq="$(awk '{print $1, $2}' "$TOOL_LOG" | tr '\n' ';')"
expected_seq="hdiutil create;hdiutil verify;hdiutil attach;codesign --verify;xcrun stapler;spctl --assess;hdiutil detach;"
[ "$seq" = "$expected_seq" ] || fail "happy path tool order was '$seq', expected '$expected_seq'"
# hdiutil create targets the advertised dmg with the UDZO flags.
grep -q "^hdiutil create -volname PaneFlow -srcfolder .* -ov -format UDZO $EXPECTED_DMG\$" "$TOOL_LOG" \
    || fail "hdiutil create did not target the advertised path: $(cat "$TOOL_LOG")"
grep -q "^hdiutil verify $EXPECTED_DMG\$" "$TOOL_LOG" || fail "hdiutil verify did not target the dmg"
# attach mounts at <mktemp dir>/PaneFlow in the per-user temp dir, never /Volumes.
attach_line="$(grep '^hdiutil attach ' "$TOOL_LOG")"
mp="$(printf '%s\n' "$attach_line" | sed -E 's/.* -mountpoint ([^ ]+) .*/\1/')"
case "$mp" in
    /Volumes/*) fail "attach mountpoint '$mp' is under /Volumes: $attach_line" ;;
    "$SYS_TMP"/*/PaneFlow|"$SYS_TMP_REAL"/*/PaneFlow) ;;
    *) fail "attach mountpoint '$mp' is not a mktemp dir under $SYS_TMP: $attach_line" ;;
esac
printf '%s\n' "$attach_line" | grep -q -- "-nobrowse -readonly -noautoopen -mountpoint $mp $EXPECTED_DMG" \
    || fail "attach flags/target unexpected: $attach_line"
# Every verification step runs against the bundle inside that mountpoint.
grep -qF "codesign --verify --deep --strict $mp/PaneFlow.app" "$TOOL_LOG" || fail "codesign target: $(cat "$TOOL_LOG")"
grep -qF "xcrun stapler validate $mp/PaneFlow.app" "$TOOL_LOG" || fail "stapler target: $(cat "$TOOL_LOG")"
grep -qF "spctl --assess --type exec --verbose $mp/PaneFlow.app" "$TOOL_LOG" || fail "spctl target: $(cat "$TOOL_LOG")"
# Detach uses the device attach printed, once (the trap must not detach again).
grep -q "^hdiutil detach /dev/disk99 -quiet\$" "$TOOL_LOG" || fail "detach did not use the attached device: $(cat "$TOOL_LOG")"
[ "$(grep -c '^hdiutil detach' "$TOOL_LOG")" -eq 1 ] || fail "expected exactly one detach: $(cat "$TOOL_LOG")"
# The trap removed the temp mount root and staging dir.
[ ! -e "$(dirname "$mp")" ] || fail "temp mount root $(dirname "$mp") survived the EXIT trap"
pass "create_dmg_main happy path: create, verify, attach at temp mountpoint, codesign, stapler, spctl, detach; output $(basename "$EXPECTED_DMG")"

# --- failing codesign --verify: main dies, trap still detaches -------------
rm -f "$EXPECTED_DMG"
: > "$TOOL_LOG"
rc=0
env PATH="$STUB_BIN:$PATH" STUB_CODESIGN_FAIL=1 \
    HDIUTIL_RETRY_ATTEMPTS=3 HDIUTIL_RETRY_SLEEP_SEC=0.01 \
    bash "$SCRIPT_COPY" --version 9.9.9 --arch aarch64 --app "$FAKE_APP" >"$TMP/main-codesign-fail.out" 2>&1 || rc=$?
[ "$rc" -ne 0 ] || fail "failing codesign --verify still exited 0"
grep -q "error: codesign verification failed on enclosed bundle" "$TMP/main-codesign-fail.out" \
    || fail "codesign failure not reported: $(cat "$TMP/main-codesign-fail.out")"
grep -q '^xcrun ' "$TOOL_LOG" && fail "stapler ran after codesign failed: $(cat "$TOOL_LOG")"
grep -q '^spctl ' "$TOOL_LOG" && fail "spctl ran after codesign failed: $(cat "$TOOL_LOG")"
codesign_line="$(grep -n '^codesign ' "$TOOL_LOG" | cut -d: -f1)"
trap_detach="$(grep -n '^hdiutil detach /dev/disk99 -force' "$TOOL_LOG" | sed -n '1p' | cut -d: -f1)"
[ -n "$trap_detach" ] || fail "EXIT trap did not detach the verification device: $(cat "$TOOL_LOG")"
[ "$codesign_line" -lt "$trap_detach" ] || fail "trap detach did not follow the codesign failure: $(cat "$TOOL_LOG")"
grep -q "Detaching leftover attachment /dev/disk99" "$TMP/main-codesign-fail.out" \
    || fail "trap detach not logged: $(cat "$TMP/main-codesign-fail.out")"
grep -q "Created: " "$TMP/main-codesign-fail.out" && fail "a failed verification still advertised Created:"
fail_mp="$(grep '^hdiutil attach ' "$TOOL_LOG" | sed -E 's/.* -mountpoint ([^ ]+) .*/\1/')"
[ ! -e "$(dirname "$fail_mp")" ] || fail "temp mount root $(dirname "$fail_mp") survived the EXIT trap after die"
pass "a failing codesign --verify makes main die (exit $rc) and the EXIT trap still detaches"

echo "All tests passed."
