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
