#!/bin/sh
# Fault injection: no router paths, network, or actual services are touched.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
sed '/^clear$/,$d' setup.sh > "$ROOT/functions"
# shellcheck disable=SC1090
. "$ROOT/functions"
ARCH=arm64-v8a
RELEASE_CHANNEL=stable
LOCAL=false
make_elf() {
  printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\002\000\267\000candidate' > "$1"
}
# Execute the real watchdog before replacing the probe for foreign ELF mocks.
printf '#!/bin/sh\nexit 7\n' > "$ROOT/probe"
chmod +x "$ROOT/probe"
STAGE=$ROOT
if run_version_probe "$ROOT/probe" >/dev/null 2>&1; then exit 1; fi
printf '#!/bin/sh\nexec sleep 30\n' > "$ROOT/probe"
if run_version_probe "$ROOT/probe" >/dev/null 2>&1; then exit 1; fi
printf 'PASS version failure and watchdog timeout\n'
run_version_probe() { [ "$FAULT" != version ]; }
# Real ELF validation, with execution independently injected.
STAGE=$ROOT CANDIDATE=$ROOT/elf FAULT=none
make_elf "$CANDIDATE"
validate_candidate "$CANDIDATE"
printf 'HTML error' > "$CANDIDATE"
if validate_candidate "$CANDIDATE" >/dev/null 2>&1; then exit 1; fi
make_elf "$CANDIDATE"
printf '\010' | dd of="$CANDIDATE" bs=1 seek=18 conv=notrunc 2>/dev/null
if validate_candidate "$CANDIDATE" >/dev/null 2>&1; then exit 1; fi
curl() {
  while [ "$#" -gt 0 ]; do
    if [ "$1" = -o ]; then shift; output=$1; fi
    shift
  done
  if [ "$FAULT" = download ]; then printf 'PARTIAL' > "$output"; return 18; fi
  make_elf "$output"
  if [ "$FAULT" = architecture ]; then printf 'BAD' > "$output"; fi
}
pidof() { [ -f "$CASE/running" ]; }
killall() { rm -f "$CASE/running"; }
sleep() { :; }
sync() { [ "$FAULT" != sync ] || [ -f "$CASE/synced" ] || { touch "$CASE/synced"; return 1; }; }
cp() {
  if [ "$FAULT" = backup ] && [ "$1" = -p ] && [ "$2" = "$XKEENUI_BIN" ]; then return 1; fi
  command cp "$@"
}
mv() {
  if [ "$FAULT" = rename ] && [ "$2" = "$CANDIDATE" ]; then return 1; fi
  if [ "$FAULT" = init-rename ] && [ "$3" = "$XKEENUI_INIT" ] && [ ! -f "$CASE/mv-failed" ]; then touch "$CASE/mv-failed"; return 1; fi
  command mv "$@"
}
finish_setup() { :; }
for FAULT in download architecture version backup sync stop rename init-rename start health recovery none; do
  CASE=$ROOT/case-$FAULT; mkdir "$CASE"
  XKEENUI_BIN=$CASE/xkeen-ui
  XKEENUI_INIT=$CASE/init
  printf OLD > "$XKEENUI_BIN"
  chmod 755 "$XKEENUI_BIN"
  cat > "$XKEENUI_INIT" <<'INIT'
#!/bin/sh
case "$1" in
 stop) [ "$FAULT" != stop ] || exit 1; rm -f "$CASE/running" ;;
 start)
   if [ "$FAULT" = recovery ]; then exit 1; fi
   if [ "$FAULT" = start ] && [ "$(cat "$XKEENUI_BIN")" != OLD ]; then exit 1; fi
   touch "$CASE/running" ;;
 status)
   [ "$FAULT" != health ] || [ "$(cat "$XKEENUI_BIN")" = OLD ] || exit 1
   test -f "$CASE/running" ;;
esac
INIT
  chmod 755 "$XKEENUI_INIT"
  command cp "$XKEENUI_INIT" "$CASE/expected-init"
  touch "$CASE/running"
  export CASE FAULT XKEENUI_BIN XKEENUI_INIT
  if update_xkeenui > "$CASE/log" 2>&1; then
    if [ "$FAULT" != none ]; then cat "$CASE/log"; echo "Unexpected success: $FAULT"; exit 1; fi
    test "$(cat "$XKEENUI_BIN")" != OLD
  else
    if [ "$FAULT" = none ]; then cat "$CASE/log"; exit 1; fi
    test "$(cat "$XKEENUI_BIN")" = OLD
    cmp "$XKEENUI_INIT" "$CASE/expected-init"
  fi
  if [ "$FAULT" = recovery ]; then
    test -d "$CASE/.xkeen-ui-update.lock"
    recovery_stage=$(cat "$CASE/.xkeen-ui-update.lock/owner")
    test "$(cat "$recovery_stage/previous")" = OLD
    grep -q 'Восстановление требует вмешательства' "$CASE/log"
    printf 'PASS recovery (backup and lock retained)\n'
    continue
  fi
  test -f "$CASE/running"
  test ! -d "$CASE/.xkeen-ui-update.lock"
  test -z "$(find "$CASE" -name '*.stage.*' -print)"
  printf 'PASS %s\n' "$FAULT"
done
# Lock contention leaves live files/services untouched.
mkdir "$CASE/.xkeen-ui-update.lock"
if update_xkeenui >/dev/null 2>&1; then exit 1; fi
test -d "$CASE/.xkeen-ui-update.lock"
# Reinstallation follows the same transaction and never calls uninstall.
FAULT=download
uninstall_xkeenui() { echo 'unexpected uninstall' >&2; exit 99; }
command cp "$XKEENUI_BIN" "$CASE/expected-bin"
rmdir "$CASE/.xkeen-ui-update.lock"
if install_xkeenui >/dev/null 2>&1; then exit 1; fi
cmp "$XKEENUI_BIN" "$CASE/expected-bin"
test -f "$CASE/running"
# First installation creates init only after validation and removes it on failure.
command cp "$XKEENUI_INIT" "$ROOT/init-template"
create_xkeenui_init() { command cp "$ROOT/init-template" "$1" && chmod +x "$1"; }
for FAULT in start none; do
  CASE=$ROOT/first-$FAULT; mkdir "$CASE"
  XKEENUI_BIN=$CASE/xkeen-ui XKEENUI_INIT=$CASE/init
  export CASE FAULT XKEENUI_BIN XKEENUI_INIT
  if install_xkeenui > "$CASE/log" 2>&1; then
    test "$FAULT" = none
    test -f "$XKEENUI_BIN"
    test -f "$CASE/running"
  else
    test "$FAULT" = start
    test ! -f "$XKEENUI_BIN"
    test ! -f "$XKEENUI_INIT"
    test ! -f "$CASE/running"
  fi
  test ! -d "$CASE/.xkeen-ui-update.lock"
  printf 'PASS first install %s\n' "$FAULT"
done
printf 'Installer transaction tests passed\n'
