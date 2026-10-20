// screens/running.js — tela "Em execução": só os projetos com serviço rodando e,
// dentro de cada um, só os serviços rodando. O recorte e a classificação por tipo
// (app Git/zip/imagem, banco, broker, compose) vêm prontos do daemon em
// `snapshot.dashboard` (api/http/dashboard.rs) — aqui só se desenha.
import { formatBytes, serviceWarmLabel } from "../format.js";

/** `https://host/webhook/<id>/<token>` → `https://host/webhook/<id>/••••••••`. */
export function maskWebhook(url) {
  return String(url || "").replace(/\/[^/]*$/, "/••••••••");
}

document.addEventListener("alpine:init", () => {
  Alpine.data("running", () => ({
    get store() {
      return Alpine.store("app");
    },
    /** service_id → true quando o usuário pediu para ver o token do webhook. */
    revealed: {},

    /** Projetos do painel, filtrados pelo termo da busca da topbar. */
    get projects() {
      const store = this.store;
      const term = (store.search || "").toLowerCase();
      const metricsById = store.metricsById || {};
      const list = (store.snap && store.snap.dashboard) || [];
      return list
        .map((project) => {
          const projectMatches = project.project_name.toLowerCase().includes(term);
          const services = project.services
            .filter((svc) => !term || projectMatches || svc.name.toLowerCase().includes(term))
            .map((svc) => {
              const metricsPoint = metricsById[svc.service_id];
              const [warmLabel, warmKind] = serviceWarmLabel(svc.warm);
              return {
                ...svc,
                isDegraded: svc.status === "Degraded",
                cpu: metricsPoint ? `${(metricsPoint.cpu_percent || 0).toFixed(1)}%` : "—",
                mem: metricsPoint ? formatBytes(metricsPoint.mem_used_bytes) : "—",
                hasWarm: !!svc.warm,
                warmLabel,
                warmKind,
                warmReason: svc.warm?.reason || "",
                webhookShown: svc.webhook ? (this.revealed[svc.service_id] ? svc.webhook : maskWebhook(svc.webhook)) : "",
                isRevealed: !!this.revealed[svc.service_id],
                hasChecks: svc.pre_deploy_checks.length > 0,
                domainsText: svc.domains.join(", "),
              };
            });
          return { ...project, services, count: services.length };
        })
        .filter((project) => project.services.length > 0);
    },

    get totalServices() {
      return this.projects.reduce((total, project) => total + project.count, 0);
    },

    toggleReveal(serviceId) {
      this.revealed = { ...this.revealed, [serviceId]: !this.revealed[serviceId] };
    },
  }));
});
