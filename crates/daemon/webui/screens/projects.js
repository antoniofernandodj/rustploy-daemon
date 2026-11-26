import { t } from "../i18n.js";
// screens/projects.js — tela "Projects": grid de cards + criar/editar/
// remover. Porta de format.project_rows (crates/rustploy-gui/views/scripts/
// format/dashboard.luau) e handlers/projects.luau (create/edit/delete), sem o
// grid em N-colunas do client iced (aqui é CSS grid nativo, ver .grid em
// app.css) nem a janela separada de criação (o browser não tem multi-janela
// — o form fica inline, mesma tela).
// Caracteres da descrição no card: as 2 linhas do card mais estreito (.grid é
// minmax(340px, 1fr); .project-card__description em app.css corta em 2 linhas).
// Na GUI o orçamento sai da largura real do card (description_budget em
// format/dashboard.luau); aqui o clamp do CSS é o corte e este número só decide
// quando o popover passa a valer.
const PROJECT_CARD_DESCRIPTION_MAX = 90;

// Porta de Util.ellipsis (format/util.luau): corta por caractere, não por byte.
function ellipsis(text, max) {
  const characters = Array.from(text);
  return characters.length > max ? characters.slice(0, max).join("") + "…" : text;
}

document.addEventListener("alpine:init", () => {
  Alpine.data("projects", () => ({
    get store() {
      return Alpine.store("app");
    },
    showNewForm: false,
    newName: "",
    newDescription: "",
    newError: "",
    editingId: null,
    editName: "",
    editDescription: "",
    // Id do card com o popover da descrição aberto (só um por vez).
    descriptionOpenId: null,

    get servicesCount() {
      return ((this.store.snap && this.store.snap.services) || []).length;
    },

    get rows() {
      const store = this.store;
      const services = (store.snap && store.snap.services) || [];
      const projects = (store.snap && store.snap.projects) || [];
      // Um ambiente aparece logo depois do projeto de origem.
      const origins = projects.filter((project) => !project.environment);
      const ordered = [];
      for (const origin of origins) {
        ordered.push(origin);
        for (const environment of projects) {
          if (environment.environment && environment.environment.origin_project_id === origin.id) ordered.push(environment);
        }
      }
      for (const project of projects) if (!ordered.includes(project)) ordered.push(project);
      return ordered.map((project) => {
        const svcs = services.filter((service) => service.service.spec.project_id === project.id);
        return {
          environmentLabel: project.environment ? t`ambiente: ${project.environment.name}` : "",
          id: project.id,
          name: project.name,
          description: project.description || "",
          descriptionShort: ellipsis(project.description || "", PROJECT_CARD_DESCRIPTION_MAX),
          descriptionTruncated: Array.from(project.description || "").length > PROJECT_CARD_DESCRIPTION_MAX,
          serviceCount: svcs.length,
          runningCount: svcs.filter((svc) => svc.service.status === "Running").length,
          canDelete: svcs.length === 0,
        };
      });
    },

    async submitNew() {
      this.newError = "";
      const createProjectResult = await this.store.createProject(this.newName, this.newDescription);
      if (createProjectResult.ok) {
        this.newName = "";
        this.newDescription = "";
        this.showNewForm = false;
      } else {
        this.newError = createProjectResult.error;
      }
    },

    startEdit(row) {
      this.editingId = row.id;
      this.editName = row.name;
      this.editDescription = row.description;
    },
    cancelEdit() {
      this.editingId = null;
    },
    async saveEdit() {
      const updateProjectResult = await this.store.updateProject(this.editingId, this.editName, this.editDescription);
      if (updateProjectResult.ok) this.editingId = null;
    },
  }));

  // Janela "Novo projeto"/"Editar projeto" (wm.js). O estado de abertura mora
  // no store (`projectWin`); aqui só o formulário.
  Alpine.data("projectWin", () => ({
    get store() {
      return Alpine.store("app");
    },
    name: "",
    desc: "",
    error: "",
    busy: false,
    get editing() {
      return !!(this.store.projectWin && this.store.projectWin.id);
    },
    init() {
      this.$watch(
        () => this.store.projectWin,
        (windowState) => {
          if (!windowState) return;
          this.name = windowState.name || "";
          this.desc = windowState.description || "";
          this.error = "";
          this.busy = false;
        }
      );
    },
    async submit() {
      this.error = "";
      this.busy = true;
      const windowState = this.store.projectWin;
      const saveResult = windowState && windowState.id
        ? await this.store.updateProject(windowState.id, this.name, this.desc)
        : await this.store.createProject(this.name, this.desc);
      this.busy = false;
      if (saveResult.ok) this.store.closeProjectWin();
      else this.error = saveResult.error;
    },
  }));
});
