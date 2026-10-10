// screens/project_detail.js — projeto aberto (view=project_services no
// client iced): sub-abas Serviços/Variáveis/Secrets/Jobs. Porta de
// format.service_rows, o cabeçalho editável e as sub-abas env/secrets/jobs de
// shell.gv + handlers/projects.luau (aba "Jobs" filtra `snap.jobs` pelo
// projeto aberto — mesma lógica de stream.luau::update_open_project). O
// wizard "novo job" acionado pelo botão desta aba é o mesmo modal global do
// store (app.js), compartilhado com a tela "Schedules".
import {
  serviceStatusLabelKind,
  dotenvFromVars,
  parseDotenv,
  envRowsWithComments,
  jobSummaryRows,
  formatBytes,
} from "../format.js";

/** Container "primário" de um serviço pra exibir no card: o live, senão o
 * primeiro em execução, senão o primeiro da lista. `extra` é "+N" quando há
 * mais de um container. Porta de format/dashboard.luau::primary_container. */
function primaryContainer(service) {
  const list = service.containers || [];
  if (list.length === 0) return { name: "—", id: "", extra: "" };
  let chosen = list[0];
  for (const item of list) {
    if (service.live_container_id && item.id === service.live_container_id) {
      chosen = item;
      break;
    }
    if (item.state === "running" && chosen.state !== "running") chosen = item;
  }
  const extra = list.length > 1 ? `+${list.length - 1}` : "";
  return { name: chosen.name || "—", id: (chosen.id || "").slice(0, 12), extra };
}

document.addEventListener("alpine:init", () => {
  Alpine.data("projectDetail", () => ({
    get store() {
      return Alpine.store("app");
    },
    editing: false,
    editName: "",
    editDescription: "",
    projectTab: "services", // "services" | "env" | "secrets" | "jobs"

    get project() {
      const store = this.store;
      return ((store.snap && store.snap.projects) || []).find((project) => project.id === store.selectedProjectId) || null;
    },

    get services() {
      const store = this.store;
      const projectId = store.selectedProjectId;
      const services = (store.snap && store.snap.services) || [];
      const metricsById = store.metricsById || {};
      return services
        .filter((entry) => entry.service.spec.project_id === projectId)
        .map((entry) => {
          const service = entry.service;
          const [label, kind] = serviceStatusLabelKind(service.status);
          const metricsPoint = metricsById[service.id];
          const container = primaryContainer(service);
          return {
            id: service.id,
            name: service.spec.name,
            port: service.spec.port,
            statusLabel: label,
            statusKind: kind,
            cpu: metricsPoint ? `${(metricsPoint.cpu_percent || 0).toFixed(1)}%` : "—",
            mem: metricsPoint ? formatBytes(metricsPoint.mem_used_bytes) : "—",
            containerName: container.name,
            containerId: container.id,
            containerExtra: container.extra,
          };
        });
    },

    get canDelete() {
      return this.services.length === 0;
    },

    // ── Jobs do projeto ──────────────────────────────────────────────
    get jobs() {
      const projectId = this.store.selectedProjectId;
      const list = (this.store.snap?.jobs || []).filter((job) => job.job.project_id === projectId);
      return jobSummaryRows(list, "", this.store.jobsInflight);
    },

    startEdit() {
      const project = this.project;
      if (!project) return;
      this.editName = project.name;
      this.editDescription = project.description || "";
      this.editing = true;
    },
    cancelEdit() {
      this.editing = false;
    },
    async saveEdit() {
      const updateProjectResult = await this.store.updateProject(this.store.selectedProjectId, this.editName, this.editDescription);
      if (updateProjectResult.ok) this.editing = false;
    },

    // ── Variáveis do projeto ─────────────────────────────────────────
    newEnvKey: "",
    newEnvValue: "",
    envTextOpen: false,
    envText: "",

    get envVars() {
      const project = this.project;
      if (!project) return [];
      return envRowsWithComments(project.env_vars, project.env_comments);
    },

    async addEnvVar() {
      if (!this.newEnvKey.trim()) return;
      const project = this.project;
      const vars = (project.env_vars || []).filter((envVar) => envVar.key !== this.newEnvKey.trim());
      vars.push({ key: this.newEnvKey.trim(), value: { Plain: this.newEnvValue } });
      const saveProjectEnvResult = await this.store.saveProjectEnv(vars, project.env_comments || []);
      if (saveProjectEnvResult.ok) {
        this.newEnvKey = "";
        this.newEnvValue = "";
      }
    },
    async deleteEnvVar(key) {
      const project = this.project;
      const vars = (project.env_vars || []).filter((envVar) => envVar.key !== key);
      const comments = (project.env_comments || []).filter((envComment) => envComment.before_key !== key);
      await this.store.saveProjectEnv(vars, comments);
    },

    openEnvText() {
      const project = this.project;
      this.envText = dotenvFromVars(project?.env_vars, project?.env_comments);
      this.envTextOpen = true;
    },
    closeEnvText() {
      this.envTextOpen = false;
    },
    async saveEnvText() {
      const { vars, comments } = parseDotenv(this.envText);
      const saveProjectEnvResult = await this.store.saveProjectEnv(vars, comments);
      if (saveProjectEnvResult.ok) this.envTextOpen = false;
    },

    // ── Secrets do projeto ────────────────────────────────────────────
    newSecretName: "",
    newSecretValue: "",

    get secrets() {
      const project = this.project;
      return (project?.secrets || []).map((name) => ({ name }));
    },

    async submitSecret() {
      const addSecretResult = await this.store.addSecret(this.newSecretName, this.newSecretValue);
      if (addSecretResult.ok) {
        this.newSecretName = "";
        this.newSecretValue = "";
      }
    },
  }));
});
