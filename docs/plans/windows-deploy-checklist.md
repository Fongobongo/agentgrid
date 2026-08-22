# Windows-деплой agentgrid — пошаговый чек-лист

> Дата: 2026-08-16. Обзорный план с обоснованиями и сравнением: [`windows-deploy-4-variants.md`](./windows-deploy-4-variants.md)
>
> **Как пользоваться:** отмечайте выполненные шаги `[x]`. Дорожки независимы, кроме B (строится
> поверх A). На 4-поточном CPU **не запускайте больше одной дорожки одновременно**.
> Состояние машины на момент составления: Win10 Pro 22H2, VT-x включён в BIOS, Hyper-V/WSL2/Docker не установлены.

---

## Этап 0 — Общая подготовка (нужен для дорожек A, B, C; для D не обязателен)

### 0.1 Проверка машины

- [x] Редакция Windows — Pro или выше (для Hyper-V):
  ```powershell
  (Get-CimInstance Win32_OperatingSystem).Caption
  ```
- [x] Виртуализация включена в BIOS (ожидаемо `True`; если `False` — включить VT-x в BIOS):
  ```powershell
  (Get-CimInstance Win32_Processor).VirtualizationFirmwareEnabled
  ```
- [x] Гипервизор свободен (ожидаемо `False` до начала работ):
  ```powershell
  (Get-CimInstance Win32_ComputerSystem).HypervisorPresent
  ```
- [x] Свободно ≥25 ГБ на диске под Linux-окружение (VM/VHDX/образы)

### 0.2 Релизные артефакты (заготовка для дорожек A/B/C)

- [x] Скачать release **v0.3.2**, тарболл `x86_64-unknown-linux-musl` со страницы Releases репозитория
- [x] Распаковать в рабочую папку, например `~/release-bin/`. Ожидаемое содержимое:
  `agentgrid-control-plane`, `agentgrid-node-daemon`, `ag`, `agentgrid-gateway`,
  `agentgrid-acp-agent`, `adapter-mock`, `adapter-claude`, `adapter-opencode`, `adapter-fake-acp`
  (Лаборатория: файл сумм в тарболле называется `SHA256SUMS`, а не `checksums.txt`; бинари в
  тарболле без бита `+x` — после распаковки обязателен `chmod 0755`, иначе «Permission denied».)
- [x] Сверить контрольные суммы (внутри Linux-окружения дорожки):
  ```bash
  cd ~/release-bin && sha256sum -c --ignore-missing SHA256SUMS
  ```

---

## Дорожка A — WSL2 + статические бинарники (вариант 1, рекомендуемый старт)

**Результат:** работающие control-plane + нода внутри одного WSL2-дистрибутива, sandbox=none.
**Ресурсы:** ~0.5–1 ГБ RAM. **Время:** ~30 мин. **Откат:** `wsl --unregister Ubuntu`.

### A.1 Включение WSL2 (PowerShell от администратора)

- [x] Установить WSL2 и Ubuntu (затем reboot):
  ```powershell
  wsl --install
  ```
  (На 19045 это включает компоненты «Virtual Machine Platform» + «Подсистема Windows для Linux»
  и ставит Ubuntu по умолчанию.)
- [x] После reboot: открыть Ubuntu, задать пользователя/пароль
- [x] Включить systemd внутри дистрибутива — добавить в `/etc/wsl.conf`:
  ```ini
  [boot]
  systemd=true
  ```
  затем из PowerShell: `wsl --shutdown` и заново открыть Ubuntu
- [x] Проверить, что systemd жив (ожидаемо список юнитов, не пустой):
  ```bash
  systemctl list-units --type=service --state=running | head
  ```

### A.2 Лимиты ресурсов WSL2 (файл `%UserProfile%\.wslconfig` на Windows-стороне)

- [x] Создать/дополнить `.wslconfig`:
  ```ini
  [wsl2]
  memory=3GB
  processors=3
  swap=2GB
  ```
- [x] Применить: `wsl --shutdown`, открыть Ubuntu заново
- [x] Проверить изнутри: `free -h` (должно показывать ~3 GiB), `nproc` (=3)

### A.3 Установка control-plane (внутри Ubuntu)

- [x] Положить бинарники из тарболла v0.3.2 в `/usr/local/bin`:
  ```bash
  sudo install -m 0755 ~/release-bin/agentgrid-control-plane ~/release-bin/ag /usr/local/bin/
  ```
- [x] Установить CP штатным скриптом репозитория (склонировать репо внутрь WSL2 — на ext4,
  не на `/mnt/*`; либо скопировать только `deploy/`):
  ```bash
  sudo bash deploy/install-control-plane.sh --listen 127.0.0.1:7800
  ```
  Скрипт создаёт systemd-юнит `agentgrid-control-plane` с харднением.
- [x] Запустить и посмотреть лог — **в логах CP при первом старте печатается one-time setup-токен**,
  выписать его:
  ```bash
  sudo systemctl enable --now agentgrid-control-plane
  journalctl -u agentgrid-control-plane --no-pager | grep -i "setup"
  ```
- [x] Health-check:
  ```bash
  curl -fsS http://127.0.0.1:7800/health/ready && echo OK
  curl -fsS http://127.0.0.1:7800/health/live  && echo ALIVE
  ```

### A.4 Bootstrap админа и enrollment-токен

- [x] Создать первого пользователя (setup-токен из логов A.3), сохранить JWT:
  ```bash
  JWT=$(curl -fsS -X POST http://127.0.0.1:7800/v1/auth/setup \
    -H 'content-type: application/json' \
    -d '{"username":"admin","password":"<придуманный-пароль>","setup_token":"<токен-из-логов>"}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  echo "$JWT" > ~/.agentgrid-jwt && chmod 600 ~/.agentgrid-jwt
  ```
  (Если поле `setup_token` называется иначе — свериться с фактическим ответом/логом CP.)
- [x] Проверить логин:
  ```bash
  curl -fsS -X POST http://127.0.0.1:7800/v1/auth/login \
    -H 'content-type: application/json' \
    -d '{"username":"admin","password":"<пароль>"}' | head -c 80
  ```
- [x] Выписать enrollment-токен ноды и сохранить:
  ```bash
  ENROLL=$(curl -fsS -X POST http://127.0.0.1:7800/v1/nodes/enrollment-token \
    -H "authorization: Bearer $JWT")
  echo "$ENROLL" > ~/.agentgrid-enroll && chmod 600 ~/.agentgrid-enroll
  ```

### A.5 Установка ноды (внутри Ubuntu)

- [x] Установить ноду штатным скриптом (создаст пользователя `agentgrid`, харднеженный юнит,
  каталоги `/var/lib/agentgrid/{workspace,repos,artifacts}`, зароллит ноду):
  ```bash
  sudo bash deploy/install-node.sh --server http://127.0.0.1:7800 \
       --token "$(cat ~/.agentgrid-enroll)" \
       --staging ~/release-bin --adapters mock
  ```
- [x] Наблюдать логи ноды до появления heartbeat:
  ```bash
  journalctl -u agentgrid-node -f
  ```
- [x] Нода видна и здорова:
  ```bash
  curl -fsS http://127.0.0.1:7800/v1/nodes -H "authorization: Bearer $JWT" | python3 -m json.tool | head -30
  ag nodes doctor <node-id>    # CLI из /usr/local/bin; базовый URL/токен — по `ag login`
  ```

### A.6 Приёмочная задача (mock)

- [x] Отправить тестовую задачу:
  ```bash
  TASK_ID=$(curl -fsS -X POST http://127.0.0.1:7800/v1/tasks \
    -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
    -d '{"prompt":"test","adapter":"mock","repository":"*","timeout_secs":60}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
  echo "Task: $TASK_ID"
  ```
- [x] Дождаться `succeeded` (полликать статус) и посмотреть события:
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

- [x] Установить рантайм + алиас `docker`→`podman` (spawn-путь проекта хардкодит `docker run`):
  ```bash
  sudo apt update && sudo apt install -y podman podman-docker
  ```
  (Лаборатория 2026-08-17: podman 5.7.0; для rootless также нужны записи в `/etc/subuid`/
  `/etc/subgid` для пользователя `agentgrid`.)
- [x] Проверить версию рантайма (это же делает стартовая проба ноды):
  ```bash
  podman version --format '{{.Server.Version}}'
  docker version --format '{{.Server.Version}}'   # должен ответить через алиас
  ```
- [x] Подтянуть базовый образ, собрать образ песочницы с адаптером и зафиксировать digest:
  ```bash
  podman pull ubuntu:24.04
  podman build -t agentgrid-sandbox:lab -f Dockerfile.sandbox .
  DIGEST=$(podman inspect --format '{{.Digest}}' agentgrid-sandbox:lab)
  echo "$DIGEST"   # sha256:…
  ```
  GHCR-образ ноды (`ghcr.io/<owner>/agentgrid-node-daemon:…`) как база песочницы **не годится**:
  его ENTRYPOINT — демон ноды, внутри песочницы стартует демон и умирает. Вместо него —
  локальный образ: `ubuntu:24.04` + адаптеры, `ENTRYPOINT []`:

  ```dockerfile
  FROM ubuntu:24.04
  COPY adapter-mock /usr/local/bin/adapter-mock
  ENTRYPOINT []
  ```

  Подводный камень: `COPY` сохраняет режимы исходного файла — если бинарь на хосте лежит
  с mode 644 (как в релизном тарболле), в образе он будет не исполняемым, и проба
  `command -v adapter-mock` вернёт «adapter missing». `chmod 0755` перед сборкой обязателен.
- [x] Создать DropIn к юниту ноды — `/etc/systemd/system/agentgrid-node.service.d/sandbox.conf`:
  ```ini
  [Service]
  RuntimeDirectory=agentgrid
  Environment=XDG_RUNTIME_DIR=/run/agentgrid
  Environment=AGENTGRID_SANDBOX=podman
  Environment=AGENTGRID_SANDBOX_RUNTIME=podman
  Environment=AGENTGRID_SANDBOX_NETWORK=none
  Environment=AGENTGRID_SANDBOX_IMAGE=agentgrid-sandbox:lab
  Environment=AGENTGRID_SANDBOX_IMAGE_DIGEST=<sha256-из-предыдущего-шага>
  ```
  Пин по digest обязателен (fail closed при отсутствии пина — Hardening §32).
  Для rootless-подмана сервисному пользователю нужен приватный runtime-каталог:
  `RuntimeDirectory=agentgrid` + `XDG_RUNTIME_DIR` в том же drop-in.
- [x] Перезапустить ноду и убедиться в логах, что рантайм найден и probe адаптера в образе прошёл:
  ```bash
  sudo systemctl daemon-reload && sudo systemctl restart agentgrid-node
  journalctl -u agentgrid-node --no-pager | grep -iE "runtime|sandbox|adapter"
  # ожидаемо: "container runtime ready" и отсутствие degraded
  ```
  (Лаборатория: `adapter present in sandbox image`, `container runtime ready`, runtime 5.7.0.)
- [x] Отправить задачу (как в A.6) и убедиться, что спавн пошёл через контейнер:
  ```bash
  journalctl -u agentgrid-node --no-pager | grep -i "container create"
  runuser -u agentgrid -- env XDG_RUNTIME_DIR=/run/agentgrid podman ps -a | head
  ```
  (Лаборатория: mock-задача `succeeded`, в журнале полный цикл `container create/init/remove`
  с label `agentgrid.node=<id>`; `podman ps -a` пуст — `--rm` отработал. Запуск контейнера
  идёт из rootless-стора сервисного пользователя, поэтому `podman ps` — от `agentgrid`,
  не от root.)
- [x] Сверить overhead холодного старта с базлайном `docs/deploy/sandbox-benchmark.md`
  (ориентир: +1–2 с на попытку) — в лаборатории чистый
  `podman run --rm --cap-drop=ALL --network none <img> true` занимал 0.47–0.91 с,
  задача end-to-end (с тиком CP) — ~0.7 с.

**Нюанс rootless:** контейнер стартует от пользователя; если адаптеру в образе нужны права на
worktree-маунт — проверить UID-маппинг (`/etc/subuid`, `/etc/subgid`). Workspace обязан лежать
на ext4 внутри WSL2, не на `/mnt/*`.

---

## Дорожка C — Hyper-V VM, без WSL (вариант 3)

**Результат:** стоковый Ubuntu-хост в VM — максимально близко к поддерживаемой конфигурации.
**Ресурсы:** 4 ГБ RAM статически, ~40 ГБ диск. **Время:** ~1–1.5 ч.
**Откат:** `Remove-VM` + удалить VHDX.

### C.1 Hyper-V (PowerShell от администратора)

- [x] Включить роль (затем reboot):
  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All
  ```
- [x] Скачать ISO Ubuntu Server 24.04 LTS (x86_64)
  (Лаборатория: интерактивная установка и subiquity-autoinstall через live-ISO не
  завелись — патч grub.cfg у xorriso ломает El Torito-записи, а без явного kernel-параметра
  `autoinstall` установщик не стартует. Перешли на cloud image — см. C.2.)

### C.2 Создание VM

- [x] Создать VM (Gen2, 3 vCPU, 4 ГБ, 40 ГБ VHDX):
  ```powershell
  New-VM -Name agentgrid -Generation 2 -MemoryStartupBytes 4GB `
    -NewVHDPath D:\HyperV\agentgrid.vhdx -NewVHDSizeBytes 40GB -SwitchName "Default Switch"
  Set-VM agentgrid -ProcessorCount 3 -CheckpointType Production
  Add-VMDvdDrive -VMName agentgrid -Path D:\ISO\ubuntu-24.04-live-server-amd64.iso
  # в Firmware загрузка с DVD первой; для Gen2: выключить Secure Boot или поставить шаблон Microsoft UEFI:
  Set-VMFirmware -VMName agentgrid -EnableSecureBoot Off
  ```
  (Лаборатория: вместо установки с ISO — Ubuntu cloud image `noble-server-cloudimg-amd64.img`
  (SHA256 сверён), конвертация `qemu-img convert -f qcow2 -O vhdx` → `D:\HyperV\agentgrid-cloud.vhdx`,
  `Resize-VHD` до 32 ГБ, подключён как SCSI0-0. NoCloud seed `seed.iso` (volid CIDATA) на DVD1.
  Secure Boot выключен. **Подводный камень seed-конфига:** `users:` с `primary_group: <group>`
  требует отдельного блока `groups: [<group>]`, иначе cloud-init падает с
  `useradd: group 'X' does not exist` и юзер не создаётся. Пароль/ключ — в `user-data`
  (`passwd: <openssl passwd -6 ...>`, `ssh_authorized_keys`, `ssh_pwauth: true`).
  После смены seed обязательно менять `instance-id` в `meta-data`, иначе старый экземпляр кэша не обновится.)
- [x] Установить Ubuntu Server (пользователь, SSH), зайти по SSH через IP из `Get-VM agentgrid | Get-VMNetworkAdapter`
  (Лаборатория: пользователь `agentgrid` создан cloud-init; SSH-ключ `D:\agentgrid-release\vm-id`;
  `ssh_pwauth: true`. При пересоздании образа host keys меняются — чистить старую запись
  `ssh-keygen -R <ip>`.)
- [x] ~~Сделать чекпоинт чистой системы:~~
  ```powershell
  Checkpoint-VM -VMName agentgrid -SnapshotName clean-install
  ```
  (Пропущено осознанно: production-чекпоинт создаёт .avhdx, который бесконтрольно растёт
  на живом VM и может переполнить диск. Одиночный случайный снапшот пришлось мержить:
  `Remove-VMSnapshot` на работающей VM делает live-merge. Обновление 2026-08-22: снапшот
  `clean-install` (создан 20.08 11:42) к этому дню уже удалён, VM грузится напрямую с
  `agentgrid-cloud.vhdx`; осиротевший после merge `.avhdx` (0.94 ГБ, вне цепочки) вычищен
  вручную. Чекпоинт «чистой системы» более невоспроизводим — система полностью развёрнута;
  при необходимости откатиться к рабочему состоянию создавать снапшот *после* остановки VM
  (`Stop-VM`), с другим именем, например `deployed-ok`.)

### C.3 Установка agentgrid внутри VM

- [x] Выполнить **этап 0.2** (артефакты) внутри VM — например `scp` тарболл с Windows-хоста
  (Лаборатория: `scp agentgrid-x86_64-unknown-linux-musl.tar.gz` → `~/release-bin`, распаковка,
  `sha256sum -c --ignore-missing SHA256SUMS` — все 8 бинарников OK.)
- [x] Выполнить **шаги A.3–A.6 дословно** — внутри VM это стоковый Ubuntu с systemd,
  WSL-специфики нет (кроме `.wslconfig`/`wsl.conf` — не нужно)
  (Лаборатория: CP на `0.0.0.0:7800`, админ `admin`, нода `online` (id `c17fa176-…`),
  mock-задача `succeeded`, `/metrics` отдаёт счётчики. Замечания:
  ① bash-скрипты репо переносятся на Windows с CRLF/битыми кавычками — при копировании через
  PowerShell конвертировать в LF и проверять `bash -n`; исправлена закоммиченная строка 84 в
  `deploy/install-control-plane.sh` (две `echo` были слиты в одну с битыми кавычками).
  ② `GET /v1/nodes/{id}` появился после v0.3.2, поэтому `ag nodes doctor` на CP v0.3.2
  даёт HTTP 405; симптом-проверка заменена на `ag status` + `/v1/nodes`.
  (Обновление 2026-08-22: CP и нода в VM подняты до v0.3.4 — `ag nodes doctor` работает,
  `doctor: OK — no symptoms`.)
  ③ web UI: CP ищет `web/dist` рядом с бинарником либо `AGENTGRID_WEB_ROOT`; dist скопирован в
  `/usr/local/share/agentgrid/web`. JWT-секрет (≥32 Б) задан drop-in'ом
  `/etc/systemd/system/agentgrid-control-plane.service.d/override.conf`
  (`AGENTGRID_JWT_SECRET`, `AGENTGRID_WEB_ROOT`); после смены секрета нужно заново `ag login`.)
- [x] (Опционально, вместо podman-шагов дорожки B) при недоверенных задачах — поставить
  `docker` или `podman` по выбору и повторить конфигурацию DropIn из B
  (Лаборатория 2026-08-22: **rootful podman 4.9.3** внутри VM, aлиас `podman-docker`
  (spawn-путь ноды зовёт `docker`), образ `agentgrid-sandbox:lab` (ubuntu:24.04 + `adapter-mock`,
  `ENTRYPOINT []`, digest запинен). Бинарники VM обновлены до **v0.3.4** (v0.3.2 недостаточно:
  проба адаптера там без `--network none` — под жёстким юнитом bridge-сеть через netavark
  не поднимается, и проба ложно рапортует «adapter missing in sandbox image»; там же был
  сломан orphan-фильтр — голый `agentgrid.node=` вместо `label=…`, чинится только кодом).
  Запуск контейнерного рантайма **внутри** hardened-юнита потребовал цепочки ослаблений в
  DropIn сверх дорожки B — каждое сняло конкретный отказ (порядок возникновения):
  `ReadWritePaths=/var/lib/containers /run/containers /var/cache/containers`
  (последний — кэш short-name-алиасов, podman пишет его при каждом резолве короткого имени образа),
  `RestrictNamespaces=false` + `ProtectKernelTunables=false` (создание namespace'ов контейнера),
  `ProtectControlGroups=false` + `Delegate=yes` (conmon/crun не могли создать
  `machine.slice/libpod-*.scope`), `PrivateDevices=false` (`mknod /dev/null`), 
  `ProtectHostname=false` (seccomp-фильтр systemd режет `sethostname` даже в UTS-namespace
  контейнера), `NoNewPrivileges=false` (`capset` у crun). Итог: `adapter present in sandbox
  image`, рантайм ready, mock-задача `succeeded` в контейнере (label `agentgrid.node=<id>`,
  digest-pinned образ, `network none`, `--rm` отработал — `podman ps -a` пуст), нода `online`
  без degraded, `ag nodes doctor` — OK. Остаточные WARN о `changes.patch`/`validation.log`
  от mock-адаптера — ожидаемы, он эти артефакты не создаёт.)

### C.4 Доступ из Windows

- [x] Из Windows проверен health-check по IP VM:
  ```powershell
  curl.exe -fsS http://<vm-ip>:7800/health/ready
  ```
  (Лаборатория: `ready=200`, `live=200`.)
- [x] Web-UI (если поднят) открывается из браузера Windows по `http://<vm-ip>:7800`
  (Лаборатория: `GET /` → 200, `index.html` отдаётся.)

---

## Дорожка D — Docker Desktop, compose-путь (вариант 4, demo/eval)

**Результат:** вся система (CP + 2 ноды) в контейнерах, минимум ручных шагов, web-UI.
**Ресурсы:** 1.5–2.5 ГБ RAM. **Время:** ~40 мин. **Откат:** `deploy/compose/down.sh` + uninstall.
**Важно:** нода в контейнере без внутренней docker-песочницы (`AGENTGRID_SANDBOX` не задан) —
граница изоляции = контейнер ноды. Только demo/eval, не для недоверенных агентов в бою.

- [x] Установить Docker Desktop (без перезагрузки хоста в лаборатории):
  per-user + WSL2 backend + данные на D:
  ```powershell
  curl.exe -fL -o D:\DockerDesktop-Installer.exe "https://desktop.docker.com/win/main/amd64/Docker Desktop Installer.exe"
  Start-Process 'D:\DockerDesktop-Installer.exe' -Wait -ArgumentList @(
    'install','--user','--backend=wsl-2','--accept-license',
    "--installation-dir=D:\Program Files\Docker\Docker",
    "--wsl-default-data-root=D:\Docker\data",'--no-windows-containers','--quiet')
  Start-Process 'D:\Program\Docker Desktop.exe'
  ```
  (Лаборатория 2026-08-19: Docker Desktop 4.87.0 / engine 29.7.2, per-user mode,
  install-dir указал `D:\Program Files\Docker\Docker`, но инсталлятор осел в `D:\Program`
  из-за пробела в `--installation-dir` — некритично; образы/данные на `D:\Docker\data`.
  **Подводный камень 1:** `docker.exe` и хелпер `docker-credential-desktop` в `%LOCALAPPDATA%\..`,
  но не в PATH текущей shell-сессии — перед сборкой добавить `D:\Program\resources\bin` в `$env:Path`,
  иначе сборка падает с `exec: docker-credential-desktop not found in %PATH%`.
  **Подводный камень 2:** `up.sh` требует `python3` в PATH (в чистом Windows есть только `python.exe`)
  и `curl`; решение — shim/python-вместо-curl или Git Bash с доступным python3.
  **Подводный камень 3:** Windows curl в PowerShell ломает JSON в одинарных кавычках
  (`Failed to parse the request body as JSON`) — для bootstrap-шагов использовать python/файл,
  а не inline curl.)
- [x] ~~В Settings → General снять галку «Use the WSL 2 based engine»~~ — в лаборатории выбран
  **WSL2 backend** (--backend=wsl-2), это текущий дефолт Docker и не требует Hyper-V backend,
  который был нужен только в старом варианте; для Hyper-V backend нужен all-users режим
- [x] Проверить, что Docker жив: `docker version` (Server ответил: engine 29.7.2, containerd v2.2.5)
- [x] Собрать образы из корня репозитория (оба `exit=0`):
  ```powershell
  docker build -f Dockerfile.control-plane -t ag-cp:test .
  docker build -f Dockerfile.node-daemon  -t ag-node:test .
  ```
  (Лаборатория: `.dockerignore` корректно исключает target/ и node_modules, контекст компактный.
  CP-сборка включает web UI `npm ci && npm run build`.)
- [x] Поднять стек штатным скриптом — генерирует секреты, читает setup-токен из логов CP,
  делает bootstrap, пишет `deploy/compose/.env`:
  ```bash
  bash deploy/compose/up.sh
  ```
  (Лаборатория: CP healthy, node-1/node-2 online, реципиент-токены выпущены и использованы,
  `.env` содержит только JWT_SECRET (токены стерты после зачисления — Hardening P0 item 6/29).
  Admin: `admin / OYG2vfgAuCFsbgbk4cf4my1N` — показан один раз, в `.env` не пишется.)
- [x] Проверить здоровье и контейнеры:
  ```powershell
  curl.exe -fsS http://127.0.0.1:7800/health/ready && echo OK
  docker ps --format '{{.Names}} {{.Status}}'
  ```
  (Лаборатория: `agentgrid-control-plane-1 Up (healthy)`, `node-1-1`/`node-2-1` Up;
  `/health/ready` 200; обе ноды `online` с heartbeat.)
- [x] Взять пароль админа: `grep ADMIN_PASS deploy/compose/.env` — **в `.env` не пишется**,
  пароль печатается один раз в конце `up.sh`: `>> login: admin / <pass>`
- [x] Выполнить приёмочную mock-задачу по шагам **A.6** (URL `http://127.0.0.1:7800`):
  task `209c9f90-6275-4dbb-aa7d-8923f4d18e81` → `succeeded`, события stdout/result видны
- [x] Открыть web-UI в браузере: `http://127.0.0.1:7800` (`GET /` → 200, отдаёт index.html)

**Дополнение:** если CP поднят **одновременно** с Hyper-V VM (дорожка C), локальный порт 7800
не конфликтует — VM CP висит на `172.28.157.79:7800`, compose CP на `127.0.0.1:7800`.

---

## Финальная проверка (любая дорожка)

- [x] `curl -fsS http://<cp>/health/ready` → OK
- [x] Нода в списке `/v1/nodes` не `degraded`, heartbeat обновляется
- [x] `ag nodes doctor <id>` без ошибок
- [x] Mock-задача дошла до `succeeded`, события видны в `/v1/tasks/<id>/events`
- [x] `/metrics` отдаёт счётчики (транспорт, SQLite lock failures — см. `OPS-STARTER.md`)

## Откат (полная зачистка)

**Выполнен 2026-08-22.** Лаборатория по всем трём дорожкам демонтирована после
успешной верификации («Финальная проверка» — all-green) и подтверждения, что
открытые пункты остальных планов (docs/*.md в корне) живых окружений не требуют.
Всё, что нужно для реконструкции, задокументировано выше в этом чеклисте.

- [x] Дорожка A/B: `wsl --unregister Ubuntu` (удаляет дистрибутив целиком)
  (Выполнено: после деинсталляции Docker Desktop его `docker-desktop`-дистрибутив
  ушёл сам; `wsl -l -v` → «не имеет установленных дистрибутивов». Понадобился
  `wsl --shutdown` перед удалением `D:\Docker` — WSL-сервис держал docker_data.vhdx.)
- [x] Дорожка C: `Remove-VM agentgrid` + удалить `D:\HyperV\agentgrid-cloud.vhdx` +
      `Disable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All`
  (Выполнено: `Stop-VM` → `Remove-VM -Force` → vhdx удалён, `D:\HyperV` удалён;
  фича `Microsoft-Hyper-V-All` отключена с `-NoRestart` — вступает в силу при
  следующей перезагрузке хоста, WSL2/Docker это не задело, т.к. им достаточно
  Virtual Machine Platform. Каталог `D:\agentgrid-release\` (ключи, инсталляторы,
  тарболлы, seed.iso) удалён вручную.)
- [x] Дорожка D: `bash deploy/compose/down.sh` + Docker Desktop → Troubleshoot → Uninstall
  (Выполнено: движок запущен → `down.sh --purge` (контейнеры, все 3 volume, сеть,
  `deploy/compose/.env`) → процессы остановлены → `DockerDesktop-Installer.exe
  uninstall --quiet`: `D:\Program` удалён самой деинсталляцией; вручную добиты
  `D:\Docker\data` (6.6 ГБ) и `%LOCALAPPDATA%\Docker`.)

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
