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
run_version_probe() { [ -z "${CASE:-}" ] || touch "$CASE/probed"; [ "$FAULT" != version ]; }
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
  first=$1
  output='' url='' direct=false protocol=false write_status=false
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -o) shift; output=$1 ;;
      --noproxy) shift; [ "$1" = '*' ] && direct=true ;;
      --proto) shift; [ "$1" = '=https' ] && protocol=true ;;
      --write-out) shift; write_status=true ;;
      https://*) url=$1 ;;
      -fL|-fLsS|-L) case "$url" in *api.github.com*) return 99;; esac ;;
    esac
    shift
  done
  case "$url" in
    https://api.github.com/*)
      [ "$first" = -q ] && [ "$direct" = true ] && [ "$protocol" = true ] && [ "$write_status" = true ] || return 99
      if [ "$FAULT" = metadata-network ]; then return 60; fi
      if [ "$FAULT" = metadata-redirect ]; then printf '302'; return 0; fi
      case "$url" in
        *'/releases?'*) printf '[{"tag_name":"v0.0.1-fork.8","draft":false,"prerelease":false}]' > "$output" ;;
        *)
          make_elf "$STAGE/hash-source"
          if [ "$FAULT" = architecture ]; then printf BAD > "$STAGE/hash-source"; fi
          digest=$(command sha256sum "$STAGE/hash-source"); digest=${digest%% *}
          case "$FAULT" in
            missing-digest) digest='' ;;
            invalid-digest) digest=invalid ;;
          esac
          jq -n --arg digest "sha256:$digest" '{tag_name:"v0.0.1-fork.8",draft:false,assets:[{name:"xkeen-ui-arm64-v8a",state:"uploaded",digest:$digest}]}' > "$output"
          case "$FAULT" in
            duplicate-asset) jq '.assets += .assets' "$output" > "$output.tmp"; command mv "$output.tmp" "$output" ;;
            wrong-tag) jq '.tag_name = "v9.0.0"' "$output" > "$output.tmp"; command mv "$output.tmp" "$output" ;;
            draft) jq '.draft = true' "$output" > "$output.tmp"; command mv "$output.tmp" "$output" ;;
          esac ;;
      esac
      printf 200 ;;
    *)
      if [ "$FAULT" = download ]; then printf 'PARTIAL' > "$output"; return 18; fi
      make_elf "$output"
      if [ "$FAULT" = architecture ]; then printf BAD > "$output"; fi
      if [ "$FAULT" = tamper ]; then printf tampered >> "$output"; fi ;;
  esac
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
for FAULT in metadata-network metadata-redirect missing-digest invalid-digest duplicate-asset wrong-tag draft tamper download architecture version backup sync stop rename init-rename start health recovery none; do
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
  case "$FAULT" in
    metadata-*|*-digest|duplicate-asset|wrong-tag|draft|tamper|download)
      test ! -f "$CASE/probed"
      test ! -f "$CASE/synced" ;;
  esac
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

# Shared service (lighttpd) safety tests (R35)
eval "$(sed -n '/^uninstall_xkeenui() {$/,/^}/p' "$ROOT/functions")"
LIGHT_CASE=$ROOT/case-lighttpd; mkdir -p "$LIGHT_CASE"
LIGHTTPD_DIR=$LIGHT_CASE/lighttpd
LIGHTTPD_CONF=$LIGHTTPD_DIR/conf.d/90-xkeenui.conf
LIGHTTPD_INIT=$LIGHT_CASE/init-lighttpd
XKEENUI_BIN=$LIGHT_CASE/xkeen-ui
XKEENUI_INIT=$LIGHT_CASE/init-xkeenui
STATIC_DIR=$LIGHT_CASE/www
export LIGHTTPD_DIR LIGHTTPD_CONF LIGHTTPD_INIT XKEENUI_BIN XKEENUI_INIT STATIC_DIR
spinner() { wait "$1" 2>/dev/null || :; }

cat > "$LIGHTTPD_INIT" <<INIT
#!/bin/sh
case "\$1" in
  status) test -f "$LIGHT_CASE/running" ;;
  start) touch "$LIGHT_CASE/running" "$LIGHT_CASE/started" ;;
  stop) rm -f "$LIGHT_CASE/running"; touch "$LIGHT_CASE/stopped" ;;
  restart) touch "$LIGHT_CASE/running" "$LIGHT_CASE/restarted" ;;
esac
INIT
chmod 755 "$LIGHTTPD_INIT"

reset_light_test() {
  rm -rf "$LIGHTTPD_DIR" "$STATIC_DIR" "$LIGHT_CASE"/*-called "$LIGHT_CASE"/started "$LIGHT_CASE"/stopped "$LIGHT_CASE"/restarted
  mkdir -p "$LIGHTTPD_DIR/conf.d" "$STATIC_DIR"
  printf 'server.port := 1000\n' > "$LIGHTTPD_CONF"
  printf 'server.modules += ( "mod_status" )\n' > "$LIGHTTPD_DIR/conf.d/10-other.conf"
  printf 'server.document-root = "/var/www"\n' > "$LIGHTTPD_DIR/lighttpd.conf"
  printf 'xkeen-binary' > "$XKEENUI_BIN"; chmod 755 "$XKEENUI_BIN"
  printf '#!/bin/sh\nexit 0\n' > "$XKEENUI_INIT"; chmod 755 "$XKEENUI_INIT"
  printf '<html></html>' > "$STATIC_DIR/index.html"
}

# 1. uninstall_xkeenui with running lighttpd preserves other configs and restarts server
reset_light_test
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='y'
opkg() { touch "$LIGHT_CASE/opkg-called"; return 1; }
uninstall_xkeenui
test ! -f "$LIGHTTPD_CONF"
test -f "$LIGHTTPD_DIR/conf.d/10-other.conf"
test -f "$LIGHTTPD_DIR/lighttpd.conf"
test -d "$LIGHTTPD_DIR"
test -f "$LIGHT_CASE/running"
test -f "$LIGHT_CASE/restarted"
test ! -f "$LIGHT_CASE/opkg-called"
test ! -f "$XKEENUI_BIN"
test ! -f "$XKEENUI_INIT"
test ! -d "$STATIC_DIR"
printf 'PASS uninstall preserves shared lighttpd and other configs\n'

# 2. uninstall_xkeenui with stopped lighttpd preserves stopped state
reset_light_test
rm -f "$LIGHT_CASE/running"
FORCE_RESPONSE='y'
uninstall_xkeenui
test ! -f "$LIGHTTPD_CONF"
test -f "$LIGHTTPD_DIR/conf.d/10-other.conf"
test ! -f "$LIGHT_CASE/running"
test ! -f "$LIGHT_CASE/started"
test ! -f "$LIGHT_CASE/opkg-called"
printf 'PASS uninstall preserves stopped lighttpd state\n'

# 3. legacy_installation_check with running lighttpd, response 'n'
reset_light_test
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='n'
legacy_installation_check
test ! -f "$LIGHTTPD_CONF"
test -f "$LIGHTTPD_DIR/conf.d/10-other.conf"
test -f "$LIGHTTPD_DIR/lighttpd.conf"
test -d "$LIGHTTPD_DIR"
test -f "$LIGHT_CASE/running"
test -f "$LIGHT_CASE/restarted"
test ! -f "$LIGHT_CASE/opkg-called"
printf 'PASS legacy migration with answer N restarts lighttpd without opkg\n'

# 4. legacy_installation_check with stopped lighttpd, response 'n'
reset_light_test
rm -f "$LIGHT_CASE/running"
FORCE_RESPONSE='n'
legacy_installation_check
test ! -f "$LIGHTTPD_CONF"
test ! -f "$LIGHT_CASE/running"
test ! -f "$LIGHT_CASE/started"
test ! -f "$LIGHT_CASE/opkg-called"
printf 'PASS legacy migration with answer N keeps stopped lighttpd\n'

# 5. legacy_installation_check with response 'y', but other configs exist
reset_light_test
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='y'
legacy_installation_check
test ! -f "$LIGHTTPD_CONF"
test -f "$LIGHTTPD_DIR/conf.d/10-other.conf"
test ! -f "$LIGHT_CASE/opkg-called"
test -f "$LIGHT_CASE/running"
printf 'PASS legacy migration protects lighttpd when other configs exist\n'

# 6. legacy_installation_check with response 'y', no other configs, but opkg remove fails
reset_light_test
rm -f "$LIGHTTPD_DIR/conf.d/10-other.conf"
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='y'
opkg() {
  test "$1" = remove && test "$2" = lighttpd && test "$#" -eq 2 || return 99
  touch "$LIGHT_CASE/opkg-safe-called"
  return 1
}
legacy_installation_check
test ! -f "$LIGHTTPD_CONF"
test -d "$LIGHTTPD_DIR"
test -f "$LIGHT_CASE/opkg-safe-called"
test -f "$LIGHT_CASE/running"
printf 'PASS legacy migration restores running lighttpd on opkg failure\n'

# 7. legacy_installation_check with response 'y', no other configs, and opkg remove succeeds
reset_light_test
rm -f "$LIGHTTPD_DIR/conf.d/10-other.conf"
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='y'
opkg() {
  test "$1" = remove && test "$2" = lighttpd && test "$#" -eq 2 || return 99
  touch "$LIGHT_CASE/opkg-safe-called"
  rm -f "$LIGHT_CASE/running"
  return 0
}
legacy_installation_check
test ! -f "$LIGHTTPD_CONF"
test -d "$LIGHTTPD_DIR"
test -f "$LIGHT_CASE/opkg-safe-called"
test ! -f "$LIGHT_CASE/running"
printf 'PASS legacy migration safe opkg remove never wipes shared directory\n'

# 8. update_xkeenui triggers legacy migration and cleans owned config
reset_light_test
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='n'
replace_xkeenui() { :; }
finish_setup() { :; }
update_xkeenui
test ! -f "$LIGHTTPD_CONF"
test -f "$LIGHT_CASE/running"
test -f "$LIGHT_CASE/restarted"
printf 'PASS update_xkeenui cleans legacy lighttpd config\n'

# 9. install_xkeenui triggers legacy migration and cleans owned config
reset_light_test
touch "$LIGHT_CASE/running"
FORCE_RESPONSE='n'
install_xkeenui
test ! -f "$LIGHTTPD_CONF"
test -f "$LIGHT_CASE/running"
test -f "$LIGHT_CASE/restarted"
printf 'PASS install_xkeenui cleans legacy lighttpd config\n'
