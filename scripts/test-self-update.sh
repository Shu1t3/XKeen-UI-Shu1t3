#!/bin/sh
# Exercise the detached self-update supervisor using filesystem/service mocks.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
mkdir "$ROOT/mocks"
cat > "$ROOT/mocks/init" <<'MOCK'
#!/bin/sh
content=$(cat "$TARGET")
case "$1" in
 stop)
   [ "$FAULT" != stop ] || exit 1
   rm -f "$CASE/running" ;;
 start)
   if [ "$content" = NEW ] && { [ "$FAULT" = start ] || [ "$FAULT" = recovery ]; }; then exit 1; fi
   [ "$FAULT" != recovery ] || exit 1
   printf '123\n' > "$CASE/running"
   printf '%s\n' "$TARGET" > "$CASE/proc/123/exe"
   printf '%s\n' "$content" >> "$CASE/starts" ;;
esac
MOCK
cat > "$ROOT/mocks/pidof" <<'MOCK'
#!/bin/sh
cat "$CASE/running" 2>/dev/null
MOCK
cat > "$ROOT/mocks/readlink" <<'MOCK'
#!/bin/sh
cat "$1"
MOCK
cat > "$ROOT/mocks/curl" <<'MOCK'
#!/bin/sh
if [ "$FAULT" = cancel-health ] && [ "$(cat "$TARGET")" = NEW ] && [ ! -f "$CASE/signal" ]; then
 touch "$CASE/signal"
 kill -TERM "$PPID"
 exit 1
fi
if { [ "$FAULT" = health ] || [ "$FAULT" = restore ]; } && [ "$(cat "$TARGET")" = NEW ]; then exit 22; fi
if [ "$FAULT" = wrong-process ] && [ "$(cat "$TARGET")" = NEW ]; then
 printf '/another/binary\n' > "$CASE/proc/123/exe"
fi
printf '%s\n' "$(cat "$TARGET")" >> "$CASE/health"
MOCK
cat > "$ROOT/mocks/sleep" <<'MOCK'
#!/bin/sh
case "$1" in
 30|90) exec /bin/sleep 30 ;;
 *) exec /bin/sleep 0.02 ;;
esac
MOCK
cat > "$ROOT/mocks/sync" <<'MOCK'
#!/bin/sh
[ "$FAULT" != sync ]
MOCK
cat > "$ROOT/mocks/mv" <<'MOCK'
#!/bin/sh
case "$2" in
 */new)
   [ "$FAULT" != rename ] || exit 1
   /bin/mv "$@" || exit 1
   if [ "$FAULT" = cancel-rename ]; then kill -TERM "$PPID"; fi
   exit 0 ;;
 */restore|*/old)
   [ "$FAULT" != restore ] || exit 1 ;;
esac
exec /bin/mv "$@"
MOCK
chmod +x "$ROOT/mocks/"*
PATH="$ROOT/mocks:$PATH"
export PATH
for FAULT in none stop start rename sync health wrong-process restore recovery cancel-health cancel-rename; do
 CASE=$ROOT/$FAULT
 mkdir -p "$CASE/work" "$CASE/lock" "$CASE/proc/123"
 TARGET=$CASE/panel
 printf OLD > "$TARGET"
 printf OLD > "$CASE/work/old"
 printf NEW > "$CASE/work/new"
 printf '%s\n' "$CASE/work" > "$CASE/lock/owner"
 printf '123\n' > "$CASE/running"
 printf '%s\n' "$TARGET" > "$CASE/proc/123/exe"
 export CASE TARGET FAULT
 if sh backend/src/self_update.sh "$TARGET" "$ROOT/mocks/init" "$CASE/work" "$CASE/lock" "$CASE/status" job-test 1000 "$CASE/proc" > "$CASE/log" 2>&1; then
   test "$FAULT" = none || { cat "$CASE/log"; echo "Unexpected success $FAULT"; exit 1; }
 else
   test "$FAULT" != none || { cat "$CASE/log"; exit 1; }
 fi
 state=$(jq -r .state "$CASE/status")
 case "$FAULT" in
 none)
   test "$state" = succeeded
   test "$(cat "$TARGET")" = NEW
   test "$(wc -l < "$CASE/health" | tr -d ' ')" -ge 3
   test ! -d "$CASE/work" ;;
 restore|recovery)
   test "$state" = rollback_failed
   test -d "$CASE/lock"
   test "$(cat "$CASE/work/old")" = OLD ;;
 *)
   test "$state" = failed
   test "$(cat "$TARGET")" = OLD
   test -f "$CASE/running"
   test ! -d "$CASE/work" ;;
 esac
 if [ "$state" != rollback_failed ]; then test ! -d "$CASE/lock"; fi
 printf 'PASS self update %s (%s)\n' "$FAULT" "$state"
done
printf 'Self-update supervisor tests passed\n'
