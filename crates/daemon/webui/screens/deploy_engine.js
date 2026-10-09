// screens/deploy_engine.js — tela "Deploy Engine": fila global (um deploy por
// vez), execução em andamento e histórico das últimas 24h. Porta da seção
// `equals="deploy_engine"` de home.gv — tudo vem de `snap.engine`
// (DeployEngineSummary), já anexado ao Snapshot pelo daemon (sem RPC extra).
import { fmtUptime, engActiveRows, engQueuedRows, engRecentRows } from "../fmt.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("deployEngine", () => ({
    get store() {
      return Alpine.store("app");
    },

    // Aba ativa (mesmas 3 da GUI: Fila / Executando / Histórico 24h).
    tab: "fila",

    // Reordenar a fila arrastando (DeployQueueReorder, como o drag-and-drop da
    // GUI): solta a linha A sobre a B e A passa a ficar antes de B.
    dragId: null,
    overId: null,
    dragStart(ev, id) {
      this.dragId = id;
      ev.dataTransfer.effectAllowed = "move";
      ev.dataTransfer.setData("text/plain", id);
    },
    dragEnd() {
      this.dragId = null;
      this.overId = null;
    },
    async dropOn(targetId) {
      const from = this.dragId;
      this.dragEnd();
      if (!from || from === targetId) return;
      const order = this.queued.map((q) => q.deploymentId).filter((x) => x !== from);
      order.splice(order.indexOf(targetId), 0, from);
      await this.store.queueReorder(order);
    },

    get engine() {
      return this.store.snap?.engine || null;
    },

    get active() {
      return engActiveRows(this.engine?.active);
    },
    // Modal de detalhes: guarda só o serviceId; a linha é relida de `active`
    // a cada snapshot, então o histórico e os passos andam ao vivo. Se o
    // deploy terminar e sair de "Executando agora", o modal fecha.
    detailId: null,
    get detail() {
      return this.detailId ? this.active.find((a) => a.serviceId === this.detailId) || null : null;
    },
    openDetail(id) {
      this.detailId = id;
    },
    closeDetail() {
      this.detailId = null;
    },
    get queued() {
      return engQueuedRows(this.engine?.queued);
    },
    get recent() {
      return engRecentRows(this.engine?.recent);
    },
    get paused() {
      return !!this.engine?.paused;
    },
    get uptime() {
      return this.engine ? fmtUptime(this.engine.uptime_secs) : "…";
    },
    get successCount() {
      return this.engine?.successful_24h ?? 0;
    },
    get failedCount() {
      return this.engine?.failed_24h ?? 0;
    },
    get totalCount() {
      return this.engine?.total_24h ?? 0;
    },
  }));
});
