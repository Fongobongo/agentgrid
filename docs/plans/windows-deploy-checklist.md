# Windows-деплой agentgrid — пошаговый чек-лист

> Дата: 2026-08-16. Обзорный план с обоснованиями и сравнением: [`windows-deploy-4-variants.md`](./windows-deploy-4-variants.md)
>
> **Как пользоваться:** отмечайте выполненные шаги `[x]`. Дорожки независимы, кроме B (строится
> поверх A). На 4-поточном CPU **не запускайте больше одной дорожки одновременно**.
> Состояние машины на момент составления: Win10 Pro 22H2, VT-x включён в BIOS, Hyper-V/WSL2/Docker не установлены.

---

## Этап 0 — Общая подготовка (нужен для дорожек A, B, C; для D не обязателен)

### 0.1 Проверка машины

- [ ] Редакция Windows — Pro или выше (для Hyper-V):
  ```powershell
  (Get-CimInstance Win32_OperatingSystem).Caption
  ```
- [ ] Виртуализация включена в BIOS (ожидаемо `True`; если `False` — включить VT-x в BIOS):
  ```powershell
  (Get-CimInstance Win32_Processor).VirtualizationFirmwareEnabled
  ```
- [ ] Гипервизор свободен (ожидаемо `False` до начала работ):
  ```powershell
  (Get-CimInstance Win32_ComputerSystem).HypervisorPresent
  ```
- [ ] Свободно ≥25 ГБ на диске под Linux-окружение (VM/VHDX/образы)

### 0.2 Релизные артефакты (заготовка для дорожек A/B/C)

- [ ] Скачать release **v0.3.1**, тарболл `x86_64-unknown-linux-musl` со страницы Releases репозитория
- [ ] Распаковать в рабочую папку, например `~/release-bin/`. Ожидаемое содержимое:
  `agentgrid-control-plane`, `agentgrid-node-daemon`, `ag`, `agentgrid-gateway`,
  `agentgrid-acp-agent`, `adapter-mock`, `adapter-claude`, `adapter-opencode`, `adapter-fake-acp`
- [ ] Сверить контрольные суммы (внутри Linux-окружения дорожки):
  ```bash
  cd ~/release-bin && sha256sum -c --ignore-missing checksums.txt
  ```

---

## Дорожка A — WSL2 + статические бинарники (вариант 1, рекомендуемый старт)

**Результат:** работающие control-plane + нода внутри одного WSL2-дистрибутива, sandbox=none.
**Ресурсы:** ~0.5–1 ГБ RAM. **Время:** ~30 мин. **Откат:** `wsl --unregister Ubuntu`.

### A.1 Включение WSL2 (PowerShell от администратора)

- [ ] Установить WSL2 и Ubuntu (затем reboot):
  ```powershell
  wsl --install
  ```
  (На 19045 это включает компоненты «Virtual Machine Platform» + «Подсистема Windows для Linux»
  и ставит Ubuntu по умолчанию.)
- [ ] После reboot: открыть Ubuntu, задать пользователя/пароль
- [ ] Включить systemd внутри дистрибутива — добавить в `/etc/wsl.conf`:
  ```ini
  [boot]
  systemd=true
  ```
  затем из PowerShell: `wsl --shutdown` и заново открыть Ubuntu
- [ ] Проверить, что systemd жив (ожидаемо список юнитов, не пустой):
  ```bash
  systemctl list-units --type=service --state=running | head
  ```

### A.2 Лимиты ресурсов WSL2 (файл `%UserProfile%\.wslconfig` на Windows-стороне)

- [ ] Создать/дополнить `.wslconfig`:
  ```ini
  [wsl2]
  memory=3GB
  processors=3
  swap=2GB
  ```
- [ ] Применить: `wsl --shutdown`, открыть Ubuntu заново
- [ ] Проверить изнутри: `free -h` (должно показывать ~3 GiB), `nproc` (=3)

### A.3 Установка control-plane (внутри Ubuntu)

- [ ] Положить бинарники из тарболла v0.3.1 в `/usr/local/bin`:
  ```bash
  sudo install -m 0755 ~/release-bin/agentgrid-control-plane ~/release-bin/ag /usr/local/bin/
  ```
- [ ] Установить CP штатным скриптом репозитория (склонировать репо внутрь WSL2 — на ext4,
  не на `/mnt/*`; либо скопировать только `deploy/`):
  ```bash
  sudo bash deploy/install-control-plane.sh --listen 127.0.0.1:7800
  ```
  Скрипт создаёт systemd-юнит `agentgrid-control-plane` с харднением.
- [ ] Запустить и посмотреть лог — **в логах CP при первом старте печатается one-time setup-токен**,
  выписать его:
  ```bash
  sudo systemctl enable --now agentgrid-control-plane
  journalctl -u agentgrid-control-plane --no-pager | grep -i "setup"
  ```
- [ ] Health-check:
  ```bash
  curl -fsS http://127.0.0.1:7800/health/ready && echo OK
  curl -fsS http://127.0.0.1:7800/health/live  && echo ALIVE
  ```

### A.4 Bootstrap админа и enrollment-токен

- [ ] Создать первого пользователя (setup-токен из логов A.3), сохранить JWT:
  ```bash
  JWT=$(curl -fsS -X POST http://127.0.0.1:7800/v1/auth/setup \
    -H 'content-type: application/json' \
    -d '{"username":"admin","password":"<придуманный-пароль>","setup_token":"<токен-из-логов>"}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  echo "$JWT" > ~/.agentgrid-jwt && chmod 600 ~/.agentgrid-jwt
  ```
  (Если поле `setup_token` называется иначе — свериться с фактическим ответом/логом CP.)
- [ ] Проверить логин:
  ```bash
  curl -fsS -X POST http://127.0.0.1:7800/v1/auth/login \
    -H 'content-type: application/json' \
    -d '{"username":"admin","password":"<пароль>"}' | head -c 80
  ```
- [ ] Выписать enrollment-токен ноды и сохранить:
  ```bash
  ENROLL=$(curl -fsS -X POST http://127.0.0.1:7800/v1/nodes/enrollment-token \
    -H "authorization: Bearer $JWT")
  echo "$ENROLL" > ~/.agentgrid-enroll && chmod 600 ~/.agentgrid-enroll
  ```

### A.5 Установка ноды (внутри Ubuntu)

- [ ] Установить ноду штатным скриптом (создаст пользователя `agentgrid`, харднеженный юнит,
  каталоги `/var/lib/agentgrid/{workspace,repos,artifacts}`, зароллит ноду):
  ```bash
  sudo bash deploy/install-node.sh --server http://127.0.0.1:7800 \
       --token "$(cat ~/.agentgrid-enroll)" \
       --staging ~/release-bin --adapters mock
  ```
- [ ] Наблюдать логи ноды до появления heartbeat:
  ```bash
  journalctl -u agentgrid-node -f
  ```
- [ ] Нода видна и здорова:
  ```bash
  curl -fsS http://127.0.0.1:7800/v1/nodes -H "authorization: Bearer $JWT" | python3 -m json.tool | head -30
  ag nodes doctor <node-id>    # CLI из /usr/local/bin; базовый URL/токен — по `ag login`
  ```

### A.6 Приёмочная задача (mock)

- [ ] Отправить тестовую задачу:
  ```bash
  TASK_ID=$(curl -fsS -X POST http://127.0.0.1:7800/v1/tasks \
    -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
    -d '{"prompt":"test","adapter":"mock","repository":"*","timeout_secs":60}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
  echo "Task: $TASK_ID"
  ```
- [ ] Дождаться `succeeded` (полликать статус) и посмотреть события:
  ```bash
  curl -fsS http://127.0.0.1:7800/v1/tasks/$TASK_ID -H "authorization: Bearer $JWT" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'
  curl -fsS http://127.0.0.1:7800/v1/tasks/$TASK_ID/events -H "authorization: Bearer $JWT" | python3 -m json.tool | tail -30
  ```

**Дорожка A завершена, когда:** mock-задача прошла статус `succeeded`, нода не `degraded`,
`/health/ready` отвечает.

---

## Дорожка B — Песочница podman (вариант 2, поверх дорожки A)

**Результат:** агенты недоверенных задач запускаются в rootless-контейнерах
(`--cap-drop=ALL`, `--network none`, read-only корень). **Время:** +15–20 мин к A.
**Откат:** удалить DropIn → возврат к sandbox=none.

- [ ] Установить рантайм + алиас `docker`→`podman` (spawn-путь проекта хардкодит `docker run`):
  ```bash
  sudo apt update && sudo apt install -y podman podman-docker
  ```
- [ ] Проверить версию рантайма (это же делает стартовая проба ноды):
  ```bash
  podman version --format '{{.Server.Version}}'
  docker version --format '{{.Server.Version}}'   # должен ответить через алиас
  ```
- [ ] Подтянуть образ песочницы и зафиксировать digest:
  ```bash
  podman pull ubuntu:24.04
  DIGEST=$(podman inspect --format '{{.Digest}}' docker.io/library/ubuntu:24.04)
  echo "$DIGEST"   # sha256:…
  ```
  (Альтернатива — образ проекта с запечёнными адаптерами:
  `podman pull ghcr.io/<owner>/agentgrid-node-daemon:v0.3.1`.)
- [ ] Создать DropIn к юниту ноды — `/etc/systemd/system/agentgrid-node.service.d/sandbox.conf`:
  ```ini
  [Service]
  Environment=AGENTGRID_SANDBOX=podman
  Environment=AGENTGRID_SANDBOX_RUNTIME=podman
  Environment=AGENTGRID_SANDBOX_NETWORK=none
  Environment=AGENTGRID_SANDBOX_IMAGE=ubuntu:24.04
  Environment=AGENTGRID_SANDBOX_IMAGE_DIGEST=<sha256-из-предыдущего-шага>
  ```
  Пин по digest обязателен (fail closed при отсутствии пина — Hardening §32).
- [ ] Перезапустить ноду и убедиться в логах, что рантайм найден и probe адаптера в образе прошёл:
  ```bash
  sudo systemctl daemon-reload && sudo systemctl restart agentgrid-node
  journalctl -u agentgrid-node --no-pager | grep -iE "runtime|sandbox|adapter"
  # ожидаемо: "container runtime ready" и отсутствие degraded
  ```
- [ ] Отправить задачу (как в A.6) и убедиться, что спавн пошёл через контейнер:
  ```bash
  journalctl -u agentgrid-node --no-pager | grep -i "docker run"
  podman ps -a | head    # следы --rm-контейнеров
  ```
- [ ] Сверить overhead холодного старта с базлайном `docs/deploy/sandbox-benchmark.md`
  (ориентир: +1–2 с на попытку)

**Нюанс rootless:** контейнер стартует от пользователя; если адаптеру в образе нужны права на
worktree-маунт — проверить UID-маппинг (`/etc/subuid`, `/etc/subgid`). Workspace обязан лежать
на ext4 внутри WSL2, не на `/mnt/*`.

---

## Дорожка C — Hyper-V VM, без WSL (вариант 3)

**Результат:** стоковый Ubuntu-хост в VM — максимально близко к поддерживаемой конфигурации.
**Ресурсы:** 4 ГБ RAM статически, ~40 ГБ диск. **Время:** ~1–1.5 ч.
**Откат:** `Remove-VM` + удалить VHDX.

### C.1 Hyper-V (PowerShell от администратора)

- [ ] Включить роль (затем reboot):
  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All
  ```
- [ ] Скачать ISO Ubuntu Server 24.04 LTS (x86_64)

### C.2 Создание VM

- [ ] Создать VM (Gen2, 3 vCPU, 4 ГБ, 40 ГБ VHDX):
  ```powershell
  New-VM -Name agentgrid -Generation 2 -MemoryStartupBytes 4GB `
    -NewVHDPath D:\HyperV\agentgrid.vhdx -NewVHDSizeBytes 40GB -SwitchName "Default Switch"
  Set-VM agentgrid -ProcessorCount 3 -CheckpointType Production
  Add-VMDvdDrive -VMName agentgrid -Path D:\ISO\ubuntu-24.04-live-server-amd64.iso
  # в Firmware загрузка с DVD первой; для Gen2: выключить Secure Boot или поставить шаблон Microsoft UEFI:
  Set-VMFirmware -VMName agentgrid -EnableSecureBoot Off
  ```
- [ ] Установить Ubuntu Server (пользователь, SSH), зайти по SSH через IP из `Get-VM agentgrid | Get-VMNetworkAdapter`
- [ ] Сделать чекпоинт чистой системы:
  ```powershell
  Checkpoint-VM -VMName agentgrid -SnapshotName clean-install
  ```

### C.3 Установка agentgrid внутри VM

- [ ] Выполнить **этап 0.2** (артефакты) внутри VM — например `scp` тарболл с Windows-хоста
- [ ] Выполнить **шаги A.3–A.6 дословно** — внутри VM это стоковый Ubuntu с systemd,
  WSL-специфики нет (кроме `.wslconfig`/`wsl.conf` — не нужно)
- [ ] (Опционально, вместо podman-шагов дорожки B) при недоверенных задачах — поставить
  `docker` или `podman` по выбору и повторить конфигурацию DropIn из B

### C.4 Доступ из Windows

- [ ] Из Windows проверен health-check по IP VM:
  ```powershell
  curl.exe -fsS http://<vm-ip>:7800/health/ready
  ```
- [ ] Web-UI (если поднят) открывается из браузера Windows по `http://<vm-ip>:7800`

---

## Дорожка D — Docker Desktop, compose-путь (вариант 4, demo/eval)

**Результат:** вся система (CP + 2 ноды) в контейнерах, минимум ручных шагов, web-UI.
**Ресурсы:** 1.5–2.5 ГБ RAM. **Время:** ~40 мин. **Откат:** `deploy/compose/down.sh` + uninstall.
**Важно:** нода в контейнере без внутренней docker-песочницы (`AGENTGRID_SANDBOX` не задан) —
граница изоляции = контейнер ноды. Только demo/eval, не для недоверенных агентов в бою.

- [ ] Установить Docker Desktop (перезагрузка после установки)
- [ ] В Settings → General **снять** галку «Use the WSL 2 based engine» → Apply & Restart
  (Hyper-V backend; требует Pro — есть)
- [ ] Проверить, что Docker жив: `docker version` (Server должен ответить)
- [ ] Собрать образы из корня репозитория (Git Bash):
  ```bash
  cd /d/PythonProjects/agentgrid
  docker build -f Dockerfile.control-plane -t ag-cp:test .
  docker build -f Dockerfile.node-daemon  -t ag-node:test .
  ```
  (Либо: тянуть `ghcr.io/<owner>/agentgrid-node-daemon:v0.3.1` и переписать теги в
  `docker-compose.yml`.)
- [ ] Поднять стек штатным скриптом — генерирует секреты, читает setup-токен из логов CP,
  делает bootstrap, пишет `deploy/compose/.env`:
  ```bash
  bash deploy/compose/up.sh
  ```
- [ ] Проверить здоровье и контейнеры:
  ```bash
  curl -fsS http://127.0.0.1:7800/health/ready && echo OK
  docker ps --format '{{.Names}} {{.Status}}'
  # ожидаемо: ag-control-plane-1, ag-node-1-1, ag-node-2-1 — все Up
  ```
- [ ] Взять пароль админа: `grep ADMIN_PASS deploy/compose/.env`
- [ ] Выполнить приёмочную mock-задачу по шагам **A.6** (только URL — `http://127.0.0.1:7800`)
- [ ] Открыть web-UI в браузере: `http://127.0.0.1:7800`

---

## Финальная проверка (любая дорожка)

- [ ] `curl -fsS http://<cp>/health/ready` → OK
- [ ] Нода в списке `/v1/nodes` не `degraded`, heartbeat обновляется
- [ ] `ag nodes doctor <id>` без ошибок
- [ ] Mock-задача дошла до `succeeded`, события видны в `/v1/tasks/<id>/events`
- [ ] `/metrics` отдаёт счётчики (транспорт, SQLite lock failures — см. `OPS-STARTER.md`)

## Откат (полная зачистка)

- [ ] Дорожка A/B: `wsl --unregister Ubuntu` (удаляет дистрибутив целиком)
- [ ] Дорожка C: `Remove-VM agentgrid` + удалить `D:\HyperV\agentgrid.vhdx` +
      `Disable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All`
- [ ] Дорожка D: `bash deploy/compose/down.sh` + Docker Desktop → Troubleshoot → Uninstall

## Справочник

**Ключевые env ноды:** `AGENTGRID_SERVER`, `AGENTGRID_ENROLL_TOKEN`, `AGENTGRID_NODE_NAME`,
`AGENTGRID_DATA_DIR`, `AGENTGRID_WORKSPACE_ROOT`, `AGENTGRID_REPOSITORY_ROOT`,
`AGENTGRID_ARTIFACT_ROOT`, `AGENTGRID_MAX_CONCURRENCY`, `AGENTGRID_TRANSPORT` (auto|ws|poll),
`AGENTGRID_SANDBOX` (none|docker|podman), `AGENTGRID_SANDBOX_RUNTIME`,
`AGENTGRID_SANDBOX_NETWORK` (none|restricted|unrestricted), `AGENTGRID_SANDBOX_IMAGE`,
`AGENTGRID_SANDBOX_IMAGE_DIGEST`.

**Control-plane:** `AGENTGRID_JWT_SECRET` (≥32 байта), `AGENTGRID_LISTEN` (0.0.0.0:7800).

**Эндпоинты:** `/health/ready`, `/health/live`, `/metrics`, `/v1/auth/setup`, `/v1/auth/login`,
`/v1/nodes`, `/v1/nodes/enrollment-token`, `/v1/tasks`, `/v1/tasks/{id}`, `/v1/tasks/{id}/events`.

**Если что-то не заводится:** `TROUBLESHOOTING.md` в корне репо, `ag nodes doctor <id>`,
логи `journalctl -u agentgrid-node -f` / `journalctl -u agentgrid-control-plane -f`.
