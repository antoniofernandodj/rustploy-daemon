// screens/dashboard.js — tela "Deployments" (view padrão do shell). Porta de
// format.deployments (crates/rustploy-gui/views/scripts/format/dashboard.luau) +
// da seção `equals="deployments"` de shell.gv.
import { dateDayMonthHourMinuteSecond, formatDuration, stateLabelKind } from "../format.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("dashboard", () => ({
    get store() {
      return Alpine.store("app");
    },

    // Ordenação por coluna, como o `tableview(:sort = deployments_sort)` da GUI:
    // clicar no cabeçalho ordena; clicar de novo inverte; Duração não ordena.
    sortKey: "",
    sortAscending: true,
    sortBy(key) {
      if (this.sortKey === key) this.sortAscending = !this.sortAscending;
      else {
        this.sortKey = key;
        this.sortAscending = true;
      }
    },
    sortMark(key) {
      return this.sortKey === key ? (this.sortAscending ? " ▲" : " ▼") : "";
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
            startIso: deployment.started_at || "",
          };
        })
        .sort((left, right) => {
          if (!this.sortKey) return 0;
          const field = this.sortKey === "start" ? "startIso" : this.sortKey;
          const order = String(left[field]).localeCompare(String(right[field]), undefined, { numeric: true });
          return this.sortAscending ? order : -order;
        });
    },
  }));
});
