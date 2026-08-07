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
