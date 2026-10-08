# Проверки и сборка бинарников

`backend/Cargo.lock` и `frontend/bun.lock` хранятся в Git. Rust-команды CI
используют `--locked`, установка frontend — `--frozen-lockfile`. Изменение
зависимостей выполняется отдельно с проверкой diff lockfile; сборка релиза
не разрешает зависимости заново.

Rust для тестов, Clippy и ARM64 закреплён в `rust-toolchain.toml` на 1.99.0.
MIPS/MIPSel требуют nightly-2026-10-06. В build-rust.yml закреплены Bun 1.4.2,
Node 26.7.0, commit cross и commit SHA всех используемых Actions. Образы
трёх архитектур в `backend/Cross.toml` закреплены по digest. Cross устанавливается
в отдельный временный каталог из закреплённого commit с его собственным lockfile;
ранее восстановленный бинарник cross из cache не используется.

Версия панели передаётся сборке через `XKEEN_UI_VERSION`, а не редактирование
Cargo.toml. Без переменной сохраняется `v<package.version>`. Переменная передаётся
в контейнер cross. Cargo.lock и версии зависимостей одинаковы при проверках
и при сборке каждой архитектуры. Embedded frontend использует deterministic
timestamps; SOURCE_DATE_EPOCH в CI берётся из времени commit.

## Обязательные проверки

- Push в main и pull request с изменениями входов сборки запускают проверки
  установщика/обновления, frontend и backend, затем все три release-сборки.
- Frontend: frozen install, ESLint с нулём предупреждений, regression tests,
  production build.
- Backend: Clippy всех targets с `-D warnings` и tests в обычном режиме и
  local-dev. Набор включает auth/control/config/update/WS-регрессии.
- ARM64, MIPS, MIPSel собираются `--locked --release` без local-dev.
  Каждая сборка запускает свой бинарник через runner cross с `--version`
  и проверяет заданную версию. Изменение Cargo.lock запрещено.

Локальные команды из корня проекта:

```sh
cargo clippy --locked --manifest-path backend/Cargo.toml --all-targets -- -D warnings
cargo clippy --locked --manifest-path backend/Cargo.toml --all-targets --features local-dev -- -D warnings
cargo test --locked --manifest-path backend/Cargo.toml
cargo test --locked --manifest-path backend/Cargo.toml --features local-dev
cd frontend
bun install --frozen-lockfile
bunx eslint . --max-warnings 0
bun test tests
bun run build
```

## Публикация

Релизная автоматизация объединена в едином Rust-конвейере `build-rust.yml`;
устаревший workflow сборки Go удалён. Релиз может быть запущен двумя способами:

1. **Push тега `v*`:** автоматически запускает проверки и сборки для всех
   трёх архитектур роутеров. Канал определяется форматом тега: версии с
   суффиксом дефиса (например, `v0.0.1-fork.10`, `v1.0.0-beta.1`) публикуются
   как prerelease (бета-канал), а теги без дефиса (например, `v1.0.0`) — как
   полноправный стабильный релиз (`prerelease=false`, отметка `latest`).
2. **Workflow dispatch:** позволяет вручную указать `version=<tag>`, выбрать
   канал `channel` (`beta` по умолчанию или `stable`), а также флаг
   `publish` (`true`/`false`).

Для любого релиза обязателен файл описания `docs/releases/<tag>.md` с заголовком
`## Исправления` и перечнем изменений; его наличие и формат проверяются на
первом шаге до сборки бинарников. Его содержимое становится описанием GitHub Releases.
После публикации CI сверяет SHA-256 опубликованных GitHub-ассетов с собранными бинарниками.

Для проверочного прогона выбранной версии без публикации укажите `publish=false`
в `workflow_dispatch`. Push в ветку `main` и pull requests собирают и
smoke-тестируют бинарники с версией `v0.0.0` без публикации. Stable-канал
по умолчанию не выбирается и требует явного указания или стабильного тега.

Закрепление входов фиксирует набор dependencies/toolchain/images/actions,
но не является доказательством побайтовой идентичности независимых пересборок:
host GitHub runner и инфраструктура остаются внешними. Smoke-test через QEMU
не заменяет проверку сервиса на реальном ARM/MIPS-роутере. Обновление любого
pin требует повторного прохождения всех архитектур.
