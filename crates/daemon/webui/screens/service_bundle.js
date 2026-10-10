// screens/service_bundle.js — copiar um serviço entre servidores
// (docs/plano-copiar-servico-entre-servidores.md).
//
//   • `serviceExport` — a janela "Exportar serviço" (aberta pelo botão da tela
//     do serviço): checkboxes das variáveis do PROJETO que o app talvez use,
//     caixa "incluir valores" e a saída em arquivo .yml ou texto.
//   • `serviceImport` — o passo "Importar" do wizard "Novo serviço": o destino é
//     SEMPRE o projeto aberto (`store.selectedProjectId`), como numa criação
//     qualquer. Analisa com `dry_run`, mostra o que não veio e o que precisa de
//     decisão, e só então cria.
//
// O parse/geração do YAML é do daemon; aqui só se carrega o texto.

document.addEventListener("alpine:init", () => {
  // ── Exportar ─────────────────────────────────────────────────────────
  Alpine.data("serviceExport", () => ({
    get store() {
      return Alpine.store("app");
    },
    loading: false,
    error: "",
    plan: null,
    picked: {}, // chave de variável do projeto → marcada?
    includeValues: true,
    filter: "",
    copyMessage: "",

    init() {
      this.$watch("$store.app.exportWin", (exportWindow) => (exportWindow ? this.load(exportWindow.serviceId) : this.reset()));
    },

    reset() {
      this.loading = false;
      this.error = "";
      this.plan = null;
      this.picked = {};
      this.includeValues = true;
      this.filter = "";
      this.copyMessage = "";
    },

    async load(serviceId) {
      this.reset();
      this.loading = true;
      const serviceExportPlanResponse = await this.store.api.rpcChecked({ ServiceExportPlan: { service_id: serviceId } });
      this.loading = false;
      if (!serviceExportPlanResponse.ok || !serviceExportPlanResponse.value?.ServiceExportPlan) {
        this.error = serviceExportPlanResponse.ok ? "resposta inesperada do daemon" : serviceExportPlanResponse.error;
        return;
      }
      this.plan = serviceExportPlanResponse.value.ServiceExportPlan;
      this.pickSuggested();
    },

    get projectVars() {
      const filterText = this.filter.trim().toLowerCase();
      const all = this.plan?.project_env || [];
      return filterText ? all.filter((item) => item.key.toLowerCase().includes(filterText)) : all;
    },
    get pickedKeys() {
      return (this.plan?.project_env || []).filter((envVar) => this.picked[envVar.key]).map((envVar) => envVar.key);
    },
    pickSuggested() {
      const pickedMap = {};
      for (const envVar of this.plan?.project_env || []) pickedMap[envVar.key] = !!envVar.suggested;
      this.picked = pickedMap;
    },
    pickAll() {
      const pickedMap = {};
      for (const envVar of this.plan?.project_env || []) pickedMap[envVar.key] = true;
      this.picked = pickedMap;
    },
    pickNone() {
      this.picked = {};
    },

    /** Pede o pacote ao daemon com as escolhas da tela. */
    async generate() {
      this.error = "";
      const serviceExportResponse = await this.store.api.rpcChecked({
        ServiceExport: {
          service_id: this.store.exportWin.serviceId,
          include_values: this.includeValues,
          project_env_keys: this.pickedKeys,
        },
      });
      if (!serviceExportResponse.ok || !serviceExportResponse.value?.ServiceBundleYaml) {
        this.error = serviceExportResponse.ok ? "resposta inesperada do daemon" : serviceExportResponse.error;
        return null;
      }
      return serviceExportResponse.value.ServiceBundleYaml;
    },

    async download() {
      const bundle = await this.generate();
      if (!bundle) return;
      const url = URL.createObjectURL(new Blob([bundle.yaml], { type: "text/yaml" }));
      const link = document.createElement("a");
      link.href = url;
      link.download = bundle.filename;
      document.body.appendChild(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      this.store.toastOk(`arquivo gerado: ${bundle.filename}`);
    },

    async copy() {
      const bundle = await this.generate();
      if (!bundle) return;
      try {
        await navigator.clipboard.writeText(bundle.yaml);
        this.store.toastOk("pacote copiado como texto");
      } catch {
        this.error = "o navegador não deixou copiar; use \"Baixar .yml\"";
      }
    },

    close() {
      this.store.closeExportWin();
    },
  }));

  // ── Importar ─────────────────────────────────────────────────────────
  Alpine.data("serviceImport", () => ({
    get store() {
      return Alpine.store("app");
    },
    text: "",
    fileName: "",
    report: null,
    analyzing: false,
    creating: false,
    error: "",
    seq: 0, // descarta resposta de análise velha se outra já foi pedida

    // Escolhas do usuário (viram o ServiceImportReq).
    name: "",
    dropDomains: false,
    gitProviderId: "",
    deploy: false,
    vars: {}, // ${CHAVE} do serviço → valor
    projectVars: {}, // idem, do projeto
    secrets: {}, // secret ausente no destino → valor (vazio = seguir sem)
    projectEnv: {}, // chave de variável do projeto → "Keep" | "Overwrite" | "ServiceOnly" | "Ignore"
    gitProviders: [],

    async pickFile(changeEvent) {
      const file = changeEvent.target.files && changeEvent.target.files[0];
      if (!file) return;
      this.fileName = file.name;
      this.text = await file.text();
      await this.analyze(true);
    },

    clear() {
      this.text = "";
      this.fileName = "";
      this.report = null;
      this.error = "";
      this.name = "";
      this.dropDomains = false;
      this.gitProviderId = "";
      this.deploy = false;
      this.vars = {};
      this.projectVars = {};
      this.secrets = {};
      this.projectEnv = {};
    },

    nonEmpty(map) {
      const out = {};
      for (const [key, value] of Object.entries(map)) if (value !== "") out[key] = value;
      return out;
    },

    buildReq(dryRun) {
      return {
        yaml: this.text,
        project_id: this.store.selectedProjectId,
        name: this.name.trim() || null,
        drop_domains: this.dropDomains,
        git_provider_id: this.gitProviderId || null,
        vars: this.nonEmpty(this.vars),
        project_vars: this.nonEmpty(this.projectVars),
        secrets: this.nonEmpty(this.secrets),
        project_env: this.projectEnv,
        deploy: this.deploy,
        dry_run: dryRun,
      };
    },

    /** Pré-visualização (nada é gravado). `first`: primeira análise do arquivo,
     * quando o nome ainda vem do pacote. */
    async analyze(first = false) {
      if (!this.text.trim()) {
        this.error = "escolha um arquivo ou cole o texto do pacote";
        return;
      }
      if (first) this.name = "";
      this.error = "";
      const mine = ++this.seq;
      this.analyzing = true;
      const serviceImportResponse = await this.store.api.rpcChecked({ ServiceImport: this.buildReq(true) });
      if (mine !== this.seq) return;
      this.analyzing = false;
      if (!serviceImportResponse.ok || !serviceImportResponse.value?.ServiceImportReport) {
        this.report = null;
        this.error = serviceImportResponse.ok ? "resposta inesperada do daemon" : serviceImportResponse.error;
        return;
      }
      this.report = serviceImportResponse.value.ServiceImportReport;
      if (first || !this.name) this.name = this.report.service_name;
      for (const statusEntry of this.report.project_env) {
        if (!this.projectEnv[statusEntry.key]) this.projectEnv[statusEntry.key] = statusEntry.choice;
      }
      if (this.report.missing_git_provider && this.gitProviders.length === 0) {
        const gitProviderListResponse = await this.store.api.rpc("GitProviderList");
        this.gitProviders = (gitProviderListResponse.ok && gitProviderListResponse.value?.GitProviders) || [];
      }
    },

    /** Sobrescrever mexe no projeto e vale para os outros serviços dele. */
    chooseEnv(key, choice) {
      if (choice === "Overwrite" && !confirm(`Sobrescrever ${key} no projeto? Os outros serviços do projeto também passam a ver o valor novo.`)) {
        this.projectEnv[key] = "Keep";
        return;
      }
      this.projectEnv[key] = choice;
      this.analyze();
    },

    get hasMissing() {
      const report = this.report;
      return !!report && (report.missing_service_vars.length > 0 || report.missing_project_vars.length > 0);
    },
    get canCreate() {
      return !!this.report && !this.report.name_conflict && !this.hasMissing && !this.analyzing;
    },

    stateLabel(state) {
      return { New: "nova", Same: "igual", Conflict: "em conflito" }[state] || state;
    },
    choiceLabel(statusEntry, choice) {
      if (choice === "Keep") return statusEntry.state === "New" ? "criar no projeto" : "manter a do projeto";
      return { Overwrite: "sobrescrever a do projeto", ServiceOnly: "só neste serviço", Ignore: "não trazer" }[choice];
    },
    choicesFor(statusEntry) {
      return statusEntry.state === "Conflict" ? ["Keep", "Overwrite", "ServiceOnly", "Ignore"] : ["Keep", "ServiceOnly", "Ignore"];
    },

    async create() {
      this.error = "";
      this.creating = true;
      const serviceImportResponse = await this.store.api.rpcChecked({ ServiceImport: this.buildReq(false) });
      this.creating = false;
      if (!serviceImportResponse.ok || !serviceImportResponse.value?.ServiceImportReport) {
        this.error = serviceImportResponse.ok ? "resposta inesperada do daemon" : serviceImportResponse.error;
        return;
      }
      const done = serviceImportResponse.value.ServiceImportReport;
      this.store.toastOk(`serviço '${done.service_name}' criado`);
      for (const warning of done.warnings.filter((warning) => warning.code === "deploy_not_started")) this.store.toastWarn(warning.message);
      await this.store.refreshNow();
      this.store.closeNewService();
      if (done.service_id) this.store.openService(done.service_id);
    },
  }));
});
