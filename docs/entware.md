# Установка Entware на Keenetic

[← README](../README.md#️-быстрый-старт-установкаобновлениеудаление)

Entware подготавливает окружение `/opt` для установки XKeen и панели.
Источник: [каталог Entware / OPKG на Artnet](https://forum.artnet.biz/threads/entware-katalog-gotovykh-reshenii-opkg.426/).
Полный перечень моделей приведён в теме; сверяйте индекс KN, а не только название.

## Выбор установщика

| Архитектура | Примеры моделей из темы | Архив |
| --- | --- | --- |
| mipsel | Giga KN-1010/1011, Ultra KN-1810, Hopper KN-3810 | [mipsel-installer.tar.gz](https://bin.entware.net/mipselsf-k3.4/installer/mipsel-installer.tar.gz) |
| mips | Giga SE KN-2410, Ultra SE KN-2510, Hopper DSL KN-3610 | [mips-installer.tar.gz](https://bin.entware.net/mipssf-k3.4/installer/mips-installer.tar.gz) |
| aarch64 | Peak KN-2710, Ultra KN-1811, Giga KN-1012, Hopper KN-3811, Hopper SE KN-3812 | [aarch64-installer.tar.gz](https://bin.entware.net/aarch64-k3.10/installer/aarch64-installer.tar.gz) |

## Во внутреннюю память

Этот вариант доступен только на поддерживаемых моделях. Установите компонент
KeeneticOS «Поддержка открытых пакетов» (OPKG) и откройте CLI:
[http://192.168.1.1/a](http://192.168.1.1/a) (замените адрес, если он изменён).

Выполните **одну** команду для своей архитектуры:

**mipsel:**

```text
opkg disk storage:/ https://bin.entware.net/mipselsf-k3.4/installer/mipsel-installer.tar.gz
```

**mips:**

```text
opkg disk storage:/ https://bin.entware.net/mipssf-k3.4/installer/mips-installer.tar.gz
```

**aarch64:**

```text
opkg disk storage:/ https://bin.entware.net/aarch64-k3.10/installer/aarch64-installer.tar.gz
```

Дождитесь завершения установки в журнале роутера. Синтаксис `storage:/`
соответствует официальным инструкциям Keenetic для
[mips](https://support.keenetic.com/hero-dsl/kn-2410/en/18482-installing-opkg-entware-in-the-router-s-internal-memory.html) и
[aarch64](https://support.keenetic.com/titan/kn-1811/en/18482-installing-opkg-entware-in-the-router-s-internal-memory.html).

## На USB-накопитель вручную

1. Сохраните данные: форматирование удалит их. Подготовьте раздел EXT4, например через [Paragon Partition Manager](https://www.paragon-software.com/free/pm-express/).
2. Подключите накопитель к роутеру. В веб-интерфейсе откройте его и создайте каталог `install`.
3. Загрузите туда подходящий архив из таблицы.
4. Выберите накопитель в разделе OPKG и сохраните настройки. Следите за журналом установки.

## Доступ и обновление

По данным темы, первоначальный SSH-доступ: `root`, пароль `keenetic`, порт `222`.

```sh
ssh -p 222 root@192.168.1.1
passwd
```

Если SSH недоступен, в CLI роутера выполните последовательно:

```text
exec sh
exec /opt/etc/init.d/S51dropbear restart
```

Для Telnet в теме указаны порт `23`, пользователь `admin` и пароль веб-интерфейса;
переход в Entware — `exec sh`.

В оболочке Entware обновите список пакетов; при необходимости обновите пакеты:

```sh
opkg update
opkg upgrade
```

Затем установите XKeen и перейдите к [установке панели в README](../README.md).
Приведённые команды не устанавливают XKeen или XKeen UI.
