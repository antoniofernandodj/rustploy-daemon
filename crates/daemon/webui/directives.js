// directives.js — diretivas Alpine próprias da webui.
//
// `x-fallback="lista"` é o equivalente webui do `fallback` do <foreach> (e do
// `foreach_fallback` do <template>) do glacier-ui: o elemento só aparece quando
// a lista não tem item nenhum — array vazio, ou a expressão ainda sem array
// (`undefined`/`null` antes do primeiro fetch), a mesma leitura do `<if empty>`
// da GUI. Vai ao lado do `<template x-for>` que ele cobre, no mesmo container:
//
//   <column class="table_body">
//     <text class="empty_state" x-fallback="queued">Nenhum deploy na fila.</text>
//     <template x-for="q in queued" :key="q.id"> … </template>
//   </column>
//
// Troca o `x-show="lista.length === 0"` escrito à mão: este estourava com
// TypeError quando a lista ainda não era array, e repetia a condição em cada
// tela. Aqui há um só lugar que decide o que é "vazio".
export function registerDirectives(Alpine) {
  Alpine.directive("fallback", (el, { expression }, { evaluateLater, effect }) => {
    const lista = evaluateLater(expression);
    effect(() => {
      lista((v) => {
        el.style.display = Array.isArray(v) && v.length > 0 ? "none" : "";
      });
    });
  });

  // `x-follow`: "tail -f" — mantém a rolagem no fim enquanto chegam linhas, mas
  // só se o usuário já estava no fim (rolou pra cima pra ler = não puxa de volta).
  Alpine.directive("follow", (el, _d, { cleanup }) => {
    let stick = true;
    const atEnd = () => el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    const onScroll = () => {
      stick = atEnd();
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    const mo = new MutationObserver(() => {
      if (stick) el.scrollTop = el.scrollHeight;
    });
    mo.observe(el, { childList: true, subtree: true });
    cleanup(() => {
      el.removeEventListener("scroll", onScroll);
      mo.disconnect();
    });
  });
}
