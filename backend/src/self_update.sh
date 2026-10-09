#!/bin/sh
# Detached supervisor: survives stopping the HTTP server that accepted the job.
set -u
target=$1
init=$2
work=$3
lock=$4
status=$5
job=$6
port=$7
proc_root=$8
changed=0
stopped=0
finished=0
preserve=0
write_status() {
    printf '{"job_id":"%s","state":"%s","error":"%s"}\n' "$job" "$1" "$2" > "$status.tmp" && mv -f "$status.tmp" "$status"
}
bounded_init() (
    "$init" "$1" &
    child=$!
    (
        sleep 90 &
        sleeper=$!
        trap 'kill "$sleeper" 2>/dev/null || :; exit 0' HUP INT TERM
        wait "$sleeper" || exit 0
        kill -KILL "$child" 2>/dev/null || :
    ) &
    timer=$!
    wait "$child"; result=$?
    kill "$timer" 2>/dev/null || :
    wait "$timer" 2>/dev/null || :
    return "$result"
)
healthy() {
    found=0
    for pid in $(pidof xkeen-ui 2>/dev/null); do
        if [ "$(readlink "$proc_root/$pid/exe" 2>/dev/null)" = "$target" ]; then found=1; fi
    done
    [ "$found" = 1 ] || return 1
    curl -fsS --noproxy '*' --connect-timeout 3 --max-time 5 "http://127.0.0.1:$port/api/auth/login" >/dev/null
}
wait_healthy() {
    tries=0
    stable=0
    while [ "$tries" -lt 25 ]; do
        if healthy; then stable=$((stable + 1)); else stable=0; fi
        [ "$stable" -ge 3 ] && return 0
        tries=$((tries + 1))
        sleep 1
    done
    return 1
}
cleanup() {
    result=$?
    trap - EXIT HUP INT TERM
    if [ "$finished" != 1 ]; then
        if [ "$changed" = 1 ]; then
            bounded_init stop || :
            if ! cp -p "$work/old" "$work/restore" || ! mv -f "$work/restore" "$target"; then preserve=1; fi
        fi
        if [ "$stopped" = 1 ] && [ "$preserve" = 0 ]; then
            if ! bounded_init start || ! wait_healthy; then preserve=1; fi
        fi
        if [ "$preserve" = 1 ]; then
            write_status rollback_failed 'Откат не завершён; backup и lock сохранены для ручного восстановления' || :
        else
            write_status failed 'Обновление не удалось; предыдущая версия восстановлена' || :
        fi
    fi
    if [ "$preserve" = 0 ]; then
        rm -rf "$work"
        if [ "$(cat "$lock/owner" 2>/dev/null)" = "$work" ]; then
            rm -f "$lock/owner"
            rmdir "$lock" || :
        fi
    fi
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
# Allow the pending HTTP response to reach the browser before stopping its server.
sleep 2
stopped=1
bounded_init stop || exit 1
# The old process must have stopped before replacing its binary.
[ -z "$(pidof xkeen-ui 2>/dev/null)" ] || exit 1
changed=1
mv -f "$work/new" "$target" || exit 1
sync || exit 1
bounded_init start || exit 1
wait_healthy || exit 1
write_status succeeded '' || exit 1
finished=1
