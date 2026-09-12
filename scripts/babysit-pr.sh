#!/usr/bin/env bash
# scripts/babysit-pr.sh — poll a PR until its checks resolve, then report.
#
# Babysitting means polling, not hoping: this script watches a PR's checks
# until they complete, reports failures with run URLs, and exits nonzero
# unless the PR is green (or merged). Run it, then fix what it reports,
# then run it again — that is the loop. (Adapted from flashforge-farm's
# scripts/babysit-pr.sh for ForgeRig's pipeline.)
#
# Usage:
#   scripts/babysit-pr.sh <PR> [interval_sec=90] [max_polls=40] [--apk[=dir]]
#
# With --apk, once the apk check passes the script downloads the built APK
# (artifact ForgeRig-Release-APK from the PR's own apk run) into dir
# (default ./apk-out).
#
# Exit codes:
#   0  all checks pass, or PR merged
#   1  a check failed
#   3  usage error / PR closed unmerged / timed out waiting / APK fetch failed
set -uo pipefail

[ -d /data/data/com.termux/files/usr/bin ] && export PATH="$PATH:/data/data/com.termux/files/usr/bin"
export HOME="${HOME:-/data/data/com.termux/files/home}"
export TMPDIR="${TMPDIR:-$HOME/.cache/babysit-tmp}"
mkdir -p "$TMPDIR"

PR=""
INTERVAL=90
MAX_POLLS=40
WANT_APK=0
APK_DIR="./apk-out"
for arg in "$@"; do
    case "$arg" in
        --apk) WANT_APK=1 ;;
        --apk=*) WANT_APK=1; APK_DIR="${arg#--apk=}" ;;
        *) if [ -z "$PR" ]; then PR="$arg";
           elif [ "$INTERVAL" = "90" ]; then INTERVAL="$arg";
           elif [ "$MAX_POLLS" = "40" ]; then MAX_POLLS="$arg";
           else echo "usage: $0 <PR> [interval_sec] [max_polls] [--apk[=dir]]" >&2; exit 3; fi ;;
    esac
done
[ -n "$PR" ] || { echo "usage: $0 <PR> [interval_sec] [max_polls] [--apk[=dir]]" >&2; exit 3; }
command -v gh >/dev/null || { echo "babysit: gh CLI not found" >&2; exit 3; }

APK_ARTIFACT="ForgeRig-Release-APK"

n=0
while [ "$n" -lt "$MAX_POLLS" ]; do
    n=$((n + 1))
    state="$(gh pr view "$PR" --json state -q .state 2>/dev/null || echo UNKNOWN)"
    if [ "$state" = "MERGED" ]; then
        echo "babysit: PR #$PR is MERGED."
        break
    fi
    if [ "$state" = "CLOSED" ]; then
        echo "babysit: PR #$PR closed without merge." >&2
        exit 3
    fi
    checks="$(gh pr checks "$PR" 2>/dev/null || echo)"
    if [ -z "$checks" ]; then
        echo "babysit: no checks reported yet... [$n/$MAX_POLLS]"
    else
        echo "$checks" | sed 's/^/babysit: /'
        if echo "$checks" | awk '{print $2}' | grep -qx "fail"; then
            echo "babysit: a check FAILED for PR #$PR." >&2
            exit 1
        fi
        if ! echo "$checks" | awk '{print $2}' | grep -Eq "pending|queued|waiting|skipping"; then
            echo "babysit: all checks pass for PR #$PR."
            break
        fi
    fi
    if [ "$n" -ge "$MAX_POLLS" ]; then
        echo "babysit: timed out waiting for PR #$PR." >&2
        exit 3
    fi
    sleep "$INTERVAL"
done

if [ "$WANT_APK" = "1" ]; then
    run_id="$(gh pr checks "$PR" 2>/dev/null | grep -oP 'runs/\K[0-9]+' | head -1)"
    [ -n "$run_id" ] || { echo "babysit: no workflow run found for PR #$PR." >&2; exit 3; }
    mkdir -p "$APK_DIR"
    if gh run download "$run_id" -n "$APK_ARTIFACT" -D "$APK_DIR" 2>&1 | tail -1; then
        echo "babysit: APK downloaded to $APK_DIR:"
        ls -lh "$APK_DIR"
    else
        echo "babysit: APK download failed for run $run_id." >&2
        exit 3
    fi
fi
