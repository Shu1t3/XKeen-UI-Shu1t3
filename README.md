<div align="center">
  <img width="128" height="128" src="frontend/src/assets/favicon.png" alt="XKeen UI">

<h1>XKeen UI</h1>

<p>
  Легковесная панель управления сервисом <b>XKeen</b> для роутеров Keenetic/Netzraze
  <br>
    <a href="https://github.com/zxc-rv/XKeen-UI/wiki">Wiki (WIP)</a>
    ·
    <a href="https://github.com/zxc-rv/XKeen-UI/wiki/FAQ">FAQ</a>
</p>
  
![preview](preview.gif)

</div>
<br>  
  
## Переход с апстрима на форк

На роутере с установленной панелью выполните от root:

```sh
curl -fL https://raw.githubusercontent.com/Shu1t3/XKeen-UI-Shu1t3/main/scripts/switch-to-fork.sh -o /opt/tmp/switch-to-fork.sh
sh /opt/tmp/switch-to-fork.sh --tag v0.0.1-fork.1
```

Указанный тег — пример: сначала опубликуйте релиз форка с бинарником для вашего
роутера. Для prerelease нужен точный `--tag`; без аргументов скрипт ищет latest
стабильный релиз. Сборку запускайте workflow **Build Rust binaries** в Actions
форка, задав версию релиза. Сборка должна быть без `local-dev`.

Для тестирования локальной сборки загрузите её на роутер (например, через SCP)
и укажите путь:

```sh
sh /opt/tmp/switch-to-fork.sh --file /opt/tmp/xkeen-ui-arm64-v8a
```

Имена артефактов: `xkeen-ui-arm64-v8a` (aarch64), `xkeen-ui-mips32le`
(mipsel), `xkeen-ui-mips32` (mips). Скрипт определяет архитектуру через Entware,
проверяет ELF и запуск `--version`, затем сохраняет старый бинарник, init-скрипт
и настройки панели в закрытую директорию `/opt/var/backups/xkeen-ui-switch`.
Скачивание и проверки проходят до остановки панели. Ошибка замены или запуска
вызывает автоматический откат; настройки и init-скрипт не заменяются.
Конфигурации Xray/Mihomo и сервис XKeen не затрагиваются.

Путь к резервной копии и команду ручного отката скрипт выводит на экран:

```sh
sh /opt/tmp/switch-to-fork.sh --rollback /opt/var/backups/xkeen-ui-switch/ИМЯ_КОПИИ
```

Откат восстанавливает только бинарник. Копии настроек сохранены отдельно для
ручного восстановления при необходимости. После перехода проверьте панель на
прежнем адресе: проверка процесса не гарантирует работу всех функций.
Новые сборки используют релизы форка для встроенного обновления панели.

## ✨ Особенности

- 🚀 Установка одной командой
- 📉 Низкое потребление ресурсов
- ⛔ Никаких зависимостей кроме XKeen
- ⚓️ Порт по умолчанию: 1000 (меняется в `/opt/etc/init.d/S99xkeen-ui`)
- 🎛️ Управление сервисом: `/opt/etc/init.d/S99xkeen-ui start|restart|stop|status`

&nbsp;

## ⚙️ Функционал

- 📊 Мониторинг и управление сервисом
- 📝 Редактирование конфигураций с валидацией и форматированием
- 📜 Просмотр логов с автообновлением и фильтрацией
- 🕒 Выбор часового пояса в логах
- 🔀 Переключение/установка/обновление ядер Xray и Mihomo
- 🔗 Генерация аутбаундов из ссылок (также доступно [отдельно по ссылке](https://zxc-rv.github.io/XKeen-UI/Outbound_Generator/))
- 🩻 Сканирование dat файлов
- ⚔️ Clash API реализация для Mihomo
- 📡 Применение и бэкап конфигов на выбранные роутеры (Настройки -> Плагины)

&nbsp;

## ⚡️ Быстрый старт (установка/обновление/удаление)

### Cтабильная/Latest версия

```SH
curl https://raw.githubusercontent.com/Shu1t3/XKeen-UI-Shu1t3/main/setup.sh | sh
```

### Бета/Pre-release версия

```SH
curl https://raw.githubusercontent.com/Shu1t3/XKeen-UI-Shu1t3/main/setup.sh | sh -s -- beta
```

<br>

## 🌐 Доступ извне

Панель разработана для работы в локальной сети. В случае необходимости использовать панель за пределами локальной сети рекомендуется использовать VPN протоколы, такие как SSTP или Wireguard.
Также поддерживается работа с KeenDNS, для этого нужно в веб-конфигураторе создать саб-домен с протоколом HTTP и портом панели. Обязательно используйте авторизацию и другие меры безопасности!
> [!CAUTION]
> Открытие доступа к панели из интернета без должных мер безопасности может привести к взлому роутера или утечке данных.
> За данные последствия автор проекта ответственность не несет.
<br>
  
## 🪙 Понравился проект? Поддержи разработку

- [**Cloudtips**](https://pay.cloudtips.ru/p/24b4c4b6)

- Банковская карта: `2204 3203 4161 6409`
  
&nbsp;

## 🙏 Благодарности

- [**Skrill0/XKeen**](https://github.com/Skrill0/XKeen)  
- [**jameszeroX/XKeen**](https://github.com/jameszeroX/XKeen)  
- [**Anonym-tsk/nfqws-keenetic**](https://github.com/Anonym-tsk/nfqws-keenetic)
