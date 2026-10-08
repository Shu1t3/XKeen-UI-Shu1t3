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

# Unified release workflow validation (R32)
test ! -f .github/workflows/build-go.yml
python3 -c '
import yaml
with open(".github/workflows/build-rust.yml") as f:
    cfg = yaml.safe_load(f)
on = cfg[True] if True in cfg else cfg["on"]
assert "v*" in on["push"]["tags"], "push tags missing v*"
inputs = on["workflow_dispatch"]["inputs"]
assert inputs["channel"]["type"] == "choice", "channel choice missing"
assert inputs["channel"]["options"] == ["beta", "stable"], "channel options invalid"
assert inputs["channel"]["default"] == "beta", "channel default must be beta"
assert inputs["publish"]["type"] == "boolean", "publish boolean missing"
assert inputs["version"]["required"] is True, "version must be required"
'
resolve_meta() {
  local EVENT_NAME=$1 INPUT_VERSION=$2 INPUT_CHANNEL=$3 REF_NAME=$4
  local version prerelease make_latest
  if [ "$EVENT_NAME" = "workflow_dispatch" ]; then
    version="$INPUT_VERSION"
    if [ "$INPUT_CHANNEL" = "stable" ]; then
      prerelease="false"
      make_latest="true"
    else
      prerelease="true"
      make_latest="false"
    fi
  else
    version="$REF_NAME"
    case "$version" in
      *-*)
        prerelease="true"
        make_latest="false"
        ;;
      *)
        prerelease="false"
        make_latest="true"
        ;;
    esac
  fi
  printf '%s:%s:%s' "$version" "$prerelease" "$make_latest"
}
test "$(resolve_meta workflow_dispatch v1.0.0 stable '')" = "v1.0.0:false:true"
test "$(resolve_meta workflow_dispatch v0.0.1-fork.10 beta '')" = "v0.0.1-fork.10:true:false"
test "$(resolve_meta push '' '' v0.0.1-fork.10)" = "v0.0.1-fork.10:true:false"
test "$(resolve_meta push '' '' v1.0.0)" = "v1.0.0:false:true"
test "$(resolve_meta push '' '' v1.2.3-rc.1)" = "v1.2.3-rc.1:true:false"

validate_release_notes() {
  local RELEASE_VERSION=$1 NOTES_DIR=$2
  printf '%s\n' "$RELEASE_VERSION" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || return 1
  local notes="$NOTES_DIR/$RELEASE_VERSION.md"
  test -s "$notes" || return 1
  grep -q '^## Исправления$' "$notes" || return 1
  grep -q '^- ' "$notes" || return 1
}
NOTES_TMP="$CHECK_ROOT/notes"
mkdir -p "$NOTES_TMP"
printf '## Исправления\n- Fix something\n' > "$NOTES_TMP/v1.0.0.md"
validate_release_notes "v1.0.0" "$NOTES_TMP"
validate_release_notes "v0.0.1-fork.9" "docs/releases"
validate_release_notes "v0.0.1-fork.10" "docs/releases"
if validate_release_notes "invalid" "$NOTES_TMP"; then exit 1; fi
if validate_release_notes "v1.0" "$NOTES_TMP"; then exit 1; fi
if validate_release_notes "v1.0.0-missing" "$NOTES_TMP"; then exit 1; fi
printf 'Invalid notes' > "$NOTES_TMP/v2.0.0.md"
if validate_release_notes "v2.0.0" "$NOTES_TMP"; then exit 1; fi
printf '## Исправления\n' > "$NOTES_TMP/v3.0.0.md"
if validate_release_notes "v3.0.0" "$NOTES_TMP"; then exit 1; fi
printf 'PASS unified release pipeline and release metadata resolution\n'

