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
Without arguments, download the latest stable release of Shu1t3/XKeen-UI-Shu1t3.
For a prerelease, specify its exact tag with --tag.
HELP
}
fail() { printf '%s\n' "Ошибка: $*" >&2; exit 1; }
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
    file) [ -f "$SOURCE" ] || fail "Не найден $SOURCE."; cp "$SOURCE" "$WORK/new" ;;
    rollback)
        [ -f "$SOURCE/xkeen-ui" ] || fail 'В резервной копии нет бинарника.'
        cp "$SOURCE/xkeen-ui" "$WORK/new" ;;
    release)
        command -v curl >/dev/null 2>&1 || fail 'Не найден curl.'
        if [ "$TAG" = latest ]; then
            URL="https://github.com/$REPO/releases/latest/download/xkeen-ui-$ARCH"
        else
            URL="https://github.com/$REPO/releases/download/$TAG/xkeen-ui-$ARCH"
        fi
        printf '%s\n' "Загрузка $URL"
        curl -fL --connect-timeout 20 --max-time 300 -o "$WORK/new" "$URL" ||
            fail 'Релиз или бинарник недоступен. Используйте --tag либо --file.' ;;
esac
chmod 755 "$WORK/new"
MAGIC=$(od -An -tx1 -N4 "$WORK/new" | tr -d ' \n')
[ "$MAGIC" = 7f454c46 ] || fail 'Файл не является ELF-бинарником.'
# Check both the architecture/loader and the CLI before stopping the panel.
command -v timeout >/dev/null 2>&1 || fail 'Не найдена команда timeout.'
timeout 15 "$WORK/new" --version || fail 'Бинарник не запускается на этом роутере.'

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
