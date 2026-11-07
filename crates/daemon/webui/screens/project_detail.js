// screens/project_detail.js — projeto aberto (view=project_services no
// client iced): sub-abas Serviços/Variáveis/Secrets/Jobs. Porta de
// format.service_rows, o cabeçalho editável e as sub-abas env/secrets/jobs de
// shell.gv + handlers/projects.luau (aba "Jobs" filtra `snap.jobs` pelo
// projeto aberto — mesma lógica de stream.luau::update_open_project). O
// wizard "novo job" acionado pelo botão desta aba é o mesmo modal global do
// store (app.js), compartilhado com a tela "Schedules".
import {
  serviceStatusLabelKind,
  serviceWarmLabel,
  dotenvFromVars,
  parseDotenv,
  dotenvErrorsMessage,
  confirmEnvKey,
  envRowsWithComments,
  envValueFromDraft,
  jobSummaryRows,
  formatBytes,
} from "../format.js";
import { t } from "../i18n.js";

/** Container "primário" de um serviço pra exibir no card: o live, senão o
 * primeiro em execução, senão o primeiro da lista. `extra` é "+N" quando há
 * mais de um container. Porta de format/dashboard.luau::primary_container. */
function primaryContainer(service) {
  const list = service.containers || [];
  if (list.length === 0) return { name: "—", id: "", idFull: "", nameFull: "", extra: "" };
  let chosen = list[0];
  for (const item of list) {
    if (service.live_container_id && item.id === service.live_container_id) {
      chosen = item;
      break;
    }
    if (item.state === "running" && chosen.state !== "running") chosen = item;
  }
  const extra = list.length > 1 ? `+${list.length - 1}` : "";
  return {
    name: chosen.name || "—",
    nameFull: chosen.name || "",
    id: (chosen.id || "").slice(0, 12),
    idFull: chosen.id || "",
    extra,
  };
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
    // Valores de secret que o usuário pediu para ver (SecretReveal). Só em memória; some ao trocar de projeto.
    revealedSecrets: {},
    init() {
      this.$watch(
        () => this.store.selectedProjectId,
        () => {
          this.revealedSecrets = {};
        }
      );
    },

    get project() {
      const store = this.store;
      return ((store.snap && store.snap.projects) || []).find((project) => project.id === store.selectedProjectId) || null;
    },

    get services() {
      const store = this.store;
      const projectId = store.selectedProjectId;
      const services = (store.snap && store.snap.services) || [];
      const metricsById = store.metricsById || {};
      const warmById = store.serviceWarmById || {};
      return services
        .filter((entry) => entry.service.spec.project_id === projectId)
        .map((entry) => {
          const service = entry.service;
          const [label, kind] = serviceStatusLabelKind(service.status);
          const metricsPoint = metricsById[service.id];
          const container = primaryContainer(service);
          const warm = warmById[service.id];
          const [warmLabel, warmKind] = serviceWarmLabel(warm);
          return {
            hasWarm: !!warm,
            warmLabel,
            warmKind,
            warmReason: warm?.reason || "",
            id: service.id,
            name: service.spec.name,
            port: service.spec.port,
            statusLabel: label,
            statusKind: kind,
            cpu: metricsPoint ? `${(metricsPoint.cpu_percent || 0).toFixed(1)}%` : "—",
            mem: metricsPoint ? formatBytes(metricsPoint.mem_used_bytes) : "—",
            containerName: container.name,
            containerId: container.id,
            containerNameFull: container.nameFull,
            containerIdFull: container.idFull,
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
      if (!confirmEnvKey(this.store, this.newEnvKey.trim(), this.project.env_vars)) return;
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
      const { vars, comments, errors } = parseDotenv(this.envText);
      if (errors.length) {
        this.store.toastError(dotenvErrorsMessage(errors));
        return;
      }
      const saveProjectEnvResult = await this.store.saveProjectEnv(vars, comments);
      if (saveProjectEnvResult.ok) this.envTextOpen = false;
    },

    // ── Secrets do projeto ────────────────────────────────────────────
    newSecretName: "",
    newSecretValue: "",

    get secrets() {
      const project = this.project;
      return (project?.secrets || []).map((name) => ({ name, value: this.revealedSecrets[name] ?? null }));
    },

    /** 👁: pede o valor ao daemon (SecretReveal grava auditoria: quem, qual, quando; nunca o valor). */
    async revealSecret(name) {
      if (!confirm(t`Ver o valor do secret "${name}"? O valor vai aparecer na tela e o acesso fica registrado na auditoria do servidor (quem, qual secret e quando; nunca o valor).`)) return;
      const secretRevealResponse = await this.store.api.rpc({ SecretReveal: { project_id: this.store.selectedProjectId, name } });
      const value = secretRevealResponse.ok ? secretRevealResponse.value?.SecretValue : undefined;
      if (value === undefined) {
        this.store.toastError(secretRevealResponse.error || t("não foi possível ler o secret"));
        return;
      }
      this.revealedSecrets = { ...this.revealedSecrets, [name]: value };
    },
    hideSecret(name) {
      const { [name]: _hidden, ...rest } = this.revealedSecrets;
      this.revealedSecrets = rest;
    },
    copySecret(name) {
      return this.store.copyText(this.revealedSecrets[name] ?? "", t("valor"));
    },

    // ── Edição no lugar da chave/valor de uma variável ─────────────────
    // `envEdit` = { key, field: "key" | "value", draft } — a própria linha vira um input, sem levar
    // a variável para um formulário (numa lista grande o formulário ficava fora da tela).
    // Enter ou clicar fora grava (se mudou); Esc desiste. Chave: EnvKeyRename; valor: EnvValueSet.
    envEdit: null,
    startEnvEdit(row, field) {
      this.envEdit = { key: row.key, field, draft: field === "key" ? row.key : row.isSecret ? row.secretName : row.value };
    },
    isEnvEditing(key, field) {
      return !!this.envEdit && this.envEdit.key === key && this.envEdit.field === field;
    },
    cancelEnvEdit() {
      this.envEdit = null;
    },
    async commitEnvEdit(row) {
      const edit = this.envEdit;
      if (!edit) return;
      this.envEdit = null; // antes do await: o blur que vem junto com o Enter não grava duas vezes
      const projectId = this.store.selectedProjectId;
      if (edit.field === "key") {
        const to = edit.draft.trim();
        if (!to || to === row.key) return;
        const renameResponse = await this.store.api.rpcChecked({ EnvKeyRename: { project_id: projectId, from: row.key, to } });
        this.store.toastResult(renameResponse, t`renomeado: ${row.key} → ${to}`);
      } else {
        const current = row.isSecret ? row.secretName : row.value;
        if (edit.draft === current) return;
        const setResponse = await this.store.api.rpcChecked({
          EnvValueSet: { project_id: projectId, key: row.key, value: envValueFromDraft(edit.draft, row.isSecret) },
        });
        this.store.toastResult(setResponse, t`valor de ${row.key} salvo`);
      }
      await this.store.refreshNow();
    },

    // ── Renomear (EnvKeyRename / SecretRename) ─────────────────────────
    // O valor nunca passa pela tela (nem pelo agente): só a chave da variável, ou o nome do secret
    // — e as variáveis que o referenciam são reapontadas pelo daemon.
    async projectRename(kind, from) {
      const answer = prompt(t`Renomear "${from}" para:`, from);
      const to = answer === null ? "" : answer.trim();
      if (!to || to === from) return;
      const body = { project_id: this.store.selectedProjectId, from, to };
      const renameResponse = await this.store.api.rpcChecked(
        kind === "secret" ? { SecretRename: body } : { EnvKeyRename: body },
      );
      this.store.toastResult(renameResponse, t`renomeado: ${from} → ${to}`);
      if (renameResponse.ok) await this.store.refreshNow();
    },

    // ── Movimentos entre escopos (SecretMove) ─────────────────────────
    // O destino dos "↓ para o serviço" é o serviço escolhido no seletor do topo da aba.
    moveTargetServiceId: "",
    get serviceOptions() {
      const projectId = this.store.selectedProjectId;
      return ((this.store.snap && this.store.snap.services) || [])
        .filter((entry) => entry.service.spec.project_id === projectId)
        .map((entry) => ({ id: entry.service.id, name: entry.service.spec.name }));
    },
    async projectMove(op, name, serviceId, doneMessage) {
      const secretMoveResponse = await this.store.api.rpcChecked({
        SecretMove: { project_id: this.store.selectedProjectId, service_id: serviceId || null, name, op },
      });
      this.store.toastResult(secretMoveResponse, doneMessage);
      if (secretMoveResponse.ok) await this.store.refreshNow();
    },
    targetService() {
      if (this.moveTargetServiceId) return this.moveTargetServiceId;
      this.store.toastError(t("Escolha antes o serviço de destino, no seletor do topo."));
      return null;
    },
    projectEnvToSecret(key) {
      return this.projectMove("EnvToSecret", key, null, t`variável ${key} movida para secret`);
    },
    projectSecretToEnv(name) {
      if (!confirm(t`O valor de "${name}" deixa de ser cifrado e passa a aparecer em claro na variável (e no estado versionado). Continuar?`)) return;
      return this.projectMove("SecretToEnv", name, null, t`secret ${name} movido para variável`);
    },
    projectSecretToService(name) {
      const serviceId = this.targetService();
      if (serviceId) return this.projectMove("ProjectSecretToService", name, serviceId, t`secret ${name} movido para o serviço`);
    },
    projectEnvToService(key) {
      const serviceId = this.targetService();
      if (serviceId) return this.projectMove("EnvProjectToService", key, serviceId, t`variável ${key} movida para o serviço`);
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
