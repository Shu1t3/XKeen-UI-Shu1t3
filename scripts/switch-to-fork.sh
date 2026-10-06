#!/bin/sh
# Run on the router as root. Only the panel binary is replaced.
set -eu

REPO=Shu1t3/XKeen-UI-Shu1t3
BIN=/opt/sbin/xkeen-ui
INIT=/opt/etc/init.d/S99xkeen-ui
BACKUP_ROOT=/opt/var/backups/xkeen-ui-switch

usage() {
    cat <<'HELP'
Usage: sh switch-to-fork.sh [--tag TAG | --file /path/to/binary | --rollback BACKUP_DIR]
Without arguments, download the newest published fork release, including prereleases.
All remote releases (including --tag) require jq and sha256sum.
--file and --rollback explicitly trust the supplied local binary.
Use --tag to select an exact release.
HELP
}
fail() { printf '%s\n' "Ошибка: $*" >&2; exit 1; }
# Use only shell builtins and sleep: minimal BusyBox may not provide timeout.
check_version() (
    "$1" --version &
    version_pid=$!
    (
        sleep 15 &
        timer_pid=$!
        trap 'kill "$timer_pid" 2>/dev/null || :; exit 0' HUP INT TERM
        wait "$timer_pid" || exit 0
        kill -KILL "$version_pid" 2>/dev/null || :
    ) &
    watchdog_pid=$!
    trap 'kill "$version_pid" "$watchdog_pid" 2>/dev/null || :; exit 1' HUP INT TERM
    if wait "$version_pid"; then result=0; else result=$?; fi
    kill "$watchdog_pid" 2>/dev/null || :
    wait "$watchdog_pid" 2>/dev/null || :
    exit "$result"
)
select_published_tag() {
    jq -er '[.[] | select(.draft != true)] | first | .tag_name | select(type == "string" and test("^v[0-9]+[.][0-9]+[.][0-9]+(-[0-9A-Za-z.-]+)?$"))' "$@"
}
# Metadata is trusted only when obtained directly from GitHub over HTTPS.
# Do not follow redirects or inherit environment proxy settings for this request.
get_trusted_release() {
  local status
  status=$(curl -q -fsS --noproxy '*' --proto '=https' --connect-timeout 20 --max-time 60 \
    -o "$2" --write-out '%{http_code}' "$1") || return 1
  [ "$status" = 200 ] || { printf 'GitHub API вернул неожиданный HTTP-статус: %s\n' "$status" >&2; return 1; }
}
release_asset_digest() {
  jq -er --arg tag "$2" --arg name "$3" '
    select(.draft == false and .tag_name == $tag)
    | [.assets[] | select(.name == $name)] | select(length == 1) | .[0] | select(.state == "uploaded") | .digest
    | select(type == "string" and test("^sha256:[0-9a-fA-F]{64}$"))
    | .[7:] | ascii_downcase' "$1"
}
verify_release_file() {
  local actual
  actual=$(sha256sum "$1") || return 1
  actual=${actual%% *}
  [ "$actual" = "$2" ] || { printf 'SHA-256 бинарника не совпадает с доверенными метаданными GitHub\n' >&2; return 1; }
}

MODE=release
TAG=latest
SOURCE=
case "${1:-}" in
    '') ;;
    --tag|--file|--rollback)
        [ "$#" -eq 2 ] || { usage; exit 1; }
        case "$1" in
            --tag) TAG=$2; [ -n "$TAG" ] || fail 'Пустой тег.'
                case "$TAG" in *[!a-zA-Z0-9._-]*) fail 'Недопустимый тег.';; esac ;;
            --file) MODE=file; SOURCE=$2 ;;
            --rollback) MODE=rollback; SOURCE=$2 ;;
        esac ;;
    --help|-h) usage; exit 0 ;;
    *) usage; exit 1 ;;
esac
[ "$(id -u)" = 0 ] || fail 'Запустите скрипт от root.'
[ -f "$BIN" ] && [ ! -L "$BIN" ] || fail "Не найден обычный файл $BIN. Сначала установите панель."
[ -x "$INIT" ] && [ ! -L "$INIT" ] || fail "Не найден обычный init-скрипт $INIT."
command -v pidof >/dev/null 2>&1 || fail 'Не найдена команда pidof.'
command -v opkg >/dev/null 2>&1 || fail 'Не найден Entware (opkg).'
case "$(opkg print-architecture)" in
    *aarch64*) ARCH=arm64-v8a ;;
    *mipsel*) ARCH=mips32le ;;
    *mips*) ARCH=mips32 ;;
    *) fail 'Архитектура не поддерживается.' ;;
esac

# Staging and replacement are on the same filesystem as the installed binary.
LOCK=/opt/sbin/.xkeen-ui-switch.lock
mkdir "$LOCK" 2>/dev/null || fail "Другой переход уже выполняется (lock: $LOCK)."
WORK=$(mktemp -d /opt/sbin/.xkeen-ui-switch.XXXXXX) || { rmdir "$LOCK"; exit 1; }
CHANGED=0
STOPPED=0
WAS_RUNNING=0
BACKUP=
if pidof xkeen-ui >/dev/null 2>&1; then WAS_RUNNING=1; fi
cleanup() {
    result=$?
    trap - EXIT HUP INT TERM
    if [ "$result" -ne 0 ] && [ "$STOPPED" -eq 1 ]; then
        printf '%s\n' 'Восстанавливаем предыдущую панель...' >&2
        if [ "$CHANGED" -eq 1 ]; then
            "$INIT" stop >/dev/null 2>&1 || :
            if cp -p "$BACKUP/xkeen-ui" "$WORK/restore" && mv -f "$WORK/restore" "$BIN"; then
                sync
            else
                printf '%s\n' "Не удалось восстановить бинарник. Резервная копия: $BACKUP" >&2
            fi
        fi
        if [ "$WAS_RUNNING" -eq 1 ]; then "$INIT" start || :; fi
    fi
    rm -rf "$WORK"
    rmdir "$LOCK"
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM

case "$MODE" in
    file) printf '%s\n' 'Локальный файл: источник доверия подтверждается вручную.'; [ -f "$SOURCE" ] || fail "Не найден $SOURCE."; cp "$SOURCE" "$WORK/new" ;;
    rollback)
        printf '%s\n' 'Откат из локальной копии: источник доверия подтверждается вручную.'
        [ -f "$SOURCE/xkeen-ui" ] || fail 'В резервной копии нет бинарника.'
        cp "$SOURCE/xkeen-ui" "$WORK/new" ;;
    release)
        command -v curl >/dev/null 2>&1 || fail 'Не найден curl.'
        command -v jq >/dev/null 2>&1 || fail 'Для проверки релиза установите jq: opkg install jq.'
        command -v sha256sum >/dev/null 2>&1 || fail 'Для проверки релиза необходим sha256sum.'
        if [ "$TAG" = latest ]; then
            get_trusted_release "https://api.github.com/repos/$REPO/releases?per_page=100" "$WORK/releases.json" ||
                fail 'Не удалось напрямую получить релизы GitHub. Установка запрещена.'
            TAG=$(select_published_tag "$WORK/releases.json") || fail 'Нет опубликованного релиза форка.'
        fi
        get_trusted_release "https://api.github.com/repos/$REPO/releases/tags/$TAG" "$WORK/release.json" ||
            fail 'Не удалось напрямую получить метаданные релиза GitHub. Установка запрещена.'
        DIGEST=$(release_asset_digest "$WORK/release.json" "$TAG" "xkeen-ui-$ARCH") ||
            fail 'Нет доверенного SHA-256 для выбранного релиза. Установка запрещена.'
        URL="https://github.com/$REPO/releases/download/$TAG/xkeen-ui-$ARCH"
        printf '%s\n' "Загрузка $URL"
        curl -fL --connect-timeout 20 --max-time 300 -o "$WORK/new" "$URL" ||
            fail 'Релиз или бинарник недоступен. Используйте --tag либо --file.'
        verify_release_file "$WORK/new" "$DIGEST" || fail 'Проверка целостности не пройдена.' ;;
esac
chmod 755 "$WORK/new"
# Minimal router BusyBox od supports -b, but may lack -A, -t and -N.
MAGIC=$(dd if="$WORK/new" bs=4 count=1 2>/dev/null | od -b | awk 'NR == 1 { print $2 $3 $4 $5 }')
[ "$MAGIC" = 177105114106 ] || fail 'Файл не является ELF-бинарником.'
# Check both the architecture/loader and the CLI before stopping the panel.
check_version "$WORK/new" || fail 'Бинарник не запускается на этом роутере.'

mkdir -p "$BACKUP_ROOT"
chmod 700 "$BACKUP_ROOT"
BACKUP=$(mktemp -d "$BACKUP_ROOT/$(date +%Y%m%d-%H%M%S).XXXXXX")
cp -p "$BIN" "$BACKUP/xkeen-ui"
cp -p "$INIT" "$BACKUP/S99xkeen-ui"
for CONFIG in /opt/etc/xkeen/xkeen-ui.json /opt/share/www/XKeen-UI/config.json; do
    if [ -f "$CONFIG" ]; then
        case "$CONFIG" in
            */xkeen-ui.json) cp -p "$CONFIG" "$BACKUP/xkeen-ui.json" ;;
            */config.json) cp -p "$CONFIG" "$BACKUP/config-legacy.json" ;;
        esac
    fi
done
printf '%s\n' "Резервная копия: $BACKUP"
# An unsuccessful stop can still have stopped the process, so arm recovery first.
STOPPED=1
"$INIT" stop || fail 'Не удалось остановить панель.'
if pidof xkeen-ui >/dev/null 2>&1; then fail 'Панель всё ещё работает.'; fi
CHANGED=1
mv -f "$WORK/new" "$BIN"
sync
"$INIT" start || fail 'Не удалось запустить панель.'
sleep 3
pidof xkeen-ui >/dev/null 2>&1 || fail 'Процесс панели завершился после запуска.'
STOPPED=0
printf '%s\n' 'Панель запущена. Проверьте веб-интерфейс на прежнем адресе.'
printf '%s\n' "Откат: sh switch-to-fork.sh --rollback $BACKUP"
printf '%s\n' 'Откат заменяет только бинарник; текущие настройки сохраняются.'
