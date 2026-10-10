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
function lsSet(key, v) {
  try {
    localStorage.setItem(LS_PREFIX + key, JSON.stringify(v));
  } catch {
    /* modo privado / cota: perde só a memória da posição */
  }
}

function bringToFront(layer) {
  layer.style.zIndex = String(++zTop);
}

/** Mantém a janela alcançável: ao menos a barra de título dentro da tela. */
function clamp(box) {
  if (box.classList.contains("win_max")) return;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const r = box.getBoundingClientRect();
  if (!box.style.left) return;
  const w = Math.min(r.width, vw - MARGIN * 2);
  let x = Math.min(Math.max(r.left, MARGIN - w + 120), vw - 120);
  let y = Math.min(Math.max(r.top, 0), vh - 40);
  box.style.left = x + "px";
  box.style.top = y + "px";
}

/** Fixa a janela em coordenadas. Sem posição lembrada, nasce centralizada
 *  (levemente acima do centro) e escalonada: cada janela aberta ao mesmo
 *  tempo desce/avança um degrau para não cobrir a anterior por completo. */
let cascade = 0;
function pin(box) {
  if (box.style.left) return;
  const w = parseFloat(box.style.width) || box.offsetWidth;
  const h = parseFloat(box.style.height) || box.offsetHeight;
  const off = (cascade++ % 5) * 26;
  box.style.position = "fixed";
  box.style.left = Math.max(MARGIN, (window.innerWidth - w) / 2 + off - 52) + "px";
  box.style.top = Math.max(MARGIN, (window.innerHeight - h) / 2 + off - 52) + "px";
  box.style.margin = "0";
}

export function registerWindows(Alpine) {
  // Esc fecha a janela visível mais à frente.
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || e.defaultPrevented) return;
    let best = null;
    for (const layer of document.querySelectorAll(".modal_backdrop")) {
      if (layer.style.display === "none" || !layer.firstElementChild) continue;
      if (getComputedStyle(layer).display === "none") continue;
      if (!best || Number(layer.style.zIndex || 0) >= Number(best.style.zIndex || 0)) best = layer;
    }
    const close = best && best.querySelector(".win_close");
    if (close) {
      e.preventDefault();
      close.click();
    }
  });

  window.addEventListener("resize", () => {
    document.querySelectorAll(".modal_box").forEach(clamp);
  });

  Alpine.directive("win", (box, { expression }, { evaluate, cleanup }) => {
    const key = expression ? String(evaluate(expression)) : "";
    const layer = box.parentElement;
    const head = box.querySelector(":scope > .modal_head");
    if (!layer || !head) return;

    box.classList.add("win");
    layer.classList.add("win_layer");

    // ── Barra de título: pontos de controle à esquerda ─────────────────
    const closeBtn = [...head.querySelectorAll("button")].find((b) => b.textContent.trim() === "Fechar");
    const ctl = document.createElement("div");
    ctl.className = "win_ctls";
    const dClose = document.createElement("button");
    dClose.type = "button";
    dClose.className = "win_dot win_close";
    dClose.title = "Fechar (Esc)";
    dClose.setAttribute("aria-label", "Fechar janela");
    const dMax = document.createElement("button");
    dMax.type = "button";
    dMax.className = "win_dot win_maxbtn";
    dMax.title = "Maximizar / restaurar";
    dMax.setAttribute("aria-label", "Maximizar janela");
    ctl.append(dClose, dMax);
    head.prepend(ctl);
    // O "Fechar" do markup continua visível, agora como ✕ à direita da barra
    // (o ponto vermelho de 12px sozinho passava despercebido).
    if (closeBtn) {
      closeBtn.textContent = "✕";
      closeBtn.title = "Fechar (Esc)";
      closeBtn.setAttribute("aria-label", "Fechar janela");
      closeBtn.classList.add("win_x");
    }
    dClose.addEventListener("click", () => {
      if (closeBtn) closeBtn.click();
      else layer.dispatchEvent(new CustomEvent("win-close", { bubbles: true }));
    });

    // ── Tamanho / posição iniciais ─────────────────────────────────────
    const dw = Number(box.dataset.w) || 900;
    const dh = Number(box.dataset.h) || 640;
    const saved = key ? lsGet(key) : null;
    const vw = () => window.innerWidth;
    const vh = () => window.innerHeight;
    box.style.width = Math.min(saved?.w || dw, vw() - MARGIN * 2) + "px";
    box.style.height = Math.min(saved?.h || dh, vh() - MARGIN * 2) + "px";
    if (saved && Number.isFinite(saved.x)) {
      box.style.position = "fixed";
      box.style.left = saved.x + "px";
      box.style.top = saved.y + "px";
      box.style.margin = "0";
    }

    const persist = () => {
      if (!key || !box.style.left || box.classList.contains("win_max")) return;
      const r = box.getBoundingClientRect();
      if (r.width < 50) return; // oculta
      lsSet(key, { x: r.left, y: r.top, w: r.width, h: r.height });
    };

    // ── Aparecer: traz à frente, ajusta e foca o 1º campo ──────────────
    let wasShown = false;
    const mo = new MutationObserver(() => {
      const shown = layer.style.display !== "none";
      if (shown && !wasShown) {
        bringToFront(layer);
        pin(box);
        requestAnimationFrame(() => {
          clamp(box);
          const f = box.querySelector(".modal_body input:not([type=checkbox]):not([type=hidden]), .modal_body textarea");
          if (f && !box.contains(document.activeElement)) f.focus({ preventScroll: true });
        });
      }
      wasShown = shown;
    });
    mo.observe(layer, { attributes: true, attributeFilter: ["style"] });
    wasShown = layer.style.display !== "none";
    if (wasShown) bringToFront(layer);

    // ── Foco ao clicar em qualquer ponto da janela ─────────────────────
    box.addEventListener("pointerdown", () => bringToFront(layer), true);

    // ── Arrastar pela barra ────────────────────────────────────────────
    head.addEventListener("pointerdown", (e) => {
      if (e.button !== 0 || e.target.closest("button, input, select, textarea, a")) return;
      if (box.classList.contains("win_max") || window.matchMedia("(max-width: 700px)").matches) return;
      pin(box);
      const sx = e.clientX - box.offsetLeft;
      const sy = e.clientY - box.offsetTop;
      head.setPointerCapture(e.pointerId);
      box.classList.add("win_dragging");
      const move = (ev) => {
        box.style.left = ev.clientX - sx + "px";
        box.style.top = ev.clientY - sy + "px";
      };
      const up = () => {
        head.removeEventListener("pointermove", move);
        head.removeEventListener("pointerup", up);
        head.removeEventListener("pointercancel", up);
        box.classList.remove("win_dragging");
        clamp(box);
        persist();
      };
      head.addEventListener("pointermove", move);
      head.addEventListener("pointerup", up);
      head.addEventListener("pointercancel", up);
    });

    // ── Maximizar / restaurar ──────────────────────────────────────────
    const toggleMax = () => {
      if (box.classList.contains("win_max")) {
        box.classList.remove("win_max");
        clamp(box);
      } else {
        pin(box);
        box.classList.add("win_max");
      }
    };
    dMax.addEventListener("click", toggleMax);
    head.addEventListener("dblclick", (e) => {
      if (!e.target.closest("button, input, select, textarea, a")) toggleMax();
    });

    // ── Redimensionar (alça nativa) → persiste ao parar ────────────────
    let t = 0;
    const ro = new ResizeObserver(() => {
      clearTimeout(t);
      t = setTimeout(persist, 250);
    });
    ro.observe(box);

    cleanup(() => {
      mo.disconnect();
      ro.disconnect();
    });
  });
}
