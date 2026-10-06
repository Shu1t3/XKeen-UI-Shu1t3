#!/bin/sh

GREEN=$'\033[32m'
GREEN_BOLD=$'\033[1;32m'
RED=$'\033[31m'
RED_BOLD=$'\033[1;31m'
NC=$'\033[0m'
NCN="$NC\n\n"
BLUE=$'\033[1;34m'
YELLOW=$'\033[1;33m'
CYAN=$'\033[1;96m'

ERROR="\n${RED} ❌${RED_BOLD}"
SUCCESS="\n${GREEN} ✅${GREEN_BOLD}"
INFO="\n${CYAN} ℹ️ "

XKEENUI_BIN="/opt/sbin/xkeen-ui"
XKEENUI_INIT="/opt/etc/init.d/S99xkeen-ui"
STATIC_DIR="/opt/share/www/XKeen-UI"
LIGHTTPD_INIT="/opt/etc/init.d/S80lighttpd"
LIGHTTPD_DIR="/opt/etc/lighttpd"
LIGHTTPD_CONF="$LIGHTTPD_DIR/conf.d/90-xkeenui.conf"

# Standalone mirror of backend/release-source.json; checked by backend tests.
UI_REPOSITORY='Shu1t3/XKeen-UI-Shu1t3'
RELEASE_CHANNEL=latest
LOCAL=false
case "${1:-latest}" in
  latest|stable|beta) RELEASE_CHANNEL="${1:-latest}" ;;
  *) printf 'Неизвестный канал: %s (latest, stable, beta)\n' "$1" >&2; exit 1 ;;
esac

spinner() {
  local pid=$1 msg=$2
  trap 'kill "$pid" 2>/dev/null; printf "\r${RED} ❌ ${NC}%s\033[K\n" "$msg"; printf "\033[?25h"; return 130' INT
  set -- ⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏
  printf "\033[?25l"
  while kill -0 "$pid" 2>/dev/null; do
    printf "\r${GREEN} %s ${NC} %s\033[K" "$1" "$msg"
    set -- "$@" "$1"
    shift
    usleep 100000
  done
  printf "\033[?25h"
  wait "$pid" && printf "\r ✔  %s\033[K\n" "$msg" || { printf "\r ❌ %s\033[K\n" "$msg"; return 1; }
}

get_arch() {
  case "$(opkg print-architecture)" in
    *aarch64*) ARCH='arm64-v8a' ;;
    *mipsel*)  ARCH='mips32le' ;;
    *mips*)    ARCH='mips32' ;;
    *) printf "${RED_BOLD}\n Не удалось определить архитектуру.${NCN}" >&2; exit 1 ;;
  esac
}

select_release_tag() {
  jq -er --arg channel "$RELEASE_CHANNEL" '[.[] | select(.draft != true) | select($channel == "latest" or ($channel == "beta" and .prerelease == true) or ($channel == "stable" and .prerelease != true))] | first | .tag_name | select(type == "string" and test("^v[0-9]+[.][0-9]+[.][0-9]+(-[0-9A-Za-z.-]+)?$"))'
}

# Transaction helpers run in a subshell so traps cannot leak into the menu.
# The stage and backup live beside the installed binary: rename is atomic.
validate_candidate() {
  local header
  header=$(dd if="$1" bs=20 count=1 2>/dev/null | od -b | awk 'NF > 1 {for (i=2;i<=NF;i++) printf "%s ", $i}') || return 1
  set -- $header
  [ "$#" -eq 20 ] || return 1
  [ "$1 $2 $3 $4" = '177 105 114 106' ] || return 1
  [ "$7" = 001 ] || return 1
  local class=$5 endian=$6
  shift 18
  case "$ARCH:$class:$endian:$1:$2" in
    arm64-v8a:002:001:267:000|mips32:001:002:000:010|mips32le:001:001:010:000) ;;
    *) printf 'Неверная архитектура ELF\n' >&2; return 1 ;;
  esac
  chmod 755 "$CANDIDATE" || return 1
  run_version_probe "$CANDIDATE"
}

run_version_probe() {
  "$1" --version > "$STAGE/version" 2>&1 &
  local probe=$!
  (
    sleeper=''
    trap 'kill "$sleeper" 2>/dev/null || :; exit' TERM INT HUP
    sleep 10 & sleeper=$!
    wait "$sleeper"
    kill -KILL "$probe" 2>/dev/null || :
  ) &
  local watchdog=$!
  wait "$probe"
  local result=$?
  kill "$watchdog" 2>/dev/null || :
  wait "$watchdog" 2>/dev/null || :
  [ "$result" -eq 0 ] && [ -s "$STAGE/version" ]
}

download_files() {
  local base_url="https://github.com/$UI_REPOSITORY/releases"
  local download_url="$base_url/latest/download"
  local bin_name="xkeen-ui-$ARCH"
  if [ "$LOCAL" != true ] && [ "$RELEASE_CHANNEL" != stable ]; then
    curl -fsS --connect-timeout 20 --max-time 60 "https://api.github.com/repos/$UI_REPOSITORY/releases?per_page=100" > "$STAGE/releases" || return 1
    local tag
    tag=$(select_release_tag < "$STAGE/releases") || return 1
    download_url="$base_url/download/$tag"
  fi
  if [ "$LOCAL" = true ] && [ -f "/opt/tmp/$bin_name" ]; then
    cp "/opt/tmp/$bin_name" "$CANDIDATE" || return 1
  else
    curl -fLsS --connect-timeout 20 --max-time 300 -o "$CANDIDATE" "$download_url/$bin_name" || return 1
  fi
  validate_candidate "$CANDIDATE"
}

service_healthy() {
  sleep 2
  "$XKEENUI_INIT" status >/dev/null 2>&1 && pidof xkeen-ui >/dev/null 2>&1
}

transaction_cleanup() {
  local result=$?
  trap - EXIT HUP INT TERM
  if [ "$COMMITTED" != true ] && [ "$TOUCHED" = true ]; then
    "$XKEENUI_INIT" stop >/dev/null 2>&1 || :
    killall -q -9 xkeen-ui >/dev/null 2>&1 || :
    local restored=true
    if [ "$HAD_BIN" = true ]; then
      cp -p "$STAGE/previous" "$STAGE/restore" && mv -f "$STAGE/restore" "$XKEENUI_BIN" || restored=false
    else
      rm -f "$XKEENUI_BIN" || restored=false
    fi
    if [ "$HAD_INIT" = true ]; then
      cp -p "$STAGE/previous-init" "$INIT_STAGE" && mv -f "$INIT_STAGE" "$XKEENUI_INIT" || restored=false
    else
      rm -f "$XKEENUI_INIT" || restored=false
    fi
    if [ "$restored" = true ] && [ "$WAS_RUNNING" = true ]; then
      "$XKEENUI_INIT" start >/dev/null 2>&1 && service_healthy || restored=false
    fi
    if [ "$restored" != true ]; then
      printf 'Восстановление требует вмешательства; резервные файлы: %s\n' "$STAGE" >&2
      KEEP_STAGE=true
    fi
  fi
  [ -z "$INIT_STAGE" ] || rm -f "$INIT_STAGE"
  [ "$KEEP_STAGE" = true ] || [ -z "$STAGE" ] || rm -rf "$STAGE"
  if [ "$KEEP_STAGE" != true ] && { [ ! -f "$LOCK/owner" ] || [ "$(cat "$LOCK/owner" 2>/dev/null)" = "$STAGE" ]; }; then
    rm -f "$LOCK/owner"
    rmdir "$LOCK" 2>/dev/null || :
  fi
  exit "$result"
}

replace_xkeenui() (
  local LOCK="$(dirname "$XKEENUI_BIN")/.xkeen-ui-update.lock" STAGE='' INIT_STAGE=''
  local COMMITTED=false TOUCHED=false KEEP_STAGE=false
  local HAD_BIN=false HAD_INIT=false WAS_RUNNING=false
  mkdir "$LOCK" 2>/dev/null || { printf 'Другая установка уже выполняется: %s\n' "$LOCK" >&2; exit 1; }
  trap transaction_cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
  STAGE=$(mktemp -d "${XKEENUI_BIN}.stage.XXXXXX") || exit 1
  printf '%s\n' "$STAGE" > "$LOCK/owner" || exit 1
  CANDIDATE="$STAGE/candidate"
  download_files || { printf 'Подготовка нового бинарника не удалась; текущая установка сохранена\n' >&2; exit 1; }
  if [ -f "$XKEENUI_BIN" ]; then
    cp -p "$XKEENUI_BIN" "$STAGE/previous" || exit 1
    HAD_BIN=true
  fi
  if [ -f "$XKEENUI_INIT" ]; then
    cp -p "$XKEENUI_INIT" "$STAGE/previous-init" || exit 1
    HAD_INIT=true
  fi
  INIT_STAGE=$(mktemp "${XKEENUI_INIT}.stage.XXXXXX") || exit 1
  if [ "$HAD_INIT" = true ]; then
    sed 's|^PROCS=/opt/sbin/xkeen-ui$|PROCS=xkeen-ui|' "$XKEENUI_INIT" > "$INIT_STAGE" || exit 1
    chmod 755 "$INIT_STAGE" || exit 1
  else
    create_xkeenui_init "$INIT_STAGE" || exit 1
  fi
  pidof xkeen-ui >/dev/null 2>&1 && WAS_RUNNING=true
  sync || exit 1
  TOUCHED=true
  if [ "$WAS_RUNNING" = true ]; then
    if [ "$HAD_INIT" = true ]; then
      "$XKEENUI_INIT" stop >/dev/null 2>&1 || exit 1
    fi
    killall -q -9 xkeen-ui >/dev/null 2>&1 || :
    pidof xkeen-ui >/dev/null 2>&1 && exit 1
  fi
  mv -f "$CANDIDATE" "$XKEENUI_BIN" || exit 1
  mv -f "$INIT_STAGE" "$XKEENUI_INIT" || exit 1
  sync || exit 1
  "$XKEENUI_INIT" start >/dev/null 2>&1 && service_healthy || exit 1
  COMMITTED=true
)

install_xkeenui() {
  [ -f "/opt/tmp/xkeen-ui-$ARCH" ] && LOCAL=true
  replace_xkeenui || return 1
  finish_setup "установлен"
}

update_xkeenui() {
  [ -f "$XKEENUI_BIN" ] || { printf "${ERROR} Ошибка: XKeen UI не установлен!${NCN}"; return 1; }
  replace_xkeenui || return 1
  finish_setup "обновлен"
}

uninstall_xkeenui() {
  printf "\n Данное действие ${RED_BOLD}удалит${NC} XKeen UI, его файлы и зависимости.\n\n"
  read -p " Продолжить? [y/N]: " response < /dev/tty
  response=$(printf '%s' "$response" | tr -cd 'YyNn')
  case "$response" in
    [Yy]) printf "${INFO} Начинаем удаление...${NCN}";;
    *) printf "${ERROR} Отмена операции.${NCN}"; exit 1;;
  esac

  (
    if [[ -f "$LIGHTTPD_INIT" && -f "$LIGHTTPD_CONF" ]]; then
      if $LIGHTTPD_INIT status &>/dev/null; then
          $LIGHTTPD_INIT stop &>/dev/null || :
          opkg remove --autoremove --force-removal-of-dependent-packages lighttpd &>/dev/null
          rm -rf $LIGHTTPD_DIR
      fi
    fi
    if [ -f $XKEENUI_INIT ]; then
      if $XKEENUI_INIT status &>/dev/null; then
        $XKEENUI_INIT stop &>/dev/null || :
        killall -q -9 xkeen-ui || :
      fi
    fi
  ) &
  spinner $! "Остановка XKeen UI..."

  (rm -rf $STATIC_DIR; rm -f $XKEENUI_BIN $XKEENUI_INIT) &
  spinner $! "Удаление файлов XKeen UI..."
  printf "${SUCCESS} Удаление XKeen-UI завершено${NCN}"
}

finish_setup() {
  local ip=$(ip -4 a s br0 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p'); ip=${ip:-"IP_Роутера"}
  local port=$(sed -n 's/.*-p \([0-9]*\).*/\1/p' $XKEENUI_INIT 2>/dev/null); port=${port:-1000}

  printf "${SUCCESS} XKeen UI успешно $1!${NCN}"
  printf " Панель доступна по адресу: ${GREEN_BOLD}http://$ip:$port${NC}\n\n"
}

legacy_installation_check() {
  if [ -f "$LIGHTTPD_CONF" ]; then
    $LIGHTTPD_INIT status &>/dev/null && $LIGHTTPD_INIT stop
    rm -f "$LIGHTTPD_CONF"
    printf "${YELLOW}\n Веб-сервер lighttpd для работы XKeen UI более не используется.\n${NC}"
    read -p " Удалить его? [Y/n]: " response < /dev/tty
    response=$(printf '%s' "$response" | tr -cd 'YyNn')
    case "$response" in
      [Nn]) return;;
      *) opkg remove --autoremove --force-removal-of-dependent-packages lighttpd; rm -rf $LIGHTTPD_DIR;;
    esac
  fi
}

create_xkeenui_init() {
  cat << EOF > "${1:-$XKEENUI_INIT}" || return 1
#!/bin/sh

ENABLED=yes
PROCS=xkeen-ui
ARGS="-p 1000"
PREARGS=""
DESC="\$PROCS"
PATH=/opt/sbin:/opt/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

. /opt/etc/init.d/rc.func
EOF
  chmod 755 "${1:-$XKEENUI_INIT}"
}

get_status() {
  [ ! -f "$XKEENUI_BIN" ] && printf "Статус панели: ${RED_BOLD}не установлена${NC}" && return

  local version=$($XKEENUI_BIN -v 2>/dev/null | awk 'NR==1{print $3}')
  local status="${RED_BOLD}не запущена"

  version=${version:-"N/A"}

  pidof xkeen-ui &>/dev/null && status="${GREEN_BOLD}запущена"
  printf "Статус панели: $status ${NC}[$version]"
}

clear
get_arch
printf "${CYAN}"
cat <<'EOF'
   _  __  __ __                       __  __ ____
  | |/ / / //_/___   ___   ____      / / / //  _/
  |   / / ,<  / _ \ / _ \ / __ \    / / / / / /
 /   | / /| |/  __//  __// / / /   / /_/ /_/ /
/_/|_|/_/ |_|\___/ \___//_/ /_/    \____//___/
EOF

printf "${NC}\n$(get_status)\n"
printf "Архитектура: ${GREEN_BOLD}$ARCH\n"
printf "\nДобро пожаловать! Выберите действие:${NCN}"
printf "  1. Установить/переустановить\n"
printf "  2. Обновить\n"
printf "  3. Удалить\n"
printf "\n  0. Выйти\n\n"

read -p "${GREEN_BOLD}>: ${NC}" response < /dev/tty

case $response in
  1) install_xkeenui;;
  2) update_xkeenui;;
  3) uninstall_xkeenui;;
  0) echo; exit;;
  *) printf "${ERROR} Неверный выбор.${NCN}"; exit 1;;
esac
