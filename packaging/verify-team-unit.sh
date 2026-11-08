#!/usr/bin/env bash
# Manual root check of the hardened team unit (plan §10.4 "(verify, M3)", M3 accept 3).
#
# Run as root on a test machine after `sudo cinelog install --team`:
#   sudo CINELOG_IT_ROOT=1 packaging/verify-team-unit.sh
#
# 1. Runs a probe shell in a transient service with every [Service] setting of the
#    installed unit: the writable paths are writable, root-owned 0600/0700 casts are
#    readable with the empty capability set, the rest of the system is read-only, /home is
#    empty, /proc stat and boot_id are readable, no capabilities, no new privileges.
# 2. Restarts cinelog-team and checks the live server: active, CapEff 0, NoNewPrivs,
#    server.json, /healthz, and no permission errors in its journal since the restart.
# 3. Creates two throwaway member tokens, logs in with both, and checks that they see the
#    same team history; revokes them again on exit. Status, live sessions and fresh hits
#    move between two requests (indexer lag, legacy import, live root shells), so it waits
#    for the indexer to catch up and compares only what is stable: the server identity,
#    the ended sessions, and the hits on ended sessions from before the restart.
#
# It leaves the install as it was, apart from the restart.
set -euo pipefail

[[ ${CINELOG_IT_ROOT:-} == 1 ]] || { echo "root-only manual check: set CINELOG_IT_ROOT=1" >&2; exit 2; }
(( EUID == 0 )) || { echo "run it as root" >&2; exit 2; }

UNIT=${UNIT:-/etc/systemd/system/cinelog-team.service}
NAME=cinelog-team.service
CINELOG=${CINELOG:-/usr/local/bin/cinelog}
[[ -r $UNIT ]] || { echo "$UNIT is missing: run 'sudo cinelog install --team' first" >&2; exit 2; }
for tool in curl jq systemd-run; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 2; }
done

failed=0
ok() { printf 'ok    %s\n' "$*"; }
bad() { printf 'FAIL  %s\n' "$*"; failed=1; }

# ------------------------------------------------------------------ 1. the sandbox
props=()
logs=/root/logs
while IFS= read -r line; do
    case $line in '' | '#'* | '['*) continue ;; esac
    key=${line%%=*}
    case $key in
        ExecStart | Restart | RestartSec) continue ;;
        BindPaths) logs=${line#*=} ;;
    esac
    props+=(-p "$line")
done < <(sed -n '/^\[Service\]/,/^\[/{/^\[/!p}' "$UNIT")

probe='
set -u
r=0
say() { printf "%s\n" "$*"; }
for d in /run/cinelog /var/lib/cinelog /etc/cinelog/tls "$LOGS"; do
    f=$d/.cinelog-probe.$$
    if printf x >"$f" 2>/dev/null && [ "$(cat "$f")" = x ] && rm -f "$f"; then
        say "ok    writable $d"
    else
        say "FAIL  not writable: $d"; r=1
    fi
done
for d in /etc/cinelog /usr/local/bin /etc /var/lib; do
    if : >"$d/.cinelog-probe.$$" 2>/dev/null; then
        rm -f "$d/.cinelog-probe.$$"; say "FAIL  writable: $d"; r=1
    else
        say "ok    read-only $d"
    fi
done
for f in /etc/cinelog/team.toml /etc/cinelog/tokens.toml /proc/sys/kernel/random/boot_id /proc/1/stat; do
    if [ -e "$f" ] && ! head -c1 "$f" >/dev/null 2>&1; then say "FAIL  unreadable: $f"; r=1; fi
done
cast=$(find "$LOGS" -type f \( -name "*.cast" -o -name "*.cast.zst" \) 2>/dev/null | head -n1)
if [ -n "$cast" ]; then
    if head -c1 "$cast" >/dev/null 2>&1; then say "ok    reads casts ($cast)"; else say "FAIL  cannot read $cast"; r=1; fi
else
    say "note  no cast below $LOGS yet: start a root shell (sudo -i) and run this again"
fi
if [ -n "$(ls -A /home 2>/dev/null)" ]; then say "FAIL  /home is visible"; r=1; else say "ok    /home is empty"; fi
caps=$(sed -n "s/^CapEff:[[:space:]]*//p" /proc/self/status)
case $caps in *[!0]*) say "FAIL  CapEff $caps"; r=1 ;; *) say "ok    no capabilities" ;; esac
[ "$(sed -n "s/^NoNewPrivs:[[:space:]]*//p" /proc/self/status)" = 1 ] && say "ok    NoNewPrivs" || { say "FAIL  NoNewPrivs"; r=1; }
exit $r
'
if systemd-run --quiet --wait --pipe --collect --setenv=LOGS="$logs" "${props[@]}" /bin/sh -c "$probe"; then
    ok "sandbox probe"
else
    bad "sandbox probe (see above)"
fi

# ------------------------------------------------------------------ 2. the live server
start=$(date +%s)
systemctl restart "$NAME"
port=
for _ in $(seq 1 40); do
    if [[ -r /run/cinelog/server.json ]] && (( $(stat -c %Y /run/cinelog/server.json) >= start )); then
        port=$(sed -n 's/.*"port":[[:space:]]*\([0-9]*\).*/\1/p' /run/cinelog/server.json)
        [[ -n $port ]] && break
    fi
    sleep 0.5
done
if systemctl is-active --quiet "$NAME"; then ok "$NAME is active"; else bad "$NAME is not active"; fi
pid=$(systemctl show -p MainPID --value "$NAME")
if [[ $pid != 0 && -r /proc/$pid/status ]]; then
    caps=$(sed -n 's/^CapEff:[[:space:]]*//p' "/proc/$pid/status")
    [[ $caps =~ ^0+$ ]] && ok "server has no capabilities" || bad "server CapEff $caps"
    [[ $(sed -n 's/^NoNewPrivs:[[:space:]]*//p' "/proc/$pid/status") == 1 ]] && ok "server NoNewPrivs" || bad "server NoNewPrivs"
fi
if [[ -z $port ]]; then
    bad "no fresh /run/cinelog/server.json within 20 s"
else
    ok "server.json written (port $port)"
    base=https://localhost:$port
    if curl -sfk "$base/healthz" >/dev/null; then ok "GET /healthz"; else bad "GET /healthz on $base"; fi
fi
sleep 2
if journalctl -q -u "$NAME" --since "@$start" --no-pager |
    grep -Ei 'permission denied|read-only file system|operation not permitted|EACCES|EPERM'; then
    bad "permission errors in the journal since the restart (above)"
else
    ok "no permission errors in the journal"
fi

# ------------------------------------------------------------------ 3. two members
if [[ -n $port ]]; then
    work=$(mktemp -d)
    a=verify-a-$$ b=verify-b-$$
    cleanup() {
        "$CINELOG" token revoke "$a" >/dev/null 2>&1 || true
        "$CINELOG" token revoke "$b" >/dev/null 2>&1 || true
        rm -rf "$work"
    }
    trap cleanup EXIT
    secret() { "$CINELOG" token new "$1" | sed -n 's/.*(shown once): //p'; }
    ta=$(secret "$a") tb=$(secret "$b")
    login() {
        curl -sk -o /dev/null -w '%{http_code}' -c "$work/$1.jar" -X POST "$base/api/login" \
            -H "Origin: $base" -H 'Content-Type: application/json' --data "{\"token\":\"$2\"}"
    }
    get() { curl -sk -b "$work/$1.jar" -w '\n%{http_code}' "$base$2"; }
    # Body of a 200 answer, else nothing (and status 1).
    body() {
        local r
        r=$(get "$1" "$2")
        [[ ${r##*$'\n'} == 200 ]] || return 1
        printf '%s' "${r%$'\n'*}"
    }
    # Runs the jq filter $3 on member $1's answer to $2.
    view() {
        local raw
        raw=$(body "$1" "$2") || return 1
        jq -cS "$3" <<<"$raw"
    }
    # Both members' views of $1 (jq filter $2) agree; retried, since an ended session or a
    # finished import can land between the two requests.
    same() {
        local va vb
        for _ in 1 2 3 4 5; do
            if va=$(view a "$1" "$2") && vb=$(view b "$1" "$2") && [[ $va == "$vb" ]]; then
                printf '%s' "$va"
                return 0
            fi
            sleep 1
        done
        return 1
    }
    if [[ $(login a "$ta") == 204 && $(login b "$tb") == 204 ]]; then
        ok "both member tokens log in"
        [[ $(get a /api/me) == *"\"principal\":\"$a\""* ]] && ok "/api/me names the member" || bad "/api/me principal"

        # The indexer catches up after the restart (and finishes a running legacy import).
        settled=
        for _ in $(seq 1 120); do
            if view a /api/status '.backlog_bytes == 0 and
                (.legacy | .files <= .imported + .hidden + .skipped + .failed)' | grep -qx true; then
                settled=1
                break
            fi
            sleep 1
        done
        [[ -n $settled ]] && ok "indexer caught up" || bad "indexer backlog or legacy import still running after 120 s"

        if same /api/status '{scope, host, version, protocol, port, fingerprint, schema_version}' >/dev/null; then
            ok "same server for both members (/api/status)"
        else
            bad "different or failed /api/status answers"
        fi
        if ended=$(same '/api/sessions?live=false&limit=500' '[.sessions[] | {uuid, user, host, state, started_at, ended_at}]'); then
            ok "same ended sessions for both members ($(jq length <<<"$ended"))"
        else
            bad "different or failed /api/sessions answers"
        fi
        if [[ -n ${ended:-} ]]; then
            uuids=$(jq -c '[.[].uuid]' <<<"$ended")
            filter="[.hits[] | select(.session as \$s | $uuids | index(\$s))]"
            if hits=$(same "/api/search?q=e&in=cmd&to=$start&limit=500" "$filter"); then
                ok "same hits on ended sessions for both members ($(jq length <<<"$hits"))"
            else
                bad "different or failed /api/search answers"
            fi
        fi
        "$CINELOG" token revoke "$b" >/dev/null
        [[ $(get b /api/me) == *$'\n'401 ]] && ok "a revoked member's session ends" || bad "revoked session still works"
        [[ $(get a /api/me) == *$'\n'200 ]] && ok "the other member stays logged in" || bad "revoking one member ended the other"
    else
        bad "member login failed"
    fi
fi

(( failed == 0 )) && echo "all checks passed" || echo "some checks FAILED"
exit "$failed"
