// zip.js — o `.zip` do Infra as Code no navegador.
//
// O app desktop exporta/importa o manifesto como UM arquivo `.zip` com exatamente
// `rustploy.yml` + `rustploy.vars.toml` na raiz (rustploy-gui/src/manifest_zip.rs).
// Este módulo faz o mesmo no navegador, sem biblioteca: escreve um zip sem
// compressão (são dois arquivos de texto pequenos) e lê o que o desktop grava
// (deflate) ou qualquer zip com entradas "stored"/"deflate". As regras de
// validação e as mensagens são as do desktop — mudou lá, muda aqui.

export const YML_ENTRY = "rustploy.yml";
export const TOML_ENTRY = "rustploy.vars.toml";

const textEncoder = new TextEncoder();

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(bytes) {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) crc = CRC_TABLE[(crc ^ bytes[i]) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

/** Data/hora no formato DOS (o zip não guarda fuso). */
function dosDateTime(date) {
  const time = (date.getHours() << 11) | (date.getMinutes() << 5) | (date.getSeconds() >> 1);
  const day = ((date.getFullYear() - 1980) << 9) | ((date.getMonth() + 1) << 5) | date.getDate();
  return { time: time & 0xffff, day: day & 0xffff };
}

/** Monta o zip do manifesto: `rustploy.yml` e `rustploy.vars.toml`, sem compressão. */
export function buildManifestZip(yaml, toml) {
  const { time, day } = dosDateTime(new Date());
  const entries = [
    { name: textEncoder.encode(YML_ENTRY), data: textEncoder.encode(yaml) },
    { name: textEncoder.encode(TOML_ENTRY), data: textEncoder.encode(toml) },
  ];

  const chunks = [];
  const central = [];
  let offset = 0;
  for (const entry of entries) {
    const crc = crc32(entry.data);
    const local = new DataView(new ArrayBuffer(30));
    local.setUint32(0, 0x04034b50, true);
    local.setUint16(4, 20, true); // versão mínima
    local.setUint16(6, 0x0800, true); // flag: nome em UTF-8
    local.setUint16(8, 0, true); // método: stored
    local.setUint16(10, time, true);
    local.setUint16(12, day, true);
    local.setUint32(14, crc, true);
    local.setUint32(18, entry.data.length, true);
    local.setUint32(22, entry.data.length, true);
    local.setUint16(26, entry.name.length, true);
    local.setUint16(28, 0, true);
    chunks.push(new Uint8Array(local.buffer), entry.name, entry.data);

    const head = new DataView(new ArrayBuffer(46));
    head.setUint32(0, 0x02014b50, true);
    head.setUint16(4, 20, true); // versão que criou
    head.setUint16(6, 20, true); // versão mínima
    head.setUint16(8, 0x0800, true);
    head.setUint16(10, 0, true);
    head.setUint16(12, time, true);
    head.setUint16(14, day, true);
    head.setUint32(16, crc, true);
    head.setUint32(20, entry.data.length, true);
    head.setUint32(24, entry.data.length, true);
    head.setUint16(28, entry.name.length, true);
    head.setUint32(42, offset, true);
    central.push(new Uint8Array(head.buffer), entry.name);

    offset += 30 + entry.name.length + entry.data.length;
  }

  const centralSize = central.reduce((total, part) => total + part.length, 0);
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true);
  end.setUint16(8, entries.length, true);
  end.setUint16(10, entries.length, true);
  end.setUint32(12, centralSize, true);
  end.setUint32(16, offset, true);
  return new Blob([...chunks, ...central, new Uint8Array(end.buffer)], { type: "application/zip" });
}

async function inflateRaw(bytes) {
  if (typeof DecompressionStream === "undefined") {
    throw new Error("este navegador não descomprime zip — use um navegador recente");
  }
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

/** Lê as entradas do zip: `[{ name, isDir, bytes }]`. */
async function readEntries(buffer) {
  const bytes = new Uint8Array(buffer);
  const view = new DataView(buffer);

  // Fim do diretório central: a assinatura mais perto do fim (o comentário do zip pode ter até 64 KiB).
  let eocd = -1;
  for (let i = bytes.length - 22; i >= Math.max(0, bytes.length - 22 - 0xffff); i--) {
    if (view.getUint32(i, true) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new Error("zip inválido: não achei o fim do arquivo (não é um .zip?)");

  const count = view.getUint16(eocd + 10, true);
  let pos = view.getUint32(eocd + 16, true);
  if (count === 0xffff || pos === 0xffffffff) throw new Error("zip inválido: zip64 não é suportado");

  const decoder = new TextDecoder("utf-8", { fatal: false });
  const entries = [];
  for (let i = 0; i < count; i++) {
    if (pos + 46 > bytes.length || view.getUint32(pos, true) !== 0x02014b50) {
      throw new Error(`erro ao ler a entrada ${i} do zip`);
    }
    const flags = view.getUint16(pos + 8, true);
    const method = view.getUint16(pos + 10, true);
    const compressedSize = view.getUint32(pos + 20, true);
    const nameLength = view.getUint16(pos + 28, true);
    const extraLength = view.getUint16(pos + 30, true);
    const commentLength = view.getUint16(pos + 32, true);
    const localOffset = view.getUint32(pos + 42, true);
    const name = decoder.decode(bytes.subarray(pos + 46, pos + 46 + nameLength));
    pos += 46 + nameLength + extraLength + commentLength;

    const isDir = name.endsWith("/");
    if (isDir) {
      entries.push({ name, isDir, bytes: new Uint8Array(0) });
      continue;
    }
    if (flags & 1) throw new Error(`${name}: o zip está protegido por senha`);
    if (localOffset + 30 > bytes.length || view.getUint32(localOffset, true) !== 0x04034b50) {
      throw new Error(`erro ao ler a entrada ${i} do zip`);
    }
    const dataStart = localOffset + 30 + view.getUint16(localOffset + 26, true) + view.getUint16(localOffset + 28, true);
    const raw = bytes.subarray(dataStart, dataStart + compressedSize);
    if (method === 0) entries.push({ name, isDir, bytes: raw });
    else if (method === 8) entries.push({ name, isDir, bytes: await inflateRaw(raw) });
    else throw new Error(`${name}: método de compressão ${method} não suportado`);
  }
  return entries;
}

/**
 * Valida e lê o `.zip` importado: **exatamente** um `*.yml`/`*.yaml` e um
 * `*.toml`, na raiz, e nada mais (as mesmas regras do app desktop).
 * Devolve `{ ok: true, yaml, toml }` ou `{ ok: false, error }`.
 */
export async function readManifestZip(file) {
  let entries;
  try {
    entries = await readEntries(await file.arrayBuffer());
  } catch (error) {
    const message = String(error?.message || error);
    return { ok: false, error: message.startsWith("zip inválido") ? message : `zip inválido: ${message}` };
  }

  const decoder = new TextDecoder("utf-8", { fatal: true });
  let yaml = null;
  let toml = null;
  const extras = [];
  for (const entry of entries) {
    if (entry.isDir || entry.name.includes("/")) {
      extras.push(entry.name);
      continue;
    }
    const lower = entry.name.toLowerCase();
    const isYaml = lower.endsWith(".yml") || lower.endsWith(".yaml");
    const isToml = lower.endsWith(".toml");
    if (!isYaml && !isToml) {
      extras.push(entry.name);
      continue;
    }
    let content;
    try {
      content = decoder.decode(entry.bytes);
    } catch {
      return { ok: false, error: `${entry.name} não é texto UTF-8 válido` };
    }
    if (isYaml) {
      if (yaml !== null) return { ok: false, error: "o zip tem mais de um arquivo .yml" };
      yaml = content;
    } else {
      if (toml !== null) return { ok: false, error: "o zip tem mais de um arquivo .toml" };
      toml = content;
    }
  }
  if (extras.length) return { ok: false, error: `o zip só pode ter um .yml e um .toml — sobrou: ${extras.join(", ")}` };
  if (yaml === null) return { ok: false, error: "o zip não tem um arquivo .yml" };
  if (toml === null) return { ok: false, error: "o zip não tem um arquivo .toml" };
  return { ok: true, yaml, toml };
}
