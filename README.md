# 🌌 AntigravityUnlocker (Client v1.0.78 + Self-Hosted Server)

[![GitHub releases](https://img.shields.io/github/v/release/VlDubr/AntigravityUnlocker?style=flat-square&label=Версия)](https://github.com/VlDubr/AntigravityUnlocker/releases)
[![Platforms](https://img.shields.io/badge/Платформы-Windows%20|%20Linux%20Server-blue?style=flat-square)](https://github.com/VlDubr/AntigravityUnlocker/releases)

**AntigravityUnlocker** — клиент (**Antigravity Client**) и собственный self-hosted шлюз (`server/`) для работы с **Antigravity IDE**, **Antigravity 2.0**, **Antigravity CLI (agy)** и OpenAI-совместимыми инструментами (включая **VS Code / GitHub Copilot BYOK**) через ваш личный VPS без общего VPN на компьютере.

---

## 🆕 Что нового в v1.0.78

- **Собственный сервер-шлюз (`server/`):** лёгкий Rust-сервер под Linux x86_64 (`musl`), запуск через `systemd`, поддержка HTTPS через Caddy (`flush_interval -1`) и WebSocket reverse-tunnel (`routing_mode`). Подробная инструкция по развёртыванию: [server/README.md](server/README.md).
- **OpenAI Chat Completions API (`/v1/chat/completions`):** opt-in адаптер (`AG_OPENAI_ENABLED=true`) с поддержкой стриминга (SSE) и function calling (`tool_calls` + сохранение `thoughtSignature`).
- **Интеграция с Copilot в интерфейсе клиента:** рядом с кнопками `IDE`, `V2.0` и `CLI` добавлена кнопка **API** с пошаговой инструкцией подключения к VS Code / GitHub Copilot.
- **Карточка характеристик модели:** клик по модели в списке открывает модальное окно с её ID, именем, лимитами контекста/вывода и поддержкой возможностей (изображения, tools), если эти данные предоставлены сервером.
- **Исправления Antigravity 2.0 и авторизации:**
  - Исключён loopback из прокси (`--proxy-bypass-list=localhost;127.0.0.1;[::1]`) — исправлен чёрный экран (`ERR_EMPTY_RESPONSE`) при запуске Antigravity 2.0.
  - Исправлена запись/чтение учётных данных Windows Credential Manager через нативный Win32 API (`CredWriteW` / `CredDeleteW` / `CredReadW`).
  - Скрипт `server/tools/extract-creds.py` по умолчанию выводит только рекомендуемую consumer-пару `client_id` / `client_secret` для Antigravity 2.0 (`--all-clients` для полной диагностики).
  - Discovery `loadCodeAssist` переведено на Antigravity Hub User-Agent и `ideType=ANTIGRAVITY`, благодаря чему личные аккаунты используют управляемый проект (`aicode-consumers`) без ручного `AG_GOOGLE_PROJECT_ID`.

---

## 🌟 Ключевые возможности

* **⚡ Работа без общего VPN:** Клиент поднимает локальный точечный прокси (`127.0.0.1:8047`) только для трафика IDE и CLI. Остальная система работает напрямую.
* **🛡️ Полный контроль на своём VPS:** Вы сами разворачиваете `antigravity-server` на своём сервере, храните токены локально и ограничиваете доступ своим `AG_CLIENT_KEY`.
* **🔌 OpenAI-совместимый шлюз:** Используйте модели из Antigravity в VS Code (Manage Language Models → Custom Endpoint) и других совместимых клиентах.
* **🔄 Одновременный запуск IDE, 2.0 и CLI:** Общий локальный шлюз без конфликтов портов.

---

## 📥 Скачать релиз (v1.0.78)

### 🖥️ Antigravity Client (Windows)

| Компонент | Формат | Ссылка на скачивание |
|---|---|---|
| 🪟 **Windows (10/11 x64)** | Setup EXE (установщик NSIS) | [**Antigravity.Client_1.0.78_x64-setup.exe**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.78/Antigravity.Client_1.0.78_x64-setup.exe) |
| 🪟 **Windows (10/11 x64)** | MSI Installer | [**Antigravity.Client_1.0.78_x64_en-US.msi**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.78/Antigravity.Client_1.0.78_x64_en-US.msi) |
| 🪟 **Windows (10/11 x64)** | Portable EXE | [**antigravity-client.exe**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.78/antigravity-client.exe) |

### 🐧 Self-Hosted Server (Linux VPS)

| Компонент | Архитектура | Ссылка на скачивание |
|---|---|---|
| 🐧 **Linux (static musl)** | x86_64 | [**antigravity-server-linux-x86_64**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.78/antigravity-server-linux-x86_64) |

📦 Все сборки доступны на [**Странице релизов (Releases)**](https://github.com/VlDubr/AntigravityUnlocker/releases).

---

## 🚀 Быстрый старт

1. **Разверните сервер** на зарубежном VPS (NL/EU/US) по инструкции в [server/README.md](server/README.md) и настройте HTTPS (например, через Caddy).
2. **Скачайте и запустите Antigravity Client** на Windows.
3. В настройках укажите **Server URL** (например, `https://<ваш-домен>`) и ваш **Token** (`AG_CLIENT_KEY`).
4. Нажмите **«Подключиться»** и запускайте **IDE**, **V2.0**, **CLI** или подключите **API** в VS Code Copilot.

---

## 🤝 Ссылки

* 📖 **Документация сервера и деплоя**: [server/README.md](server/README.md)
* 🐛 **Сообщить о проблеме**: [GitHub Issues](https://github.com/VlDubr/AntigravityUnlocker/issues)
