// directives.js — diretivas Alpine próprias da webui.
//
// `x-fallback="lista"` é o equivalente webui do `fallback` do <foreach> (e do
// `foreach_fallback` do <template>) do glacier-ui: o elemento só aparece quando
// a lista não tem item nenhum — array vazio, ou a expressão ainda sem array
// (`undefined`/`null` antes do primeiro fetch), a mesma leitura do `<if empty>`
// da GUI. Vai ao lado do `<template x-for>` que ele cobre, no mesmo container:
//
//   <column class="table__body">
//     <text class="empty-state" x-fallback="queued">Nenhum deploy na fila.</text>
//     <template x-for="q in queued" :key="q.id"> … </template>
//   </column>
//
// Troca o `x-show="lista.length === 0"` escrito à mão: este estourava com
// TypeError quando a lista ainda não era array, e repetia a condição em cada
// tela. Aqui há um só lugar que decide o que é "vazio".
export function registerDirectives(Alpine) {
  Alpine.directive("fallback", (element, { expression }, { evaluateLater, effect }) => {
    const lista = evaluateLater(expression);
    effect(() => {
      lista((items) => {
        element.style.display = Array.isArray(items) && items.length > 0 ? "none" : "";
      });
    });
  });

  // `x-follow`: "tail -f" — mantém a rolagem no fim enquanto chegam linhas, mas
  // só se o usuário já estava no fim (rolou pra cima pra ler = não puxa de volta).
  Alpine.directive("follow", (element, _directive, { cleanup }) => {
    let stick = true;
    const atEnd = () => element.scrollHeight - element.scrollTop - element.clientHeight < 24;
    const onScroll = () => {
      stick = atEnd();
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    const mutationObserver = new MutationObserver(() => {
      if (stick) element.scrollTop = element.scrollHeight;
    });
    mutationObserver.observe(element, { childList: true, subtree: true });
    cleanup(() => {
      element.removeEventListener("scroll", onScroll);
      mutationObserver.disconnect();
    });
  });
}
