# 🌌 AntigravityUnlocker (Client v1.0.79 + Self-Hosted Server)

[![GitHub releases](https://img.shields.io/github/v/release/VlDubr/AntigravityUnlocker?style=flat-square&label=Версия)](https://github.com/VlDubr/AntigravityUnlocker/releases)
[![Platforms](https://img.shields.io/badge/Платформы-Windows%20|%20Linux%20Server-blue?style=flat-square)](https://github.com/VlDubr/AntigravityUnlocker/releases)

**AntigravityUnlocker** — клиент (**Antigravity Client**) и собственный self-hosted шлюз (`server/`) для работы с **Antigravity IDE**, **Antigravity 2.0**, **Antigravity CLI (agy)** и OpenAI-совместимыми инструментами (включая **VS Code / GitHub Copilot BYOK**) через ваш личный VPS без общего VPN на компьютере.

---

## 🆕 Что нового в v1.0.79

- **CONNECT-туннель для Antigravity 2.0 (`/v1/proxy-connect`):** добавлен аутентифицированный WebSocket TCP-мост на сервере (с проверкой портов `80`/`443` и публичных IP) и обработка `CONNECT` в локальном прокси клиента.
- **Исправление `wss://` handshake в клиенте (`rustls`):** явная установка `CryptoProvider` (`ring`) при старте приложения — устраняет ошибку `forcibly closed` при запуске Antigravity 2.0 и обратного туннеля.
- **Актуальный список моделей и квот:** `/v1/models` и `/v1/quota` на сервере подтягивают реальные модели, их метаданные и остаток квоты через `fetchAvailableModels` (с кэшем на 60 секунд); клиент больше не скрывает модели жёстким фильтром.
- **Совместимость с GitHub Copilot (VS Code):** нормализация `systemInstruction` в OpenAI-адаптере (устраняет синтетический `429 Resource has been exhausted` от upstream на стандартный boilerplate Copilot) и диагностическое логирование ошибок `generateContent`.

---

## 🌟 Ключевые возможности

* **⚡ Работа без общего VPN:** Клиент поднимает локальный точечный прокси (`127.0.0.1:8047`) только для трафика IDE и CLI. Остальная система работает напрямую.
* **🛡️ Полный контроль на своём VPS:** Вы сами разворачиваете `antigravity-server` на своём сервере, храните токены локально и ограничиваете доступ своим `AG_CLIENT_KEY`.
* **🔌 OpenAI-совместимый шлюз:** Используйте модели из Antigravity в VS Code (Manage Language Models → Custom Endpoint) и других совместимых клиентах.
* **🔄 Одновременный запуск IDE, 2.0 и CLI:** Общий локальный шлюз без конфликтов портов.

---

## 📥 Скачать релиз (v1.0.79)

### 🖥️ Antigravity Client (Windows)

| Компонент | Формат | Ссылка на скачивание |
|---|---|---|
| 🪟 **Windows (10/11 x64)** | Setup EXE (установщик NSIS) | [**Antigravity.Client_1.0.79_x64-setup.exe**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.79/Antigravity.Client_1.0.79_x64-setup.exe) |
| 🪟 **Windows (10/11 x64)** | MSI Installer | [**Antigravity.Client_1.0.79_x64_en-US.msi**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.79/Antigravity.Client_1.0.79_x64_en-US.msi) |
| 🪟 **Windows (10/11 x64)** | Portable EXE | [**antigravity-client.exe**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.79/antigravity-client.exe) |

### 🐧 Self-Hosted Server (Linux VPS)

| Компонент | Архитектура | Ссылка на скачивание |
|---|---|---|
| 🐧 **Linux (static musl)** | x86_64 | [**antigravity-server-linux-x86_64**](https://github.com/VlDubr/AntigravityUnlocker/releases/download/v1.0.79/antigravity-server-linux-x86_64) |

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
