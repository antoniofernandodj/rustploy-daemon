// i18n.js — idioma da webui: inglês por padrão, português (pt-BR) a um clique, escolha lembrada.
//
// O catálogo é por TEXTO DE ORIGEM (estilo gettext): o próprio texto escrito no HTML ou no JS é a
// chave (`locales/en.js`, `locales/pt-BR.js`), e um texto sem entrada aparece como foi escrito —
// nunca some. Três formas de usar:
//
//   <text x-t>Nenhum deploy na fila.</text>            texto fixo: a diretiva troca o conteúdo
//   <input placeholder="filtrar…" x-t:placeholder />   atributo fixo: `x-t:placeholder`, `x-t:title`…
//   x-text="$t('Fila (') + n + ')'"                    dentro de uma expressão: `$t(texto)`
//
// No JS: `t("texto")` e, com valores, o template etiquetado `` t`${nome} copiado` `` — a chave é
// `{0} copiado` (cada `${}` vira `{n}`, na ordem), e a tradução pode reordenar os `{n}`.
//
// A locale ativa é `Alpine.store("i18n").locale`: toda expressão que chama `$t`/`t` ou usa `x-t`
// a lê, então trocar de idioma reescreve a tela na hora, sem recarregar. Textos que o JS já
// gravou em estado (uma mensagem de toast, um status) ficam no idioma em que nasceram.
import english from "./locales/en.js";
import portuguese from "./locales/pt-BR.js";

const CATALOGS = { en: english, "pt-BR": portuguese };
const STORAGE_KEY = "rustploy.language";
export const LANGUAGES = [
  { code: "en", label: "English" },
  { code: "pt-BR", label: "Português" },
];

function storedLanguage() {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return stored && CATALOGS[stored] ? stored : "en";
  } catch (_) {
    return "en";
  }
}

// O store reativo do Alpine (preenchido em `registerI18n`); até lá vale o idioma lido do disco.
let store = null;
let fallbackLanguage = storedLanguage();

export function currentLanguage() {
  return store ? store.locale : fallbackLanguage;
}

function lookup(source) {
  const catalog = CATALOGS[currentLanguage()];
  const translated = catalog ? catalog[source] : undefined;
  return translated === undefined ? source : translated;
}

function fill(text, values) {
  return values.length === 0 ? text : text.replace(/\{(\d+)\}/g, (placeholder, index) => (index in values ? values[index] : placeholder));
}

// `t("texto")` ou o template etiquetado `` t`texto ${valor}` ``.
export function t(first, ...values) {
  if (Array.isArray(first)) {
    const key = first.reduce((accumulated, part, index) => accumulated + (index === 0 ? "" : `{${index - 1}}`) + part, "");
    return fill(lookup(key), values);
  }
  return lookup(first);
}

function normalize(text) {
  return text.replace(/\s+/g, " ").trim();
}

export function registerI18n(Alpine) {
  Alpine.store("i18n", {
    locale: fallbackLanguage,
    languages: LANGUAGES,
    set(code) {
      if (!CATALOGS[code]) return;
      this.locale = code;
      document.documentElement.lang = code;
      try {
        localStorage.setItem(STORAGE_KEY, code);
      } catch (_) {
        // sem localStorage (modo privado): vale só nesta sessão
      }
    },
  });
  store = Alpine.store("i18n");
  document.documentElement.lang = store.locale;

  Alpine.magic("t", () => t);

  // `x-t` traduz o conteúdo do elemento; `x-t:atributo` traduz esse atributo. O original
  // (já normalizado) é guardado na primeira passada e vira a chave de todas as seguintes.
  Alpine.directive("t", (element, { value: attribute }, { effect }) => {
    if (attribute) {
      const source = normalize(element.getAttribute(attribute) ?? "");
      effect(() => element.setAttribute(attribute, lookup(source)));
      return;
    }
    const original = element.textContent;
    const leading = original.match(/^\s*/)[0];
    const trailing = original.match(/\s*$/)[0];
    const source = normalize(original);
    effect(() => {
      element.textContent = leading + lookup(source) + trailing;
    });
  });
}
