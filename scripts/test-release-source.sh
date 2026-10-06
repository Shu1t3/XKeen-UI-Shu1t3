#!/bin/sh
# Test only the installer's pure selector, never its router installation actions.
set -eu
cd "$(dirname "$0")/.."
eval "$(sed -n '/^select_release_tag() {$/,/^}$/p' setup.sh)"
fixture='[{"tag_name":"v9.0.0","draft":true},{"tag_name":"v1.0.0","prerelease":false},{"tag_name":"v0.0.1-fork.6","prerelease":true}]'
RELEASE_CHANNEL=latest
test "$(printf '%s' "$fixture" | select_release_tag)" = v1.0.0
RELEASE_CHANNEL=stable
test "$(printf '%s' "$fixture" | select_release_tag)" = v1.0.0
RELEASE_CHANNEL=beta
test "$(printf '%s' "$fixture" | select_release_tag)" = v0.0.1-fork.6
RELEASE_CHANNEL=latest
test "$(printf '%s' '[{"tag_name":"v0.0.1-fork.6","prerelease":true},{"tag_name":"v1.0.0"}]' | select_release_tag)" = v0.0.1-fork.6
for fixture in '[]' '[{"tag_name":"v9.0.0","draft":true}]' '[{"tag_name":"../../other-owner"}]'; do
  if printf '%s' "$fixture" | select_release_tag >/dev/null; then
    echo "Unexpected release selected: $fixture" >&2
    exit 1
  fi
done
RELEASE_CHANNEL=beta
if printf '%s' '[{"tag_name":"v1.0.0","prerelease":false}]' | select_release_tag >/dev/null; then
  echo 'Beta selector silently switched to stable' >&2
  exit 1
fi
eval "$(sed -n '/^select_published_tag() {$/,/^}$/p' scripts/switch-to-fork.sh)"
test "$(printf '%s' '[{"tag_name":"v9.0.0","draft":true},{"tag_name":"v0.0.1-fork.6","prerelease":true},{"tag_name":"v1.0.0"}]' | select_published_tag)" = v0.0.1-fork.6
if printf '%s' '[]' | select_published_tag >/dev/null; then
  echo 'Migration selector accepted an empty release list' >&2
  exit 1
fi
printf 'Installer and migration release selection tests passed\n'
# Both standalone entry points must enforce the same direct-API/hash boundary.
CHECK_ROOT=$(mktemp -d)
trap 'rm -rf "$CHECK_ROOT"' EXIT
for SCRIPT in setup.sh scripts/switch-to-fork.sh; do
  for HELPER in get_trusted_release release_asset_digest verify_release_file; do
    eval "$(awk -v name="$HELPER" '$0 == name "() {" {printing=1} printing {print} printing && $0 == "}" {exit}' "$SCRIPT")"
  done
  printf 'candidate bytes' > "$CHECK_ROOT/binary"
  EXPECTED=$(sha256sum "$CHECK_ROOT/binary"); EXPECTED=${EXPECTED%% *}
  jq -n --arg digest "sha256:$EXPECTED" '{tag_name:"v1.0.0",draft:false,assets:[{name:"xkeen-ui-arm64-v8a",state:"uploaded",digest:$digest}]}' > "$CHECK_ROOT/release"
  test "$(release_asset_digest "$CHECK_ROOT/release" v1.0.0 xkeen-ui-arm64-v8a)" = "$EXPECTED"
  verify_release_file "$CHECK_ROOT/binary" "$EXPECTED"
  printf changed >> "$CHECK_ROOT/binary"
  if verify_release_file "$CHECK_ROOT/binary" "$EXPECTED" >/dev/null 2>&1; then exit 1; fi
  for MUTATION in '.draft = true' '.draft = null' '.tag_name = "v2.0.0"' '.assets += .assets' '.assets[0].digest = null' '.assets[0].digest = "sha256:bad"' '.assets = []' '.assets[0].state = null' '.assets[0].state = "starter"'; do
    jq "$MUTATION" "$CHECK_ROOT/release" > "$CHECK_ROOT/invalid"
    if release_asset_digest "$CHECK_ROOT/invalid" v1.0.0 xkeen-ui-arm64-v8a >/dev/null 2>&1; then echo "Accepted invalid metadata: $SCRIPT $MUTATION" >&2; exit 1; fi
  done
  curl() {
    test "$1" = -q || return 99
    direct=false protocol=false code=false
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --noproxy) shift; test "$1" = '*'; direct=true ;;
        --proto) shift; test "$1" = '=https'; protocol=true ;;
        --write-out) shift; test "$1" = '%{http_code}'; code=true ;;
        -L|--location|-fL|-fLsS) return 99 ;;
      esac
      shift
    done
    test "$direct" = true && test "$protocol" = true && test "$code" = true || return 99
    printf '%s' "$HTTP_STATUS"
  }
  HTTP_STATUS=200
  get_trusted_release https://api.github.com/repos/test/releases "$CHECK_ROOT/output"
  HTTP_STATUS=302
  if get_trusted_release https://api.github.com/repos/test/releases "$CHECK_ROOT/output" >/dev/null 2>&1; then echo 'API redirect accepted' >&2; exit 1; fi
  printf 'PASS trusted metadata and integrity: %s\n' "$SCRIPT"
done
# Manual local trust must be explicitly selected; the default stays remote.
sed '/^clear$/,$d' setup.sh > "$CHECK_ROOT/setup-functions"
sh -c 'source_file=$1; set -- --local; . "$source_file"; test "$LOCAL" = true' sh "$CHECK_ROOT/setup-functions"
sh -c 'source_file=$1; set -- latest; . "$source_file"; test "$LOCAL" = false; replace_xkeenui() { test "$LOCAL" = false; }; finish_setup() { :; }; install_xkeenui' sh "$CHECK_ROOT/setup-functions"
printf 'PASS explicit local trust and remote default\n'
