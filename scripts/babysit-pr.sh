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
#   scripts/babysit-pr.sh <PR> [interval_sec=90] [max_polls=40] [--apk[=dir]] [--latest-apk[=dir]]
#
# With --apk, once the apk check passes the script downloads the built APK
# (artifact ForgeRig-Release-APK from the PR's own apk run) into dir
# (default ./apk-out).
#
# With --latest-apk, after the merge the script waits for main's android.yml
# run on the merge commit (the post-merge dispatch rebuild), downloads the
# republished `latest`-release APK into dir (default ./apk-out), then prunes
# that dir: keeps only the newest forgerig-*.apk and the newest 2
# forgerig-install-*.log*, deleting the rest as stale. Pruning only ever
# touches forgerig-* filenames; nothing else in the dir is a candidate.
#
# With --prune=dir, skips CI entirely and just prunes dir immediately
# (same keep rules). Useful for cleaning the device Downloads folder.
#
# Exit codes:
#   0  all checks pass, or PR merged (+ APK fetched with --apk/--latest-apk)
#   1  a check failed, or the main run failed
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
WANT_LATEST=0
LATEST_DIR="./apk-out"
WANT_PRUNE=0
PRUNE_DIR=""
PRUNE_AFTER_FETCH=1
for arg in "$@"; do
    case "$arg" in
        --apk) WANT_APK=1 ;;
        --apk=*) WANT_APK=1; APK_DIR="${arg#--apk=}" ;;
        --latest-apk) WANT_LATEST=1 ;;
        --latest-apk=*) WANT_LATEST=1; LATEST_DIR="${arg#--latest-apk=}" ;;
        --prune=*) WANT_PRUNE=1; PRUNE_DIR="${arg#--prune=}" ;;
        --no-prune) PRUNE_AFTER_FETCH=0 ;;
        *) if [ -z "$PR" ]; then PR="$arg";
           elif [ "$INTERVAL" = "90" ]; then INTERVAL="$arg";
           elif [ "$MAX_POLLS" = "40" ]; then MAX_POLLS="$arg";
           else echo "usage: $0 <PR> [interval_sec] [max_polls] [--apk[=dir]] [--latest-apk[=dir]] [--prune=dir] [--no-prune]" >&2; exit 3; fi ;;
    esac
done
if [ "$WANT_PRUNE" = "1" ]; then
    [ -n "$PRUNE_DIR" ] || { echo "usage: $0 --prune=dir" >&2; exit 3; }
    [ -d "$PRUNE_DIR" ] || { echo "babysit: not a directory: $PRUNE_DIR" >&2; exit 3; }
    PRUNE_ONLY=1
else
    PRUNE_ONLY=0
    [ -n "$PR" ] || { echo "usage: $0 <PR> [interval_sec] [max_polls] [--apk[=dir]] [--latest-apk[=dir]] [--prune=dir]" >&2; exit 3; }
fi
command -v gh >/dev/null || { echo "babysit: gh CLI not found" >&2; exit 3; }

APK_ARTIFACT="ForgeRig-Release-APK"

if [ "$PRUNE_ONLY" = "0" ]; then
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
fi

if [ "$WANT_APK" = "1" ]; then
    run_id="$(gh pr checks "$PR" 2>/dev/null | grep -oP 'runs/\K[0-9]+' | head -1)"
    [ -n "$run_id" ] || { echo "babysit: no workflow run found for PR #$PR." >&2; exit 3; }
    mkdir -p "$APK_DIR"
    if gh run download "$run_id" -n "$APK_ARTIFACT" -D "$APK_DIR" 2>&1 | tail -1; then
        echo "babysit: APK downloaded to $APK_DIR:"
        ls -lh "$APK_DIR"
        if [ "$PRUNE_AFTER_FETCH" = "1" ]; then
            prune_downloads "$APK_DIR"
        fi
    else
        echo "babysit: APK download failed for run $run_id." >&2
        exit 3
    fi
fi

# Remove stale siblings in a download dir. APKs live loose or one level down
# (forgerig-prNN/ dirs left by --apk fetches), so scan both; keep only the
# newest forgerig-*.apk and the newest 2 forgerig-install-*.log*. Removal
# failures are reported loudly (never silently swallowed) but do not fail
# the run — the download is the deliverable.
prune_downloads() {
    local dir="$1" keep="" f="" i=0 failed=0
    local apks=()
    while IFS= read -r f; do apks+=("$f"); done < <(ls -t "$dir"/forgerig-*.apk "$dir"/forgerig-*/forgerig-*.apk 2>/dev/null || true)
    if [ "${#apks[@]}" -gt 0 ]; then
        keep="${apks[0]}"
        for f in "${apks[@]:1}"; do
            echo "babysit: removing stale APK: $f"
            rm -f "$f" || { echo "babysit: WARNING: could not remove $f" >&2; failed=1; }
        done
        for f in "$dir"/forgerig-*/; do
            [ -d "$f" ] || continue
            if [ -z "$(ls -A "$f" 2>/dev/null)" ]; then
                echo "babysit: removing emptied dir: $f"
                rmdir "$f" 2>/dev/null || { echo "babysit: WARNING: could not remove dir $f" >&2; failed=1; }
            fi
        done
    fi
    i=0
    while IFS= read -r f; do
        i=$((i + 1))
        if [ "$i" -gt 2 ]; then
            echo "babysit: removing stale log: $f"
            rm -f "$f" || { echo "babysit: WARNING: could not remove $f" >&2; failed=1; }
        fi
    done < <(ls -t "$dir"/forgerig-install-*.log* 2>/dev/null || true)
    if [ "$failed" -ne 0 ]; then
        echo "babysit: WARNING: some stale files could not be removed (see above)." >&2
    fi
    return 0
}

if [ "$WANT_LATEST" = "1" ]; then
    sha="$(gh pr view "$PR" --json mergeCommit --jq .mergeCommit.oid 2>/dev/null || echo "")"
    [ -n "$sha" ] || { echo "babysit: PR #$PR has no merge commit yet." >&2; exit 3; }
    echo "babysit: waiting for main android.yml run on merge commit $sha..."
    n=0
    done=0
    while [ "$n" -lt "$MAX_POLLS" ]; do
        n=$((n + 1))
        run_id="$(gh run list --workflow android.yml --branch main --limit 10 \
            --json databaseId,headSha,status,conclusion \
            --jq "[.[] | select(.headSha==\"$sha\")] | .[0] | .databaseId // empty" 2>/dev/null || echo "")"
        if [ -z "$run_id" ]; then
            echo "babysit: main run for $sha not started yet... [$n/$MAX_POLLS]"
        else
            status="$(gh run view "$run_id" --json status --jq .status 2>/dev/null || echo UNKNOWN)"
            concl="$(gh run view "$run_id" --json conclusion --jq .conclusion 2>/dev/null || echo "")"
            if [ "$status" = "completed" ]; then
                if [ "$concl" = "success" ]; then
                    echo "babysit: main run $run_id succeeded."
                    done=1
                    break
                fi
                echo "babysit: main run $run_id concluded: ${concl:-unknown}." >&2
                exit 1
            fi
            echo "babysit: main run $run_id $status... [$n/$MAX_POLLS]"
        fi
        sleep "$INTERVAL"
    done
    if [ "$done" != "1" ]; then
        echo "babysit: timed out waiting for main run on $sha." >&2
        exit 3
    fi
    mkdir -p "$LATEST_DIR"
    gh release download latest --pattern 'forgerig-*.apk' --dir "$LATEST_DIR" --clobber >/dev/null \
        || { echo "babysit: latest-release APK download failed." >&2; exit 3; }
    echo "babysit: latest APK downloaded to $LATEST_DIR:"
    ls -lh "$LATEST_DIR"
    prune_downloads "$LATEST_DIR"
fi

if [ "$WANT_PRUNE" = "1" ]; then
    prune_downloads "$PRUNE_DIR"
fi
