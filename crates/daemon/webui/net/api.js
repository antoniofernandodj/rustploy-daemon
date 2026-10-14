// net/api.js — cliente HTTP/JSON do daemon. Porta de
// crates/rustploy-gui/views/scripts/net/api.luau: mesma convenção de
// Command/Response (serde externally-tagged) e mesma autenticação Bearer.
//
//   • variante unitária  → string:  "DaemonStatus", "StopAllManaged"
//   • variante com campos → objeto: { ProjectDelete: { id: "..." } }

/** Teto de cada requisição. Sem ele um daemon mudo deixaria a ação (e o botão
 * bloqueado por busy.js) pendurada para sempre; estourar vira `{ ok:false }`. */
const RPC_TIMEOUT_MS = 60_000;
const UPLOAD_TIMEOUT_MS = 10 * 60_000;

/** `fetch` com timeout cobrindo cabeçalho E corpo; o erro de timeout vira
 * mensagem legível. Devolve `{ ok, status, json() }` com o corpo já lido. */
async function fetchTimeout(url, init, timeoutMs) {
  const abortController = new AbortController();
  const timer = setTimeout(() => abortController.abort(), timeoutMs);
  try {
    const httpResponse = await fetch(url, { ...init, signal: abortController.signal });
    const text = await httpResponse.text();
    return { ok: httpResponse.ok, status: httpResponse.status, json: async () => JSON.parse(text) };
  } catch (error) {
    if (abortController.signal.aborted) throw new Error("tempo esgotado (" + Math.round(timeoutMs / 1000) + "s) sem resposta do daemon");
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

// Token de conexão (ver api/agent/access_key.rs no daemon): base64 padrão,
// com padding, de `host:porta::token` — sem esquema.
const ACCESS_KEY_SEPARATOR = "::";

/** Decodifica o token de conexão em `{ address, token }` (`address` =
 * `host:porta`), ou `null` se a string não estiver nesse formato. Aceita
 * também o alfabeto url-safe e a falta de padding. */
export function decodeAccessKey(accessKey) {
  const trimmed = (accessKey || "").trim();
  if (!trimmed) return null;
  let text;
  try {
    const base64 = trimmed.replace(/=+$/, "").replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(base64 + "===".slice((base64.length + 3) % 4));
    text = new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
  } catch {
    return null;
  }
  // O primeiro `::` fora do endereço: um IPv6 (`[::1]:9797`) tem os seus.
  const authorityStart = text.includes("://") ? text.indexOf("://") + 3 : 0;
  const searchFrom = text[authorityStart] === "[" ? text.indexOf("]", authorityStart) : authorityStart;
  const at = searchFrom < 0 ? -1 : text.indexOf(ACCESS_KEY_SEPARATOR, searchFrom);
  if (at < 0) return null;
  // Um esquema posto à mão não faz parte do formato: descartado.
  const address = text.slice(authorityStart, at).trim().replace(/\/+$/, "");
  const token = text.slice(at + ACCESS_KEY_SEPARATOR.length).trim();
  return address && token ? { address, token } : null;
}

/** Monta o token de conexão de um endereço (URL ou `host:porta`) + token da
 * API — o inverso de `decodeAccessKey`. A porta sempre vai, mesmo a padrão.
 * O token é ASCII (hex/base64url), então `btoa` direto basta. */
export function encodeAccessKey(url, token) {
  let address = url.replace(/\/+$/, "");
  try {
    const parsed = new URL(address.includes("://") ? address : "http://" + address);
    address = parsed.hostname + ":" + (parsed.port || (parsed.protocol === "https:" ? "443" : "80"));
  } catch {
    // não é URL: segue como veio
  }
  return btoa(address + ACCESS_KEY_SEPARATOR + token);
}

/** `POST /api/login`: troca o token de conexão pelo token da API. A webui não
 * usa mais (decodifica e usa o token direto); fica para depuração/agentes. Devolve
 * `{ ok: true, token }` ou `{ ok: false, error }` com a explicação do daemon. */
export async function login(baseUrl, accessKey) {
  let httpResponse;
  try {
    httpResponse = await fetchTimeout(
      baseUrl.replace(/\/+$/, "") + "/api/login",
      { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ key: accessKey.trim() }) },
      RPC_TIMEOUT_MS,
    );
  } catch (error) {
    return { ok: false, error: error && error.message ? error.message : "falha de rede" };
  }
  let decoded;
  try {
    decoded = await httpResponse.json();
  } catch {
    return { ok: false, error: "HTTP " + httpResponse.status };
  }
  if (!httpResponse.ok) {
    return { ok: false, error: decoded?.error?.message || "HTTP " + httpResponse.status };
  }
  return { ok: true, token: decoded.access_token };
}

export class Api {
  constructor(baseUrl, token) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.token = token || "";
  }

  headers() {
    const headers = { "Content-Type": "application/json" };
    if (this.token) headers["Authorization"] = "Bearer " + this.token;
    return headers;
  }

  /** Executa um Command. Retorna { ok: true, value } ou { ok: false, error }. */
  async rpc(command) {
    let httpResponse;
    try {
      httpResponse = await fetchTimeout(
        this.baseUrl + "/api/rpc",
        { method: "POST", headers: this.headers(), body: JSON.stringify(command) },
        RPC_TIMEOUT_MS,
      );
    } catch (error) {
      return { ok: false, error: error && error.message ? error.message : "falha de rede" };
    }
    if (!httpResponse.ok) {
      return { ok: false, error: "HTTP " + httpResponse.status };
    }
    let decoded;
    try {
      decoded = await httpResponse.json();
    } catch {
      return { ok: false, error: "resposta inválida do daemon" };
    }
    return { ok: true, value: decoded };
  }

  /** Como rpc(), mas trata `Response::Err { code, message }` como falha. */
  async rpcChecked(command) {
    const rpcResponse = await this.rpc(command);
    if (!rpcResponse.ok) return rpcResponse;
    const decoded = rpcResponse.value;
    if (decoded && typeof decoded === "object" && decoded.Err) {
      return { ok: false, error: decoded.Err.message || decoded.Err.code || "erro" };
    }
    return rpcResponse;
  }

  /** `POST /api/services/<id>/archive` — corpo binário cru (não é RPC JSON).
   * Porta de net/api.luau::upload_archive; aqui o `File` já traz os bytes
   * (sem o round-trip por base64 que o Luau precisa pro `fetch("file://…")`). */
  async uploadArchive(serviceId, file) {
    let httpResponse;
    try {
      const headers = { "Content-Type": "application/zip" };
      if (this.token) headers["Authorization"] = "Bearer " + this.token;
      headers["X-Rustploy-Filename"] = file.name || "archive.zip";
      httpResponse = await fetchTimeout(
        `${this.baseUrl}/api/services/${serviceId}/archive`,
        { method: "POST", headers: headers, body: file },
        UPLOAD_TIMEOUT_MS,
      );
    } catch (error) {
      return { ok: false, error: error && error.message ? error.message : "falha de rede" };
    }
    if (!httpResponse.ok) {
      return { ok: false, error: "HTTP " + httpResponse.status };
    }
    let decoded;
    try {
      decoded = await httpResponse.json();
    } catch {
      return { ok: false, error: "resposta inválida do daemon" };
    }
    if (decoded && typeof decoded === "object" && decoded.Err) {
      return { ok: false, error: decoded.Err.message || decoded.Err.code || "erro" };
    }
    return { ok: true, value: decoded };
  }
}
