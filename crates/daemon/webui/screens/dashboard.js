// screens/dashboard.js — tela "Deployments" (view padrão do shell). Porta de
// format.deployments (crates/rustploy-gui/views/scripts/format/dashboard.luau) +
// da seção `equals="deployments"` de shell.gv.
import { dateDayMonthHourMinuteSecond, formatDuration, stateLabelKind } from "../format.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("dashboard", () => ({
    get store() {
      return Alpine.store("app");
    },

    /** Linhas formatadas da tabela, filtradas pelo termo de busca da topbar. */
    get rows() {
      const store = this.store;
      const term = (store.search || "").toLowerCase();
      const list = (store.snap && store.snap.deployments) || [];
      // O resumo traz só o id do serviço; o do projeto sai da lista de serviços.
      const projectByService = new Map(
        ((store.snap && store.snap.services) || []).map((entry) => [entry.service.id, entry.service.spec.project_id]),
      );
      return list
        .filter((entry) => {
          if (!term) return true;
          return (
            entry.service_name.toLowerCase().includes(term) ||
            entry.project_name.toLowerCase().includes(term)
          );
        })
        .map((entry) => {
          const deployment = entry.deployment;
          const [label, kind] = stateLabelKind(deployment.state);
          return {
            service: entry.service_name,
            serviceId: deployment.service_id,
            project: entry.project_name,
            projectId: projectByService.get(deployment.service_id) || "",
            stateLabel: label,
            stateKind: kind,
            duration: formatDuration(deployment),
            start: dateDayMonthHourMinuteSecond(deployment.started_at),
          };
        });
    },
  }));
});
