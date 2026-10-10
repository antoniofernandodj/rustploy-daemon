// wm.js — gerenciador de janelas da webui (diretiva Alpine `x-win`).
//
// A GUI abre "Novo projeto", "Novo serviço", logs etc. em janelas próprias do
// SO. No navegador a mesma ideia vira uma janela flutuante: arrastável pela
// barra de título, redimensionável, que vem à frente ao clicar, maximiza com
// duplo clique, fecha com Esc e lembra onde ficou.
//
// Estrutura esperada (a mesma dos antigos modais, só mudou o comportamento):
//
//   <column class="modal_backdrop" x-show="aberta">      ← "camada": cobre a tela
//     <column class="modal_box" x-win="'chave'" data-w="720" data-h="520">
//       <row class="modal_head"> <text/> <button>Fechar</button> </row>
//       <column class="modal_body"> … </column>
//     </column>                                           mas não captura clique
//   </column>
//
// • A camada (backdrop) é só o contexto de empilhamento: `pointer-events:none`,
//   sem escurecer — a página por baixo continua usável e várias janelas podem
//   estar abertas ao mesmo tempo.
// • O botão "Fechar" do markup é aproveitado: a diretiva o remove da barra e
//   liga o ponto vermelho a ele, então cada janela mantém o seu próprio
//   handler de fechamento (limpeza de estado, parar SSE, etc.).
// • Geometria por `chave` em localStorage ("rustploy.win.<chave>").

const LS_PREFIX = "rustploy.win.";
const MARGIN = 8;
let zTop = 50;

function lsGet(key) {
  try {
    return JSON.parse(localStorage.getItem(LS_PREFIX + key) || "null");
  } catch {
    return null;
  }
}
function lsSet(key, value) {
  try {
    localStorage.setItem(LS_PREFIX + key, JSON.stringify(value));
  } catch {
    /* modo privado / cota: perde só a memória da posição */
  }
}

function bringToFront(layer) {
  layer.style.zIndex = String(++zTop);
}

/** Mantém a janela alcançável: ao menos a barra de título dentro da tela. */
function clamp(box) {
  if (box.classList.contains("win--max")) return;
  const viewportWidth = window.innerWidth;
  const viewportHeight = window.innerHeight;
  const boxRect = box.getBoundingClientRect();
  if (!box.style.left) return;
  const boxWidth = Math.min(boxRect.width, viewportWidth - MARGIN * 2);
  let left = Math.min(Math.max(boxRect.left, MARGIN - boxWidth + 120), viewportWidth - 120);
  let top = Math.min(Math.max(boxRect.top, 0), viewportHeight - 40);
  box.style.left = left + "px";
  box.style.top = top + "px";
}

/** Fixa a janela em coordenadas. Sem posição lembrada, nasce centralizada
 *  (levemente acima do centro) e escalonada: cada janela aberta ao mesmo
 *  tempo desce/avança um degrau para não cobrir a anterior por completo. */
let cascade = 0;
function pin(box) {
  if (box.style.left) return;
  const boxWidth = parseFloat(box.style.width) || box.offsetWidth;
  const boxHeight = parseFloat(box.style.height) || box.offsetHeight;
  const off = (cascade++ % 5) * 26;
  box.style.position = "fixed";
  box.style.left = Math.max(MARGIN, (window.innerWidth - boxWidth) / 2 + off - 52) + "px";
  box.style.top = Math.max(MARGIN, (window.innerHeight - boxHeight) / 2 + off - 52) + "px";
  box.style.margin = "0";
}

export function registerWindows(Alpine) {
  // Esc fecha a janela visível mais à frente.
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || event.defaultPrevented) return;
    let best = null;
    for (const layer of document.querySelectorAll(".modal__backdrop")) {
      if (layer.style.display === "none" || !layer.firstElementChild) continue;
      if (getComputedStyle(layer).display === "none") continue;
      if (!best || Number(layer.style.zIndex || 0) >= Number(best.style.zIndex || 0)) best = layer;
    }
    const close = best && best.querySelector(".win__dot--close");
    if (close) {
      event.preventDefault();
      close.click();
    }
  });

  window.addEventListener("resize", () => {
    document.querySelectorAll(".modal__box").forEach(clamp);
  });

  Alpine.directive("win", (box, { expression }, { evaluate, cleanup }) => {
    const key = expression ? String(evaluate(expression)) : "";
    const layer = box.parentElement;
    const head = box.querySelector(":scope > .modal__head");
    if (!layer || !head) return;

    box.classList.add("win");
    layer.classList.add("win__layer");

    // ── Barra de título: pontos de controle à esquerda ─────────────────
    const closeBtn = [...head.querySelectorAll("button")].find((button) => button.textContent.trim() === "Fechar");
    const controls = document.createElement("div");
    controls.className = "win__ctls";
    const dClose = document.createElement("button");
    dClose.type = "button";
    dClose.className = "win__dot win__dot--close";
    dClose.title = "Fechar (Esc)";
    dClose.setAttribute("aria-label", "Fechar janela");
    const dMax = document.createElement("button");
    dMax.type = "button";
    dMax.className = "win__dot win__dot--max";
    dMax.title = "Maximizar / restaurar";
    dMax.setAttribute("aria-label", "Maximizar janela");
    controls.append(dClose, dMax);
    head.prepend(controls);
    // O "Fechar" do markup continua visível, agora como ✕ à direita da barra
    // (o ponto vermelho de 12px sozinho passava despercebido).
    if (closeBtn) {
      closeBtn.textContent = "✕";
      closeBtn.title = "Fechar (Esc)";
      closeBtn.setAttribute("aria-label", "Fechar janela");
      closeBtn.classList.add("win__x");
    }
    dClose.addEventListener("click", () => {
      if (closeBtn) closeBtn.click();
      else layer.dispatchEvent(new CustomEvent("win-close", { bubbles: true }));
    });

    // ── Tamanho / posição iniciais ─────────────────────────────────────
    const defaultWidth = Number(box.dataset.w) || 900;
    const defaultHeight = Number(box.dataset.h) || 640;
    const saved = key ? lsGet(key) : null;
    const viewportWidth = () => window.innerWidth;
    const viewportHeight = () => window.innerHeight;
    box.style.width = Math.min(saved?.w || defaultWidth, viewportWidth() - MARGIN * 2) + "px";
    box.style.height = Math.min(saved?.h || defaultHeight, viewportHeight() - MARGIN * 2) + "px";
    if (saved && Number.isFinite(saved.x)) {
      box.style.position = "fixed";
      box.style.left = saved.x + "px";
      box.style.top = saved.y + "px";
      box.style.margin = "0";
    }

    const persist = () => {
      if (!key || !box.style.left || box.classList.contains("win--max")) return;
      const boxRect = box.getBoundingClientRect();
      if (boxRect.width < 50) return; // oculta
      lsSet(key, { x: boxRect.left, y: boxRect.top, w: boxRect.width, h: boxRect.height });
    };

    // ── Aparecer: traz à frente, ajusta e foca o 1º campo ──────────────
    let wasShown = false;
    const mutationObserver = new MutationObserver(() => {
      const shown = layer.style.display !== "none";
      if (shown && !wasShown) {
        bringToFront(layer);
        pin(box);
        requestAnimationFrame(() => {
          clamp(box);
          const firstField = box.querySelector(".modal__body input:not([type=checkbox]):not([type=hidden]), .modal__body textarea");
          if (firstField && !box.contains(document.activeElement)) firstField.focus({ preventScroll: true });
        });
      }
      wasShown = shown;
    });
    mutationObserver.observe(layer, { attributes: true, attributeFilter: ["style"] });
    wasShown = layer.style.display !== "none";
    if (wasShown) bringToFront(layer);

    // ── Foco ao clicar em qualquer ponto da janela ─────────────────────
    box.addEventListener("pointerdown", () => bringToFront(layer), true);

    // ── Arrastar pela barra ────────────────────────────────────────────
    head.addEventListener("pointerdown", (event) => {
      if (event.button !== 0 || event.target.closest("button, input, select, textarea, a")) return;
      if (box.classList.contains("win--max") || window.matchMedia("(max-width: 700px)").matches) return;
      pin(box);
      const grabOffsetX = event.clientX - box.offsetLeft;
      const grabOffsetY = event.clientY - box.offsetTop;
      head.setPointerCapture(event.pointerId);
      box.classList.add("win--dragging");
      const move = (moveEvent) => {
        box.style.left = moveEvent.clientX - grabOffsetX + "px";
        box.style.top = moveEvent.clientY - grabOffsetY + "px";
      };
      const up = () => {
        head.removeEventListener("pointermove", move);
        head.removeEventListener("pointerup", up);
        head.removeEventListener("pointercancel", up);
        box.classList.remove("win--dragging");
        clamp(box);
        persist();
      };
      head.addEventListener("pointermove", move);
      head.addEventListener("pointerup", up);
      head.addEventListener("pointercancel", up);
    });

    // ── Maximizar / restaurar ──────────────────────────────────────────
    const toggleMax = () => {
      if (box.classList.contains("win--max")) {
        box.classList.remove("win--max");
        clamp(box);
      } else {
        pin(box);
        box.classList.add("win--max");
      }
    };
    dMax.addEventListener("click", toggleMax);
    head.addEventListener("dblclick", (event) => {
      if (!event.target.closest("button, input, select, textarea, a")) toggleMax();
    });

    // ── Redimensionar (alça nativa) → persiste ao parar ────────────────
    let persistTimer = 0;
    const resizeObserver = new ResizeObserver(() => {
      clearTimeout(persistTimer);
      persistTimer = setTimeout(persist, 250);
    });
    resizeObserver.observe(box);

    cleanup(() => {
      mutationObserver.disconnect();
      resizeObserver.disconnect();
    });
  });
}
