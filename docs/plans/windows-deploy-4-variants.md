# Запуск agentgrid-нод на Windows — 4 варианта

> Дата: 2026-08-16. Целевая машина: Windows 10 Pro 22H2 (19045), Intel i5-4670 (4C/4T), 16 ГБ RAM.
> Проверено на машине: VT-x включён в BIOS (`VirtualizationFirmwareEnabled: True`), гипервизор не занят
> (`HypervisorPresent: False`), Hyper-V/WSL2/Docker не установлены — «чистый лист».
>
> Пошаговый чек-лист выполнения: [`windows-deploy-checklist.md`](./windows-deploy-checklist.md)

Проект — Linux-only (матрица поддержки в README: «enforced»), поэтому на Windows Linux-окружение
живёт «где-то рядом». Ниже 4 варианта, где именно, — от самого дешёвого к самому тяжёлому
по ресурсам и зависимостям.

## Сводка

| | 1: WSL2 | 2: WSL2+podman | 3: Hyper-V VM | 4: Docker Desktop |
|---|---|---|---|---|
| RAM (idle, сверх Windows) | ~0.5–1 ГБ | +1–2 ГБ диск под образы | 3–4 ГБ статически | 1.5–2.5 ГБ |
| Зависимости на Windows | 2 компонента + дистрибутив | те же + пакеты в Linux | роль Hyper-V + ISO | пакет Docker Desktop |
| Песочница агентов | нет (sandbox=none) | **да (rootless podman)** | да (podman/docker в VM) | контейнер ноды как граница |
| Близость к поддерживаемой конфигурации | высокая | высокая | **максимальная** | demo-путь |
| Время до работающей ноды | ~30 мин | +15 мин к вар. 1 | ~1–1.5 ч | ~40 мин |

Рекомендуемая траектория: **вариант 1 → расширить до варианта 2** при появлении недоверенных
задач (это DropIn-файл systemd-юнита, не переустановка). Вариант 4 — параллельно для быстрого
обзора всей системы с web-UI. Вариант 3 — только при принципиальном запрете WSL.
На 4-поточном CPU не запускать больше одного варианта одновременно.

## Общая часть (для всех вариантов)

**Артефакты релиза v0.3.1** — статические musl-бинарники, не требуют Rust-тулчейна и glibc:
`agentgrid-control-plane`, `agentgrid-node-daemon`, `ag` (CLI), `adapter-mock`, `adapter-claude`,
`adapter-opencode` + `checksums.txt`. Образ ноды на GHCR: `ghcr.io/<owner>/agentgrid-node-daemon:v0.3.1`
(amd64+arm64; owner = владелец репозитория).

**Топология.** Простейшая схема — control-plane и нода в одном Linux-окружении: CP слушает `:7800`,
нода ходит на `http://127.0.0.1:7800`. Единственный порт наружу — 7800.

**Флоу первого запуска** (одинаков везде, различается только «где»):

1. Запустить CP: `AGENTGRID_JWT_SECRET=<≥32 байта> AGENTGRID_LISTEN=0.0.0.0:7800 agentgrid-control-plane`
2. Взять one-time setup-токен из логов CP → `POST /v1/auth/setup` → JWT админа
3. `POST /v1/nodes/enrollment-token` → enrollment-токен ноды
4. Запустить ноду: `AGENTGRID_SERVER`, `AGENTGRID_ENROLL_TOKEN`, `AGENTGRID_NODE_NAME`
5. Проверка: `/health/ready`, mock-задача (скрипты в `OPS-STARTER.md`), `ag nodes doctor <id>`

## Вариант 1 — WSL2 + статические бинарники (минимум ресурсов)

**Когда:** доверенные агенты (свои промпты, свои репозитории), `sandbox=none`.

- Включить WSL2 (`wsl --install -d Ubuntu`), включить systemd в `/etc/wsl.conf`
- Ограничить аппетиты через `%UserProfile%\.wslconfig` (`memory=3GB`, `processors=3`)
- Скачать release v0.3.1 (musl), сверить checksums
- Установить CP и ноду штатными скриптами (`deploy/install-control-plane.sh`,
  `deploy/install-node.sh --server http://127.0.0.1:7800 --token <…> --staging ./release-bin`)
- Всё живёт в одном WSL2-дистрибутиве, workspace/repo-roots на ext4 (не `/mnt/*`)

**Откат:** `wsl --unregister Ubuntu`.

## Вариант 2 — WSL2 + podman (песочница)

**Когда:** недоверенные агенты / задачи из общей очереди. Строится поверх варианта 1.

- `sudo apt install podman podman-docker` (алиас `docker`→`podman`: spawn-путь проекта
  хардкодит `docker run` — `crates/node-daemon/src/sandbox.rs`)
- DropIn к systemd-юниту: `AGENTGRID_SANDBOX=podman`, `AGENTGRID_SANDBOX_RUNTIME=podman`,
  `AGENTGRID_SANDBOX_NETWORK=none`, `AGENTGRID_SANDBOX_IMAGE=ubuntu:24.04` +
  `AGENTGRID_SANDBOX_IMAGE_DIGEST=<sha256:…>` (пин обязателен)
- Образ заранее `podman pull` в локальный стор; стартовые пробы сами проверят рантайм и адаптер в образе
- Overhead холодного старта песочницы: ~1–2 с (см. `docs/deploy/sandbox-benchmark.md`)

**Откат:** удалить DropIn → возврат к варианту 1.

## Вариант 3 — Hyper-V VM (без WSL)

**Когда:** запрет WSL, или нужна конфигурация, на 100% совпадающая с поддерживаемой матрицей
(внутри VM — стоковый Ubuntu-хост). Цена: статические 3–4 ГБ RAM, 30–40 ГБ диска, обслуживание полной ОС.

- `Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All` + reboot
- `New-VM -Generation 2 -MemoryStartupBytes 4GB …` + Ubuntu 24.04 Server ISO, 3 vCPU, чекпоинт перед экспериментами
- Внутри VM — шаги варианта 1 без каких-либо WSL-специфичных отличий

**Откат:** `Remove-VM` + удалить VHDX; компонент Hyper-V отключается обратной командой.

## Вариант 4 — Docker Desktop (Hyper-V backend), compose-путь

**Когда:** быстрый demo/eval всей системы (CP + 2 ноды + web-UI). Нюанс: нода в контейнере с
`cap-drop=ALL`, docker-песочницы внутри нет (`AGENTGRID_SANDBOX` не задан) — границей изоляции
служит сам контейнер ноды. Это demo-режим, не боевой для недоверенных агентов.

- Установить Docker Desktop; в Settings → General **снять** «Use the WSL 2 based engine»
- `docker build -f Dockerfile.control-plane -t ag-cp:test .` и `-f Dockerfile.node-daemon -t ag-node:test .`
- `bash deploy/compose/up.sh` — сам сгенерирует секреты, прочитает setup-токен, сделает bootstrap
- Проверка: `/health/ready`, `docker ps` (`ag-control-plane-1`, `ag-node-1-1`, `ag-node-2-1`)

**Откат:** `bash deploy/compose/down.sh` + uninstall Docker Desktop.
