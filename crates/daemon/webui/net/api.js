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

/** Prefixo da chave de acesso (ver api/agent/access_key.rs no daemon). */
const ACCESS_KEY_PREFIX = "rpk1_";

/** Decodifica `rpk1_<base64url de {"u","t"}>` em `{ url, token }`, ou `null`
 * se a string não estiver nesse formato. Só para exibir/conferir o servidor:
 * quem valida a chave é o `POST /api/login`. */
export function decodeAccessKey(accessKey) {
  const trimmed = (accessKey || "").trim();
  if (!trimmed.startsWith(ACCESS_KEY_PREFIX)) return null;
  try {
    const base64 = trimmed.slice(ACCESS_KEY_PREFIX.length).replace(/-/g, "+").replace(/_/g, "/");
    const decoded = JSON.parse(atob(base64 + "===".slice((base64.length + 3) % 4)));
    return decoded && decoded.u && decoded.t ? { url: decoded.u, token: decoded.t } : null;
  } catch {
    return null;
  }
}

/** Monta a chave de um par endereço + token (o inverso de `decodeAccessKey`).
 * O token é ASCII (hex/base64url), então `btoa` direto basta. */
export function encodeAccessKey(url, token) {
  const base64 = btoa(JSON.stringify({ u: url, t: token }));
  return ACCESS_KEY_PREFIX + base64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** `POST /api/login`: troca a chave pelo token de acesso. Devolve
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
