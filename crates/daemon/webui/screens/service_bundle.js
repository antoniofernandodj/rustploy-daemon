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
      this.$watch("$store.app.exportWin", (w) => (w ? this.load(w.serviceId) : this.reset()));
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
      const r = await this.store.api.rpcChecked({ ServiceExportPlan: { service_id: serviceId } });
      this.loading = false;
      if (!r.ok || !r.value?.ServiceExportPlan) {
        this.error = r.ok ? "resposta inesperada do daemon" : r.error;
        return;
      }
      this.plan = r.value.ServiceExportPlan;
      this.pickSuggested();
    },

    get projectVars() {
      const t = this.filter.trim().toLowerCase();
      const all = this.plan?.project_env || [];
      return t ? all.filter((v) => v.key.toLowerCase().includes(t)) : all;
    },
    get pickedKeys() {
      return (this.plan?.project_env || []).filter((v) => this.picked[v.key]).map((v) => v.key);
    },
    pickSuggested() {
      const p = {};
      for (const v of this.plan?.project_env || []) p[v.key] = !!v.suggested;
      this.picked = p;
    },
    pickAll() {
      const p = {};
      for (const v of this.plan?.project_env || []) p[v.key] = true;
      this.picked = p;
    },
    pickNone() {
      this.picked = {};
    },

    /** Pede o pacote ao daemon com as escolhas da tela. */
    async generate() {
      this.error = "";
      const r = await this.store.api.rpcChecked({
        ServiceExport: {
          service_id: this.store.exportWin.serviceId,
          include_values: this.includeValues,
          project_env_keys: this.pickedKeys,
        },
      });
      if (!r.ok || !r.value?.ServiceBundleYaml) {
        this.error = r.ok ? "resposta inesperada do daemon" : r.error;
        return null;
      }
      return r.value.ServiceBundleYaml;
    },

    async download() {
      const b = await this.generate();
      if (!b) return;
      const url = URL.createObjectURL(new Blob([b.yaml], { type: "text/yaml" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = b.filename;
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      this.store.toastOk(`arquivo gerado: ${b.filename}`);
    },

    async copy() {
      const b = await this.generate();
      if (!b) return;
      try {
        await navigator.clipboard.writeText(b.yaml);
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

    async pickFile(ev) {
      const f = ev.target.files && ev.target.files[0];
      if (!f) return;
      this.fileName = f.name;
      this.text = await f.text();
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
      for (const [k, v] of Object.entries(map)) if (v !== "") out[k] = v;
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
      const r = await this.store.api.rpcChecked({ ServiceImport: this.buildReq(true) });
      if (mine !== this.seq) return;
      this.analyzing = false;
      if (!r.ok || !r.value?.ServiceImportReport) {
        this.report = null;
        this.error = r.ok ? "resposta inesperada do daemon" : r.error;
        return;
      }
      this.report = r.value.ServiceImportReport;
      if (first || !this.name) this.name = this.report.service_name;
      for (const s of this.report.project_env) {
        if (!this.projectEnv[s.key]) this.projectEnv[s.key] = s.choice;
      }
      if (this.report.missing_git_provider && this.gitProviders.length === 0) {
        const g = await this.store.api.rpc("GitProviderList");
        this.gitProviders = (g.ok && g.value?.GitProviders) || [];
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
      const r = this.report;
      return !!r && (r.missing_service_vars.length > 0 || r.missing_project_vars.length > 0);
    },
    get canCreate() {
      return !!this.report && !this.report.name_conflict && !this.hasMissing && !this.analyzing;
    },

    stateLabel(s) {
      return { New: "nova", Same: "igual", Conflict: "em conflito" }[s] || s;
    },
    choiceLabel(s, choice) {
      if (choice === "Keep") return s.state === "New" ? "criar no projeto" : "manter a do projeto";
      return { Overwrite: "sobrescrever a do projeto", ServiceOnly: "só neste serviço", Ignore: "não trazer" }[choice];
    },
    choicesFor(s) {
      return s.state === "Conflict" ? ["Keep", "Overwrite", "ServiceOnly", "Ignore"] : ["Keep", "ServiceOnly", "Ignore"];
    },

    async create() {
      this.error = "";
      this.creating = true;
      const r = await this.store.api.rpcChecked({ ServiceImport: this.buildReq(false) });
      this.creating = false;
      if (!r.ok || !r.value?.ServiceImportReport) {
        this.error = r.ok ? "resposta inesperada do daemon" : r.error;
        return;
      }
      const done = r.value.ServiceImportReport;
      this.store.toastOk(`serviço '${done.service_name}' criado`);
      for (const w of done.warnings.filter((x) => x.code === "deploy_not_started")) this.store.toastWarn(w.message);
      await this.store.refreshNow();
      this.store.closeNewService();
      if (done.service_id) this.store.openService(done.service_id);
    },
  }));
});
