#!/usr/bin/env bash
# GPU-free tests for scripts/notarize-macos.sh (issue #715).
# A stub xcrun earlier on PATH stands in for notarytool and stapler.
# ditto only builds the submission zip locally. No Apple network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"

command -v ditto >/dev/null 2>&1 || {
    echo "FAIL: ditto not found (these tests run on macOS)" >&2
    exit 1
}
command -v python3 >/dev/null 2>&1 || {
    echo "FAIL: python3 not found" >&2
    exit 1
}

# The date stub's budget jump must match the script's ceiling.
grep -F -x 'set -euo pipefail' "$SCRIPT_DIR/notarize-macos.sh" >/dev/null \
    || { echo "FAIL: set -euo pipefail missing from notarize-macos.sh" >&2; exit 1; }
grep -F -x 'POLL_INTERVAL=30' "$SCRIPT_DIR/notarize-macos.sh" >/dev/null \
    || { echo "FAIL: POLL_INTERVAL=30 missing; update the sleep assertion" >&2; exit 1; }
grep -F -x "MAX_WAIT_SECONDS=\$((90 * 60))" "$SCRIPT_DIR/notarize-macos.sh" >/dev/null \
    || { echo "FAIL: MAX_WAIT_SECONDS changed; update the date stub's 5400" >&2; exit 1; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/notarize-macos-test.XXXXXX")"
cleanup_test() {
    rm -rf "$TMP"
}
trap cleanup_test EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

pass() {
    echo "ok - $*"
}

APP="$TMP/PaneFlow.app"
mkdir -p "$APP/Contents/MacOS"
printf '#!/bin/sh\nexit 0\n' > "$APP/Contents/MacOS/paneflow"
chmod +x "$APP/Contents/MacOS/paneflow"

APPLE_ID="dev@example.com"
APPLE_APP_SPECIFIC_PASSWORD="app-specific-password"
APPLE_TEAM_ID="ABCDE12345"

STUB_BIN="$TMP/bin"
mkdir -p "$STUB_BIN"
TOOL_LOG="$TMP/tools.log"
STAPLE_LOG="$TMP/staple.log"
INFO_COUNT_FILE="$TMP/info-count"
DATE_COUNT_FILE="$TMP/date-count"
SLEEP_COUNT_FILE="$TMP/sleep-count"

cat > "$STUB_BIN/xcrun" <<'EOF'
#!/usr/bin/env bash
# Stand-in for `xcrun notarytool` and `xcrun stapler`. Never calls Apple.
set -euo pipefail

echo "xcrun $*" >> "$TOOL_LOG"

if [ "${1:-}" = "stapler" ]; then
    action="${2:-}"
    target="${3:-}"
    if [ "$#" -ne 3 ] || { [ "$action" != "staple" ] && [ "$action" != "validate" ]; }; then
        echo "xcrun stub: unexpected stapler invocation: $*" >&2
        exit 2
    fi
    printf '%s %s\n' "$action" "$target" >> "$STAPLE_LOG"
    exit 0
fi

if [ "${1:-}" != "notarytool" ] || [ "$#" -lt 2 ]; then
    echo "xcrun stub: unexpected invocation: $*" >&2
    exit 2
fi

cmd="$2"
shift 2

apple_id=""
password=""
team_id=""
output_format=""
positional=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --apple-id)
            apple_id="${2:-}"
            shift 2
            ;;
        --password)
            password="${2:-}"
            shift 2
            ;;
        --team-id)
            team_id="${2:-}"
            shift 2
            ;;
        --output-format)
            output_format="${2:-}"
            shift 2
            ;;
        -*)
            echo "xcrun stub: unknown flag $1" >&2
            exit 2
            ;;
        *)
            if [ -n "$positional" ]; then
                echo "xcrun stub: extra positional $1" >&2
                exit 2
            fi
            positional="$1"
            shift
            ;;
    esac
done

if [ "$apple_id" != "${APPLE_ID:-}" ] \
    || [ "$password" != "${APPLE_APP_SPECIFIC_PASSWORD:-}" ] \
    || [ "$team_id" != "${APPLE_TEAM_ID:-}" ]; then
    echo "xcrun stub: credentials were not passed through" >&2
    exit 2
fi

case "$cmd" in
    submit)
        [ "$output_format" = "json" ] || { echo "xcrun stub: submit without json" >&2; exit 2; }
        [ -n "$positional" ] || { echo "xcrun stub: submit missing zip" >&2; exit 2; }
        printf '%s\n' '{"id":"stub-submission-id"}'
        ;;
    info)
        [ "$output_format" = "json" ] || { echo "xcrun stub: info without json" >&2; exit 2; }
        [ "$positional" = "stub-submission-id" ] || {
            echo "xcrun stub: unexpected submission id: $positional" >&2
            exit 2
        }
        n=$(cat "$INFO_COUNT_FILE")
        n=$((n + 1))
        printf '%s\n' "$n" > "$INFO_COUNT_FILE"
        case "${NOTARY_STUB_MODE:-}" in
            fail-once-then-accepted)
                if [ "$n" -eq 1 ]; then
                    echo "notarytool: transient network error" >&2
                    exit 1
                fi
                printf '%s\n' '{"status":"Accepted"}'
                ;;
            nonjson-then-accepted)
                if [ "$n" -eq 1 ]; then
                    printf '%s\n' 'not-json'
                    exit 0
                fi
                printf '%s\n' '{"status":"Accepted"}'
                ;;
            in-progress-then-accepted)
                if [ "$n" -eq 1 ]; then
                    printf '%s\n' '{"status":"In Progress"}'
                    exit 0
                fi
                printf '%s\n' '{"status":"Accepted"}'
                ;;
            unexpected-then-accepted)
                if [ "$n" -eq 1 ]; then
                    printf '%s\n' '{"status":"Weird"}'
                    exit 0
                fi
                printf '%s\n' '{"status":"Accepted"}'
                ;;
            rejected)
                printf '%s\n' '{"status":"Rejected"}'
                ;;
            invalid)
                printf '%s\n' '{"status":"Invalid"}'
                ;;
            always-fail)
                echo "notarytool: still unreachable" >&2
                exit 1
                ;;
            *)
                echo "xcrun stub: unknown NOTARY_STUB_MODE=${NOTARY_STUB_MODE:-}" >&2
                exit 2
                ;;
        esac
        ;;
    log)
        [ -z "$output_format" ] || { echo "xcrun stub: log unexpectedly asked for a format" >&2; exit 2; }
        [ "$positional" = "stub-submission-id" ] || {
            echo "xcrun stub: unexpected log id: $positional" >&2
            exit 2
        }
        printf '%s\n' 'stub developer log'
        ;;
    *)
        echo "xcrun stub: unknown notarytool command: $cmd" >&2
        exit 2
        ;;
esac
EOF

cat > "$STUB_BIN/sleep" <<'EOF'
#!/usr/bin/env bash
# Record the poll wait and return. A real sleep would make the suite take
# 30s per retry; the script must still invoke sleep rather than spin.
set -euo pipefail
echo "sleep $*" >> "$TOOL_LOG"
n=$(cat "$SLEEP_COUNT_FILE")
n=$((n + 1))
printf '%s\n' "$n" > "$SLEEP_COUNT_FILE"
if [ "$n" -gt 5 ]; then
    echo "sleep stub: poll loop exceeded 5 sleeps (not bounded, or spinning)" >&2
    exit 1
fi
exit 0
EOF

cat > "$STUB_BIN/date" <<'EOF'
#!/usr/bin/env bash
# Real date unless NOTARY_STUB_DATE=advance, which walks the poll clock
# from t0 to t0+30s to t0+MAX_WAIT_SECONDS (5400).
set -euo pipefail
if [ "${NOTARY_STUB_DATE:-}" != "advance" ]; then
    exec /bin/date "$@"
fi
[ "${1:-}" = "+%s" ] || { echo "date stub: unexpected args: $*" >&2; exit 2; }
n=$(cat "$DATE_COUNT_FILE")
n=$((n + 1))
printf '%s\n' "$n" > "$DATE_COUNT_FILE"
base=1000000000
max_wait=5400
case "$n" in
    1) printf '%s\n' "$base" ;;
    2) printf '%s\n' $((base + 30)) ;;
    *) printf '%s\n' $((base + max_wait)) ;;
esac
EOF

cat > "$STUB_BIN/spctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "spctl $*" >> "$TOOL_LOG"
exit 0
EOF

chmod +x "$STUB_BIN/xcrun" "$STUB_BIN/sleep" "$STUB_BIN/date" "$STUB_BIN/spctl"

# run_notarize LOG MODE [DATE_MODE]
run_notarize() {
    local log="$1"
    local mode="$2"
    local date_mode="${3:-}"
    : > "$TOOL_LOG"
    : > "$STAPLE_LOG"
    printf '0\n' > "$INFO_COUNT_FILE"
    printf '0\n' > "$DATE_COUNT_FILE"
    printf '0\n' > "$SLEEP_COUNT_FILE"
    rc=0
    env PATH="$STUB_BIN:${PATH}" \
        APPLE_ID="$APPLE_ID" \
        APPLE_APP_SPECIFIC_PASSWORD="$APPLE_APP_SPECIFIC_PASSWORD" \
        APPLE_TEAM_ID="$APPLE_TEAM_ID" \
        TOOL_LOG="$TOOL_LOG" \
        STAPLE_LOG="$STAPLE_LOG" \
        INFO_COUNT_FILE="$INFO_COUNT_FILE" \
        DATE_COUNT_FILE="$DATE_COUNT_FILE" \
        SLEEP_COUNT_FILE="$SLEEP_COUNT_FILE" \
        NOTARY_STUB_MODE="$mode" \
        NOTARY_STUB_DATE="$date_mode" \
        bash "$SCRIPT_DIR/notarize-macos.sh" "$APP" >"$log" 2>&1 || rc=$?
}

call_seq() {
    awk '
        $1 == "sleep" { print $1, $2; next }
        $1 == "spctl" { print "spctl", $2; next }
        $1 == "xcrun" && $2 == "notarytool" { print $1, $2, $3; next }
        $1 == "xcrun" && $2 == "stapler" { print $1, $2, $3; next }
        { printf "UNEXPECTED %s\n", $0 }
    ' "$TOOL_LOG" | tr '\n' ';'
}

expect_seq() {
    local want="$1"
    local got
    got="$(call_seq)"
    [ "$got" = "$want" ] || fail "tool sequence was '$got', expected '$want'
--- script ---
$(cat "$2")"
}

expect_stapled() {
    local staple validate
    staple="$(sed -n '1p' "$STAPLE_LOG")"
    validate="$(sed -n '2p' "$STAPLE_LOG")"
    [ "$staple" = "staple $APP" ] || fail "stapler staple record was '$staple'"
    [ "$validate" = "validate $APP" ] || fail "stapler validate record was '$validate'"
    [ "$(awk 'END { print NR }' "$STAPLE_LOG")" -eq 2 ] \
        || fail "expected 2 stapler records, got: $(cat "$STAPLE_LOG")"
}

expect_not_stapled() {
    [ ! -s "$STAPLE_LOG" ] || fail "stapler ran: $(cat "$STAPLE_LOG")"
}

ACCEPTED_SEQ="xcrun notarytool submit;xcrun notarytool info;sleep 30;xcrun notarytool info;xcrun stapler staple;xcrun stapler validate;spctl --assess;"
ZIP="${APP%.app}.zip"

assert_credentials() {
    local log="$1"
    grep -F -q -x "xcrun notarytool submit $ZIP --apple-id $APPLE_ID --password $APPLE_APP_SPECIFIC_PASSWORD --team-id $APPLE_TEAM_ID --output-format json" "$TOOL_LOG" \
        || fail "submit invocation did not match: $(cat "$TOOL_LOG")"
    grep -F -q -x "xcrun notarytool info stub-submission-id --apple-id $APPLE_ID --password $APPLE_APP_SPECIFIC_PASSWORD --team-id $APPLE_TEAM_ID --output-format json" "$TOOL_LOG" \
        || fail "info invocation did not match: $(cat "$TOOL_LOG")"
    [ ! -e "$ZIP" ] || fail "submission zip survived the EXIT trap: $ZIP"
    [ -f "$log" ] || fail "missing script transcript: $log"
}

# --- info exits non-zero once, then Accepted reaches staple ---------------
run_notarize "$TMP/fail-once.out" fail-once-then-accepted
[ "$rc" -eq 0 ] || fail "fail-once-then-accepted exited $rc: $(cat "$TMP/fail-once.out")"
expect_seq "$ACCEPTED_SEQ" "$TMP/fail-once.out"
expect_stapled
assert_credentials "$TMP/fail-once.out"
grep -F -q "notarytool info failed - retrying" "$TMP/fail-once.out" \
    || fail "missing info-failure retry: $(cat "$TMP/fail-once.out")"
grep -F -q "notarytool info status could not be parsed" "$TMP/fail-once.out" \
    && fail "a non-zero info was treated as a parse failure: $(cat "$TMP/fail-once.out")"
grep -F -q "Accepted by Apple" "$TMP/fail-once.out" \
    || fail "Accepted was not reported: $(cat "$TMP/fail-once.out")"
grep -F -q "Notarized + stapled: $APP (submission_id=stub-submission-id)" "$TMP/fail-once.out" \
    || fail "script did not finish after staple: $(cat "$TMP/fail-once.out")"
grep -F -q "spctl --assess --type exec --verbose $APP" "$TOOL_LOG" \
    || fail "spctl was not run against the bundle: $(cat "$TOOL_LOG")"
[ "$(cat "$INFO_COUNT_FILE")" -eq 2 ] || fail "expected 2 info polls, got $(cat "$INFO_COUNT_FILE")"
pass "a single failed notarytool info retries after sleep 30 and then staples"

# --- info exits 0 with a non-JSON body, then Accepted ---------------------
run_notarize "$TMP/nonjson.out" nonjson-then-accepted
[ "$rc" -eq 0 ] || fail "nonjson-then-accepted exited $rc: $(cat "$TMP/nonjson.out")"
expect_seq "$ACCEPTED_SEQ" "$TMP/nonjson.out"
expect_stapled
grep -F -q "notarytool info status could not be parsed - retrying" "$TMP/nonjson.out" \
    || fail "missing parse retry: $(cat "$TMP/nonjson.out")"
grep -F -q "notarytool info failed - retrying" "$TMP/nonjson.out" \
    && fail "a 0-exit non-JSON body was treated as an info failure: $(cat "$TMP/nonjson.out")"
grep -F -q "Accepted by Apple" "$TMP/nonjson.out" \
    || fail "Accepted was not reported after a parse retry: $(cat "$TMP/nonjson.out")"
pass "an unparseable notarytool info body retries after sleep 30 and then staples"

# --- In Progress still sleeps once, then Accepted staples -----------------
run_notarize "$TMP/progress.out" in-progress-then-accepted
[ "$rc" -eq 0 ] || fail "in-progress-then-accepted exited $rc: $(cat "$TMP/progress.out")"
expect_seq "$ACCEPTED_SEQ" "$TMP/progress.out"
expect_stapled
grep -F -q "In Progress... (next poll in 30s)" "$TMP/progress.out" \
    || fail "missing in-progress heartbeat: $(cat "$TMP/progress.out")"
grep -F -q "Accepted by Apple" "$TMP/progress.out" \
    || fail "Accepted was not reported after In Progress: $(cat "$TMP/progress.out")"
pass "In Progress sleeps POLL_INTERVAL once, then Accepted staples"

# --- unknown parsed status keeps polling, then Accepted -------------------
run_notarize "$TMP/weird.out" unexpected-then-accepted
[ "$rc" -eq 0 ] || fail "unexpected-then-accepted exited $rc: $(cat "$TMP/weird.out")"
expect_seq "$ACCEPTED_SEQ" "$TMP/weird.out"
expect_stapled
grep -F -q "Unexpected status: Weird - continuing to poll" "$TMP/weird.out" \
    || fail "missing unexpected-status log: $(cat "$TMP/weird.out")"
pass "an unexpected parsed status keeps polling and Accepted still staples"

# --- Rejected fails, fetches the log, does not staple ---------------------
run_notarize "$TMP/rejected.out" rejected
[ "$rc" -eq 1 ] || fail "Rejected exited $rc, expected 1: $(cat "$TMP/rejected.out")"
expect_seq "xcrun notarytool submit;xcrun notarytool info;xcrun notarytool log;" "$TMP/rejected.out"
expect_not_stapled
grep -F -q "::error title=Notarization::Apple rejected submission (status=Rejected, id=stub-submission-id)" "$TMP/rejected.out" \
    || fail "Rejected was not reported: $(cat "$TMP/rejected.out")"
grep -F -q -e "--- notarytool log stub-submission-id ---" "$TMP/rejected.out" \
    || fail "developer log header missing: $(cat "$TMP/rejected.out")"
grep -F -q "stub developer log" "$TMP/rejected.out" \
    || fail "developer log body missing: $(cat "$TMP/rejected.out")"
grep -F -q "xcrun notarytool log stub-submission-id --apple-id $APPLE_ID --password $APPLE_APP_SPECIFIC_PASSWORD --team-id $APPLE_TEAM_ID" "$TOOL_LOG" \
    || fail "log invocation did not match: $(cat "$TOOL_LOG")"
grep -F -q "Notarized + stapled:" "$TMP/rejected.out" \
    && fail "Rejected still reported success: $(cat "$TMP/rejected.out")"
[ ! -e "$ZIP" ] || fail "submission zip survived a rejected run"
pass "Rejected exits 1, fetches notarytool log, and does not staple"

# --- Invalid fails the same way -------------------------------------------
run_notarize "$TMP/invalid.out" invalid
[ "$rc" -eq 1 ] || fail "Invalid exited $rc, expected 1: $(cat "$TMP/invalid.out")"
expect_seq "xcrun notarytool submit;xcrun notarytool info;xcrun notarytool log;" "$TMP/invalid.out"
expect_not_stapled
grep -F -q "::error title=Notarization::Apple rejected submission (status=Invalid, id=stub-submission-id)" "$TMP/invalid.out" \
    || fail "Invalid was not reported: $(cat "$TMP/invalid.out")"
grep -F -q "Notarized + stapled:" "$TMP/invalid.out" \
    && fail "Invalid still reported success: $(cat "$TMP/invalid.out")"
pass "Invalid exits 1 and does not staple"

# --- repeated info failure stays inside MAX_WAIT_SECONDS ------------------
# Clock: start, then +30s (retry + sleep), then +5400s (timeout, no sleep).
run_notarize "$TMP/timeout.out" always-fail advance
[ "$rc" -eq 1 ] || fail "always-fail exited $rc, expected 1: $(cat "$TMP/timeout.out")"
expect_seq "xcrun notarytool submit;xcrun notarytool info;sleep 30;xcrun notarytool info;" "$TMP/timeout.out"
expect_not_stapled
grep -F -q "notarytool info failed - retrying" "$TMP/timeout.out" \
    || fail "under-budget failure did not retry: $(cat "$TMP/timeout.out")"
retry_lines="$(grep -c -F "notarytool info failed - retrying" "$TMP/timeout.out" || true)"
[ "$retry_lines" -eq 1 ] || fail "expected one retry-and-sleep, got $retry_lines: $(cat "$TMP/timeout.out")"
grep -F -q "[+90:00] notarytool info failed" "$TMP/timeout.out" \
    || fail "over-budget failure was not logged: $(cat "$TMP/timeout.out")"
grep -F -q "::error title=Notarization timeout::Submission stub-submission-id still pending after 90 minutes." "$TMP/timeout.out" \
    || fail "timeout was not reported: $(cat "$TMP/timeout.out")"
grep -F -q "xcrun stapler staple $APP" "$TMP/timeout.out" \
    || fail "timeout recovery did not name stapler: $(cat "$TMP/timeout.out")"
grep -F -q "Notarized + stapled:" "$TMP/timeout.out" \
    && fail "timeout still reported success: $(cat "$TMP/timeout.out")"
[ "$(cat "$INFO_COUNT_FILE")" -eq 2 ] || fail "timeout polled $(cat "$INFO_COUNT_FILE") times, expected 2"
[ "$(cat "$SLEEP_COUNT_FILE")" -eq 1 ] || fail "timeout slept $(cat "$SLEEP_COUNT_FILE") times, expected 1"
[ ! -e "$ZIP" ] || fail "submission zip survived a timeout"
pass "repeated notarytool info failures retry once, then stop at MAX_WAIT_SECONDS without stapling"

echo "All tests passed."
