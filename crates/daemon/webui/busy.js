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
// O botão libera no PRIMEIRO toast de resposta da ação (toast/toastOk/toastError/
// toastWarn/toastResult), não só quando o método inteiro termina: depois do toast
// os métodos ainda costumam reler serviço/snapshot (mais rpcs) e esperar isso
// deixaria o botão travado segundos além da resposta. Toasts disparados por
// eventos do stream (SSE) não contam — não são resposta a clique nenhum.
// Métodos síncronos passam intactos (não piscam) e devolvem o botão a `current`
// para um `@click="a(); b()"` em que só o `b` é assíncrono.

const SAFETY_MS = 15 * 60 * 1000; // rede de segurança: nunca prende um botão para sempre
const SKIP = new Set(["init", "destroy"]);
const TOASTS = new Set(["toast", "toastOk", "toastError", "toastWarn", "toastResult"]);
const STREAM = new Set(["onStreamEvent", "applyBusEvent", "applySnapshot"]);

const held = new Set(); // release() de cada botão ocupado
let fromStream = 0; // >0 enquanto roda código de evento SSE (síncrono)

let current = null;

const isBusy = (element) => element && element.hasAttribute("data-busy");

function arm(button) {
  current = button || null;
  if (current) setTimeout(() => current === button && (current = null), 0);
}

function lock(button) {
  if (!button || isBusy(button)) return () => {};
  // Botão com `:disabled` do Alpine: não mexe na propriedade (o binding manda).
  const own = !button.disabled && !button.hasAttribute(":disabled") && !button.hasAttribute("x-bind:disabled");
  button.setAttribute("data-busy", "");
  button.setAttribute("aria-busy", "true");
  if (own) button.disabled = true;
  let done = false;
  const release = () => {
    if (done) return;
    done = true;
    held.delete(release);
    clearTimeout(safety);
    button.removeAttribute("data-busy");
    button.removeAttribute("aria-busy");
    if (own) button.disabled = false;
  };
  const safety = setTimeout(release, SAFETY_MS);
  held.add(release);
  return release;
}

function wrap(originalFunction) {
  return function (...args) {
    const button = current;
    current = null; // chamadas aninhadas não disputam o mesmo botão
    let out;
    try {
      out = originalFunction.apply(this, args);
    } catch (error) {
      current = button;
      throw error;
    }
    if (button && out && typeof out.then === "function") {
      const release = lock(button);
      out.then(release, release);
    } else {
      current = button;
    }
    return out;
  };
}

/** Toast de resposta: libera os botões ocupados (fora de eventos do stream). */
function wrapToast(originalFunction) {
  return function (...args) {
    const out = originalFunction.apply(this, args);
    if (!fromStream) [...held].forEach((release) => release());
    return out;
  };
}

/** Evento SSE: os toasts de dentro não são resposta a um clique. */
function wrapStream(originalFunction) {
  return function (...args) {
    fromStream++;
    try {
      return originalFunction.apply(this, args);
    } finally {
      fromStream--;
    }
  };
}

function track(obj) {
  if (!obj || typeof obj !== "object") return obj;
  for (const key of Object.keys(obj)) {
    if (SKIP.has(key)) continue;
    const descriptor = Object.getOwnPropertyDescriptor(obj, key);
    if (descriptor && typeof descriptor.value === "function") {
      const wrappedFunction = TOASTS.has(key) ? wrapToast(descriptor.value) : STREAM.has(key) ? wrapStream(descriptor.value) : wrap(descriptor.value);
      Object.defineProperty(obj, key, { ...descriptor, value: wrappedFunction });
    }
  }
  return obj;
}

export function registerBusy(Alpine) {
  const guard = (event, button) => {
    if (isBusy(button)) {
      event.preventDefault();
      event.stopImmediatePropagation();
      return;
    }
    arm(button);
  };
  document.addEventListener("click", (event) => guard(event, event.target.closest?.("button")), true);
  document.addEventListener("submit", (event) => guard(event, event.submitter), true);

  const store = Alpine.store.bind(Alpine);
  Alpine.store = (...storeArguments) => (storeArguments.length < 2 ? store(...storeArguments) : store(storeArguments[0], track(storeArguments[1])));

  const data = Alpine.data.bind(Alpine);
  Alpine.data = (name, factory) =>
    data(name, function (...dataArguments) {
      return track(factory.apply(this, dataArguments));
    });
}
