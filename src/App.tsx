import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
// import { getCurrentWindow } from '@tauri-apps/api/window';
import { 
  Shield, Zap, RefreshCw, LogIn, Key, 
  Server, CheckCircle, XCircle, Cpu, Globe, Settings,
  Wifi, WifiOff, LogOut, Square, ChevronDown, Terminal,
  Code, AlertTriangle, Info
} from 'lucide-react';
import pkg from '../package.json';

async function rustFetch(url: string, options: any = {}): Promise<Response> {
  const method = options.method || 'GET';
  const headers = options.headers || {};
  const body = options.body || null;
  
  try {
    const responseText = await invoke<string>('request_server', {
      method,
      url,
      headers,
      body,
    });
    
    return {
      ok: true,
      status: 200,
      json: async () => JSON.parse(responseText),
      text: async () => responseText,
    } as Response;
  } catch (err: any) {
    console.error("rustFetch error", err);
    let status = 500;
    let text = typeof err === 'string' ? err : (err.message || err.toString());
    if (typeof text === 'string' && text.startsWith('HTTP ')) {
      const parts = text.split(' : ');
      status = parseInt(parts[0].replace('HTTP ', ''), 10) || 500;
      text = parts.slice(1).join(' : ');
    }
    
    return {
      ok: false,
      status,
      json: async () => {
        try {
          return JSON.parse(text);
        } catch {
          return { error: text };
        }
      },
      text: async () => text,
    } as Response;
  }
}


// ─── Types ────────────────────────────────────────────────────────

interface ModelInfo {
  id: string;
  object?: string;
  owned_by?: string;
  display_name?: string;
  context_window?: number;
  max_output_tokens?: number;
  supports_images?: boolean;
  supports_tools?: boolean;
  thinking?: boolean;
  supports_reasoning_effort?: string[];
  reasoning_variants?: Record<string, string>;
  quota_remaining_fraction?: number;
  quota_reset_time?: string;
}

interface QuotaBucket {
  bucket_id: string;
  window: string;
  percent: number;
  reset_time?: string | null;
}

interface QuotaGroup {
  display_name: string;
  buckets: QuotaBucket[];
}

interface ModelsResponse {
  data: ModelInfo[];
  object?: string;
}

// ─── Constants ────────────────────────────────────────────────────


const LS_LANG_KEY = 'ag_lang';

const dict = {
  en: {
    secureClient: "Secure Proxy Client",
    serverUrl: "Server URL",
    accessToken: "Access Token",
    connect: "Connect",
    connecting: "Connecting...",
    connected: "Connected",
    ping: "Ping",
    status: "Status",
    active: "Active",
    tokenAuth: "Token authenticated",
    models: "Models",
    availModels: "Available AI models",
    credits: "Credits",
    totalCredits: "Total combined credits",
    server: "Server",
    proxyPing: "Proxy endpoint ping",
    ready: "Ready to Code?",
    connectToIDE: "Connect to Antigravity IDE with your secure proxy token. A local proxy on port 8047 will route all IDE requests through the Manager.",
    localProxy: "Local Proxy Active on",
    stop: "Stop",
    ideLaunched: "IDE launched successfully! Proxy running.",
    reconnect: "Reconnect IDE",
    connectTo: "Connect to",
    logout: "Logout",
    settings: "Settings",
    ideSettings: "IDE Settings",
    customExe: "Custom Executable Path (Optional)",
    leaveBlank: "Leave blank to use default OS path.",
    customDb: "Custom DB / AppData Path (Optional)",
    pathToVscdb: "Path to state.vscdb for portable installations.",
    done: "Done",
    saveReconnect: "Save and Reconnect",
    proxyUrlForIde: "Proxy URL for IDE:",
    error: "Error",
    offline: "Offline",
    availableModelsTitle: "Available Models",
    modelsCount: "models",
    serverConfig: "Server Configuration",
    daysLeft: "days left",
    serverStatus: "Server Status",
    support: "Support",
    getToken: "Get Token",
    plan: "Plan",
    checkForUpdates: "Check for Updates",
    checkingUpdates: "Checking for updates...",
    updateAvailable: "New Version Available",
    updateBtn: "Update Now",
    upToDate: "You have the latest version",
    updateError: "Failed to check or install update",
    updating: "Updating...",
    activeProxy: "Active",
    secureProxy: "Secure proxy via port 8047",
    launchIde: "Launch IDE",
    launchV2: "Launch 2.0",
    launchCli: "Launch CLI",
    launchAll: "Launch All",
    proxyActive: "Proxy Active (:8047)",
    stopProxy: "Stop Proxy",
    apiHelpBtn: "API",
    apiHelpTitle: "API Access",
    apiBaseLabel: "OpenAI-compatible base URL:",
    apiKeyLine: "The API key is the same access token you use to log in here.",
    apiModelsLine: "Model IDs: GET /v1/models fetches the models available to your upstream account (or the gateway override list if configured).",
    apiPrereqTitle: "Server prerequisite",
    apiPrereqText: "Upgrade the gateway and set AG_OPENAI_ENABLED=true on the server. Without the adapter, Chat Completions requests are not translated.",
    apiNoteTitle: "Note",
    apiNoteText: "The adapter supports text, streaming responses, function tools, and reasoning_effort routing for consolidated models.",
    apiSecurityTitle: "Security",
    apiSecurityText: "Sending requests over remote HTTP exposes your API key and data to interception. Use HTTPS for remote connections.",
    apiCopilotTitle: "VS Code Copilot (BYOK)",
    apiCopilotUnsupported: "BYOK covers Chat and agent mode; inline completions are not supported.",
    apiVscodeSteps: [
      "Open Command Palette (Ctrl+Shift+P) and run \"Chat: Manage Language Models\".",
      "Choose \"Add Models\", then \"Custom Endpoint\" and \"Chat Completions\". Enter the gateway key when prompted.",
      "In the generated chatLanguageModels.json, set the model URL to: {endpoint}",
      "Use a model ID returned by GET /v1/models.",
      "Set \"toolCalling\": true and \"vision\": false. Adjust the token limits for the selected model.",
      "Select the model in Chat. Keep the key in VS Code's secret input, not in the file.",
    ],
    apiConfigTitle: "Example chatLanguageModels.json",
    apiExampleHint: "Set the local AG_CLIENT_KEY environment variable to your gateway key and replace MODEL_ID before running.",
    apiExampleTitle: "Example request",
    apiClose: "Close",
    apiCopy: "Copy",
    apiCopied: "Copied",
    apiCopyError: "Copy failed. Select and copy the text manually.",
    modelDetailsHint: "Click for details",
    modelDetailsNote: "Model list returned by GET /v1/models from the gateway (fetched from upstream when available).",
    mdModelId: "Model ID",
    mdDisplayName: "Display name",
    mdOwnedBy: "Provider",
    mdContextWindow: "Context window",
    mdMaxOutput: "Max output tokens",
    mdImageInput: "Image input",
    mdToolSupport: "Tool calling",
    mdReasoningEffort: "Reasoning modes",
    mdVariants: "Upstream variants",
    mdQuotaRemaining: "Remaining quota",
    mdCopyCopilot: "Copy Copilot JSON",
    quota5h: "5h limit",
    quotaWeekly: "Weekly limit",
    mdUnknown: "Unknown",
    mdNotProvided: "Not provided by server",
    mdYes: "Supported",
    mdNo: "Not supported"
  },
  ru: {
    secureClient: "Безопасный прокси-клиент",
    serverUrl: "Адрес сервера",
    accessToken: "Токен доступа",
    connect: "Подключиться",
    connecting: "Подключение...",
    connected: "Подключено",
    ping: "Пинг",
    status: "Статус",
    active: "Активен",
    tokenAuth: "Токен подтвержден",
    models: "Модели",
    availModels: "Доступные ИИ-модели",
    credits: "Кредиты",
    totalCredits: "Общий баланс кредитов",
    server: "Сервер",
    proxyPing: "Пинг до прокси",
    ready: "Готовы программировать?",
    connectToIDE: "Подключите Antigravity IDE, используя ваш токен. Локальный прокси на порту 8047 перенаправит все запросы IDE через Менеджер.",
    localProxy: "Локальный прокси запущен на",
    stop: "Остановить",
    ideLaunched: "IDE успешно запущена! Прокси работает.",
    reconnect: "Переподключить IDE",
    connectTo: "Подключить к",
    logout: "Выйти",
    settings: "Настройки",
    ideSettings: "Настройки IDE",
    customExe: "Путь к исполняемому файлу (Опционально)",
    leaveBlank: "Оставьте пустым для стандартного пути ОС.",
    customDb: "Путь к БД / AppData (Опционально)",
    pathToVscdb: "Путь к state.vscdb для портативных версий.",
    done: "Готово",
    saveReconnect: "Сохранить и переподключиться",
    proxyUrlForIde: "URL прокси для IDE:",
    error: "Ошибка",
    offline: "Не в сети",
    availableModelsTitle: "Доступные модели",
    modelsCount: "моделей",
    serverConfig: "Настройки сервера",
    daysLeft: "дней осталось",
    serverStatus: "Статус сервера",
    support: "Поддержка",
    getToken: "Получить токен",
    plan: "Тариф",
    checkForUpdates: "Проверить обновления",
    checkingUpdates: "Проверка обновлений...",
    updateAvailable: "Доступна новая версия",
    updateBtn: "Обновить сейчас",
    upToDate: "Установлена последняя версия",
    updateError: "Не удалось проверить или установить обновление",
    updating: "Обновление...",
    activeProxy: "Активен",
    secureProxy: "Защищенный прокси на порту 8047",
    launchIde: "Запустить IDE",
    launchV2: "Запустить 2.0",
    launchCli: "Запустить CLI",
    launchAll: "Запустить все",
    proxyActive: "Шлюз активен (:8047)",
    stopProxy: "Остановить шлюз",
    apiHelpBtn: "API",
    apiHelpTitle: "Доступ через API",
    apiBaseLabel: "OpenAI-совместимый базовый URL:",
    apiKeyLine: "API-ключ — это тот же токен доступа, которым вы входите здесь.",
    apiModelsLine: "ID моделей: GET /v1/models запрашивает список моделей, доступных вашему аккаунту на upstream (или использует список из конфигурации шлюза).",
    apiPrereqTitle: "Требование к серверу",
    apiPrereqText: "Обновите шлюз и установите на сервере AG_OPENAI_ENABLED=true. Без адаптера запросы Chat Completions не преобразуются.",
    apiNoteTitle: "Примечание",
    apiNoteText: "Адаптер поддерживает текст, потоковые ответы, вызовы функций и маршрутизацию reasoning_effort для объединённых моделей.",
    apiSecurityTitle: "Безопасность",
    apiSecurityText: "Запросы по удалённому HTTP раскрывают API-ключ и данные. Для удалённого подключения используйте HTTPS.",
    apiCopilotTitle: "VS Code Copilot (BYOK)",
    apiCopilotUnsupported: "BYOK работает в Chat и агентном режиме; автодополнение в строке кода не поддерживается.",
    apiVscodeSteps: [
      "Откройте палитру команд (Ctrl+Shift+P) и выполните \"Chat: Manage Language Models\".",
      "Выберите \"Add Models\", затем \"Custom Endpoint\" и \"Chat Completions\". При запросе введите ключ шлюза.",
      "В открывшемся chatLanguageModels.json укажите URL модели: {endpoint}",
      "Используйте ID модели из ответа GET /v1/models.",
      "Установите \"toolCalling\": true и \"vision\": false. Укажите лимиты токенов выбранной модели.",
      "Выберите модель в чате. Храните ключ в секрете VS Code, а не в файле.",
    ],
    apiConfigTitle: "Пример chatLanguageModels.json",
    apiExampleHint: "Перед запуском задайте локальную переменную окружения AG_CLIENT_KEY с ключом шлюза и замените MODEL_ID.",
    apiExampleTitle: "Пример запроса",
    apiClose: "Закрыть",
    apiCopy: "Копировать",
    apiCopied: "Скопировано",
    apiCopyError: "Не удалось скопировать. Выделите текст и скопируйте его вручную.",
    modelDetailsHint: "Нажмите, чтобы посмотреть сведения",
    modelDetailsNote: "Список моделей получен от шлюза (GET /v1/models) на основе доступных моделей upstream.",
    mdModelId: "ID модели",
    mdDisplayName: "Отображаемое имя",
    mdOwnedBy: "Провайдер",
    mdContextWindow: "Контекстное окно",
    mdMaxOutput: "Максимум токенов вывода",
    mdImageInput: "Входные изображения",
    mdToolSupport: "Вызов инструментов",
    mdReasoningEffort: "Режимы рассуждения",
    mdVariants: "Варианты модели",
    mdQuotaRemaining: "Остаток лимита",
    mdCopyCopilot: "Скопировать JSON для Copilot",
    quota5h: "Лимит 5ч",
    quotaWeekly: "Недельный лимит",
    mdUnknown: "Неизвестно",
    mdNotProvided: "Не предоставлено сервером",
    mdYes: "Поддерживается",
    mdNo: "Не поддерживается"
  }
};

const LS_TOKEN_KEY = 'ag_token';
const LS_SERVER_KEY = 'ag_server_url';
const LS_IDE_TYPE_KEY = 'ag_ide_type';
const LS_CUSTOM_EXE_IDE_KEY = 'ag_custom_exe_ide';
const LS_CUSTOM_DB_IDE_KEY = 'ag_custom_db_ide';
const LS_CUSTOM_EXE_V2_KEY = 'ag_custom_exe_v2';
const LS_CUSTOM_DB_V2_KEY = 'ag_custom_db_v2';
const LS_CUSTOM_EXE_CLI_KEY = 'ag_custom_exe_cli';

const DEFAULT_SERVER_URL = (import.meta as any).env?.VITE_SERVER_URL || 'https://api.antigravity.dev';

// ─── Model Display Helpers ────────────────────────────────────────

const MODEL_CATEGORIES: Record<string, { label: string; color: string; icon: string }> = {
  'gemini':  { label: 'Gemini',  color: 'from-blue-500 to-cyan-400',    icon: '✦' },
  'claude':  { label: 'Claude',  color: 'from-orange-500 to-amber-400', icon: '◈' },
  'gpt':     { label: 'GPT',     color: 'from-emerald-500 to-green-400',icon: '◉' },
};

const IDE_ALLOWED_MODELS = [
  'claude-sonnet-4-6',
  'claude-sonnet-4-6-thinking',
  'claude-opus-4-6',
  'claude-opus-4-6-thinking',
  'gemini-3.8-flash-high',
  'gemini-3.8-flash-medium',
  'gemini-3.8-flash-low',
  'gemini-3.8-flash-tiered',
  'gemini-3.8-flash-thinking',
  'gemini-3.8-flash',
  'gemini-3.7-flash-high',
  'gemini-3.7-flash-medium',
  'gemini-3.7-flash-low',
  'gemini-3.7-flash-tiered',
  'gemini-3.7-flash-thinking',
  'gemini-3.7-flash',
  'gemini-3.6-flash-high',
  'gemini-3.6-flash-medium',
  'gemini-3.6-flash-low',
  'gemini-3.6-flash-tiered',
  'gemini-3.5-flash-high',
  'gemini-3.5-flash-medium',
  'gemini-3.5-flash-low',
  'gemini-3.1-pro-high',
  'gemini-3.1-pro-low',
  'gemini-3.1-flash-lite',
  'gemini-3.1-flash-image',
  'gemini-3-flash',
  'gemini-3-flash-agent',
  'gemini-pro-agent',
  'gpt-oss-120b-medium'
];

function isAllowedModel(id: string): boolean {
  const lower = id.toLowerCase();
  // Exclude 2.5 series
  if (lower.includes('2.5') || lower.includes('2-5')) {
    return false;
  }
  // GPT: only gpt-oss allowed
  if (lower.startsWith('gpt')) {
    return lower.includes('oss');
  }
  // Claude: only 4.6 allowed
  if (lower.startsWith('claude')) {
    return lower.includes('4-6') || lower.includes('4.6');
  }
  return true;
}

function getModelCategory(modelId: string) {
  for (const [key, val] of Object.entries(MODEL_CATEGORIES)) {
    if (modelId.toLowerCase().includes(key)) return val;
  }
  return { label: 'Other', color: 'from-purple-500 to-pink-400', icon: '◇' };
}

function formatModelName(id: string): string {
  return id
    .replace(/-/g, ' ')
    .replace(/\b\w/g, c => c.toUpperCase());
}

const EFFORT_ORDER = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const;
const EFFORT_SUFFIXES: ReadonlyArray<readonly [string, string]> = [
  ['-minimal', 'minimal'],
  ['-xhigh', 'xhigh'],
  ['-medium', 'medium'],
  ['-high', 'high'],
  ['-low', 'low'],
  ['-max', 'max'],
];

function splitEffortSuffix(id: string): [string, string] | null {
  for (const [suffix, effort] of EFFORT_SUFFIXES) {
    if (id.endsWith(suffix) && id.length > suffix.length) {
      return [id.slice(0, -suffix.length), effort];
    }
  }
  return null;
}

function stripEffortFromDisplayName(name: string): string {
  const trimmed = name.trim();
  const tags = [
    '(XHigh)', '(Minimal)', '(Medium)', '(High)', '(Low)', '(Max)',
    'XHigh', 'Minimal', 'Medium', 'High', 'Low', 'Max',
  ];
  const lower = trimmed.toLowerCase();
  for (const tag of tags) {
    if (lower.length > tag.length && lower.endsWith(tag.toLowerCase())) {
      const prefix = trimmed.slice(0, trimmed.length - tag.length).trimEnd();
      if (prefix.length > 0) return prefix;
    }
  }
  return trimmed;
}

function fractionToPercent(fraction: number): number {
  if (fraction <= 0) return 0;
  if (fraction >= 1) return 100;
  return Math.min(99, Math.max(1, Math.round(fraction * 100)));
}

function consolidateClientModels(list: ModelInfo[]): ModelInfo[] {
  const groups = new Map<string, Array<{ effort: string | null; model: ModelInfo }>>();
  for (const model of list) {
    if (model.supports_reasoning_effort && model.supports_reasoning_effort.length > 0) {
      groups.set(model.id, [{ effort: null, model }]);
      continue;
    }
    const split = splitEffortSuffix(model.id);
    const key = split ? split[0] : model.id;
    const effort = split ? split[1] : null;
    const arr = groups.get(key) || [];
    arr.push({ effort, model });
    groups.set(key, arr);
  }

  const out: ModelInfo[] = [];
  for (const [baseId, items] of groups.entries()) {
    const hasEffort = items.some(i => i.effort !== null);
    if (items.length < 2 || !hasEffort) {
      for (const item of items) out.push(item.model);
      continue;
    }

    const variantMap: Record<string, string> = {};
    for (const item of items) {
      if (item.effort) variantMap[item.effort] = item.model.id;
    }
    const efforts = EFFORT_ORDER.filter(e => Boolean(variantMap[e]));
    const firstWithName = items.find(i => i.model.display_name)?.model.display_name;
    let displayName = firstWithName ? stripEffortFromDisplayName(firstWithName) : formatModelName(baseId);
    if (baseId.endsWith('-tiered') && !displayName.toLowerCase().includes('tiered')) {
      displayName = `${displayName} (Tiered)`;
    }

    const contexts = items.map(i => i.model.context_window).filter((n): n is number => typeof n === 'number' && n > 0);
    const outputs = items.map(i => i.model.max_output_tokens).filter((n): n is number => typeof n === 'number' && n > 0);
    const imgFlags = items.map(i => i.model.supports_images).filter((b): b is boolean => typeof b === 'boolean');
    const toolFlags = items.map(i => i.model.supports_tools).filter((b): b is boolean => typeof b === 'boolean');

    let minFraction: number | undefined;
    let minReset: string | undefined;
    for (const { model } of items) {
      if (typeof model.quota_remaining_fraction === 'number') {
        if (minFraction === undefined || model.quota_remaining_fraction < minFraction) {
          minFraction = model.quota_remaining_fraction;
          minReset = model.quota_reset_time || minReset;
        }
      } else if (!minReset && model.quota_reset_time) {
        minReset = model.quota_reset_time;
      }
    }

    const orderedVariants: Record<string, string> = {};
    for (const e of efforts) orderedVariants[e] = variantMap[e];

    out.push({
      id: baseId,
      object: 'model',
      owned_by: items[0].model.owned_by || 'antigravity',
      display_name: displayName,
      context_window: contexts.length > 0 ? Math.max(...contexts) : undefined,
      max_output_tokens: outputs.length > 0 ? Math.max(...outputs) : undefined,
      supports_images: imgFlags.length > 0 ? imgFlags.some(Boolean) : undefined,
      supports_tools: toolFlags.length > 0 ? toolFlags.some(Boolean) : undefined,
      thinking: true,
      supports_reasoning_effort: [...efforts],
      reasoning_variants: orderedVariants,
      quota_remaining_fraction: minFraction,
      quota_reset_time: minReset,
    });
  }

  out.sort((a, b) => a.id.localeCompare(b.id));
  return out;
}

function getModelQuota(
  model: ModelInfo,
  modelPercentages: Record<string, number>,
  modelResets: Record<string, string>,
): { pct?: number; resetTime?: string } {
  if (modelPercentages[model.id] !== undefined) {
    return { pct: modelPercentages[model.id], resetTime: modelResets[model.id] };
  }
  if (model.reasoning_variants) {
    let minPct: number | undefined;
    let resetTime: string | undefined;
    for (const vid of Object.values(model.reasoning_variants)) {
      const p = modelPercentages[vid];
      if (p !== undefined && (minPct === undefined || p < minPct)) {
        minPct = p;
        resetTime = modelResets[vid] || resetTime;
      } else if (!resetTime && modelResets[vid]) {
        resetTime = modelResets[vid];
      }
    }
    if (minPct !== undefined) {
      return { pct: minPct, resetTime };
    }
  }
  if (typeof model.quota_remaining_fraction === 'number') {
    return {
      pct: fractionToPercent(model.quota_remaining_fraction),
      resetTime: model.quota_reset_time,
    };
  }
  return { resetTime: modelResets[model.id] || model.quota_reset_time };
}

// Renders optional server-advertised token counts. Never derived from the model ID.
function formatTokenCount(value: unknown, t: { mdUnknown: string; mdNotProvided: string }) {
  if (value === undefined || value === null) {
    return <span className="text-xs text-gray-500 italic">{t.mdNotProvided}</span>;
  }
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return <span className="text-xs text-gray-500 italic">{t.mdUnknown}</span>;
  }
  return <span className="text-xs text-gray-200 font-mono">{value.toLocaleString()}</span>;
}

// Renders optional server-advertised capability flags (true/false), unknown if absent.
function formatSupportLabel(value: unknown, t: { mdYes: string; mdNo: string; mdUnknown: string; mdNotProvided: string }) {
  if (value === true) return <span className="text-xs text-emerald-300">{t.mdYes}</span>;
  if (value === false) return <span className="text-xs text-rose-300">{t.mdNo}</span>;
  if (value === undefined || value === null) {
    return <span className="text-xs text-gray-500 italic">{t.mdNotProvided}</span>;
  }
  return <span className="text-xs text-gray-500 italic">{t.mdUnknown}</span>;
}

function CountdownTimer({ resetTime }: { resetTime: string }) {
  const [timeLeft, setTimeLeft] = useState<string>('');

  useEffect(() => {
    const calculateTime = () => {
      const target = new Date(resetTime).getTime();
      const now = Date.now();
      const diff = target - now;

      if (diff <= 0) {
        setTimeLeft('now');
        return;
      }

      const days = Math.floor(diff / (1000 * 60 * 60 * 24));
      const hours = Math.floor((diff % (1000 * 60 * 60 * 24)) / (1000 * 60 * 60));
      const minutes = Math.floor((diff % (1000 * 60 * 60)) / (1000 * 60));
      const seconds = Math.floor((diff % (1000 * 60)) / 1000);

      if (days > 0) {
        setTimeLeft(`${days}d ${hours}h`);
        return;
      }

      const parts = [];
      if (hours > 0) parts.push(`${hours}h`);
      if (minutes > 0 || hours > 0) parts.push(`${minutes}m`);
      parts.push(`${seconds}s`);

      setTimeLeft(parts.join(' '));
    };

    calculateTime();
    const interval = setInterval(calculateTime, 1000);
    return () => clearInterval(interval);
  }, [resetTime]);

  if (!timeLeft) return null;
  return <span className="text-[10px] text-amber-500 font-medium ml-1 flex items-center gap-0.5">⏳ {timeLeft}</span>;
}

// ─── App Component ────────────────────────────────────────────────

export default function App() {
    const [lang, setLang] = useState<'en'|'ru'>('en');
  const [showLangDropdown, setShowLangDropdown] = useState(false);
  const t = dict[lang];

  useEffect(() => {
    const savedLang = localStorage.getItem(LS_LANG_KEY) as 'en'|'ru';
    if (savedLang) setLang(savedLang);
  }, []);

  const changeLang = (newLang: 'en'|'ru') => {
    setLang(newLang);
    localStorage.setItem(LS_LANG_KEY, newLang);
    setShowLangDropdown(false);
  };

  // Close dropdown on click outside
  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (!(e.target as Element).closest('.lang-dropdown')) {
        setShowLangDropdown(false);
      }
    };
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, []);



  const [token, setToken] = useState('');
  const [serverUrl, setServerUrl] = useState(DEFAULT_SERVER_URL);
  
  // Settings State
  const [showSettings, setShowSettings] = useState(false);
  const [showApiHelp, setShowApiHelp] = useState(false);
  const [apiCopiedSection, setApiCopiedSection] = useState<'config' | 'request' | null>(null);
  const [apiCopyError, setApiCopyError] = useState(false);
  const apiDialogRef = useRef<HTMLDivElement>(null);
  const [detailsModel, setDetailsModel] = useState<ModelInfo | null>(null);
  const [detailsCopied, setDetailsCopied] = useState(false);
  const detailsDialogRef = useRef<HTMLDivElement>(null);
  const [ideType, setIdeType] = useState('Antigravity IDE');
  const [customExePathIde, setCustomExePathIde] = useState('');
  const [customDbPathIde, setCustomDbPathIde] = useState('');
  const [customExePathV2, setCustomExePathV2] = useState('');
  const [customDbPathV2, setCustomDbPathV2] = useState('');
  const [customExePathCli, setCustomExePathCli] = useState('');

  // Main App State
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [modelPercentages, setModelPercentages] = useState<Record<string, number>>({});
  const [modelResets, setModelResets] = useState<Record<string, string>>({});
  const [quotaGroups, setQuotaGroups] = useState<QuotaGroup[]>([]);
  const [_totalCredits, setTotalCredits] = useState<number | null>(null);
  const [creditOverages, setCreditOverages] = useState(false);
  const [connected, setConnected] = useState(false);
  const [expiresAt, setExpiresAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  // Client Update States
  const CURRENT_VERSION = pkg.version;
  const [updateInfo, setUpdateInfo] = useState<{
    version: string;
    name: string;
    body: string;
    asset_id: string | number;
    size?: number;
  } | null>(null);
  const [checkingUpdates, setCheckingUpdates] = useState(false);
  const [updateError, setUpdateError] = useState('');
  const [updating, setUpdating] = useState(false);
  const [updateStatus, setUpdateStatus] = useState<'none' | 'up-to-date' | 'available' | 'error'>('none');

  const getOS = () => {
    const userAgent = window.navigator.userAgent.toLowerCase();
    if (userAgent.includes("win")) return "windows";
    if (userAgent.includes("mac")) return "macos";
    if (userAgent.includes("linux")) return "linux";
    return "windows";
  };

  const checkUpdates = async (silent = false) => {
    if (!silent) {
      setCheckingUpdates(true);
      setUpdateError('');
      setUpdateStatus('none');
    }
    try {
      let os = getOS();
      try {
        const tauriOs = await invoke<string>('get_platform_info');
        if (tauriOs) os = tauriOs;
      } catch { /* fallback to getOS() */ }
      const res = await rustFetch(`${serverUrl}/v1/client-update?platform=${os}`);
      if (!res.ok) {
        throw new Error(`Failed to check updates: ${res.status}`);
      }
      const data = await res.json();
      if (data.version && data.version !== CURRENT_VERSION) {
        setUpdateInfo(data);
        setUpdateStatus('available');
      } else {
        setUpdateInfo(null);
        setUpdateStatus('up-to-date');
      }
    } catch (e: any) {
      console.error(e);
      if (!silent) {
        setUpdateError(e.message || 'Error checking for updates');
        setUpdateStatus('error');
      }
    } finally {
      if (!silent) setCheckingUpdates(false);
    }
  };

  const handleInstallUpdate = async () => {
    if (!updateInfo) return;
    setUpdating(true);
    setUpdateError('');
    try {
      const downloadUrl = `${serverUrl}/v1/client-update/download/${updateInfo.asset_id}`;
      await invoke('install_client_update', { downloadUrl });
    } catch (e: any) {
      setUpdateError(e.toString() || 'Failed to install update');
      setUpdating(false);
    }
  };
  const [launchingTool, setLaunchingTool] = useState<string | null>(null);
  const [launchedTools, setLaunchedTools] = useState<Record<string, boolean>>({});
  const [serverOnline, setServerOnline] = useState<boolean | null>(null);
  const [ping, setPing] = useState<number | null>(null);
  const pingHistoryRef = useRef<number[]>([]);
  const [proxyRunning, setProxyRunning] = useState(false);
  const [stoppingProxy, setStoppingProxy] = useState(false);
  const [tier, setTier] = useState<number | null>(null);
  const [planName, setPlanName] = useState<string | null>(null);

  // ─── Load saved state ────────────────────────────────────────────

  useEffect(() => {
    let savedToken = localStorage.getItem(LS_TOKEN_KEY);
    invoke<string>('get_saved_token').then(diskToken => {
      const activeToken = diskToken || savedToken;
      if (activeToken) {
        setToken(activeToken);
        validateToken(activeToken, url);
      } else {
        checkServerHealth(url);
      }
    }).catch(() => {
      if (savedToken) {
        setToken(savedToken);
        validateToken(savedToken, url);
      } else {
        checkServerHealth(url);
      }
    });
    const savedServer = localStorage.getItem(LS_SERVER_KEY) || DEFAULT_SERVER_URL;
    const savedIdeType = localStorage.getItem(LS_IDE_TYPE_KEY);
    const savedExePathLegacy = localStorage.getItem('ag_custom_exe') || '';
    const savedDbPathLegacy = localStorage.getItem('ag_custom_db') || '';

    const savedExePathIde = localStorage.getItem(LS_CUSTOM_EXE_IDE_KEY) || (savedIdeType === 'Antigravity IDE' ? savedExePathLegacy : '') || '';
    const savedDbPathIde = localStorage.getItem(LS_CUSTOM_DB_IDE_KEY) || (savedIdeType === 'Antigravity IDE' ? savedDbPathLegacy : '') || '';
    const savedExePathV2 = localStorage.getItem(LS_CUSTOM_EXE_V2_KEY) || (savedIdeType === 'Antigravity 2.0' ? savedExePathLegacy : '') || '';
    const savedDbPathV2 = localStorage.getItem(LS_CUSTOM_DB_V2_KEY) || (savedIdeType === 'Antigravity 2.0' ? savedDbPathLegacy : '') || '';
    const savedExePathCli = localStorage.getItem(LS_CUSTOM_EXE_CLI_KEY) || '';

    if (savedServer) setServerUrl(savedServer);
    if (savedIdeType) setIdeType(savedIdeType);
    setCustomExePathIde(savedExePathIde);
    setCustomDbPathIde(savedDbPathIde);
    setCustomExePathV2(savedExePathV2);
    setCustomDbPathV2(savedDbPathV2);
    setCustomExePathCli(savedExePathCli);
    
    const url = savedServer || DEFAULT_SERVER_URL;
    
    // Token loading handled via disk/localStorage above
  }, []);

  // ─── Server Health Check ─────────────────────────────────────────

  const checkServerHealth = useCallback(async (url: string) => {
    const startTime = performance.now();
    try {
      const res = await rustFetch(`${url}/health`, { 
        method: 'GET',
      });
      const endTime = performance.now();
      if (res.ok) {
        const rawPing = Math.round(endTime - startTime);
        pingHistoryRef.current.push(rawPing);
        if (pingHistoryRef.current.length > 5) {
            pingHistoryRef.current.shift();
        }
        setPing(Math.min(...pingHistoryRef.current));
        setServerOnline(true);
      } else {
        pingHistoryRef.current = [];
        setPing(null);
        setServerOnline(false);
      }
    } catch {
      pingHistoryRef.current = [];
      setPing(null);
      setServerOnline(false);
    }
  }, []);

  // ─── Token Validation via /v1/models ─────────────────────────────

  const validateToken = useCallback(async (tokenToUse: string, url?: string, forceRefresh = false) => {
    const serverBase = url || serverUrl;
    if (!tokenToUse) return;
    
    setLoading(true);
    setError('');
    const refreshSuffix = forceRefresh ? '?refresh=1' : '';
    
    try {
      const res = await rustFetch(`${serverBase}/v1/models${refreshSuffix}`, {
        headers: {
          'Authorization': `Bearer ${tokenToUse}`,
        },
      });

      if (res.ok) {
        const data: ModelsResponse = await res.json();
        const serverModels = data.data || [];
        const rawList: ModelInfo[] =
          serverModels.length > 0
            ? serverModels
            : IDE_ALLOWED_MODELS.filter(isAllowedModel).map(id => ({
                id,
                object: 'model',
                owned_by: 'antigravity',
              }));
        const modelList = consolidateClientModels(rawList);
        setModels(modelList);
        
        try {
          const qRes = await rustFetch(`${serverBase}/v1/quota${refreshSuffix}`, {
            headers: { 'Authorization': `Bearer ${tokenToUse}` },
          });
          if (qRes.ok) {
            const qData = await qRes.json();
            setTotalCredits(qData.total_credits);
            setExpiresAt(qData.expires_at || null);
            setCreditOverages(qData.enable_credit_overages);
            setModelPercentages(qData.models || {});
            setModelResets(qData.resets || {});
            setQuotaGroups(Array.isArray(qData.quota_groups) ? qData.quota_groups : []);
            setTier(qData.tier !== undefined ? qData.tier : null);
            setPlanName(typeof qData.plan_name === 'string' && qData.plan_name.trim() ? qData.plan_name.trim() : null);
          }
        } catch (e) {
          console.error('Failed to fetch quota', e);
        }

        setConnected(true);
        setServerOnline(true);
        const cleanToken = tokenToUse.trim();
        localStorage.setItem(LS_TOKEN_KEY, cleanToken);
        invoke('save_token', { token: cleanToken }).catch(() => {});
        localStorage.setItem(LS_SERVER_KEY, serverBase);
      } else if (res.status === 401) {
        localStorage.removeItem(LS_TOKEN_KEY);
        invoke('clear_saved_token').catch(() => {});
        throw new Error('Invalid token. Please check your API key.');
      } else if (res.status === 403) {
        // Manager returns detailed rejection reason in body
        try {
          const body = await res.json();
          const msg = body?.error?.message || 'Access denied';
          throw new Error(msg);
        } catch (e: any) {
          if (e.message && e.message !== 'Access denied') throw e;
          throw new Error('Token rejected (403). It may be expired or IP-limited.');
        }
      } else {
        throw new Error(`Server error (${res.status})`);
      }
    } catch (err: any) {
      if (err.name === 'TimeoutError' || err.name === 'AbortError') {
        setError('Connection timeout. Check server URL and make sure Manager is running.');
      } else {
        setError(err.message || 'Connection error');
      }
      setConnected(false);
      setModels([]);
      checkServerHealth(serverBase);
    } finally {
      setLoading(false);
    }
  }, [serverUrl, checkServerHealth]);

  useEffect(() => {
    if (serverOnline) {
      checkUpdates(true);
    }
  }, [serverOnline]);

  // ─── Handlers ────────────────────────────────────────────────────

  const handleLogin = (e: React.FormEvent) => {
    e.preventDefault();
    const trimmed = serverUrl.replace(/\/+$/, '');
    setServerUrl(trimmed);
    localStorage.setItem(LS_SERVER_KEY, trimmed);
    validateToken(token, trimmed);
  };

  const handleLogout = () => {
    localStorage.removeItem(LS_TOKEN_KEY);
    setToken('');
    setConnected(false);
    setModels([]);
    setModelResets({});
    setQuotaGroups([]);
    setError('');
    setTier(null);
    setPlanName(null);
  };

  const handleRefresh = () => {
    validateToken(token, serverUrl, true);
  };

  const handleLaunchTool = async (type: string) => {
    setLaunchingTool(type);
    setError('');
    const exePath = type === 'Antigravity 2.0' ? customExePathV2 : 
                    type === 'Antigravity CLI' ? customExePathCli : customExePathIde;
    const dbPath = type === 'Antigravity 2.0' ? customDbPathV2 : 
                   type === 'Antigravity CLI' ? null : customDbPathIde;
    try {
      await invoke('inject_token_and_start_ide', {
        token: token,
        proxyUrl: `${serverUrl}/v1`,
        ideType: type,
        customExePath: exePath || null,
        customDbPath: dbPath || null,
      });
      setLaunchedTools(prev => ({ ...prev, [type]: true }));
      setProxyRunning(true);
      setTimeout(() => {
        setLaunchedTools(prev => ({ ...prev, [type]: false }));
      }, 4000);
    } catch (err: any) {
      setError(`${type} connection failed: ${err}`);
    } finally {
      setLaunchingTool(null);
    }
  };

  const handleStopProxy = async () => {
    setStoppingProxy(true);
    try {
      await invoke('stop_proxy');
      setProxyRunning(false);
      setLaunchedTools({});
    } catch (err: any) {
      setError(`Failed to stop proxy: ${err}`);
    } finally {
      setStoppingProxy(false);
    }
  };

  // Poll proxy status
  useEffect(() => {
    const checkProxy = async () => {
      try {
        const running = await invoke<boolean>('get_proxy_status');
        setProxyRunning(running);
      } catch { /* ignore */ }
    };
    checkProxy();
    const interval = setInterval(checkProxy, 3000);
    return () => clearInterval(interval);
  }, []);

  // Poll quota
  useEffect(() => {
    if (!connected || !token) return;
    const fetchQuota = async () => {
      try {
        const qRes = await rustFetch(`${serverUrl}/v1/quota`, {
          headers: { 'Authorization': `Bearer ${token}` },
        });
        if (qRes.ok) {
          const qData = await qRes.json();
          setTotalCredits(qData.total_credits);
          setExpiresAt(qData.expires_at || null);
          setCreditOverages(qData.enable_credit_overages);
          setModelPercentages(qData.models || {});
          setModelResets(qData.resets || {});
          setQuotaGroups(Array.isArray(qData.quota_groups) ? qData.quota_groups : []);
          setTier(qData.tier !== undefined ? qData.tier : null);
          setPlanName(typeof qData.plan_name === 'string' && qData.plan_name.trim() ? qData.plan_name.trim() : null);
        }
      } catch {
        // Keep last known quota on transient poll errors instead of faking 100%.
      }
    };
    const interval = setInterval(fetchQuota, 10000);
    return () => clearInterval(interval);
  }, [connected, token, serverUrl]);

  // Poll server health / ping every 5 seconds
  useEffect(() => {
    checkServerHealth(serverUrl);
    const interval = setInterval(() => {
      checkServerHealth(serverUrl);
    }, 5000);
    return () => clearInterval(interval);
  }, [serverUrl, checkServerHealth]);

  const handleSaveServerUrl = () => {
    const trimmed = serverUrl.replace(/\/+$/, '');
    setServerUrl(trimmed);
    localStorage.setItem(LS_SERVER_KEY, trimmed);
    localStorage.setItem(LS_IDE_TYPE_KEY, ideType);
    localStorage.setItem(LS_CUSTOM_EXE_IDE_KEY, customExePathIde);
    localStorage.setItem(LS_CUSTOM_DB_IDE_KEY, customDbPathIde);
    localStorage.setItem(LS_CUSTOM_EXE_V2_KEY, customExePathV2);
    localStorage.setItem(LS_CUSTOM_DB_V2_KEY, customDbPathV2);
    setShowSettings(false);
    checkServerHealth(trimmed);
    if (token && connected) {
      validateToken(token, trimmed);
    }
  };

  // ─── API Help (OpenAI-compatible gateway) ───────────────────────
  const apiBaseUrl = `${serverUrl.replace(/\/+$/, '')}/v1`;
  const apiChatEndpoint = `${apiBaseUrl}/chat/completions`;
  const apiModelsEndpoint = `${apiBaseUrl}/models`;
  const buildCopilotModelEntry = (model: ModelInfo) => {
    const entry: Record<string, unknown> = {
      id: model.id,
      name: model.display_name || formatModelName(model.id),
      url: apiChatEndpoint,
      toolCalling: model.supports_tools ?? true,
      vision: model.supports_images ?? false,
    };
    if (model.supports_reasoning_effort && model.supports_reasoning_effort.length > 0) {
      entry.thinking = true;
      entry.supportsReasoningEffort = model.supports_reasoning_effort;
    } else if (model.thinking) {
      entry.thinking = true;
    }
    if (model.context_window) entry.maxInputTokens = model.context_window;
    if (model.max_output_tokens) entry.maxOutputTokens = model.max_output_tokens;
    return entry;
  };
  const exampleModel = models.find(m => m.supports_reasoning_effort?.length) || models[0];
  const apiConfigExample = JSON.stringify([{
    name: 'Antigravity',
    vendor: 'customendpoint',
    apiKey: '${input:antigravityApiKey}',
    apiType: 'chat-completions',
    models: [
      exampleModel
        ? buildCopilotModelEntry(exampleModel)
        : {
            id: 'claude-opus-5-5',
            name: 'Claude Opus 5.5',
            url: apiChatEndpoint,
            toolCalling: true,
            vision: true,
            thinking: true,
            supportsReasoningEffort: ['low', 'medium', 'high'],
            maxInputTokens: 1000000,
            maxOutputTokens: 128000,
          },
    ],
  }], null, 2);
  const apiExampleRequest = getOS() === 'windows'
    ? [
      '$headers = @{ Authorization = "Bearer $env:AG_CLIENT_KEY" }',
      `Invoke-RestMethod -Uri '${apiModelsEndpoint.replace(/'/g, "''")}' -Headers $headers`,
      '$body = @{ model = "MODEL_ID"; messages = @(@{ role = "user"; content = "Hello" }) } | ConvertTo-Json -Depth 5',
      `Invoke-RestMethod -Uri '${apiChatEndpoint.replace(/'/g, "''")}' -Method Post -Headers $headers -ContentType 'application/json' -Body $body`,
    ].join('\n')
    : [
      `curl --fail-with-body '${apiModelsEndpoint.replace(/'/g, "'\\''")}' -H "Authorization: Bearer $AG_CLIENT_KEY"`,
      `curl --fail-with-body '${apiChatEndpoint.replace(/'/g, "'\\''")}' \\`,
      '  -H "Authorization: Bearer $AG_CLIENT_KEY" \\',
      '  -H "Content-Type: application/json" \\',
      '  -d \'{"model":"MODEL_ID","messages":[{"role":"user","content":"Hello"}]}\'',
    ].join('\n');

  const copyApiText = async (section: 'config' | 'request') => {
    setApiCopyError(false);
    try {
      await navigator.clipboard.writeText(section === 'config' ? apiConfigExample : apiExampleRequest);
      setApiCopiedSection(section);
    } catch (e) {
      console.error('Failed to copy API instructions', e);
      setApiCopiedSection(null);
      setApiCopyError(true);
    }
  };

  useEffect(() => {
    if (!apiCopiedSection) return;
    const timer = window.setTimeout(() => setApiCopiedSection(null), 2000);
    return () => window.clearTimeout(timer);
  }, [apiCopiedSection]);

  useEffect(() => {
    if (!showApiHelp) return;
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const dialog = apiDialogRef.current;
    const buttons = () => Array.from(dialog?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') || []);
    buttons()[0]?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setShowApiHelp(false);
      if (e.key !== 'Tab') return;
      const focusable = buttons();
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (!first || !last) {
        e.preventDefault();
      } else if (e.shiftKey && (document.activeElement === first || !dialog?.contains(document.activeElement))) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && (document.activeElement === last || !dialog?.contains(document.activeElement))) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      previousFocus?.focus();
    };
  }, [showApiHelp]);

  useEffect(() => {
    if (!detailsModel) return;
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const dialog = detailsDialogRef.current;
    const buttons = () => Array.from(dialog?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') || []);
    buttons()[0]?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setDetailsModel(null);
      if (e.key !== 'Tab') return;
      const focusable = buttons();
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (!first || !last) {
        e.preventDefault();
      } else if (e.shiftKey && (document.activeElement === first || !dialog?.contains(document.activeElement))) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && (document.activeElement === last || !dialog?.contains(document.activeElement))) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      previousFocus?.focus();
    };
  }, [detailsModel]);

  // @ts-ignore - function unused in new UI but kept for API completeness
  const toggleCreditOverages = async () => {
    try {
      const newState = !creditOverages;
      setCreditOverages(newState); // optimistic update
      await rustFetch(`${serverUrl}/v1/quota/config`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'Authorization': `Bearer ${token}`
        },
        body: JSON.stringify({ enable_credit_overages: newState }),
      });
    } catch (e) {
      console.error('Failed to toggle config', e);
      setCreditOverages(!creditOverages); // revert on error
    }
  };

  // ─── Render: Login Screen ───────────────────────────────────────

  if (!connected) {
    return (
      <div className="min-h-screen bg-[#0A0A0A] flex items-center justify-center relative overflow-hidden text-white font-sans">
                {/* Language Dropdown */}
        <div className="absolute top-4 right-4 z-50 lang-dropdown">
          <button
            onClick={() => setShowLangDropdown(!showLangDropdown)}
            className="flex items-center space-x-1 px-3 h-10 bg-white/5 hover:bg-white/10 rounded-lg transition-colors border border-white/5 text-sm font-medium text-gray-300"
            title="Change Language"
          >
            <Globe className="w-4 h-4" />
            <span>{lang.toUpperCase()}</span>
            <ChevronDown className={`w-4 h-4 transition-transform ${showLangDropdown ? 'rotate-180' : ''}`} />
          </button>
          
          {showLangDropdown && (
            <div className="absolute right-0 mt-2 w-32 bg-[#1a1c23] border border-white/10 rounded-xl shadow-2xl py-1 z-50 overflow-hidden">
              <button
                onClick={() => changeLang('en')}
                className={`w-full text-left px-4 py-2 text-sm hover:bg-white/5 transition-colors ${lang === 'en' ? 'text-blue-400 font-medium' : 'text-gray-300'}`}
              >
                English (EN)
              </button>
              <button
                onClick={() => changeLang('ru')}
                className={`w-full text-left px-4 py-2 text-sm hover:bg-white/5 transition-colors ${lang === 'ru' ? 'text-blue-400 font-medium' : 'text-gray-300'}`}
              >
                Русский (RU)
              </button>
            </div>
          )}
        </div>
        {/* Background Gradients */}
        <div className="absolute top-[-20%] left-[-10%] w-[50%] h-[50%] bg-blue-600/20 blur-[120px] rounded-full pointer-events-none" />
        <div className="absolute bottom-[-20%] right-[-10%] w-[50%] h-[50%] bg-purple-600/20 blur-[120px] rounded-full pointer-events-none" />

        <div className="relative z-10 w-full max-w-md p-8 backdrop-blur-xl bg-white/5 border border-white/10 shadow-2xl rounded-3xl">
          {/* Logo */}
          <div className="flex flex-col items-center mb-8">
            <div className="w-16 h-16 bg-gradient-to-br from-blue-500 to-purple-600 rounded-2xl flex items-center justify-center mb-4 shadow-lg shadow-purple-500/30">
              <Shield className="w-8 h-8 text-white" />
            </div>
            <h1 className="text-3xl font-bold bg-clip-text text-transparent bg-gradient-to-r from-white to-white/60">
              Antigravity
            </h1>
            <p className="text-sm text-gray-400 mt-2 text-center">
              {t.secureClient}
            </p>
          </div>

          <form onSubmit={handleLogin} className="space-y-5">
            {/* Server Status Display */}
            <div>
              <div className="flex items-center justify-between bg-black/40 border border-white/10 rounded-xl p-3 px-4">
                <div className="flex items-center space-x-3">
                  <Globe className="h-5 w-5 text-purple-400" />
                  <span className="text-sm font-medium text-gray-300">{t.serverStatus}</span>
                </div>
                <div className="flex items-center space-x-2">
                  <span className={`text-sm font-bold ${
                    ping === null ? 'text-red-400' :
                    ping < 50 ? 'text-emerald-400' :
                    ping < 150 ? 'text-amber-400' : 'text-rose-400'
                  }`}>
                    {ping !== null ? `${ping} ms` : 'Offline'}
                  </span>
                  {serverOnline === true && <Wifi className="h-4 w-4 text-emerald-400" />}
                  {serverOnline === false && <WifiOff className="h-4 w-4 text-red-400" />}
                  {serverOnline === null && <div className="h-4 w-4 rounded-full bg-gray-600" />}
                </div>
              </div>
            </div>

            {/* Server URL Input */}
            <div>
              <label className="block text-sm font-medium text-gray-400 mb-2">
                {t.serverUrl}
              </label>
              <div className="relative">
                <div className="absolute inset-y-0 left-0 pl-4 flex items-center pointer-events-none">
                  <Server className="h-5 w-5 text-gray-500" />
                </div>
                <input
                  type="text"
                  value={serverUrl}
                  onChange={(e) => setServerUrl(e.target.value)}
                  onBlur={() => {
                    const trimmed = serverUrl.replace(/\/+$/, '');
                    setServerUrl(trimmed);
                    localStorage.setItem(LS_SERVER_KEY, trimmed);
                  }}
                  className="block w-full pl-11 pr-4 py-3 bg-black/40 border border-white/10 rounded-xl text-white placeholder-gray-500 focus:ring-2 focus:ring-blue-500/50 focus:border-blue-500/50 transition-all outline-none text-sm"
                  placeholder={lang === "ru" ? "http://сервер:8045" : "http://server:8045"}
                  required
                />
              </div>
            </div>

            {/* Token Input */}
            <div>
              <label className="block text-sm font-medium text-gray-400 mb-2">
                {t.accessToken}
              </label>
              <div className="relative">
                <div className="absolute inset-y-0 left-0 pl-4 flex items-center pointer-events-none">
                  <Key className="h-5 w-5 text-gray-500" />
                </div>
                <input
                  type="password"
                  value={token}
                  onChange={(e) => setToken(e.target.value.trim())}
                  className="block w-full pl-11 pr-4 py-3 bg-black/40 border border-white/10 rounded-xl text-white placeholder-gray-500 focus:ring-2 focus:ring-blue-500/50 focus:border-blue-500/50 transition-all outline-none"
                  placeholder="IMPULS-..."
                  required
                />
              </div>
            </div>

            {/* Error */}
            {error && (
              <div className="text-red-400 text-sm bg-red-400/10 p-3 rounded-lg border border-red-400/20">
                {error}
              </div>
            )}

            {/* Submit */}
            <button
              type="submit"
              disabled={loading || !token || !serverUrl}
              className="w-full py-3 px-4 bg-gradient-to-r from-blue-600 to-purple-600 hover:from-blue-500 hover:to-purple-500 text-white rounded-xl font-medium shadow-lg shadow-purple-500/25 transition-all flex items-center justify-center group disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {loading ? (
                <RefreshCw className="w-5 h-5 animate-spin" />
              ) : (
                <>
                  <LogIn className="w-5 h-5 mr-2 group-hover:translate-x-1 transition-transform" />
                  {t.connect}
                </>
              )}
            </button>
          </form>
          
          <div className="mt-6 flex items-center justify-center space-x-8">
            <a 
              href="https://t.me/AntigravityVipbot" 
              target="_blank" 
              rel="noreferrer" 
              className="inline-flex items-center space-x-2 text-gray-500 hover:text-[#2AABEE] text-xs transition-colors"
            >
              <Key className="w-4 h-4" />
              <span>{t.getToken}</span>
            </a>

            <a 
              href="https://t.me/Ultimateadvansed" 
              target="_blank" 
              rel="noreferrer" 
              className="inline-flex items-center space-x-2 text-gray-500 hover:text-[#2AABEE] text-xs transition-colors"
            >
              <svg className="w-4 h-4" viewBox="0 0 24 24" fill="currentColor">
                <path d="M12 0C5.373 0 0 5.373 0 12s5.373 12 12 12 12-5.373 12-12S18.627 0 12 0zm5.894 8.221l-1.97 9.28c-.145.658-.537.818-1.084.508l-3-2.21-1.446 1.394c-.14.18-.357.223-.548.223l.188-2.85 5.18-4.68c.223-.198-.05-.31-.346-.11l-6.4 4.03-2.76-.864c-.6-.188-.61-.6.125-.89l10.8-4.16c.5-.188.94.108.79.89z"/>
              </svg>
              <span>{t.support}</span>
            </a>
          </div>
        </div>
      </div>
    );
  }

  // ─── Render: Dashboard ──────────────────────────────────────────

  // Group models by category
  const groupedModels: Record<string, ModelInfo[]> = {};
  models.forEach(m => {
    const cat = getModelCategory(m.id);
    if (!groupedModels[cat.label]) groupedModels[cat.label] = [];
    groupedModels[cat.label].push(m);
  });

  
  const getRemainingDays = (timestamp: number) => {
    const diff = timestamp * 1000 - Date.now();
    if (diff <= 0) return 0;
    return Math.ceil(diff / (1000 * 60 * 60 * 24));
  };

  return (
    <div className="min-h-screen bg-[#0A0A0A] p-6 relative overflow-hidden text-white font-sans flex flex-col items-center">
      <div className="absolute top-0 left-1/2 -translate-x-1/2 w-[80%] h-[500px] bg-blue-600/10 blur-[150px] pointer-events-none" />

      <div className="w-full max-w-4xl relative z-10">
        {/* ─── Header ──────────────────────────────────────────── */}
        <div className="flex justify-between items-center mb-6">
                    <div className="flex items-center space-x-3">
            {/* Language Dropdown */}
            <div className="relative lang-dropdown">
              <button
                onClick={() => setShowLangDropdown(!showLangDropdown)}
                className="flex items-center space-x-1 px-3 h-10 bg-white/5 hover:bg-white/10 rounded-lg transition-colors border border-white/5 text-sm font-medium text-gray-300"
                title="Change Language"
              >
                <Globe className="w-4 h-4" />
                <span>{lang.toUpperCase()}</span>
                <ChevronDown className={`w-4 h-4 transition-transform ${showLangDropdown ? 'rotate-180' : ''}`} />
              </button>
              
              {showLangDropdown && (
                <div className="absolute right-0 mt-2 w-32 bg-[#1a1c23] border border-white/10 rounded-xl shadow-2xl py-1 z-50 overflow-hidden">
                  <button
                    onClick={() => changeLang('en')}
                    className={`w-full text-left px-4 py-2 text-sm hover:bg-white/5 transition-colors ${lang === 'en' ? 'text-blue-400 font-medium' : 'text-gray-300'}`}
                  >
                    English (EN)
                  </button>
                  <button
                    onClick={() => changeLang('ru')}
                    className={`w-full text-left px-4 py-2 text-sm hover:bg-white/5 transition-colors ${lang === 'ru' ? 'text-blue-400 font-medium' : 'text-gray-300'}`}
                  >
                    Русский (RU)
                  </button>
                </div>
              )}
            </div>
            <div className="w-10 h-10 bg-gradient-to-br from-blue-500 to-purple-600 rounded-xl flex items-center justify-center shadow-lg">
              <Shield className="w-5 h-5 text-white" />
            </div>
            <div>
              <div className="flex items-center space-x-2">
                <h2 className="text-xl font-bold text-white">Antigravity Client</h2>
                <span className="text-[10px] font-mono bg-white/10 text-gray-300 px-1.5 py-0.5 rounded-md border border-white/5">v{CURRENT_VERSION}</span>
              </div>
              <div className="flex items-center space-x-2 text-xs text-gray-400 mt-0.5">
                <CheckCircle className="w-3 h-3 text-emerald-400" />
                <span>{t.connected}. {t.ping}: {ping !== null ? `${ping} ms` : '...'}</span>
              </div>
            </div>
          </div>

          <div className="flex items-center space-x-3">
            <button
              onClick={handleRefresh}
              className="p-2 bg-white/5 hover:bg-white/10 rounded-lg transition-colors border border-white/5"
              title="Refresh"
            >
              <RefreshCw className={`w-4 h-4 text-gray-300 ${loading ? 'animate-spin' : ''}`} />
            </button>
            <button
              onClick={() => setShowSettings(!showSettings)}
              className="p-2 bg-white/5 hover:bg-white/10 rounded-lg transition-colors border border-white/5"
              title="Settings"
            >
              <Settings className="w-4 h-4 text-gray-300" />
            </button>
            <button
              onClick={handleLogout}
              className="text-sm px-3 py-2 bg-white/5 hover:bg-red-500/20 hover:text-red-400 rounded-lg transition-colors border border-white/5 flex items-center space-x-1"
            >
              <LogOut className="w-4 h-4" />
              <span>{t.logout}</span>
            </button>
          </div>
        </div>

        {/* ─── Settings Panel ──────────────────────────────────── */}
        {showSettings && (
          <div className="mb-6 backdrop-blur-xl bg-white/5 border border-white/10 rounded-2xl p-5 animate-in fade-in slide-in-from-top-2">
            <h3 className="text-sm font-semibold text-gray-300 mb-3 flex items-center space-x-2">
              <Server className="w-4 h-4" />
              <span>{t.serverConfig}</span>
            </h3>
            <div className="flex space-x-3">
              <input
                type="text"
                value={serverUrl}
                onChange={(e) => setServerUrl(e.target.value)}
                className="flex-1 px-4 py-2.5 bg-black/40 border border-white/10 rounded-xl text-white text-sm focus:ring-2 focus:ring-blue-500/50 outline-none"
                placeholder={lang === "ru" ? "http://сервер:8045" : "http://server:8045"}
              />
              <button
                onClick={handleSaveServerUrl}
                className="px-5 py-2.5 bg-blue-600 hover:bg-blue-500 text-white rounded-xl text-sm font-medium transition-colors"
              >
                {t.saveReconnect}
              </button>
            </div>
            <div className="mt-3 flex items-center space-x-4 text-xs text-gray-500">
              <span>{t.proxyUrlForIde} <code className="text-gray-400">{serverUrl}/v1</code></span>
            </div>
          </div>
        )}

        {/* ─── Error Banner ────────────────────────────────────── */}
        {error && (
          <div className="mb-6 text-red-400 text-sm bg-red-400/10 p-4 rounded-xl border border-red-400/20 flex items-start space-x-3">
            <XCircle className="w-5 h-5 flex-shrink-0 mt-0.5" />
            <div>
              <p className="font-medium">{t.error}</p>
              <p className="text-red-300/80 mt-1">{error}</p>
            </div>
          </div>
        )}

        {/* ─── Client Update Banner ────────────────────────────── */}
        {updateStatus === 'available' && updateInfo && (
          <div className="mb-6 backdrop-blur-xl bg-blue-500/10 border border-blue-500/30 rounded-2xl p-5 flex flex-col sm:flex-row items-center justify-between space-y-4 sm:space-y-0 sm:space-x-4 animate-in fade-in slide-in-from-top-2">
            <div className="flex items-center space-x-3 text-left">
              <div className="p-3 bg-blue-500/20 rounded-xl border border-blue-500/30 flex-shrink-0">
                <Cpu className="w-6 h-6 text-blue-400 animate-pulse" />
              </div>
              <div>
                <h4 className="text-sm font-semibold text-white">
                  {t.updateAvailable}: {updateInfo.name}
                </h4>
              </div>
            </div>
            <button
              onClick={handleInstallUpdate}
              disabled={updating}
              className="flex-shrink-0 whitespace-nowrap flex items-center space-x-2 px-5 py-3 bg-blue-500 hover:bg-blue-600 disabled:bg-blue-800 text-white rounded-xl text-sm font-medium transition-all shadow-[0_0_20px_rgba(59,130,246,0.3)] disabled:opacity-70 disabled:cursor-not-allowed"
            >
              {updating ? (
                <>
                  <RefreshCw className="w-4 h-4 animate-spin" />
                  <span>{t.updating}</span>
                </>
              ) : (
                <>
                  <span>{t.updateBtn}</span>
                </>
              )}
            </button>
          </div>
        )}

        {updateError && (
          <div className="mb-6 bg-red-500/10 border border-red-500/30 rounded-2xl p-4 flex items-center justify-between text-red-400 text-sm animate-in fade-in slide-in-from-top-2">
            <span>{t.updateError}: {updateError}</span>
            <button onClick={() => setUpdateError('')} className="text-xs font-semibold hover:underline">Dismiss</button>
          </div>
        )}

        {/* ─── Main Grid ───────────────────────────────────────── */}
        <div className="grid grid-cols-1 md:grid-cols-3 gap-5 mb-6">

          {/* Connection Status Card */}
          <div className="backdrop-blur-xl bg-white/5 border border-white/10 rounded-2xl p-4 relative overflow-hidden group">
            <div className="absolute -bottom-10 -right-10 w-32 h-32 bg-emerald-500/10 blur-[50px] group-hover:bg-emerald-500/20 transition-all" />
            <div className="flex items-center space-x-3 mb-2">
              <div className="p-2 bg-emerald-500/20 rounded-lg border border-emerald-500/30">
                <Wifi className="w-5 h-5 text-emerald-400" />
              </div>
              <h3 className="text-base font-medium text-gray-300">{t.status}</h3>
            </div>
            <div className="text-3xl font-bold text-emerald-400 mb-1">{t.active}</div>
            <div className="flex items-center text-xs text-gray-500 space-x-2 truncate">
              {planName ? (
                <>
                  <span className="font-semibold text-gray-300 truncate" title={planName}>
                    {t.plan}: {planName}
                  </span>
                  <span className="w-1 h-1 rounded-full bg-gray-600 shrink-0" />
                </>
              ) : tier !== null && tier > 0 ? (
                <>
                  <span className="font-semibold text-gray-300">
                    {t.plan} X{tier}
                  </span>
                  <span className="w-1 h-1 rounded-full bg-gray-600 shrink-0" />
                </>
              ) : null}
              <span className="truncate">
                {expiresAt ? `${getRemainingDays(expiresAt)} ${t.daysLeft}` : t.tokenAuth}
              </span>
            </div>
          </div>

          {/* Models Count Card */}
          <div className="backdrop-blur-xl bg-white/5 border border-white/10 rounded-2xl p-4 relative overflow-hidden group">
            <div className="absolute -top-10 -left-10 w-32 h-32 bg-blue-500/10 blur-[50px] group-hover:bg-blue-500/20 transition-all" />
            <div className="flex items-center space-x-3 mb-2">
              <div className="p-2 bg-blue-500/20 rounded-lg border border-blue-500/30">
                <Cpu className="w-5 h-5 text-blue-400" />
              </div>
              <h3 className="text-base font-medium text-gray-300">{t.models}</h3>
            </div>
            <div className="text-3xl font-bold text-white mb-1">{models.length}</div>
            <div className="text-xs text-gray-500 truncate">{t.availModels}</div>
          </div>


          {/* Server Card */}
          <div className="backdrop-blur-xl bg-white/5 border border-white/10 rounded-2xl p-4 relative overflow-hidden group">
            <div className="absolute -bottom-10 -left-10 w-32 h-32 bg-purple-500/10 blur-[50px] group-hover:bg-purple-500/20 transition-all" />
            <div className="flex items-center space-x-3 mb-2">
              <div className="p-2 bg-purple-500/20 rounded-lg border border-purple-500/30">
                <Server className="w-5 h-5 text-purple-400" />
              </div>
              <h3 className="text-base font-medium text-gray-300">{t.server}</h3>
            </div>
            <div className={`text-3xl font-bold mb-1 ${
              ping === null ? 'text-red-400' :
              ping < 50 ? 'text-emerald-400' :
              ping < 150 ? 'text-amber-400' : 'text-rose-400'
            }`}>
              {ping !== null ? `${ping} ms` : 'Offline'}
            </div>
            <div className="text-xs text-gray-500 truncate">{t.proxyPing}</div>
          </div>
        </div>

        {/* ─── Connect to IDE (Premium Banner) ──────────────────────────────────── */}
        <div className="relative mb-6 group">
          <div className="absolute inset-0 bg-gradient-to-r from-blue-600/20 via-purple-600/20 to-emerald-600/20 blur-xl rounded-2xl opacity-50 group-hover:opacity-100 transition-opacity duration-500" />
          <div className="relative backdrop-blur-xl bg-black/40 border border-white/10 rounded-2xl p-4 sm:p-5 flex flex-col sm:flex-row items-start sm:items-center justify-between shadow-2xl gap-4">
            
            {/* Left: Icon & Text */}
            <div className="flex items-center space-x-3.5 min-w-0">
              <div className="relative shrink-0">
                <div className="absolute inset-0 bg-blue-500/30 blur-md rounded-full" />
                <div className="w-11 h-11 bg-white/5 border border-white/10 rounded-full flex items-center justify-center relative z-10">
                  <Shield className="w-5 h-5 text-blue-400" />
                </div>
              </div>
              <div className="flex flex-col min-w-0">
                <h2 className="text-base sm:text-lg font-bold text-white tracking-wide truncate">{t.ready}</h2>
                <p className="text-xs text-gray-400 truncate mt-0.5">{t.secureProxy}</p>
                {proxyRunning && (
                  <div className="mt-1.5 flex items-center">
                    <span className="inline-flex items-center space-x-1.5 text-[10px] sm:text-[11px] font-mono text-emerald-400 bg-emerald-400/10 border border-emerald-400/20 px-2 py-0.5 rounded-full">
                      <span className="w-1.5 h-1.5 rounded-full bg-emerald-400 animate-pulse" />
                      <span>{t.proxyActive}</span>
                    </span>
                  </div>
                )}
              </div>
            </div>

            {/* Right: Multi-Tool Controls */}
            <div className="flex items-center gap-2 sm:shrink-0 flex-wrap justify-start sm:justify-end max-w-full">
              {/* Launch Antigravity IDE */}
              <button
                onClick={() => handleLaunchTool('Antigravity IDE')}
                disabled={launchingTool !== null}
                className={`px-3 py-2 rounded-xl text-xs font-semibold whitespace-nowrap transition-all flex items-center space-x-1.5 border shadow-sm ${
                  launchedTools['Antigravity IDE']
                    ? 'bg-emerald-500/20 border-emerald-500/40 text-emerald-300'
                    : 'bg-blue-500/10 hover:bg-blue-500/25 border-blue-500/30 hover:border-blue-500/50 text-blue-300 hover:text-white'
                } disabled:opacity-50 disabled:cursor-not-allowed`}
                title="Antigravity IDE"
              >
                {launchingTool === 'Antigravity IDE' ? (
                  <RefreshCw className="w-3.5 h-3.5 animate-spin" />
                ) : launchedTools['Antigravity IDE'] ? (
                  <CheckCircle className="w-3.5 h-3.5 text-emerald-400" />
                ) : (
                  <Cpu className="w-3.5 h-3.5 text-blue-400" />
                )}
                <span>IDE</span>
              </button>

              {/* Launch Antigravity 2.0 */}
              <button
                onClick={() => handleLaunchTool('Antigravity 2.0')}
                disabled={launchingTool !== null}
                className={`px-3 py-2 rounded-xl text-xs font-semibold whitespace-nowrap transition-all flex items-center space-x-1.5 border shadow-sm ${
                  launchedTools['Antigravity 2.0']
                    ? 'bg-emerald-500/20 border-emerald-500/40 text-emerald-300'
                    : 'bg-purple-500/10 hover:bg-purple-500/25 border-purple-500/30 hover:border-purple-500/50 text-purple-300 hover:text-white'
                } disabled:opacity-50 disabled:cursor-not-allowed`}
                title="Antigravity 2.0"
              >
                {launchingTool === 'Antigravity 2.0' ? (
                  <RefreshCw className="w-3.5 h-3.5 animate-spin" />
                ) : launchedTools['Antigravity 2.0'] ? (
                  <CheckCircle className="w-3.5 h-3.5 text-emerald-400" />
                ) : (
                  <Zap className="w-3.5 h-3.5 text-purple-400" />
                )}
                <span>v2.0</span>
              </button>

              {/* Launch Antigravity CLI */}
              <button
                onClick={() => handleLaunchTool('Antigravity CLI')}
                disabled={launchingTool !== null}
                className={`px-3 py-2 rounded-xl text-xs font-semibold whitespace-nowrap transition-all flex items-center space-x-1.5 border shadow-sm ${
                  launchedTools['Antigravity CLI']
                    ? 'bg-emerald-500/20 border-emerald-500/40 text-emerald-300'
                    : 'bg-emerald-500/10 hover:bg-emerald-500/25 border-emerald-500/30 hover:border-emerald-500/50 text-emerald-300 hover:text-white'
                } disabled:opacity-50 disabled:cursor-not-allowed`}
                title="Antigravity CLI (agy)"
              >
                {launchingTool === 'Antigravity CLI' ? (
                  <RefreshCw className="w-3.5 h-3.5 animate-spin" />
                ) : launchedTools['Antigravity CLI'] ? (
                  <CheckCircle className="w-3.5 h-3.5 text-emerald-400" />
                ) : (
                  <Terminal className="w-3.5 h-3.5 text-emerald-400" />
                )}
                <span>CLI</span>
              </button>

              {/* API Connection Help */}
              <button
                onClick={() => {
                  setApiCopyError(false);
                  setApiCopiedSection(null);
                  setShowApiHelp(true);
                }}
                className="px-3 py-2 rounded-xl text-xs font-semibold whitespace-nowrap transition-all flex items-center space-x-1.5 border shadow-sm bg-sky-500/10 hover:bg-sky-500/25 border-sky-500/30 hover:border-sky-500/50 text-sky-300 hover:text-white"
                title={t.apiHelpTitle}
              >
                <Code className="w-3.5 h-3.5 text-sky-400" />
                <span>{t.apiHelpBtn}</span>
              </button>

              {/* Stop Proxy Button */}
              <button
                onClick={handleStopProxy}
                disabled={!proxyRunning || stoppingProxy}
                className={`px-3 py-2 rounded-xl text-xs font-semibold whitespace-nowrap transition-all flex items-center space-x-1.5 border shadow-sm ${
                  proxyRunning
                    ? 'bg-rose-500/15 hover:bg-rose-500/25 border-rose-500/40 hover:border-rose-500/60 text-rose-300 hover:text-white shadow-[0_0_12px_rgba(244,63,94,0.2)] active:scale-95 cursor-pointer'
                    : 'bg-white/5 border-white/5 text-gray-500 opacity-40 cursor-not-allowed'
                }`}
                title={t.stop}
              >
                {stoppingProxy ? (
                  <RefreshCw className="w-3.5 h-3.5 animate-spin text-rose-400" />
                ) : (
                  <Square className={`w-3.5 h-3.5 fill-current ${proxyRunning ? 'text-rose-400' : 'text-gray-500'}`} />
                )}
                <span>{t.stop}</span>
              </button>
            </div>
          </div>
        </div>

        {/* ─── Models List ─────────────────────────────────────── */}
        {models.length > 0 && (
          <div className="backdrop-blur-xl bg-white/5 border border-white/10 rounded-2xl p-5 mb-6">
            <div className="flex items-center justify-between mb-4">
              <div className="flex items-center space-x-3">
                <div className="p-2 bg-blue-500/20 rounded-lg border border-blue-500/30">
                  <Zap className="w-5 h-5 text-blue-400" />
                </div>
                <h3 className="text-lg font-bold text-gray-200">{t.availableModelsTitle}</h3>
              </div>
              <div className="flex items-center space-x-3">
                <span className="text-xs text-gray-500">{models.length} {t.modelsCount}</span>
                <button
                  onClick={handleRefresh}
                  className="p-1.5 bg-white/5 hover:bg-white/10 rounded-lg transition-colors border border-white/5 flex items-center space-x-1"
                  title="Update Limits"
                >
                  <RefreshCw className={`w-3.5 h-3.5 text-gray-400 ${loading ? 'animate-spin' : ''}`} />
                </button>
              </div>
            </div>

            {quotaGroups.length > 0 && (
              <div className="grid grid-cols-1 sm:grid-cols-2 gap-2.5 mb-4">
                {quotaGroups.map(group => {
                  const isClaudeGroup =
                    group.display_name.toLowerCase().includes('claude') ||
                    group.display_name.toLowerCase().includes('gpt');
                  const accent = isClaudeGroup
                    ? 'from-orange-500 to-amber-400'
                    : 'from-blue-500 to-cyan-400';
                  return (
                    <div
                      key={group.display_name}
                      className="px-4 py-3 bg-black/25 border border-white/10 rounded-xl space-y-2.5"
                    >
                      <div className="flex items-center space-x-2">
                        <div className={`w-2 h-2 rounded-full bg-gradient-to-r ${accent}`} />
                        <span className="text-xs font-semibold text-gray-200 truncate">
                          {group.display_name}
                        </span>
                      </div>
                      <div className="space-y-2">
                        {group.buckets.map(b => {
                          const label =
                            b.window === '5h' || b.bucket_id.endsWith('-5h')
                              ? t.quota5h
                              : b.window === 'weekly' || b.bucket_id.endsWith('-weekly')
                                ? t.quotaWeekly
                                : b.window || b.bucket_id;
                          const bColor =
                            b.percent === 0
                              ? 'text-rose-400 font-semibold'
                              : b.percent <= 20
                                ? 'text-amber-400 font-semibold'
                                : 'text-gray-300';
                          const bBar =
                            b.percent === 0
                              ? 'from-rose-500 to-red-500'
                              : b.percent <= 20
                                ? 'from-amber-500 to-orange-400'
                                : accent;
                          return (
                            <div key={b.bucket_id || b.window} className="space-y-1">
                              <div className="flex items-center justify-between text-[11px] gap-2">
                                <span className="text-gray-400">{label}</span>
                                <div className="flex items-center space-x-1.5">
                                  {b.reset_time && <CountdownTimer resetTime={b.reset_time} />}
                                  <span className={`font-mono ${bColor}`}>{b.percent}%</span>
                                </div>
                              </div>
                              <div className="w-full bg-white/10 rounded-full h-1 overflow-hidden">
                                <div
                                  className={`h-1 rounded-full bg-gradient-to-r ${bBar}`}
                                  style={{ width: `${Math.min(100, Math.max(0, b.percent))}%` }}
                                />
                              </div>
                            </div>
                          );
                        })}
                      </div>
                    </div>
                  );
                })}
              </div>
            )}

            <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
              {models.map(model => {
                const catInfo = getModelCategory(model.id);
                const displayTitle = model.display_name || formatModelName(model.id);
                const { pct, resetTime } = getModelQuota(model, modelPercentages, modelResets);
                const pctColor =
                  pct === 0
                    ? 'text-rose-400 font-semibold'
                    : pct !== undefined && pct <= 20
                      ? 'text-amber-400 font-semibold'
                      : 'text-gray-400';
                const barGradient =
                  pct === 0
                    ? 'from-rose-500 to-red-500'
                    : pct !== undefined && pct <= 20
                      ? 'from-amber-500 to-orange-400'
                      : catInfo.color;
                return (
                  <button
                    type="button"
                    key={model.id}
                    onClick={() => {
                      setDetailsCopied(false);
                      setDetailsModel(model);
                    }}
                    aria-label={`${displayTitle} — ${t.modelDetailsHint}`}
                    className={`flex flex-col space-y-2 px-4 py-3 text-left bg-white/[0.03] hover:bg-white/[0.06] border rounded-xl transition-colors group/model focus:outline-none focus-visible:ring-2 focus-visible:ring-blue-500/60 ${
                      pct === 0 ? 'border-rose-500/25' : 'border-white/5'
                    }`}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div className="flex items-center space-x-3 min-w-0">
                        <div className={`w-2 h-2 rounded-full bg-gradient-to-r ${catInfo.color} flex-shrink-0`} />
                        <span className="text-sm text-gray-300 truncate group-hover/model:text-white transition-colors" title={model.id}>
                          {displayTitle}
                        </span>
                      </div>
                      <div className="flex items-center space-x-1.5 shrink-0">
                        {pct !== undefined && resetTime && resetTime !== "Безлимитно" && (
                          <CountdownTimer resetTime={resetTime} />
                        )}
                        {pct !== undefined && (
                          <span className={`text-[10px] font-mono ${pctColor}`}>{pct}%</span>
                        )}
                        <Info className="w-3.5 h-3.5 text-gray-600 group-hover/model:text-gray-400 transition-colors shrink-0" aria-hidden="true" />
                      </div>
                    </div>
                    {pct !== undefined && (
                      <div className="w-full bg-white/10 rounded-full h-1 mt-1 overflow-hidden">
                        <div className={`h-1 rounded-full bg-gradient-to-r ${barGradient}`} style={{ width: `${Math.min(100, Math.max(0, pct))}%` }}></div>
                      </div>
                    )}
                  </button>
                );
              })}
            </div>
          </div>
        )}


      </div>

      {/* ─── Settings Modal ──────────────────────────────────── */}
      {showSettings && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/60 backdrop-blur-sm">
          <div className="bg-[#1a1c23] border border-white/10 rounded-2xl w-full max-w-lg shadow-2xl flex flex-col">
            <div className="flex items-center justify-between p-6 border-b border-white/10">
              <h2 className="text-xl font-bold text-white flex items-center space-x-2">
                <Settings className="w-5 h-5 text-gray-400" />
                <span>{t.ideSettings}</span>
              </h2>
              <button 
                onClick={() => setShowSettings(false)}
                className="text-gray-400 hover:text-white transition-colors"
              >
                <XCircle className="w-6 h-6" />
              </button>
            </div>
            
            <div className="p-6 space-y-6 overflow-y-auto max-h-[60vh]">
              {/* --- Antigravity IDE Settings --- */}
              <div className="space-y-4 border-b border-white/5 pb-4">
                <h3 className="text-sm font-bold text-blue-400 flex items-center space-x-2">
                  <span>Antigravity IDE (Legacy)</span>
                </h3>
                
                <div className="space-y-2">
                  <label className="text-xs font-medium text-gray-300 block">
                    Custom Executable Path (Optional)
                  </label>
                  <div className="flex gap-2">
                    <input
                      type="text"
                      placeholder="e.g. C:\Apps\Antigravity IDE\Antigravity IDE.exe"
                      value={customExePathIde}
                      onChange={(e) => {
                        setCustomExePathIde(e.target.value);
                        localStorage.setItem(LS_CUSTOM_EXE_IDE_KEY, e.target.value);
                      }}
                      className="flex-1 bg-black/40 border border-white/10 rounded-xl px-4 py-3 text-white placeholder-gray-600 focus:outline-none focus:ring-2 focus:ring-blue-500/50 font-mono text-sm"
                    />
                    <button
                      type="button"
                      onClick={async () => {
                        try {
                          const selected = await invoke<string | null>('browse_executable');
                          if (selected) {
                            setCustomExePathIde(selected);
                            localStorage.setItem(LS_CUSTOM_EXE_IDE_KEY, selected);
                          }
                        } catch (e) {
                          console.error("Failed to browse: ", e);
                        }
                      }}
                      className="px-4 py-3 bg-white/5 hover:bg-white/10 text-gray-300 rounded-xl text-sm font-medium border border-white/5 transition-colors whitespace-nowrap"
                    >
                      {lang === 'ru' ? 'Обзор...' : 'Browse...'}
                    </button>
                  </div>
                  <p className="text-[11px] text-gray-500">{t.leaveBlank}</p>
                </div>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-gray-300 block">
                    Custom DB / AppData Path (Optional)
                  </label>
                  <input
                    type="text"
                    placeholder="e.g. C:\Data\User\globalStorage\state.vscdb"
                    value={customDbPathIde}
                    onChange={(e) => {
                      setCustomDbPathIde(e.target.value);
                      localStorage.setItem(LS_CUSTOM_DB_IDE_KEY, e.target.value);
                    }}
                    className="w-full bg-black/40 border border-white/10 rounded-xl px-4 py-3 text-white placeholder-gray-600 focus:outline-none focus:ring-2 focus:ring-blue-500/50 font-mono text-sm"
                  />
                  <p className="text-[11px] text-gray-500">{t.pathToVscdb}</p>
                </div>
              </div>

              {/* --- Antigravity 2.0 Settings --- */}
              <div className="space-y-4 pb-2">
                <h3 className="text-sm font-bold text-purple-400 flex items-center space-x-2">
                  <span>Antigravity 2.0</span>
                </h3>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-gray-300 block">
                    Custom Executable Path (Optional)
                  </label>
                  <div className="flex gap-2">
                    <input
                      type="text"
                      placeholder="e.g. C:\Apps\Antigravity\Antigravity.exe"
                      value={customExePathV2}
                      onChange={(e) => {
                        setCustomExePathV2(e.target.value);
                        localStorage.setItem(LS_CUSTOM_EXE_V2_KEY, e.target.value);
                      }}
                      className="flex-1 bg-black/40 border border-white/10 rounded-xl px-4 py-3 text-white placeholder-gray-600 focus:outline-none focus:ring-2 focus:ring-purple-500/50 font-mono text-sm"
                    />
                    <button
                      type="button"
                      onClick={async () => {
                        try {
                          const selected = await invoke<string | null>('browse_executable');
                          if (selected) {
                            setCustomExePathV2(selected);
                            localStorage.setItem(LS_CUSTOM_EXE_V2_KEY, selected);
                          }
                        } catch (e) {
                          console.error("Failed to browse: ", e);
                        }
                      }}
                      className="px-4 py-3 bg-white/5 hover:bg-white/10 text-gray-300 rounded-xl text-sm font-medium border border-white/5 transition-colors whitespace-nowrap"
                    >
                      {lang === 'ru' ? 'Обзор...' : 'Browse...'}
                    </button>
                  </div>
                  <p className="text-[11px] text-gray-500">{t.leaveBlank}</p>
                </div>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-gray-300 block">
                    Custom DB / AppData Path (Optional)
                  </label>
                  <input
                    type="text"
                    placeholder="e.g. C:\Data\User\globalStorage\state.vscdb"
                    value={customDbPathV2}
                    onChange={(e) => {
                      setCustomDbPathV2(e.target.value);
                      localStorage.setItem(LS_CUSTOM_DB_V2_KEY, e.target.value);
                    }}
                    className="w-full bg-black/40 border border-white/10 rounded-xl px-4 py-3 text-white placeholder-gray-600 focus:outline-none focus:ring-2 focus:ring-purple-500/50 font-mono text-sm"
                  />
                  <p className="text-[11px] text-gray-500">{t.pathToVscdb}</p>
                </div>
              </div>

              {/* --- Antigravity CLI Settings --- */}
              <div className="space-y-4 pb-2 border-t border-white/5 pt-4">
                <h3 className="text-sm font-bold text-emerald-400 flex items-center space-x-2">
                  <span>Antigravity CLI (agy)</span>
                </h3>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-gray-300 block">
                    Custom Executable Path (Optional)
                  </label>
                  <div className="flex gap-2">
                    <input
                      type="text"
                      placeholder={lang === 'ru' ? 'например, C:\\Users\\user\\AppData\\Local\\agy\\bin\\agy.exe' : 'e.g. C:\\Users\\user\\AppData\\Local\\agy\\bin\\agy.exe'}
                      value={customExePathCli}
                      onChange={(e) => {
                        setCustomExePathCli(e.target.value);
                        localStorage.setItem(LS_CUSTOM_EXE_CLI_KEY, e.target.value);
                      }}
                      className="flex-1 bg-black/40 border border-white/10 rounded-xl px-4 py-3 text-white placeholder-gray-600 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 font-mono text-sm"
                    />
                    <button
                      type="button"
                      onClick={async () => {
                        try {
                          const selected = await invoke<string | null>('browse_executable');
                          if (selected) {
                            setCustomExePathCli(selected);
                            localStorage.setItem(LS_CUSTOM_EXE_CLI_KEY, selected);
                          }
                        } catch (e) {
                          console.error("Failed to browse CLI: ", e);
                        }
                      }}
                      className="px-4 py-3 bg-white/5 hover:bg-white/10 text-gray-300 rounded-xl text-sm font-medium border border-white/5 transition-colors whitespace-nowrap"
                    >
                      {lang === 'ru' ? 'Обзор...' : 'Browse...'}
                    </button>
                  </div>
                  <p className="text-[11px] text-gray-500">{t.leaveBlank}</p>
                </div>
              </div>

              <div className="border-t border-white/5 pt-6 space-y-4">
                <div className="flex items-center justify-between">
                  <div>
                    <h3 className="text-sm font-semibold text-white">{t.checkForUpdates}</h3>
                    <p className="text-xs text-gray-500 mt-0.5">Current Version: {CURRENT_VERSION}</p>
                  </div>
                  <button
                    onClick={() => checkUpdates(false)}
                    disabled={checkingUpdates || updating}
                    className="px-4 py-2 bg-white/5 hover:bg-white/10 text-gray-300 rounded-xl text-sm font-semibold border border-white/5 transition-colors flex items-center space-x-1.5 disabled:opacity-50"
                  >
                    {checkingUpdates && <RefreshCw className="w-3.5 h-3.5 animate-spin" />}
                    <span>{t.checkForUpdates}</span>
                  </button>
                </div>

                {updateStatus === 'up-to-date' && (
                  <p className="text-xs text-emerald-400">{t.upToDate}</p>
                )}

                {updateStatus === 'available' && updateInfo && (
                  <div className="p-3 bg-blue-500/10 border border-blue-500/20 rounded-xl space-y-2.5">
                    <div className="flex items-center justify-between">
                      <span className="text-xs font-semibold text-blue-400">{t.updateAvailable}: {updateInfo.name}</span>
                      <button
                        onClick={handleInstallUpdate}
                        disabled={updating}
                        className="flex-shrink-0 whitespace-nowrap px-3 py-1.5 bg-blue-500 hover:bg-blue-600 text-white rounded-lg text-xs font-semibold transition-colors flex items-center space-x-1"
                      >
                        {updating ? (
                          <>
                            <RefreshCw className="w-3 animate-spin" />
                            <span>{t.updating}</span>
                          </>
                        ) : (
                          <span>{t.updateBtn}</span>
                        )}
                      </button>
                    </div>
                  </div>
                )}

                {updateStatus === 'error' && updateError && (
                  <p className="text-xs text-red-400">{t.updateError}: {updateError}</p>
                )}
              </div>
            </div>
            
            <div className="p-6 border-t border-white/10 flex justify-end">
              <button
                onClick={() => setShowSettings(false)}
                className="px-6 py-2.5 bg-white text-black font-semibold rounded-xl hover:bg-gray-200 transition-colors"
              >
                Done
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ─── API Help Modal ──────────────────────────────────── */}
      {showApiHelp && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/60 backdrop-blur-sm"
          onClick={() => setShowApiHelp(false)}
        >
          <div
            ref={apiDialogRef}
            role="dialog"
            aria-modal="true"
            aria-labelledby="api-help-title"
            className="bg-[#1a1c23] border border-white/10 rounded-2xl w-full max-w-2xl shadow-2xl flex flex-col max-h-[85vh]"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-center justify-between p-4 sm:p-6 border-b border-white/10 shrink-0">
              <h2 id="api-help-title" className="text-lg sm:text-xl font-bold text-white flex items-center space-x-2 min-w-0">
                <Code className="w-5 h-5 text-sky-400 shrink-0" />
                <span className="truncate">{t.apiHelpTitle}</span>
              </h2>
              <button
                onClick={() => setShowApiHelp(false)}
                aria-label={t.apiClose}
                className="text-gray-400 hover:text-white transition-colors shrink-0 ml-2"
              >
                <XCircle className="w-6 h-6" />
              </button>
            </div>

            <div className="p-4 sm:p-6 space-y-5 overflow-y-auto">
              {/* Prerequisite */}
              <div className="bg-amber-500/10 border border-amber-500/30 rounded-xl p-4 flex items-start space-x-3">
                <AlertTriangle className="w-4 h-4 text-amber-400 shrink-0 mt-0.5" />
                <div>
                  <h3 className="text-sm font-bold text-amber-300 mb-1">{t.apiPrereqTitle}</h3>
                  <p className="text-xs text-gray-300 leading-relaxed">{t.apiPrereqText}</p>
                </div>
              </div>

              {/* Base URL */}
              <div className="space-y-2">
                <label className="text-xs font-medium text-gray-300 block">{t.apiBaseLabel}</label>
                <code className="block text-xs font-mono text-gray-400 bg-black/40 border border-white/10 rounded-lg px-3 py-2 break-all">
                  {apiBaseUrl}
                </code>
              </div>

              {/* Key + models */}
              <ul className="space-y-2">
                <li className="text-xs text-gray-300 leading-relaxed flex items-start space-x-2">
                  <Key className="w-3.5 h-3.5 text-sky-400 shrink-0 mt-0.5" />
                  <span>{t.apiKeyLine}</span>
                </li>
                <li className="text-xs text-gray-300 leading-relaxed flex items-start space-x-2">
                  <Server className="w-3.5 h-3.5 text-sky-400 shrink-0 mt-0.5" />
                  <span>{t.apiModelsLine}</span>
                </li>
              </ul>

              {/* Streaming / tools note */}
              <div className="bg-white/5 border border-white/10 rounded-xl p-4">
                <h3 className="text-sm font-bold text-blue-400 mb-1">{t.apiNoteTitle}</h3>
                <p className="text-xs text-gray-300 leading-relaxed">{t.apiNoteText}</p>
              </div>

              {/* Security warning */}
              <div className="bg-rose-500/10 border border-rose-500/30 rounded-xl p-4 flex items-start space-x-3">
                <Shield className="w-4 h-4 text-rose-400 shrink-0 mt-0.5" />
                <div>
                  <h3 className="text-sm font-bold text-rose-300 mb-1">{t.apiSecurityTitle}</h3>
                  <p className="text-xs text-gray-300 leading-relaxed">{t.apiSecurityText}</p>
                </div>
              </div>

              {/* VS Code Copilot BYOK */}
              <div className="bg-white/5 border border-white/10 rounded-xl p-4 space-y-2">
                <h3 className="text-sm font-bold text-purple-400">{t.apiCopilotTitle}</h3>
                <ol className="list-decimal list-inside space-y-1.5">
                  {t.apiVscodeSteps.map((step: string, i: number) => (
                    <li key={i} className="text-xs text-gray-300 leading-relaxed break-words">
                      {step.includes('{endpoint}') ? (
                        <>
                          {step.split('{endpoint}')[0]}
                          <code className="font-mono text-gray-400 bg-black/40 border border-white/10 rounded px-1 py-0.5 break-all">{apiChatEndpoint}</code>
                          {step.split('{endpoint}')[1]}
                        </>
                      ) : step}
                    </li>
                  ))}
                </ol>
                <p className="text-[11px] text-gray-400 italic">{t.apiCopilotUnsupported}</p>
              </div>

              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <h3 className="text-sm font-bold text-gray-300">{t.apiConfigTitle}</h3>
                  <button
                    onClick={() => copyApiText('config')}
                    aria-label={`${t.apiCopy}: ${t.apiConfigTitle}`}
                    className="text-[11px] px-2.5 py-1 rounded-lg bg-white/5 hover:bg-white/10 border border-white/10 text-gray-400 hover:text-white transition-colors"
                  >
                    {apiCopiedSection === 'config' ? t.apiCopied : t.apiCopy}
                  </button>
                </div>
                <pre className="text-[11px] font-mono text-gray-400 bg-black/40 border border-white/10 rounded-lg px-3 py-2 overflow-x-auto whitespace-pre">{apiConfigExample}</pre>
              </div>

              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <h3 className="text-sm font-bold text-gray-300">{t.apiExampleTitle}</h3>
                  <button
                    onClick={() => copyApiText('request')}
                    aria-label={`${t.apiCopy}: ${t.apiExampleTitle}`}
                    className="text-[11px] px-2.5 py-1 rounded-lg bg-white/5 hover:bg-white/10 border border-white/10 text-gray-400 hover:text-white transition-colors"
                  >
                    {apiCopiedSection === 'request' ? t.apiCopied : t.apiCopy}
                  </button>
                </div>
                <p className="text-xs text-gray-400">{t.apiExampleHint}</p>
                <pre className="text-[11px] font-mono text-gray-400 bg-black/40 border border-white/10 rounded-lg px-3 py-2 overflow-x-auto whitespace-pre">{apiExampleRequest}</pre>
              </div>
              {apiCopyError && <p role="alert" className="text-xs text-rose-300">{t.apiCopyError}</p>}
            </div>

            <div className="p-4 sm:p-6 border-t border-white/10 flex justify-end shrink-0">
              <button
                onClick={() => setShowApiHelp(false)}
                className="px-6 py-2.5 bg-white text-black font-semibold rounded-xl hover:bg-gray-200 transition-colors"
              >
                {t.apiClose}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ─── Model Details Modal ───────────────────────── */}
      {detailsModel && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/60 backdrop-blur-sm"
          onClick={() => setDetailsModel(null)}
        >
          <div
            ref={detailsDialogRef}
            role="dialog"
            aria-modal="true"
            aria-labelledby="model-details-title"
            className="bg-[#1a1c23] border border-white/10 rounded-2xl w-full max-w-lg shadow-2xl flex flex-col max-h-[85vh]"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-center justify-between p-4 sm:p-6 border-b border-white/10 shrink-0">
              <h2 id="model-details-title" className="text-lg sm:text-xl font-bold text-white flex items-center space-x-2 min-w-0">
                <Cpu className="w-5 h-5 text-blue-400 shrink-0" />
                <span className="truncate">{detailsModel.display_name || formatModelName(detailsModel.id)}</span>
              </h2>
              <button
                onClick={() => setDetailsModel(null)}
                aria-label={t.apiClose}
                className="text-gray-400 hover:text-white transition-colors shrink-0 ml-2"
              >
                <XCircle className="w-6 h-6" />
              </button>
            </div>

            <div className="p-4 sm:p-6 space-y-4 overflow-y-auto">
              <div className="bg-amber-500/10 border border-amber-500/30 rounded-xl p-4 flex items-start space-x-3">
                <AlertTriangle className="w-4 h-4 text-amber-400 shrink-0 mt-0.5" />
                <p className="text-xs text-gray-300 leading-relaxed">{t.modelDetailsNote}</p>
              </div>

              <dl className="space-y-2">
                {(() => {
                  const dq = getModelQuota(detailsModel, modelPercentages, modelResets);
                  const singleEffort = splitEffortSuffix(detailsModel.id)?.[1];
                  const reasoningModes =
                    detailsModel.supports_reasoning_effort && detailsModel.supports_reasoning_effort.length > 0
                      ? detailsModel.supports_reasoning_effort
                      : singleEffort
                        ? [singleEffort]
                        : detailsModel.id.endsWith('-thinking')
                          ? ['thinking']
                          : [];
                  const rows = [
                    { label: t.mdModelId, value: <code className="text-xs font-mono text-gray-400 bg-black/40 border border-white/10 rounded px-1.5 py-0.5 break-all">{detailsModel.id}</code> },
                    { label: t.mdDisplayName, value: <span className="text-xs text-gray-200">{detailsModel.display_name || formatModelName(detailsModel.id)}</span> },
                    { label: t.mdOwnedBy, value: detailsModel.owned_by ? <span className="text-xs text-gray-200">{detailsModel.owned_by}</span> : <span className="text-xs text-gray-500 italic">{t.mdNotProvided}</span> },
                    {
                      label: t.mdQuotaRemaining,
                      value: dq.pct !== undefined ? (
                        <div className="flex items-center justify-end gap-1.5">
                          {dq.resetTime && dq.resetTime !== "Безлимитно" && <CountdownTimer resetTime={dq.resetTime} />}
                          <span className={`text-xs font-mono ${dq.pct === 0 ? 'text-rose-400 font-semibold' : dq.pct <= 20 ? 'text-amber-400 font-semibold' : 'text-emerald-300'}`}>{dq.pct}%</span>
                        </div>
                      ) : <span className="text-xs text-gray-500 italic">{t.mdNotProvided}</span>,
                    },
                    {
                      label: t.mdReasoningEffort,
                      value: reasoningModes.length > 0 ? (
                        <div className="flex items-center justify-end flex-wrap gap-1">
                          {reasoningModes.map(level => (
                            <span key={level} className="text-[11px] font-mono px-1.5 py-0.5 rounded bg-blue-500/15 border border-blue-500/30 text-blue-300">{level}</span>
                          ))}
                        </div>
                      ) : (
                        <span className="text-xs text-rose-300">{t.mdNo}</span>
                      ),
                    },
                    ...(detailsModel.reasoning_variants && Object.keys(detailsModel.reasoning_variants).length > 0 ? [{
                      label: t.mdVariants,
                      value: (
                        <div className="flex flex-col items-end gap-0.5">
                          {Object.entries(detailsModel.reasoning_variants).map(([effort, vid]) => (
                            <span key={effort} className="text-[11px] font-mono text-gray-400">
                              <span className="text-gray-300">{effort}</span> → {vid}
                            </span>
                          ))}
                        </div>
                      ),
                    }] : []),
                    { label: t.mdContextWindow, value: formatTokenCount(detailsModel.context_window, t) },
                    { label: t.mdMaxOutput, value: formatTokenCount(detailsModel.max_output_tokens, t) },
                    { label: t.mdImageInput, value: formatSupportLabel(detailsModel.supports_images, t) },
                    { label: t.mdToolSupport, value: formatSupportLabel(detailsModel.supports_tools, t) },
                  ];
                  return rows.map(row => (
                    <div key={row.label} className="flex items-start justify-between gap-3 px-3 py-2 bg-white/[0.03] border border-white/5 rounded-xl">
                      <dt className="text-xs text-gray-400 shrink-0">{row.label}</dt>
                      <dd className="text-right min-w-0">{row.value}</dd>
                    </div>
                  ));
                })()}
              </dl>
            </div>

            <div className="p-4 sm:p-6 border-t border-white/10 flex items-center justify-between gap-3 shrink-0">
              <button
                type="button"
                onClick={async () => {
                  try {
                    await navigator.clipboard.writeText(JSON.stringify(buildCopilotModelEntry(detailsModel), null, 2));
                    setDetailsCopied(true);
                    window.setTimeout(() => setDetailsCopied(false), 2000);
                  } catch {
                    setDetailsCopied(false);
                  }
                }}
                className="px-4 py-2.5 bg-white/5 hover:bg-white/10 border border-white/10 text-gray-200 text-xs font-semibold rounded-xl transition-colors"
              >
                {detailsCopied ? t.apiCopied : t.mdCopyCopilot}
              </button>
              <button
                onClick={() => setDetailsModel(null)}
                className="px-6 py-2.5 bg-white text-black font-semibold rounded-xl hover:bg-gray-200 transition-colors"
              >
                {t.apiClose}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
