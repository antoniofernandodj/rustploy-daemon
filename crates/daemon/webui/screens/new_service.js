// screens/new_service.js — wizard "Novo serviço", porta de new_service.gv
// (client iced): passo pick_type → passo específico por tipo. Application usa
// `ServiceCreate` direto (coleta a origem Git/registry já na criação — ver
// app.js::createServiceDirect); Compose/Database/Broker/Template usam
// `Command::WizardCreate` (o daemon monta o compose/env vars certos a partir
// dos catálogos de `Command::WizardCatalog`).
document.addEventListener("alpine:init", () => {
  Alpine.data("newService", () => ({
    get store() {
      return Alpine.store("app");
    },
    step: "pick_type",
    error: "",
    submitting: false,

    catalog: { dbs: [], brokers: [], templates: [] },
    catalogLoaded: false,
    async ensureCatalog() {
      if (this.catalogLoaded) return;
      this.catalog = await this.store.fetchWizardCatalog("");
      this.catalogLoaded = true;
    },

    // ── Application ────────────────────────────────────────────────
    appName: "",
    sourceKind: "git",
    gitUrl: "",
    gitBranch: "main",
    registryImage: "",
    appPort: 80,
    appDomain: "",

    // ── Compose Stack ─────────────────────────────────────────────
    composeName: "",
    composeText: "services:\n  app:\n    image: nginx:latest\n    ports:\n      - \"80:80\"\n",
    composePort: 80,
    composeDomain: "",

    // ── Database ──────────────────────────────────────────────────
    selectedDatabase: null,
    databaseServiceName: "",
    databaseName: "",
    databaseUser: "",
    databasePassword: "",
    databaseRootPassword: "",
    databaseImage: "",
    databaseUseReplica: false,

    // ── Broker ────────────────────────────────────────────────────
    selectedBroker: null,
    brokerServiceName: "",
    brokerUser: "",
    brokerPassword: "",
    brokerImage: "",

    // ── Template ──────────────────────────────────────────────────
    templateSearch: "",
    selectedTemplate: null,
    templateServiceName: "",
    templateValues: [],
    get filteredTemplates() {
      const searchTerm = this.templateSearch.trim().toLowerCase();
      if (!searchTerm) return this.catalog.templates;
      return this.catalog.templates.filter(
        (template) => template.name.toLowerCase().includes(searchTerm) || template.description.toLowerCase().includes(searchTerm)
      );
    },

    // ── Navegação por passos ─────────────────────────────────────────
    gotoType() {
      this.error = "";
      this.step = "pick_type";
    },
    gotoApp() {
      this.error = "";
      this.step = "app_form";
    },
    gotoImport() {
      this.error = "";
      this.step = "import";
    },
    gotoCompose() {
      this.error = "";
      this.step = "compose_form";
    },
    async gotoDatabase() {
      this.error = "";
      await this.ensureCatalog();
      this.step = "pick_db";
    },
    pickDatabase(database) {
      this.selectedDatabase = database;
      this.databaseUser = database.user;
      this.databaseImage = database.image;
      this.databaseName = "";
      this.databasePassword = "";
      this.databaseRootPassword = "";
      this.databaseUseReplica = false;
      this.databaseServiceName = "";
      this.step = "db_form";
    },
    async gotoBroker() {
      this.error = "";
      await this.ensureCatalog();
      this.step = "pick_broker";
    },
    pickBroker(broker) {
      this.selectedBroker = broker;
      this.brokerUser = broker.user;
      this.brokerImage = broker.image;
      this.brokerPassword = "";
      this.brokerServiceName = "";
      this.step = "broker_form";
    },
    async gotoTemplate() {
      this.error = "";
      await this.ensureCatalog();
      this.step = "pick_template";
    },
    pickTemplate(template) {
      this.selectedTemplate = template;
      this.templateValues = template.vars.map(() => "");
      this.templateServiceName = "";
      this.step = "template_form";
    },

    cancel() {
      this.store.closeNewService();
    },

    /** Janela reaberta: volta ao passo 1 sem lixo do uso anterior (o catálogo
     * dos bancos/brokers/templates fica em cache, não precisa refazer). */
    reset(resetEvent) {
      this.step = "pick_type";
      this.error = "";
      this.submitting = false;
      this.appName = "";
      this.gitUrl = "";
      this.registryImage = "";
      this.appDomain = "";
      this.composeName = "";
      this.composeDomain = "";
      this.templateSearch = "";
      if (resetEvent?.detail?.step === "import") this.gotoImport();
    },

    // ── Submissões ────────────────────────────────────────────────────
    baseReq(kind, selectedId) {
      return {
        kind,
        id: selectedId,
        project_id: this.store.selectedProjectId,
        name: "",
        app_name: "",
        db_name: "",
        user: "",
        password: "",
        root_password: "",
        image: "",
        use_replica: false,
        template_values: [],
        expose_external: false,
      };
    },

    async submitApp() {
      this.error = "";
      if (!this.appName.trim()) {
        this.error = "nome obrigatório";
        return;
      }
      let source;
      if (this.sourceKind === "git") {
        if (!this.gitUrl.trim()) {
          this.error = "URL do repositório obrigatória";
          return;
        }
        source = {
          Git: {
            url: this.gitUrl.trim(),
            branch: this.gitBranch.trim() || "main",
            root_path: "",
            watch_paths: [],
            submodules: false,
            dockerfile_path: "Dockerfile",
            build_context: ".",
            build_stage: null,
            credentials: null,
            username: null,
            provider_id: null,
          },
        };
      } else {
        if (!this.registryImage.trim()) {
          this.error = "imagem obrigatória";
          return;
        }
        source = { Registry: { image: this.registryImage.trim() } };
      }
      this.submitting = true;
      const createServiceDirectResult = await this.store.createServiceDirect(this.appName, source, this.appPort, this.appDomain);
      this.submitting = false;
      if (!createServiceDirectResult.ok) this.error = createServiceDirectResult.error;
    },

    async submitCompose() {
      this.error = "";
      if (!this.composeName.trim()) {
        this.error = "nome obrigatório";
        return;
      }
      if (!this.composeText.trim()) {
        this.error = "compose YAML obrigatório";
        return;
      }
      // WizardCreateReq (kind="compose") não carrega o YAML — o wizard iced
      // cria o serviço com compose VAZIO e deixa editar depois na aba
      // General. Preenchemos já na criação via ServiceCreate direto, como
      // fazemos com Application (mais direto pro usuário).
      const source = { Compose: { content: this.composeText } };
      this.submitting = true;
      const createServiceDirectResult = await this.store.createServiceDirect(this.composeName, source, this.composePort, this.composeDomain);
      this.submitting = false;
      if (!createServiceDirectResult.ok) this.error = createServiceDirectResult.error;
    },

    async submitDatabase() {
      this.error = "";
      const database = this.selectedDatabase;
      if (database.has_db_name && !this.databaseName.trim()) {
        this.error = "nome do banco obrigatório";
        return;
      }
      if (database.has_user && !this.databaseUser.trim()) {
        this.error = "usuário obrigatório";
        return;
      }
      if (!this.databasePassword.trim()) {
        this.error = "senha obrigatória";
        return;
      }
      const createRequest = this.baseReq("database", database.id);
      createRequest.name = this.databaseServiceName;
      createRequest.db_name = this.databaseName;
      createRequest.user = this.databaseUser;
      createRequest.password = this.databasePassword;
      createRequest.root_password = this.databaseRootPassword;
      createRequest.image = this.databaseImage;
      createRequest.use_replica = this.databaseUseReplica;
      this.submitting = true;
      const wizardCreateResult = await this.store.wizardCreate(createRequest);
      this.submitting = false;
      if (!wizardCreateResult.ok) this.error = wizardCreateResult.error;
    },

    async submitBroker() {
      this.error = "";
      const broker = this.selectedBroker;
      if (broker.has_user && !this.brokerUser.trim()) {
        this.error = "usuário obrigatório";
        return;
      }
      const createRequest = this.baseReq("broker", broker.id);
      createRequest.name = this.brokerServiceName;
      createRequest.user = this.brokerUser;
      createRequest.password = this.brokerPassword;
      createRequest.image = this.brokerImage;
      this.submitting = true;
      const wizardCreateResult = await this.store.wizardCreate(createRequest);
      this.submitting = false;
      if (!wizardCreateResult.ok) this.error = wizardCreateResult.error;
    },

    async submitTemplate() {
      this.error = "";
      const createRequest = this.baseReq("template", this.selectedTemplate.id);
      createRequest.name = this.templateServiceName;
      createRequest.template_values = this.templateValues;
      this.submitting = true;
      const wizardCreateResult = await this.store.wizardCreate(createRequest);
      this.submitting = false;
      if (!wizardCreateResult.ok) this.error = wizardCreateResult.error;
    },
  }));
});
