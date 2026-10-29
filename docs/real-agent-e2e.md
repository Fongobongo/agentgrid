# Real Agent E2E Test - Инструкция

## Описание

Тестирование полного рабочего процесса с реальным LLM-агентом (Claude Code) вместо mock adapter.

## Предварительные требования

1. **Установить Claude CLI:**
   ```bash
   npm install -g @anthropic-ai/claude-code
   ```

2. **Получить API ключ Anthropic:**
   ```bash
   export ANTHROPIC_API_KEY=sk-ant-...
   ```

## Запуск теста

```bash
# Поднять control plane + 2 узла
export AGENTGRID_JWT_SECRET="$(head -c 48 /dev/urandom | base64)"
export NODE1_TOKEN="$(openssl rand -hex 32)"
export NODE2_TOKEN="$(openssl rand -hex 32)"

bash deploy/compose/up.sh

# Запустить тест с реальным агентом
ANTHROPIC_API_KEY=sk-ant-... bash tests/e2e/run-real-agent.sh
```

## Ожидаемое поведение

1. Создание задачи с `adapter: claude`
2. Мониторинг выполнения в реальном времени
3. Проверка артефактов (changes.patch, лог событий)
4. Успешное применение патча к рабочему дереву

## Пример промпта для теста

```
Add a hello.py file that prints 'Hello from AgentGrid!' and commit it.
```

## Проверка результатов

После успешного завершения:
- Статус задачи: `succeeded`
- Артефакты: `changes.patch`, `agent-raw-output.log`
- В рабочем репозитории появится `hello.py`

## Troubleshooting

### Claude CLI не найден

```bash
npm install -g @anthropic-ai/claude-code
```

### API ключ недействителен

Проверить баланс на console.anthropic.com

### Task stuck в running

- Проверить логи узла: `journalctl -u agentgrid-node-daemon -f`
- Отменить задачу: `ag task cancel TASK_ID`
- Попробовать с большим timeout

## Следующие шаги

Дополнительно протестировать:
- OpenCode adapter (Google Gemini)
- Timeout и cancellation
- Permission interception mode
- Sandbox execution (Docker/Podman)

## Запуск на двух удалённых хостах

`tests/e2e/run-real-agent.sh` поднимает control plane и две ноды на одной
машине. Настоящая двуххостовая проверка (CP на одном ПК, ноды на других)
делается вручную — она полезна тем, что ловит сетевые и окруженческие
проблемы, которых нет на loopback.

Проверено 2026-10-29: CP на одном хосте, нода A с opencode 2.2.1, нода B с
opencode 1.19.5, все задачи `succeeded`, планировщик разложил две
одновременные задачи по разным хостам.

### Топология

| Роль | Что нужно |
|---|---|
| CP | `agentgrid-control-plane`, слушает `0.0.0.0:<port>` |
| нода | `agentgrid-node-daemon` **и** `adapter-<id>` в одном каталоге на `PATH`, плюс сам CLI агента |

CP должен быть доступен с ноды по сети — проверяется до запуска:

```bash
curl -sS -o /dev/null -w '%{http_code}\n' http://<cp-host>:7813/health/ready
```

### Запуск CP

```bash
AGENTGRID_LISTEN=0.0.0.0:7813 \
AGENTGRID_DB=/path/cp.db \
AGENTGRID_JWT_SECRET=<>=32 байта> \
AGENTGRID_ARTIFACT_ROOT=/path/artifacts \
  agentgrid-control-plane
```

Первый пользователь создаётся одноразовым токеном из лога (`/v1/auth/setup`),
затем обычный `/v1/auth/login`. Enrollment-токен выдаёт
`POST /v1/nodes/enrollment-token`.

### Запуск ноды

```bash
AGENTGRID_SERVER=http://<cp-host>:7813 \
AGENTGRID_NODE_NAME=host-b \
AGENTGRID_ADAPTERS=opencode \
AGENTGRID_ADAPTER_ENV="AGENTGRID_OPENCODE_MODEL=provider/model AGENTGRID_OPENCODE_BIN=/opt/oc/bin/opencode XDG_DATA_HOME=/tmp/oc-data" \
AGENTGRID_ENROLL_TOKEN=<token> \
AGENTGRID_UNSAFE_UNATTENDED=1 AGENTGRID_I_UNDERSTAND_UNSAFE=1 AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX=1 \
  agentgrid-node-daemon
```

### Четыре грабли, на которых это ломается

1. **Нода очищает окружение дочернего процесса.** Всё, что нужно агенту —
   модель, путь к CLI, `XDG_DATA_HOME`, — обязано ехать через
   `AGENTGRID_ADAPTER_ENV` (K=V через пробел). Переменная, выставленная в
   окружении самой ноды, до адаптера не дойдёт.
2. **`adapter-<id>` должен лежать рядом с демоном.** Без него нода стартует,
   но пишет `adapter for opencode not ready` и остаётся `degraded` —
   планировщик на неё задачи не кладёт.
3. **Нода отказывается работать под root.** Нужен отдельный пользователь
   (`useradd -m agtest`, `chown` на каталог, `su agtest -c ./start.sh`), а не
   `AGENTGRID_ALLOW_ROOT=1` — агент исполняет команды от того, кем запущен.
4. **`XDG_DATA_HOME` у opencode — это хранилище сессий и ключей.** Уводите
   его в отдельный каталог, иначе тест перепишет рабочие сессии хоста.

### Что проверять

- `GET /v1/nodes` — обе ноды `online` (не `degraded`);
- у каждой задачи `status=succeeded`;
- в событиях есть `tool` с непустым `callID` и парным `result`;
- `metric` с ненулевыми `tokens`;
- в логах нод `attempt finished ... exit_code:0` распределены по разным
  хостам.
