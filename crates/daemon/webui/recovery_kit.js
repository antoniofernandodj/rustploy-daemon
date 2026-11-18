// recovery_kit.js — lê o arquivo de recuperação (`.rpkit`) que o daemon gera junto com a chave
// (Settings → Chave de recuperação) e que a aba Recuperar carrega para preencher os quatro campos.
// O formato está descrito em `src/features/data_backup/recovery_kit.rs`; a GUI tem o mesmo leitor em
// `views/scripts/recovery_kit.luau`. Mude os três juntos.

import { t } from "./i18n.js";

const MAGIC = [0x89, 0x52, 0x50, 0x4b, 0x49, 0x54, 0x0d, 0x0a]; // \x89 R P K I T \r \n
const VERSION = 1;
const HEADER_LEN = 12;
const MAX_FIELD = 64 * 1024;

const TAG_FIELDS = {
  1: "infra_remote",
  2: "infra_branch",
  3: "infra_token",
  4: "private_key",
  5: "public_key",
  6: "server",
  7: "created_at",
};

/** CRC-32 (IEEE), bit a bit — o mesmo do daemon. */
export function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/** O arquivo recusado: a mensagem é a mesma do daemon e da GUI, já traduzida. */
class RecoveryKitError extends Error {}

const fail = (message) => {
  throw new RecoveryKitError(message);
};

/**
 * Lê e confere o arquivo (`Uint8Array`). Devolve o kit (`{ infra_remote, infra_branch, infra_token,
 * private_key, public_key, server, created_at }`) ou lança `Error` com a mensagem para a tela.
 */
export function decodeRecoveryKit(bytes) {
  const sameAt = (from, to) => MAGIC.slice(from, to).every((value, index) => bytes[from + index] === value);
  if (bytes.length < HEADER_LEN + 4 || !sameAt(1, 6)) {
    fail(t("não é um arquivo de recuperação do Rustploy"));
  }
  if (!sameAt(0, 8)) {
    fail(t("o arquivo de recuperação foi alterado na cópia (modo texto ou conversão de fim de linha): use o arquivo original"));
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const bodyLength = bytes.length - 4;
  if (view.getUint32(bodyLength, true) !== crc32(bytes.subarray(0, bodyLength))) {
    fail(t("o arquivo de recuperação está corrompido (a soma de verificação não confere)"));
  }
  const version = view.getUint16(8, true);
  if (version !== VERSION) {
    fail(t`arquivo de recuperação da versão ${version}; esta versão do Rustploy lê a ${VERSION}`);
  }
  const count = view.getUint16(10, true);
  const decoder = new TextDecoder("utf-8", { fatal: true });
  const kit = {
    infra_remote: "", infra_branch: "", infra_token: "", private_key: "",
    public_key: "", server: "", created_at: "",
  };
  let seenRemote = false;
  let position = HEADER_LEN;
  for (let index = 0; index < count; index++) {
    if (position + 6 > bodyLength) fail(t("o arquivo de recuperação está truncado"));
    const tag = view.getUint16(position, true);
    const length = view.getUint32(position + 2, true);
    if (length > MAX_FIELD) fail(t("o arquivo de recuperação está corrompido (campo grande demais)"));
    const start = position + 6;
    if (start + length > bodyLength) fail(t("o arquivo de recuperação está truncado"));
    let text;
    try {
      text = decoder.decode(bytes.subarray(start, start + length));
    } catch {
      fail(t("o arquivo de recuperação está corrompido (texto inválido)"));
    }
    const field = TAG_FIELDS[tag]; // etiqueta desconhecida: campo de versão futura
    if (field) {
      kit[field] = text;
      if (field === "infra_remote") seenRemote = true;
    }
    position = start + length;
  }
  if (position !== bodyLength) fail(t("o arquivo de recuperação está corrompido (sobram bytes)"));
  if (!seenRemote || !kit.infra_remote || !kit.private_key) {
    fail(t("o arquivo de recuperação está incompleto (falta o repositório ou a chave)"));
  }
  return kit;
}

/** base64 → `Uint8Array` (a resposta JSON do daemon carrega o arquivo assim). */
export function base64ToBytes(text) {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index++) bytes[index] = binary.charCodeAt(index);
  return bytes;
}
