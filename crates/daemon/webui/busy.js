// busy.js — feedback imediato e trava de clique para toda ação assíncrona.
//
// Regra única: o botão que disparou uma ação que fala com o daemon fica
// bloqueado (`data-busy`, opaco, cursor de espera) desde o clique até a
// promessa da ação assentar — resposta, erro na resposta ou timeout (o
// `Api.rpc` tem teto, ver net/api.js). Nenhum botão precisa saber disso:
//   1. um listener de captura em `click`/`submit` guarda o botão do evento em
//      `current` (e engole o evento se o botão já está ocupado);
//   2. os métodos do `Alpine.store`/`Alpine.data` são embrulhados: se o método
//      devolve uma Promise, o botão guardado fica ocupado até ela assentar.
// Métodos síncronos passam intactos (não piscam) e devolvem o botão a `current`
// para um `@click="a(); b()"` em que só o `b` é assíncrono.

const SAFETY_MS = 15 * 60 * 1000; // rede de segurança: nunca prende um botão para sempre
const SKIP = new Set(["init", "destroy"]);

let current = null;

const isBusy = (el) => el && el.hasAttribute("data-busy");

function arm(btn) {
  current = btn || null;
  if (current) setTimeout(() => current === btn && (current = null), 0);
}

function lock(btn) {
  if (!btn || isBusy(btn)) return () => {};
  // Botão com `:disabled` do Alpine: não mexe na propriedade (o binding manda).
  const own = !btn.disabled && !btn.hasAttribute(":disabled") && !btn.hasAttribute("x-bind:disabled");
  btn.setAttribute("data-busy", "");
  btn.setAttribute("aria-busy", "true");
  if (own) btn.disabled = true;
  let done = false;
  const release = () => {
    if (done) return;
    done = true;
    clearTimeout(safety);
    btn.removeAttribute("data-busy");
    btn.removeAttribute("aria-busy");
    if (own) btn.disabled = false;
  };
  const safety = setTimeout(release, SAFETY_MS);
  return release;
}

function wrap(fn) {
  return function (...args) {
    const btn = current;
    current = null; // chamadas aninhadas não disputam o mesmo botão
    let out;
    try {
      out = fn.apply(this, args);
    } catch (e) {
      current = btn;
      throw e;
    }
    if (btn && out && typeof out.then === "function") {
      const release = lock(btn);
      out.then(release, release);
    } else {
      current = btn;
    }
    return out;
  };
}

function track(obj) {
  if (!obj || typeof obj !== "object") return obj;
  for (const key of Object.keys(obj)) {
    if (SKIP.has(key)) continue;
    const d = Object.getOwnPropertyDescriptor(obj, key);
    if (d && typeof d.value === "function") {
      Object.defineProperty(obj, key, { ...d, value: wrap(d.value) });
    }
  }
  return obj;
}

export function registerBusy(Alpine) {
  const guard = (e, btn) => {
    if (isBusy(btn)) {
      e.preventDefault();
      e.stopImmediatePropagation();
      return;
    }
    arm(btn);
  };
  document.addEventListener("click", (e) => guard(e, e.target.closest?.("button")), true);
  document.addEventListener("submit", (e) => guard(e, e.submitter), true);

  const store = Alpine.store.bind(Alpine);
  Alpine.store = (...a) => (a.length < 2 ? store(...a) : store(a[0], track(a[1])));

  const data = Alpine.data.bind(Alpine);
  Alpine.data = (name, factory) =>
    data(name, function (...a) {
      return track(factory.apply(this, a));
    });
}
