// format.js — timestamps, durações e paleta de estado. Porta de
// crates/rustploy-gui/views/scripts/format/time.luau e format/util.luau (só as
// partes usadas pelo dashboard nesta fase).

/** epoch (ms) de um RFC3339 ("...Z" ou offset). */
function toEpochMs(isoTimestamp) {
  if (typeof isoTimestamp !== "string") return null;
  const epochMs = Date.parse(isoTimestamp);
  return Number.isNaN(epochMs) ? null : epochMs;
}

/** "HH:MM:SS" local. */
export function timeHms(isoTimestamp) {
  const epochMs = toEpochMs(isoTimestamp);
  if (epochMs === null) return "";
  const date = new Date(epochMs);
  const padTwoDigits = (value) => String(value).padStart(2, "0");
  return `${padTwoDigits(date.getHours())}:${padTwoDigits(date.getMinutes())}:${padTwoDigits(date.getSeconds())}`;
}

/** "dd/mm HH:MM:SS" local. */
export function dateDayMonthHourMinuteSecond(isoTimestamp) {
  const epochMs = toEpochMs(isoTimestamp);
  if (epochMs === null) return "";
  const date = new Date(epochMs);
  const padTwoDigits = (value) => String(value).padStart(2, "0");
  return `${padTwoDigits(date.getDate())}/${padTwoDigits(date.getMonth() + 1)} ${padTwoDigits(date.getHours())}:${padTwoDigits(date.getMinutes())}:${padTwoDigits(date.getSeconds())}`;
}

/** "dd/mm HH:MM" local (sem segundos — usado em listas Docker/Registry). */
export function dateDayMonthHourMinute(isoTimestamp) {
  const epochMs = toEpochMs(isoTimestamp);
  if (epochMs === null) return "";
  const date = new Date(epochMs);
  const padTwoDigits = (value) => String(value).padStart(2, "0");
  return `${padTwoDigits(date.getDate())}/${padTwoDigits(date.getMonth() + 1)} ${padTwoDigits(date.getHours())}:${padTwoDigits(date.getMinutes())}`;
}

/** `term` já em minúsculas; casa se algum campo (string) contém `term`
 * (substring, case-insensitive). Porta de format/util.luau::matches. */
function matchesTerm(term, fields) {
  if (!term) return true;
  return fields.some((field) => typeof field === "string" && field.toLowerCase().includes(term));
}

/* ── Ponte entre o <input type="time"> e o protocolo ────────────────────────
 * Porta de format/time.luau::hm_join/hm_split. O input nativo guarda o horário
 * numa string única "HH:MM"; o daemon sempre falou em `{hour, minute}`
 * separados (Recurrence::Daily/Weekly, DockerCleanupConfig). A tradução mora
 * aqui para que o contrato HTTP não mude por causa de uma troca de widget. */

/** (hour, minute) -> "HH:MM", saturando na faixa válida. */
export function hourMinuteJoin(hour, minute) {
  let hourValue = Math.floor(Number(hour) || 0);
  let minuteValue = Math.floor(Number(minute) || 0);
  if (hourValue < 0) hourValue = 0; else if (hourValue > 23) hourValue = 23;
  if (minuteValue < 0) minuteValue = 0; else if (minuteValue > 59) minuteValue = 59;
  return `${String(hourValue).padStart(2, "0")}:${String(minuteValue).padStart(2, "0")}`;
}

/** "HH:MM" -> [hour, minute]. Vazio ou malformado vira 0h00, o mesmo default
 * que o `Number(...) || 0` dos handlers já usava. */
export function hourMinuteSplit(hourMinuteText) {
  const match = /^(\d{1,2}):(\d{1,2})$/.exec(String(hourMinuteText ?? ""));
  if (!match) return [0, 0];
  let hour = Math.floor(Number(match[1]));
  let minute = Math.floor(Number(match[2]));
  if (hour < 0 || hour > 23) hour = 0;
  if (minute < 0 || minute > 59) minute = 0;
  return [hour, minute];
}

/** Ns ou Mm Ns. */
export function formatSeconds(secs) {
  const totalSeconds = Math.floor(Number(secs) || 0);
  const minutes = Math.floor(totalSeconds / 60);
  const remainingSeconds = totalSeconds % 60;
  return minutes > 0 ? `${minutes}m ${remainingSeconds}s` : `${remainingSeconds}s`;
}

/** dd hh mm / hh mm / mm ss, o maior campo não-zero primeiro. */
export function formatUptime(secs) {
  const totalSeconds = Math.floor(Number(secs) || 0);
  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const remainingSeconds = totalSeconds % 60;
  if (days > 0) return `${days}d ${hours}h ${minutes}m`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m ${remainingSeconds}s`;
}

/** Duração de um deployment (finished_at ou agora) − started_at. */
export function formatDuration(deployment) {
  const start = toEpochMs(deployment.started_at);
  if (start === null) return "0s";
  const finish = deployment.finished_at ? toEpochMs(deployment.finished_at) : Date.now();
  const secs = Math.max(0, Math.floor(((finish ?? Date.now()) - start) / 1000));
  return formatSeconds(secs);
}

/** DeployState → (rótulo, kind semântico p/ .state_<kind>). */
export function stateLabelKind(state) {
  if (state === "Live") return ["LIVE", "ok"];
  if (state === "Stopped") return ["STOPPED", "muted"];
  if (state === "Failed") return ["FAILED", "bad"];
  if (state === "PreDeployCheck") return ["PRÉ-DEPLOY CHECK", "info"];
  return ["BUILDING", "info"];
}

/** ServiceStatus (string ou `{Error: "..."}`) → (rótulo, kind). Porta de
 * format/util.luau::status_label_color/status_kind. */
export function serviceStatusLabelKind(status) {
  if (typeof status === "object" && status?.Error !== undefined) return ["Error", "bad"];
  switch (status) {
    case "Running":
      return ["Running", "ok"];
    case "Deploying":
      return ["Deploying", "info"];
    case "Queued":
      return ["Na fila", "muted"];
    case "Degraded":
      return ["Degraded", "warn"];
    case "Stopping":
    case "Stopped":
      return ["Stopped", "muted"];
    default:
      return ["Error", "bad"];
  }
}

/** Tamanho de bytes legível ("—" para 0/ausente). */
// Motivo de falha vindo do daemon (`DeployStateChanged.message`,
// `ServiceStatus::Error(...)`) reduzido ao que cabe numa linha de status:
// primeira linha não-vazia, aparada e truncada. O texto original costuma ser
// multi-linha (a saída de erro do `docker build`) — o log completo continua na
// aba de logs. Devolve "" quando não há motivo, para o chamador cair no texto
// genérico. Espelha `format/util.luau::short_reason` na GUI iced.
export function shortReason(message, max = 140) {
  if (typeof message !== "string") return "";
  for (const linha of message.split(/\r?\n/)) {
    const limpa = linha.trim();
    if (limpa) return limpa.length > max ? limpa.slice(0, max - 1) + "…" : limpa;
  }
  return "";
}

export function formatBytes(bytes) {
  const byteCount = Number(bytes) || 0;
  if (byteCount === 0) return "—";
  const KB = 1024,
    MB = KB * 1024,
    GB = MB * 1024;
  if (byteCount >= GB) return (byteCount / GB).toFixed(1) + " GB";
  if (byteCount >= MB) return (byteCount / MB).toFixed(0) + " MB";
  return (byteCount / KB).toFixed(0) + " KB";
}

/** Resumo curto da origem de um serviço (ServiceSource externally-tagged). */
export function sourceSummary(source) {
  if (!source || typeof source !== "object") return "—";
  if (source.Registry) return source.Registry.image || "—";
  if (source.Git) return `${source.Git.url} @ ${source.Git.branch}`;
  if (source.Archive) return source.Archive.original_filename || source.Archive.archive_id || "zip enviado";
  if (source.Compose) return "docker-compose";
  return "—";
}

/** Heurística "isso parece uma URL de Git, não uma imagem de registry".
 * Porta de helpers.luau::looks_like_git_url. */
export function looksLikeGitUrl(url) {
  const trimmed = (url || "").trim();
  return (
    trimmed.startsWith("https://") ||
    trimmed.startsWith("http://") ||
    trimmed.startsWith("git@") ||
    trimmed.startsWith("ssh://") ||
    trimmed.startsWith("file://") ||
    trimmed.endsWith(".git")
  );
}

/** `<secret:NOME>` ou `secret:NOME` → "NOME" (ou null se não for referência).
 * Porta de helpers.luau::parse_secret_ref. */
export function parseSecretRef(text) {
  const trimmed = (text || "").trim();
  const match = trimmed.match(/^<secret:(.+)>$/) || trimmed.match(/^secret:(.+)$/);
  if (!match) return null;
  const name = match[1].trim();
  return name || null;
}

/** env_vars + env_comments → texto `.env` (KEY=VALUE, secrets como
 * `<secret:NOME>`, comentários `# ...` na posição ancorada por `before_key`).
 * Porta literal de format/service_detail.luau::env_dotenv_with_comments. */
export function dotenvFromVars(vars, comments) {
  const activeVars = vars || [];
  const activeComments = comments || [];
  const lines = [];
  for (const activeVar of activeVars) {
    for (const activeComment of activeComments) {
      if (activeComment.before_key === activeVar.key) lines.push(activeComment.text);
    }
    const displayValue = activeVar.value?.Secret !== undefined ? `<secret:${activeVar.value.Secret}>` : activeVar.value?.Plain || "";
    lines.push(`${activeVar.key}=${displayValue}`);
  }
  for (const activeComment of activeComments) {
    if (activeComment.before_key === null || activeComment.before_key === undefined) lines.push(activeComment.text);
  }
  return lines.join("\n");
}

/** env_vars + env_comments → linhas pra exibição na lista normal (fora do
 * editor `.env` bruto), comentários `# ...` intercalados na posição ancorada
 * por `before_key`. Cada linha de comentário ganha uma `key` sintética
 * (`__cN`) pra servir de `:key` do `x-for` — comentários não têm chave
 * própria. Porta de format/service_detail.luau::env_json_with_comments. */
export function envRowsWithComments(vars, comments) {
  const activeVars = vars || [];
  const activeComments = comments || [];
  const rows = [];
  for (const activeVar of activeVars) {
    activeComments.forEach((activeComment, commentIndex) => {
      if (activeComment.before_key === activeVar.key) rows.push({ key: `__c${commentIndex}`, isComment: true, text: activeComment.text });
    });
    rows.push({
      key: activeVar.key,
      isComment: false,
      value: activeVar.value?.Plain ?? (activeVar.value?.Secret ? `<secret:${activeVar.value.Secret}>` : ""),
      isSecret: !!activeVar.value?.Secret,
    });
  }
  activeComments.forEach((activeComment, commentIndex) => {
    if (activeComment.before_key === null || activeComment.before_key === undefined) {
      rows.push({ key: `__c${commentIndex}`, isComment: true, text: activeComment.text });
    }
  });
  return rows;
}

/** Remove sequências de escape ANSI (cor/cursor/erase) de uma linha de log.
 * Porta literal de format/util.luau::strip_ansi. Sem isso, um logger colorido
 * (chalk/pino-pretty/etc.) manda bytes de controle que o HTML não interpreta
 * — em vez de cor, viram glifos de caixa (░/□) e o texto quebra visualmente
 * (linhas empilhando por cima umas das outras). */
export function stripAnsi(text) {
  let cleanedText = text || "";
  // CSI: ESC [ params byte-final(letra) — cobre SGR (cor), erase (K/J), cursor.
  cleanedText = cleanedText.replace(/\x1b\[[\d;?]*[a-zA-Z]/g, "");
  // OSC: ESC ] … BEL.
  cleanedText = cleanedText.replace(/\x1b\][^\x07]*\x07/g, "");
  // Qualquer ESC + 1 byte remanescente (ESC c, ESC =, …).
  cleanedText = cleanedText.replace(/\x1b./g, "");
  // CR/NUL soltos.
  cleanedText = cleanedText.replace(/[\r\0]/g, "");
  return cleanedText;
}

/** Texto `.env` → { vars, comments } (env_vars/env_comments do ServiceSpec/
 * Project). Porta literal de handlers/services.luau::parse_dotenv — linhas
 * `# ...` acumulam e ancoram (`before_key`) na próxima `KEY=VALUE` real;
 * sobras no fim viram comentários soltos (`before_key: null`). */
export function parseDotenv(text) {
  const vars = [];
  const comments = [];
  let pending = [];
  for (const rawLine of (text || "").split("\n")) {
    const trimmedLine = rawLine.trim();
    if (trimmedLine === "") continue;
    if (trimmedLine.startsWith("#")) {
      pending.push(trimmedLine);
      continue;
    }
    const equalsIndex = trimmedLine.indexOf("=");
    if (equalsIndex < 0) continue;
    const key = trimmedLine.slice(0, equalsIndex).trim();
    if (!key) continue;
    const rawValue = trimmedLine.slice(equalsIndex + 1).trim();
    for (const commentText of pending) comments.push({ text: commentText, before_key: key });
    pending = [];
    const secret = parseSecretRef(rawValue);
    vars.push({ key, value: secret ? { Secret: secret } : { Plain: rawValue } });
  }
  for (const commentText of pending) comments.push({ text: commentText, before_key: null });
  return { vars, comments };
}

// ── Connection tab: URLs de conexão por tipo de serviço/banco ────────────
// Porta de format/service_detail.luau (safe_name/internal_url/external_url +
// os helpers internos de credenciais/esquema/percent-encode).

/** Normaliza um nome de serviço para `[a-z0-9_]`, mesmo algoritmo de
 * `crate::normalize_name` (Rust) / `format/service_detail.luau::safe_name`. */
export function safeName(name) {
  let out = "";
  let lastDash = true;
  for (const character of name || "") {
    if (/[a-zA-Z0-9]/.test(character)) {
      out += character.toLowerCase();
      lastDash = false;
    } else if (!lastDash) {
      out += "_";
      lastDash = true;
    }
  }
  return out.replace(/^_+/, "").replace(/_+$/, "");
}

function internalScheme(databaseKind) {
  const normalizedKind = (databaseKind || "").toLowerCase();
  if (normalizedKind === "postgres" || normalizedKind === "postgresql") return "postgresql";
  if (normalizedKind === "mysql" || normalizedKind === "mariadb") return "mysql";
  if (normalizedKind === "redis") return "redis";
  if (normalizedKind === "mongodb" || normalizedKind === "mongo") return "mongodb";
  if (normalizedKind === "rabbitmq") return "amqp";
  if (normalizedKind === "nats") return "nats";
  return null; // kafka / serviço comum: passthrough sem esquema
}

/** Chave do serviço que recebe o tráfego dentro de um compose: `ingress_service`
 * se declarado, senão a primeira chave de `services:`. É o hostname que resolve
 * na rede do projeto e **não muda** quando o serviço do rustploy é renomeado
 * (o YAML continua o mesmo). Espelha `compose_host` de format/service_detail.luau. */
export function composeHost(content, ingressService) {
  if (ingressService) return ingressService;
  let inServices = false;
  for (const line of String(content || "").split("\n")) {
    if (!inServices) {
      if (/^services:\s*$/.test(line)) inServices = true;
      continue;
    }
    const match = line.match(/^\s+([\w.-]+):/);
    if (match) return match[1];
    // linha em branco ou comentário: segue; outra coisa sem recuo encerra o bloco.
    if (!/^\s*$/.test(line) && !/^\s*#/.test(line)) return null;
  }
  return null;
}

/** URL de conexão dentro da rede Docker do daemon (`rp_<safe>:<porta>`, com
 * esquema por tipo de banco). */
export function internalUrl(databaseKind, safe, port, composeHostName) {
  const host = `${composeHostName || `rp_${safe}`}:${port}`;
  const scheme = internalScheme(databaseKind);
  return scheme ? `${scheme}://${host}` : host;
}

function envPlain(vars, key) {
  const variable = (vars || []).find((variable) => variable.key === key);
  return variable?.value?.Plain || null;
}

/** (database, user, password) lidos das env vars conhecidas do banco. */
function databaseCredentials(databaseKind, vars) {
  const normalizedKind = (databaseKind || "").toLowerCase();
  if (normalizedKind === "postgres" || normalizedKind === "postgresql") {
    return [envPlain(vars, "POSTGRES_DB"), envPlain(vars, "POSTGRES_USER"), envPlain(vars, "POSTGRES_PASSWORD")];
  }
  if (normalizedKind === "mysql" || normalizedKind === "mariadb") {
    return [envPlain(vars, "MYSQL_DATABASE"), envPlain(vars, "MYSQL_USER"), envPlain(vars, "MYSQL_PASSWORD")];
  }
  if (normalizedKind === "mongodb" || normalizedKind === "mongo") {
    return [null, envPlain(vars, "MONGO_INITDB_ROOT_USERNAME"), envPlain(vars, "MONGO_INITDB_ROOT_PASSWORD")];
  }
  if (normalizedKind === "redis") return [null, null, envPlain(vars, "REDIS_PASSWORD")];
  if (normalizedKind === "rabbitmq") return [null, envPlain(vars, "RABBITMQ_DEFAULT_USER"), envPlain(vars, "RABBITMQ_DEFAULT_PASS")];
  return [null, null, null];
}

function withDatabaseCredentials(base, database, user, password) {
  let url = base;
  if (database) url += `/${database}`;
  const params = [];
  if (user) params.push(`user=${user}`);
  if (password) params.push(`password=${password}`);
  if (params.length) url += `?${params.join("&")}`;
  return url;
}

function percentEncode(text) {
  return encodeURIComponent(text).replace(/[!'()*]/g, (character) => "%" + character.charCodeAt(0).toString(16).toUpperCase());
}

function userinfo(user, password) {
  const encodedUser = user ? percentEncode(user) : "";
  const encodedPassword = password ? percentEncode(password) : "";
  if (!encodedUser && !encodedPassword) return "";
  return encodedPassword ? `${encodedUser}:${encodedPassword}@` : `${encodedUser}@`;
}

function externalScheme(databaseKind) {
  if (databaseKind === "mysql") return "mysql";
  if (databaseKind === "mariadb") return "mariadb";
  if (databaseKind === "redis") return "redis";
  if (databaseKind === "mongodb" || databaseKind === "mongo") return "mongodb";
  if (databaseKind === "rabbitmq") return "amqp";
  if (databaseKind === "nats") return "nats";
  return null;
}

function databaseConnectionUrl(databaseKind, host, port, database, user, password) {
  const normalizedKind = (databaseKind || "").toLowerCase();
  const hostAndPort = `${host}:${port}`;
  if (normalizedKind === "postgres" || normalizedKind === "postgresql") {
    return "jdbc:" + withDatabaseCredentials(`postgresql://${hostAndPort}`, database, user, password);
  }
  const scheme = externalScheme(normalizedKind);
  if (!scheme) return hostAndPort;
  let url = `${scheme}://${userinfo(user, password)}${hostAndPort}`;
  if (database) url += `/${database}`;
  else if (normalizedKind === "mongodb" || normalizedKind === "mongo") url += "/";
  if ((normalizedKind === "mongodb" || normalizedKind === "mongo") && user) url += "?authSource=admin";
  return url;
}

function urlHost(apiUrl) {
  const normalizedUrl = (apiUrl || "").replace(/\s/g, "");
  if (!normalizedUrl) return null;
  const match = normalizedUrl.match(/^[a-zA-Z][\w+.-]*:\/\/([^:/]+)/);
  return match ? match[1] : normalizedUrl.match(/^([^:/]+)/)?.[1] || null;
}

/** URL de conexão externa: domínio HTTP tem prioridade; sem domínio, cai
 * pro passthrough TCP (host_port) com a URL idiomática do banco. */
export function externalUrl(domain, tls, hostPort, databaseKind, apiUrl, envVars) {
  const [database, user, password] = databaseCredentials(databaseKind, envVars);
  if (domain && domain.trim()) {
    const clean = domain.replace(/\/+$/, "");
    return `${tls ? "https" : "http"}://${clean}`;
  }
  if (hostPort) {
    const host = urlHost(apiUrl) || "<host>";
    return databaseConnectionUrl(databaseKind, host, String(hostPort), database, user, password);
  }
  return "—";
}

// ── Deploy Engine / Monitoring / Ingress ──────────────────────────────────
// Porta de format/dashboard.luau (ingress/host_ports/monitoring/eng_*) e
// format/util.luau::domain_routes/pair_list — mesmas fórmulas, sem a truncagem
// por coluna (Util.ellipsis/col_budgets), que só existe no glacier por causa
// da janela redimensionável; aqui o CSS já cuida do overflow.

/** `msg.services` (`[{project_name, service}]`) → `[{svc, proj}]`. */
export function servicePairList(services) {
  return (services || []).map((service) => ({ svc: service.service, proj: service.project_name || "" }));
}

/** `spec.domains` se houver, senão o legado `domain`/`tls_enabled`. Porta
 * literal de format/util.luau::domain_routes — ver memória multi_domain_routes:
 * é o helper canônico, não reimplementar ad-hoc. */
export function domainRoutes(spec) {
  if (spec.domains && spec.domains.length > 0) {
    return spec.domains.map((domain) => ({ domain: domain.domain, port: domain.port ?? null, tls: domain.tls === true }));
  }
  if (spec.domain && spec.domain.trim()) {
    return [{ domain: spec.domain, port: null, tls: spec.tls_enabled === true }];
  }
  return [];
}

/** Ingress: uma linha por rota de domínio (não filtrado pela busca). */
export function ingressRows(pairs) {
  const rows = [];
  for (const pair of pairs) {
    const spec = pair.svc.spec;
    for (const route of domainRoutes(spec)) {
      if (route.domain && route.domain.trim()) {
        const cport = route.port ?? spec.port;
        rows.push({
          domain: route.domain,
          url: `${route.tls ? "https" : "http"}://${route.domain}`,
          service: spec.name,
          project: pair.proj,
          upstream: `:${cport}`,
          tls: route.tls ? "TLS" : "—",
        });
      }
    }
  }
  return rows;
}

/** Portas TCP de host: uma linha por serviço com `host_port` configurado. */
export function hostPortRows(pairs) {
  const rows = [];
  for (const pair of pairs) {
    const spec = pair.svc.spec;
    if (spec.host_port != null) {
      rows.push({
        service: spec.name,
        project: pair.proj,
        hostPort: spec.host_port,
        containerPort: spec.port,
      });
    }
  }
  rows.sort((left, right) => left.hostPort - right.hostPort);
  return rows;
}

/** Monitoring: uma linha por serviço COM métricas vivas (`metricsById[id]`
 * só existe depois do primeiro evento `ContainerMetrics`). */
export function monitoringRows(pairs, metricsById) {
  const rows = [];
  for (const pair of pairs) {
    const metricsPoint = metricsById && metricsById[pair.svc.id];
    if (metricsPoint) {
      rows.push({
        name: pair.svc.spec.name,
        project: pair.proj,
        cpu: `${(metricsPoint.cpu_percent || 0).toFixed(1)}%`,
        mem: formatBytes(metricsPoint.mem_used_bytes),
        rx: formatBytes(metricsPoint.net_rx_bytes),
        tx: formatBytes(metricsPoint.net_tx_bytes),
      });
    }
  }
  return rows;
}

/** Passos do stepper do deploy. "Obter" agrupa pull/clone/build/compose para
 * a linha não depender do tipo de serviço (imagem, git ou compose). Espelha
 * format/dashboard.luau::DEPLOY_STEPS. */
const DEPLOY_STEPS = [
  { label: "Fila", states: ["Pending"] },
  { label: "Checks", states: ["PreDeployCheck"] },
  { label: "Deps", states: ["ResolvingDeps"] },
  { label: "Obter", states: ["PullingImage", "CloningRepo", "BuildingImage", "ComposingUp"] },
  { label: "Staging", states: ["Staging"] },
  { label: "Health", states: ["HealthcheckPolling"] },
  { label: "Swap", states: ["SwappingIn", "Draining", "Promoting"] },
  { label: "Live", states: ["Live"] },
];

function deployStepIndex(state) {
  return DEPLOY_STEPS.findIndex((step) => step.states.includes(state));
}

/** Linha de passos: [{label, status}] com status done|current|failed|pending.
 * Na falha o passo marcado é o de onde a última transição saiu (`from` do
 * `→ Failed`); sem histórico, nenhum passo é marcado como falho. */
export function deployStepper(info) {
  const state = info.state;
  let currentStep = deployStepIndex(state);
  let failed = -1;
  if (state === "Live" || state === "Stopped" || state === "Pruning") {
    currentStep = DEPLOY_STEPS.length;
  } else if (state === "Failed" || state === "RollingBack") {
    const log = info.states || [];
    for (let index = log.length - 1; index >= 0; index--) {
      const stepIndex = deployStepIndex(log[index].from);
      if (stepIndex >= 0) {
        failed = stepIndex;
        break;
      }
    }
    currentStep = failed;
  }
  return DEPLOY_STEPS.map((step, index) => ({
    label: step.label,
    status: index === failed ? "failed" : index < currentStep ? "done" : index === currentStep ? "current" : "pending",
  }));
}

/** Detalhe do deploy para o modal: uma linha por transição (estado em que
 * entrou, quanto durou, mensagem). A duração do estado é até a transição
 * seguinte; o último fica com o tempo de fase corrente / total. */
export function deployDetailRows(info) {
  const log = info.states || [];
  return log.map((transition, index) => {
    const start = Date.parse(transition.at);
    const end = index + 1 < log.length ? Date.parse(log[index + 1].at) : null;
    const secs = end !== null ? Math.max(0, Math.round((end - start) / 1000)) : info.current_state_secs;
    const last = index === log.length - 1;
    return {
      state: transition.to,
      label: transition.to,
      kind: transition.to === "Failed" ? "bad" : last && info.state === transition.to ? "info" : "ok",
      dur: last && (transition.to === "Failed" || transition.to === "Live" || transition.to === "Stopped") ? "—" : formatSeconds(secs),
      msg: transition.message || "",
    };
  });
}

/** Deploy Engine: "Executando agora". */
export function deployEngineActiveRows(active) {
  return (active || []).map((info) => {
    const [label, kind] = stateLabelKind(info.state);
    return {
      service: info.service_name,
      project: info.project_name,
      stateLabel: label,
      stateKind: kind,
      steps: deployStepper(info),
      detail: deployDetailRows(info),
      total: formatSeconds(info.elapsed_secs),
      phase: formatSeconds(info.current_state_secs),
      serviceId: info.service_id,
    };
  });
}

/** Deploy Engine: "Na fila" (o primeiro é o próximo a rodar). */
export function deployEngineQueuedRows(queued) {
  return (queued || []).map((info, index) => ({
    deploymentId: info.deployment_id,
    pos: index + 1,
    service: info.service_name,
    project: info.project_name,
  }));
}

// ── Docker (host-wide: containers/imagens/volumes/networks) ──────────────
// Porta de format/dashboard.luau::docker_containers/docker_images/docker_volumes/
// docker_networks — mesmas fórmulas, sem a truncagem por coluna (decisão já
// tomada na Fase 5: o CSS cuida do overflow na web).

/** Containers do host (rodando + parados). `canRemove` só nos parados (o
 * Docker recusa `rm` de um rodando sem force). `owner` mostra projeto/serviço
 * quando atribuído, senão "rustploy" (managed) ou "—". */
export function dockerContainerRows(list, term) {
  const searchTerm = (term || "").toLowerCase();
  const sorted = [...(list || [])].sort((left, right) => (left.name || "").localeCompare(right.name || ""));
  const rows = [];
  for (const container of sorted) {
    if (!matchesTerm(searchTerm, [container.name, container.image, container.state, container.project || "", container.service || ""])) continue;
    const stateLabel = containerStateLabel(container.state);
    let owner;
    if (container.project && container.service) owner = `${container.project} / ${container.service}`;
    else if (container.managed) owner = "rustploy";
    else owner = "—";
    const image = (container.image || "—").replace(/^sha256:/, "");
    rows.push({
      idFull: container.id || "",
      id: (container.id || "").slice(0, 12),
      name: container.name || "—",
      image,
      owner,
      stateLabel: stateLabel[0],
      stateKind: stateLabel[1],
      canRemove: containerStopped(container.state),
    });
  }
  return rows;
}

/** Rótulo/kind de um estado bruto do Docker ("running","exited",...). Porta
 * literal de format/util.luau::container_state. */
function containerStateLabel(state) {
  const lowered = (state || "").toLowerCase();
  if (lowered === "running") return ["running", "ok"];
  if (lowered === "restarting") return ["restarting", "info"];
  if (lowered === "paused") return ["paused", "warn"];
  if (lowered === "created") return ["created", "info"];
  if (lowered === "dead") return ["dead", "bad"];
  return [lowered || "exited", "muted"];
}

/** Porta literal de format/util.luau::container_stopped — só os parados podem
 * ser removidos (o Docker recusa `rm` de um rodando sem force). */
function containerStopped(state) {
  const lowered = (state || "").toLowerCase();
  return lowered !== "running" && lowered !== "restarting" && lowered !== "paused";
}

export function dockerImageRows(list, term) {
  const searchTerm = (term || "").toLowerCase();
  const sorted = [...(list || [])].sort((left, right) => (left.tags || []).join(",").localeCompare((right.tags || []).join(",")));
  const rows = [];
  for (const image of sorted) {
    const tagStr = (image.tags || []).join(" ");
    if (!matchesTerm(searchTerm, [tagStr, image.project || "", image.service || ""])) continue;
    const inUse = (image.containers || 0) > 0;
    const imageId = (image.id || "").replace(/^sha256:/, "");
    rows.push({
      id: imageId.slice(0, 12),
      idFull: imageId,
      tags: (image.tags || []).length ? image.tags.join(", ") : "<none>",
      size: formatBytes(image.size_bytes),
      created: dateDayMonthHourMinute(image.created),
      project: image.project || "—",
      service: image.service || "—",
      owner: `${image.project || "—"} / ${image.service || "—"}`,
      inUse,
      inUseLabel: inUse ? "EM USO" : "SEM USO",
      inUseKind: inUse ? "ok" : "muted",
    });
  }
  return rows;
}

export function dockerVolumeRows(list, term) {
  const searchTerm = (term || "").toLowerCase();
  const sorted = [...(list || [])].sort((left, right) => (left.name || "").localeCompare(right.name || ""));
  const rows = [];
  for (const volume of sorted) {
    if (!matchesTerm(searchTerm, [volume.name, volume.driver])) continue;
    rows.push({
      name: volume.name,
      nameFull: volume.name,
      driver: volume.driver,
      mountpoint: volume.mountpoint,
      size: volume.size_bytes != null && volume.size_bytes >= 0 ? formatBytes(volume.size_bytes) : "—",
      inUse: !!volume.in_use,
      inUseLabel: volume.in_use ? "EM USO" : "SEM USO",
      inUseKind: volume.in_use ? "ok" : "muted",
    });
  }
  return rows;
}

export function dockerNetworkRows(list, term) {
  const searchTerm = (term || "").toLowerCase();
  const sorted = [...(list || [])].sort((left, right) => (left.name || "").localeCompare(right.name || ""));
  const rows = [];
  for (const network of sorted) {
    if (!matchesTerm(searchTerm, [network.name, network.project || ""])) continue;
    rows.push({
      name: network.name,
      idFull: network.id || "",
      driver: network.driver,
      scope: network.scope,
      project: network.project || "—",
      containers: network.container_count || 0,
      inUse: !!network.in_use,
      inUseLabel: network.in_use ? "EM USO" : "SEM USO",
      inUseKind: network.in_use ? "ok" : "muted",
    });
  }
  return rows;
}

// ── Registry OCI embutido ──────────────────────────────────────────────────
// Porta de format/registry.luau.

/** Lista de repositórios (filtrada pela busca global). */
export function registryRepoRows(list, term) {
  const searchTerm = (term || "").toLowerCase();
  const rows = [];
  for (const item of list || []) {
    if (!matchesTerm(searchTerm, [item.name])) continue;
    rows.push({
      name: item.name,
      tagCount: item.tag_count || 0,
      size: formatBytes(item.size_bytes || 0),
      created: dateDayMonthHourMinute(item.created_at),
    });
  }
  return rows;
}

/** Tags de UM repositório (sem filtro — lista pequena, buscada sob demanda). */
export function registryTagRows(list) {
  return (list || []).map((item) => ({
    tag: item.tag,
    digest: (item.digest || "").slice(0, 12),
    digestFull: item.digest,
    size: formatBytes(item.size_bytes || 0),
    updated: dateDayMonthHourMinute(item.updated_at),
  }));
}

/** Tokens de acesso Basic auth (sem filtro — lista pequena). */
export function registryTokenRows(list) {
  return (list || []).map((item) => ({
    name: item.name,
    scope: item.scope,
    created: dateDayMonthHourMinute(item.created_at),
    lastUsed: item.last_used_at ? dateDayMonthHourMinute(item.last_used_at) : "nunca",
  }));
}

// ── Schedules (jobs one-shot via docker-compose) ──────────────────────────
// Porta de format/jobs.luau.

const WEEKDAYS = ["seg", "ter", "qua", "qui", "sex", "sáb", "dom"];

/** Resumo textual de uma `Recurrence?` (nil = só manual). Porta literal de
 * format/jobs.luau::recurrence_label. */
export function recurrenceLabel(recurrence) {
  if (recurrence == null) return "manual";
  if (recurrence.IntervalHours != null) return `a cada ${recurrence.IntervalHours}h`;
  if (recurrence.Daily) {
    const padTwoDigits = (value) => String(value).padStart(2, "0");
    return `diariamente às ${padTwoDigits(recurrence.Daily.hour)}:${padTwoDigits(recurrence.Daily.minute)}`;
  }
  if (recurrence.Weekly) {
    const padTwoDigits = (value) => String(value).padStart(2, "0");
    const weekdayName = WEEKDAYS[((recurrence.Weekly.weekday % 7) + 7) % 7] || "?";
    return `semanalmente ${weekdayName} às ${padTwoDigits(recurrence.Weekly.hour)}:${padTwoDigits(recurrence.Weekly.minute)}`;
  }
  return "—";
}

/** Rótulo/kind da última execução de um job. `run == null` (nunca rodou) /
 * `run.success == null` (rodando agora) / `true`/`false` (ok/falhou). Porta
 * literal de format/jobs.luau::run_status. */
export function jobRunStateLabel(run) {
  if (run == null) return ["nunca rodou", "muted"];
  if (run.success == null) return ["rodando…", "warn"];
  return run.success ? ["ok", "ok"] : ["falhou", "bad"];
}

/** Tela global "Schedules": uma linha por job, de todos os projetos.
 * `inflight` (opcional): `store.jobsInflight` — job_id → true entre o click
 * em "Rodar agora" e a confirmação do servidor (ver app.js::jobRunNow). Sem
 * isso, `running` só reflete o snapshot já confirmado pelo daemon, deixando
 * o botão clicável durante todo o round-trip da RPC — como os dois call
 * sites (schedules.js/project_detail.js) são `get` do Alpine, passar
 * `store.jobsInflight` aqui é suficiente pra eles recomputarem sozinhos
 * assim que o objeto mutar (reatividade do Proxy do Alpine). */
export function jobSummaryRows(list, term, inflight) {
  const searchTerm = (term || "").toLowerCase();
  const rows = [];
  for (const item of list || []) {
    const job = item.job;
    if (!matchesTerm(searchTerm, [job.name, item.project_name, item.trigger_service_name || ""])) continue;
    const [lastRunLabel, lastRunKind] = jobRunStateLabel(item.last_run);
    rows.push({
      id: job.id,
      name: job.name,
      project: item.project_name,
      owner: item.trigger_service_name ? `${item.project_name} / ${item.trigger_service_name}` : item.project_name,
      recurrence: recurrenceLabel(job.recurrence),
      enabled: job.enabled,
      enabledLabel: job.enabled ? "Pausar" : "Ativar",
      lastRunLabel,
      lastRunKind,
      // `running`: usado pra desativar/trocar o botão "Rodar agora" enquanto
      // o job_run em voo não termina. `lastRunKind === "warn"` é o estado
      // CONFIRMADO pelo servidor; `inflight[job.id]` cobre o intervalo entre
      // o click e essa confirmação chegar.
      running: lastRunKind === "warn" || !!(inflight && inflight[job.id]),
      lastRunId: item.last_run ? item.last_run.id : "",
      nextRunAt: job.next_run_at ? dateDayMonthHourMinute(job.next_run_at) : "—",
    });
  }
  return rows;
}

// ── Settings (Web Server / Git / Infra as Code) ───────────────────────────

/** Redirect URI do OAuth (`{base}/oauth/{gitea|github}/callback`). Porta
 * literal de helpers.luau::oauth_redirect_uri. */
export function oauthRedirectUri(base, kind) {
  const providerSegment = kind === "github" ? "github" : "gitea";
  const trimmedBase = (base || "").trim().replace(/\/+$/, "");
  if (trimmedBase === "") return `<URL pública do daemon indisponível>/oauth/${providerSegment}/callback`;
  return `${trimmedBase}/oauth/${providerSegment}/callback`;
}

function gitKindLabel(kind) {
  if (kind === "Github") return "GitHub";
  if (kind === "Gitea") return "Gitea";
  return kind || "Git";
}

/** Porta literal de format/git.luau::git_providers. */
export function gitProviderRows(list) {
  return (list || []).map((item) => {
    const login = item.account?.login;
    const connected = login != null;
    return {
      id: item.id,
      name: item.name,
      kind: gitKindLabel(item.kind),
      display: connected ? `${item.name || ""} (@${login})` : `${item.name || ""} — não conectado`,
      baseUrl: item.base_url,
      authMode: item.auth_mode === "OAuth" ? "OAuth2" : "PAT",
      account: connected ? `@${login}` : "(pendente — autorize no navegador)",
      connected,
    };
  });
}

// ── Limpeza automática de Docker (Settings → Manutenção) ──────────────────
// Porta de format/docker_cleanup.luau — ver docs/plano-limpeza-automatica-docker.md.

const DOCKER_CLEANUP_LABELS = {
  containers: "containers",
  images: "imagens",
  volumes: "volumes",
  networks: "redes",
  build_cache: "cache de build",
};

export function dockerCleanupResourceLabel(name) {
  return DOCKER_CLEANUP_LABELS[name] || name;
}

/** `lr`: `DockerCleanupLastRun?` (`{ at, results: [{ resource, count,
 * reclaimed_bytes, error }] }`). `null`/`undefined` = nunca rodou. */
export function dockerCleanupLastRunSummary(lastRun) {
  if (!lastRun) return "ainda não rodou";
  let totalBytes = 0;
  let anyError = false;
  const parts = (lastRun.results || []).map((result) => {
    totalBytes += result.reclaimed_bytes || 0;
    if (result.error != null) {
      anyError = true;
      return `${dockerCleanupResourceLabel(result.resource)}: erro`;
    }
    return `${dockerCleanupResourceLabel(result.resource)} ${result.count || 0}`;
  });
  const head = `${dateDayMonthHourMinuteSecond(lastRun.at)} · ${formatBytes(totalBytes)} liberados`;
  if (parts.length === 0) return head;
  let suffix = ` (${parts.join(", ")})`;
  if (anyError) suffix += " — alguns recursos falharam, ver logs do daemon";
  return head + suffix;
}

/** Deploy Engine: "Histórico 24h". */
export function deployEngineRecentRows(recent) {
  return (recent || []).map((info) => {
    const [label, kind] = stateLabelKind(info.state);
    let icon = "○";
    if (info.state === "Live") icon = "✓";
    else if (info.state === "Failed") icon = "✕";
    return {
      icon,
      service: info.service_name,
      project: info.project_name,
      stateLabel: label,
      stateKind: kind,
      duration: formatSeconds(info.elapsed_secs),
      start: timeHms(info.started_at),
    };
  });
}
