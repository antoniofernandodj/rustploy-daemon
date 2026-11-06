// screens/service_detail.js — detalhe de um serviço. Porta de service.gv
// (client iced): as 8 abas (General/Connection/Environment/Domains/
// Deployments/Healthcheck/Logs/Advanced). A edição de origem (Compose /
// Git / conta conectada Gitea-GitHub / Zip) mora na aba General, mesmo
// lugar do iced (ver service.gv ~L192-566) — não é uma aba própria. Estado
// de navegação/fetch/mutação mora no Alpine.store('app') (app.js); este
// módulo só formata para exibição e cuida dos formulários locais (novo env
// var, novo domínio, form de origem).
import {
  serviceStatusLabelKind,
  serviceWarmLabel,
  sourceSummary,
  looksLikeGitUrl,
  dateDayMonthHourMinuteSecond,
  formatDuration,
  stateLabelKind,
  dotenvFromVars,
  parseDotenv,
  dotenvErrorsMessage,
  confirmEnvKey,
  stripAnsi,
  timeHms,
  safeName,
  internalUrl,
  composeHost,
  externalUrl,
  envRowsWithComments,
} from "../format.js";
import { t } from "../i18n.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("serviceDetail", () => ({
    get store() {
      return Alpine.store("app");
    },
    newEnvKey: "",
    newEnvValue: "",
    // Secrets DESTE serviço (só os nomes; o valor nunca volta do daemon).
    serviceSecrets: [],
    // Valores de secret que o usuário pediu para ver (SecretReveal), por nome. Só em memória; some ao trocar de serviço.
    revealedSecrets: {},
    revealedFor: null,
    newServiceSecretName: "",
    newServiceSecretValue: "",
    newDomain: "",
    newDomainPort: "",
    newDomainTls: false,
    buildLogText: "",
    buildLogFor: null,
    timeHms,

    // `x-show` mantém este componente montado por toda a sessão — abrir um
    // serviço não recria o Alpine.data. `initGeneralForm()` roda no clique da
    // aba (mesmo padrão de initHealthcheckForm/initAdvancedForm), mas a aba General é a
    // default de `openService()`, que não passa por nenhum clique — daí o
    // watch, pra sincronizar o form assim que `serviceDetail` chega do fetch.
    init() {
      this.$watch(
        () => this.store.serviceDetail,
        () => {
          if (this.store.serviceTab === "general") this.initGeneralForm();
          this.loadConnectionUrl();
          if (this.revealedFor !== this.svc?.id) {
            this.revealedFor = this.svc?.id ?? null;
            this.revealedSecrets = {};
          }
          this.loadServiceSecrets();
          this.loadSharedState();
          this.loadMigration();
          this.loadWebhook();
        }
      );
    },

    // ── Webhook de deploy (aba Deployments) ────────────────────────────────
    // Porta de handlers/services.luau (set_webhook_url / regen_webhook). Servido
    // pelo MESMO listener da API; Compose não tem webhook. O token nasce no
    // primeiro deploy — antes disso o daemon devolve `WebhookUrl: null`.
    webhookUrl: "",
    webhookBusy: false,
    get webhookSupported() {
      const spec = this.store.serviceDetail && this.store.serviceDetail.spec;
      return !!spec && !(spec.source && spec.source.Compose);
    },
    async loadWebhook() {
      this.webhookUrl = "";
      const serviceId = this.store.selectedServiceId;
      if (!serviceId || !this.webhookSupported) return;
      const getWebhookUrlResponse = await this.store.api.rpc({ GetWebhookUrl: { service_id: serviceId } });
      if (getWebhookUrlResponse.ok && typeof getWebhookUrlResponse.value?.WebhookUrl === "string") this.webhookUrl = getWebhookUrlResponse.value.WebhookUrl;
    },
    async regenWebhook() {
      if (!confirm(t("Regenerar o token do webhook? A URL anterior deixa de funcionar na hora."))) return;
      this.webhookBusy = true;
      const regenerateWebhookTokenResponse = await this.store.api.rpcChecked({ RegenerateWebhookToken: { service_id: this.store.selectedServiceId } });
      this.webhookBusy = false;
      if (regenerateWebhookTokenResponse.ok && typeof regenerateWebhookTokenResponse.value?.WebhookUrl === "string") {
        this.webhookUrl = regenerateWebhookTokenResponse.value.WebhookUrl;
        this.store.toastOk(t("token do webhook regenerado"));
      } else {
        this.store.toastError("erro: " + (regenerateWebhookTokenResponse.error || t("resposta inesperada")));
      }
    },

    // ── Migração para banco compartilhado (aba Migrar) ─────────────────────
    // Porta de handlers/services.luau (load_migration_state, mig_*). O daemon
    // roda os passos em segundo plano; enquanto `Running`, repolla a cada 3 s.
    migrationDestinations: [],
    mig: null,
    migrationTimer: null,
    serviceFormMigration: { db: "", dest: "", env: "" },
    get canMigrate() {
      return this.canShare && !this.svc?.spec?.shared;
    },
    get migrationSteps() {
      const icon = { ok: "✓", running: "…", failed: "✗", skipped: "–", pending: "·" };
      return (this.mig?.steps || []).map((step) => ({ ...step, icon: icon[step.state] || "·" }));
    },
    async loadMigration() {
      clearTimeout(this.migrationTimer);
      if (!this.canMigrate) {
        this.mig = null;
        if (this.store.serviceTab === "migrar") this.store.setServiceTab("general");
        return;
      }
      const spec = this.svc.spec;
      const serviceId = this.svc.id;
      if (!this.serviceFormMigration.db) {
        // Serviço primeiro, depois o projeto (mesma precedência do deploy).
        const projectEnv = (this.store.snap?.projects || []).find((project) => project.id === spec.project_id)?.env_vars || [];
        const get = (key) => [spec.env_vars || [], projectEnv].map((envVars) => envVars.find((v) => v.key === key)?.value?.Plain).find(Boolean);
        this.serviceFormMigration.db = get("POSTGRES_DB") || get("MYSQL_DATABASE") || "";
      }
      const [destinationsResponse, migrationsResponse] = await Promise.all([
        this.store.api.rpc("ManagedDatabaseListAll"),
        this.store.api.rpc({ MigrationList: { project_id: spec.project_id } }),
      ]);
      this.migrationDestinations = (destinationsResponse.ok ? destinationsResponse.value?.ManagedDatabases || [] : [])
        .filter((database) => database.project_id === spec.project_id)
        .map((database) => ({ id: database.id, name: `${database.name} (${database.env_var})` }));
      this.mig = (migrationsResponse.ok ? migrationsResponse.value?.Migrations || [] : []).find((migration) => migration.source_service_id === serviceId) || null;
      if (this.mig?.status === "Running") {
        this.migrationTimer = setTimeout(() => this.svc?.id === serviceId && this.loadMigration(), 3000);
      }
    },
    async startMigration() {
      if (!this.serviceFormMigration.dest || !this.serviceFormMigration.db.trim()) {
        this.store.toastError(t("Escolha o database de destino e informe o de origem"));
        return;
      }
      if (!confirm(t("Os serviços do projeto que usam este banco serão parados durante o dump/restore e subirão de novo no banco novo. O banco antigo é mantido (parado) para rollback. Iniciar?"))) return;
      const migrationStartResponse = await this.store.api.rpcChecked({
        MigrationStart: {
          source_service_id: this.svc.id,
          source_database: this.serviceFormMigration.db.trim(),
          dest_database_id: this.serviceFormMigration.dest,
          env_var: this.serviceFormMigration.env.trim(),
        },
      });
      if (!migrationStartResponse.ok) {
        this.store.toastError(migrationStartResponse.error);
        return;
      }
      this.store.toastOk(t("Migração iniciada"));
      await this.loadMigration();
    },
    async rollbackMigration() {
      if (!confirm(t("A app volta ao banco antigo. O que foi escrito no banco novo depois da migração se perde. Reverter?"))) return;
      const migrationRollbackResponse = await this.store.api.rpcChecked({ MigrationRollback: { id: this.mig.id } });
      if (!migrationRollbackResponse.ok) this.store.toastError(migrationRollbackResponse.error);
      await this.loadMigration();
    },
    async discardOldDatabase() {
      if (!confirm(t("O serviço do banco antigo é removido, com os dados dele. Sem volta. Descartar?"))) return;
      const migrationDiscardResponse = await this.store.api.rpcChecked({ MigrationDiscard: { id: this.mig.id } });
      if (!migrationDiscardResponse.ok) {
        this.store.toastError(migrationDiscardResponse.error);
        return;
      }
      this.store.toastOk(t("Banco antigo descartado"));
      this.store.nav("projects");
    },

    // ── Servidor de banco compartilhado (aba Databases) ────────────────────
    // Porta de handlers/services.luau (load_shared_state, mdb_*, shared_*).
    // O daemon cria database + usuário, conecta o servidor à rede do projeto
    // e grava a env var nele — ver docs/plano-banco-compartilhado.md.
    mdbs: [],
    sharedDatabaseProjects: [],
    sharedDatabaseUrl: "",
    sharedDatabaseLabel: "",
    serviceFormSharedDatabase: { name: "", project: "", env: "", limit: "", timeout: "", overwrite: false, skipEnv: false },
    get canShare() {
      const normalizedKind = (this.svc?.spec?.product || "").toLowerCase();
      return (
        ["postgres", "postgresql", "mysql", "mariadb", "mongodb", "mongo"].includes(normalizedKind) &&
        !!this.svc?.spec?.source?.Compose
      );
    },
    get isShared() {
      return this.canShare && !!this.svc?.spec?.shared;
    },
    async loadSharedState() {
      if (!this.isShared) {
        this.mdbs = [];
        if (this.store.serviceTab === "databases") this.store.setServiceTab("general");
        return;
      }
      const serviceId = this.svc.id;
      const [projectsResponse, databasesResponse] = await Promise.all([
        this.store.api.rpc("ProjectList"),
        this.store.api.rpc({ ManagedDatabaseList: { server_service_id: serviceId } }),
      ]);
      const projects = projectsResponse.ok ? projectsResponse.value?.Projects || [] : [];
      this.sharedDatabaseProjects = projects.map((project) => ({ id: project.id, name: project.name }));
      const names = Object.fromEntries(projects.map((project) => [project.id, project.name]));
      this.mdbs = (databasesResponse.ok ? databasesResponse.value?.ManagedDatabases || [] : []).map((database) => ({
        id: database.id,
        name: database.name,
        project: names[database.project_id] || database.project_id,
        envVar: database.env_var,
        url: database.connection_url,
      }));
    },
    async setShared(on) {
      if (!on && this.mdbs.length > 0) {
        this.store.toastError(t("Remova os databases deste servidor antes de deixar de compartilhar"));
        return;
      }
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      if (on) spec.shared = {};
      else delete spec.shared;
      await this.store.saveServiceSpec(spec, on ? t("servidor compartilhado") : t("compartilhamento desligado"));
    },
    showSharedDatabase(database) {
      this.sharedDatabaseUrl = database.url;
      this.sharedDatabaseLabel = `${database.name} → ${database.envVar}`;
    },
    async createSharedDatabase() {
      const form = this.serviceFormSharedDatabase;
      if (!form.name.trim() || !form.project) {
        this.store.toastError(t("Informe o nome e o projeto consumidor"));
        return;
      }
      const toNumberOrNull = (value) => (String(value).trim() === "" ? null : Number(value));
      const managedDatabaseCreateResponse = await this.store.api.rpcChecked({
        ManagedDatabaseCreate: {
          server_service_id: this.svc.id,
          project_id: form.project,
          name: form.name.trim(),
          env_var: form.env.trim(),
          overwrite_env: !!form.overwrite,
          skip_env: !!form.skipEnv,
          connection_limit: toNumberOrNull(form.limit),
          statement_timeout_ms: toNumberOrNull(form.timeout),
        },
      });
      if (!managedDatabaseCreateResponse.ok) {
        this.store.toastError(managedDatabaseCreateResponse.error);
        return;
      }
      this.store.toastOk(t("Database criado"));
      this.serviceFormSharedDatabase.name = "";
      await this.loadSharedState();
    },
    async deleteSharedDatabase(database) {
      if (!confirm(t("Remover o database e o usuário do servidor, com todos os dados? Irreversível."))) return;
      const managedDatabaseDeleteResponse = await this.store.api.rpcChecked({ ManagedDatabaseDelete: { id: database.id } });
      if (!managedDatabaseDeleteResponse.ok) {
        this.store.toastError(managedDatabaseDeleteResponse.error);
        return;
      }
      this.sharedDatabaseUrl = "";
      await this.loadSharedState();
    },

    // Internal URL vem pronta do daemon (`shared::connection`): host real do
    // serviço, usuário/senha/database — não é mais montada aqui.
    connectionUrl: "",
    async loadConnectionUrl() {
      const serviceId = this.svc?.id;
      if (!serviceId) return;
      try {
        const serviceConnectionInfoResponse = await this.store.api.rpc({ ServiceConnectionInfo: { service_id: serviceId } });
        if (this.svc?.id === serviceId) this.connectionUrl = serviceConnectionInfoResponse.ok ? serviceConnectionInfoResponse.value?.ConnectionInfo?.internal_url || "" : "";
      } catch (_) {
        this.connectionUrl = "";
      }
    },

    get svc() {
      return this.store.serviceDetail;
    },
    get projectName() {
      const projectId = this.svc && this.svc.spec.project_id;
      const project = ((this.store.snap && this.store.snap.projects) || []).find((project) => project.id === projectId);
      return project ? project.name : "Voltar";
    },
    get statusLabel() {
      return this.svc ? serviceStatusLabelKind(this.svc.status)[0] : "";
    },
    get statusKind() {
      return this.svc ? serviceStatusLabelKind(this.svc.status)[1] : "muted";
    },
    get warm() {
      return this.svc ? this.store.serviceWarmById?.[this.svc.id] : undefined;
    },
    get warmLabel() {
      return serviceWarmLabel(this.warm)[0];
    },
    get warmKind() {
      return serviceWarmLabel(this.warm)[1];
    },
    get sourceText() {
      return this.svc ? sourceSummary(this.svc.spec.source) : "—";
    },

    // ── General → edição de origem (Compose / Git / Zip) ────────────────
    // Porta de handlers/services.luau (compose_save/compose_cancel/gen_save/
    // archive_upload/gitea_provider_pick/gitea_repo_pick) + service.gv
    // ~L192-566. `initGeneralForm()` sincroniza os campos locais com o spec
    // atual — chamado ao abrir a aba, mesmo padrão de initHealthcheckForm/initAdvancedForm.
    composeText: "",
    composeOriginal: "",
    // Renomear (aba General). Porta de save_service_name em handlers/services.luau.
    editName: "",
    providerTab: "git", // "git" | "gitea" | "zip"
    serviceFormRepoUrl: "",
    serviceFormBranch: "",
    serviceFormGeneralPort: "",
    serviceFormUsername: "",
    serviceFormCredentials: "",
    serviceFormBuildPath: "",
    serviceFormWatchPaths: "",
    serviceFormSubmodules: false,
    serviceFormDockerfile: "",
    serviceFormContextPath: "",
    serviceFormBuildStage: "",
    serviceFormArchivePort: "",
    giteaProviderId: "",
    giteaProviders: [],
    giteaRepoFullName: "",
    giteaRepos: [],
    giteaBranches: [],
    giteaMessage: "",
    archiveFile: null,
    archiveMessage: "",

    get isCompose() {
      return !!this.svc?.spec?.source?.Compose;
    },

    async initGeneralForm() {
      const spec = this.svc?.spec;
      if (!spec) return;
      this.editName = spec.name;
      if (spec.source.Compose) {
        this.composeText = spec.source.Compose.content || "";
        this.composeOriginal = this.composeText;
        return;
      }
      this.serviceFormGeneralPort = String(spec.port ?? "");
      this.serviceFormArchivePort = this.serviceFormGeneralPort;
      const git = spec.source.Git;
      if (git) {
        this.serviceFormRepoUrl = git.url || "";
        this.serviceFormBranch = git.branch || "main";
        this.serviceFormUsername = git.username || "";
        this.serviceFormCredentials = git.credentials || "";
        this.serviceFormBuildPath = git.root_path || ".";
        this.serviceFormWatchPaths = (git.watch_paths || []).join(", ");
        this.serviceFormSubmodules = !!git.submodules;
        this.serviceFormDockerfile = git.dockerfile_path || "Dockerfile";
        this.serviceFormContextPath = git.build_context || ".";
        this.serviceFormBuildStage = git.build_stage || "";
        this.giteaProviderId = git.provider_id || "";
        this.providerTab = git.provider_id ? "gitea" : "git";
        // Repõe a lista de contas (o picker não guarda full_name/branches no
        // spec — só url/branch já resolvidos, que os campos acima preenchem).
        if (this.providerTab === "gitea") await this.setProviderTab("gitea");
      } else {
        this.serviceFormRepoUrl = spec.source.Registry?.image || "";
        this.serviceFormBranch = "main";
        this.serviceFormUsername = "";
        this.serviceFormCredentials = "";
        this.serviceFormBuildPath = ".";
        this.serviceFormWatchPaths = "";
        this.serviceFormSubmodules = false;
        this.serviceFormDockerfile = "Dockerfile";
        this.serviceFormContextPath = ".";
        this.serviceFormBuildStage = "";
        this.giteaProviderId = "";
        this.providerTab = "git";
      }
    },

    /** O que o rename muda (ou não) para este tipo de serviço. */
    get renameNote() {
      return this.isCompose
        ? t("A stack e os volumes do Docker não mudam com o nome: os dados continuam os mesmos.")
        : t("O hostname interno (rp_<nome>) só passa a valer no próximo deploy. ") +
            t("Outros serviços que usam o nome antigo em variáveis de ambiente precisam ser atualizados.");
    },

    /** Unicidade dentro do projeto é checada no daemon (a mensagem volta no toast). */
    async renameService() {
      const name = this.editName.trim();
      if (!name) {
        this.store.toastError(t("Informe o nome do serviço"));
        return;
      }
      if (name === this.svc.spec.name) return;
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.name = name;
      const saveServiceSpecResult = await this.store.saveServiceSpec(spec, t("serviço renomeado"));
      if (saveServiceSpecResult.ok) this.editName = name;
    },

    async saveCompose() {
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.source = { Compose: { content: this.composeText } };
      await this.store.saveServiceSpec(spec, t("compose salvo"));
    },
    cancelCompose() {
      this.composeText = this.composeOriginal;
    },

    async setProviderTab(tab) {
      this.providerTab = tab;
      if (tab === "gitea" && this.giteaProviders.length === 0) {
        this.giteaMessage = t("carregando contas…");
        const gitProviderListResponse = await this.store.api.rpc("GitProviderList");
        if (gitProviderListResponse.ok && gitProviderListResponse.value?.GitProviders) {
          this.giteaProviders = gitProviderListResponse.value.GitProviders;
          this.giteaMessage = "";
        } else {
          this.giteaMessage = "";
          this.store.toastError(t("erro ao listar contas conectadas"));
        }
      }
    },

    async giteaProviderPick(providerId) {
      this.giteaProviderId = providerId || "";
      this.giteaRepoFullName = "";
      this.giteaRepos = [];
      this.giteaBranches = [];
      if (!this.giteaProviderId) return;
      this.giteaMessage = t("carregando repositórios…");
      const gitRepoListResponse = await this.store.api.rpc({ GitRepoList: { provider_id: this.giteaProviderId } });
      if (gitRepoListResponse.ok && gitRepoListResponse.value?.GitRepos) {
        this.giteaRepos = gitRepoListResponse.value.GitRepos;
        this.giteaMessage = t`${gitRepoListResponse.value.GitRepos.length} repositório(s)`;
      } else {
        this.giteaMessage = "";
        this.store.toastError(t("erro ao listar repositórios"));
      }
    },

    async giteaRepoPick(fullName) {
      if (!fullName) return;
      this.giteaRepoFullName = fullName;
      const repo = this.giteaRepos.find((giteaRepo) => giteaRepo.full_name === fullName);
      if (repo?.clone_url) this.serviceFormRepoUrl = repo.clone_url;
      if (repo?.default_branch) this.serviceFormBranch = repo.default_branch;
      this.giteaBranches = [];
      if (!this.giteaProviderId) return;
      this.giteaMessage = t("carregando branches…");
      const gitBranchListResponse = await this.store.api.rpc({
        GitBranchList: { provider_id: this.giteaProviderId, repo_full_name: fullName },
      });
      if (gitBranchListResponse.ok && gitBranchListResponse.value?.GitBranches) {
        this.giteaBranches = gitBranchListResponse.value.GitBranches;
        this.giteaMessage = "";
      } else {
        this.giteaMessage = "";
        this.store.toastError(t("erro ao listar branches"));
      }
    },

    /** Porta de handlers/services.luau::gen_save — mesma heurística
     * looksLikeGitUrl decide Git vs Registry quando a origem ainda não é Git. */
    async saveSource() {
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      const port = Number(this.serviceFormGeneralPort);
      if (Number.isFinite(port) && port > 0) spec.port = Math.floor(port);
      const repo = this.serviceFormRepoUrl.trim();
      const watch = this.serviceFormWatchPaths
        .split(",")
        .map((part) => part.trim())
        .filter(Boolean);
      const git = {
        Git: {
          url: repo,
          branch: this.serviceFormBranch.trim() || "main",
          root_path: this.serviceFormBuildPath.trim() || ".",
          watch_paths: watch,
          submodules: this.serviceFormSubmodules,
          dockerfile_path: this.serviceFormDockerfile.trim() || "Dockerfile",
          build_context: this.serviceFormContextPath.trim() || ".",
          build_stage: this.serviceFormBuildStage.trim() || null,
          credentials: this.serviceFormCredentials.trim() || null,
          username: this.serviceFormUsername.trim() || null,
          provider_id: this.giteaProviderId || null,
        },
      };
      if (spec.source.Git || looksLikeGitUrl(repo)) {
        spec.source = git;
      } else {
        spec.source = { Registry: { image: repo } };
      }
      await this.store.saveServiceSpec(spec, t("origem salva"));
    },

    onArchiveFileChange(event) {
      this.archiveFile = event.target.files?.[0] || null;
    },

    async uploadArchive() {
      if (!this.archiveFile) {
        this.store.toastWarn(t("selecione um arquivo .zip"));
        return;
      }
      this.archiveMessage = t("enviando zip…");
      const uploadArchiveResult = await this.store.api.uploadArchive(this.svc.id, this.archiveFile);
      if (uploadArchiveResult.ok) {
        this.archiveMessage = "";
        this.archiveFile = null;
        this.store.toastOk(t("zip enviado"));
        await this.store.fetchServiceDetail(this.svc.id);
        await this.store.refreshNow();
      } else {
        this.archiveMessage = "";
        this.store.toastError("erro: " + uploadArchiveResult.error);
      }
      if (this.serviceFormArchivePort) {
        const spec = JSON.parse(JSON.stringify(this.svc.spec));
        const port = Number(this.serviceFormArchivePort);
        if (Number.isFinite(port) && port > 0) {
          spec.port = Math.floor(port);
          await this.store.saveServiceSpec(spec);
        }
      }
    },

    // ── Connection ────────────────────────────────────────────────────
    copyMessage: "",
    async copyToClipboard(text) {
      try {
        await navigator.clipboard.writeText(text || "");
        this.copyMessage = "copiado!";
        setTimeout(() => (this.copyMessage = ""), 1500);
        this.store.toastOk("Copiado");
      } catch {
        this.store.toastError(t("não foi possível copiar"));
      }
    },

    /** Containers reais em execução para este serviço: só vêm no snapshot
     * (ManagedContainer[], anexado por http_api.rs::snapshot), não no
     * `Service` cru de `ServiceGet` — daí o cross-reference pelo id. */
    get liveContainers() {
      const store = this.store;
      const entry = ((store.snap && store.snap.services) || []).find((serviceEntry) => serviceEntry.service.id === this.svc?.id);
      const list = entry?.service.containers || [];
      const colorFor = (state) => {
        if (state === "running") return "ok";
        if (state === "restarting" || state === "created" || state === "paused") return "warn";
        return "muted";
      };
      return list.map((item) => ({
        id: item.id,
        idShort: (item.id || "").slice(0, 12),
        name: item.name || "—",
        state: item.state || "—",
        kind: colorFor(item.state),
      }));
    },

    get connectionInfo() {
      const service = this.svc;
      if (!service) return null;
      const spec = service.spec;
      const domain = spec.domains?.[0]?.domain || spec.domain || "";
      const tls = spec.domains?.[0]?.tls ?? spec.tls_enabled ?? false;
      const safe = safeName(spec.name);
      return {
        port: spec.port,
        hostPort: spec.host_port || "—",
        domain: domain || "—",
        tls: tls ? "enabled" : "disabled",
        databaseKind: spec.product || null,
        internalUrl: this.connectionUrl || "—",
        externalUrl: externalUrl(domain, tls, spec.host_port, spec.product, this.store.api.baseUrl, spec.env_vars),
      };
    },

    get envVars() {
      const service = this.svc;
      if (!service) return [];
      return envRowsWithComments(service.spec.env_vars, service.spec.env_comments);
    },

    async addEnvVar() {
      if (!this.newEnvKey.trim()) return;
      if (!confirmEnvKey(this.store, this.newEnvKey.trim(), this.svc.spec.env_vars)) return;
      // JSON round-trip (não structuredClone): this.svc.spec é um Proxy
      // reativo do Alpine — o clonador estrutural nativo do browser não
      // sabe copiá-lo (DataCloneError). O JSON round-trip descarta a
      // reatividade de forma segura, já que o ServiceSpec é sempre dados
      // planos (sem funções/Date/referências circulares).
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.env_vars = spec.env_vars.filter((envVar) => envVar.key !== this.newEnvKey.trim());
      spec.env_vars.push({ key: this.newEnvKey.trim(), value: { Plain: this.newEnvValue } });
      const saveServiceSpecResult = await this.store.saveServiceSpec(spec, t("variável salva"));
      if (saveServiceSpecResult.ok) {
        this.newEnvKey = "";
        this.newEnvValue = "";
      }
    },

    /** Nomes dos secrets do serviço aberto (`SecretList` com `service_id`). */
    async loadServiceSecrets() {
      const serviceId = this.svc?.id;
      const projectId = this.svc?.spec?.project_id;
      if (!serviceId || !projectId) {
        this.serviceSecrets = [];
        return;
      }
      try {
        const secretListResponse = await this.store.api.rpc({ SecretList: { project_id: projectId, service_id: serviceId } });
        if (this.svc?.id === serviceId) this.serviceSecrets = secretListResponse.ok ? secretListResponse.value?.SecretNames || [] : [];
      } catch (_) {
        this.serviceSecrets = [];
      }
    },

    /** 👁: pede o valor ao daemon (SecretReveal grava auditoria: quem, qual, quando; nunca o valor). */
    async revealServiceSecret(name) {
      if (!confirm(t`Ver o valor do secret "${name}"? O valor vai aparecer na tela e o acesso fica registrado na auditoria do servidor (quem, qual secret e quando; nunca o valor).`)) return;
      const secretRevealResponse = await this.store.api.rpc({
        SecretReveal: { project_id: this.svc.spec.project_id, service_id: this.svc.id, name },
      });
      const value = secretRevealResponse.ok ? secretRevealResponse.value?.SecretValue : undefined;
      if (value === undefined) {
        this.store.toastError(secretRevealResponse.error || t("não foi possível ler o secret"));
        return;
      }
      this.revealedSecrets = { ...this.revealedSecrets, [name]: value };
    },
    hideServiceSecret(name) {
      const { [name]: _hidden, ...rest } = this.revealedSecrets;
      this.revealedSecrets = rest;
    },
    copyServiceSecret(name) {
      return this.store.copyText(this.revealedSecrets[name] ?? "", t("valor"));
    },

    /** Cria (ou sobrescreve) um secret do serviço; o novo valor vale no próximo deploy. */
    async addServiceSecret() {
      const name = this.newServiceSecretName.trim();
      if (!name || !this.newServiceSecretValue) {
        this.store.toastError(t("Preencha nome e valor do secret"));
        return;
      }
      const secretSetResponse = await this.store.api.rpcChecked({
        SecretSet: { project_id: this.svc.spec.project_id, service_id: this.svc.id, name, value: this.newServiceSecretValue },
      });
      this.store.toastResult(secretSetResponse, t`secret ${name} salvo — vale no próximo deploy`);
      if (secretSetResponse.ok) {
        this.newServiceSecretName = "";
        this.newServiceSecretValue = "";
        await this.loadServiceSecrets();
      }
    },

    async deleteServiceSecret(name) {
      if (!confirm(t`Remover o secret "${name}"? Serviços que o referenciam vão falhar no próximo deploy.`)) return;
      const secretDeleteResponse = await this.store.api.rpcChecked({
        SecretDelete: { project_id: this.svc.spec.project_id, service_id: this.svc.id, name },
      });
      this.store.toastResult(secretDeleteResponse, t`secret ${name} removido`);
      if (secretDeleteResponse.ok) await this.loadServiceSecrets();
    },

    /** "🔒" numa variável em claro: o valor passa a ser guardado cifrado e a variável vira uma
     * referência ao secret de mesmo nome (o valor não muda). */
    async envToSecret(key) {
      const secretMoveResponse = await this.store.api.rpcChecked({
        SecretMove: { project_id: this.svc.spec.project_id, service_id: this.svc.id, name: key, op: "EnvToSecret" },
      });
      this.store.toastResult(secretMoveResponse, t`variável ${key} movida para secret`);
      if (secretMoveResponse.ok) {
        await this.store.fetchServiceDetail(this.svc.id);
        await this.store.refreshNow();
        await this.loadServiceSecrets();
      }
    },

    /** ✏️: renomeia a chave de uma variável (`kind` "env") ou o nome de um secret ("secret") DESTE
     * serviço, sem o valor passar pela tela. Ver `projectRename` em project_detail.js. */
    async serviceRename(kind, from) {
      const answer = prompt(t`Renomear "${from}" para:`, from);
      const to = answer === null ? "" : answer.trim();
      if (!to || to === from) return;
      const body = { project_id: this.svc.spec.project_id, service_id: this.svc.id, from, to };
      const renameResponse = await this.store.api.rpcChecked(
        kind === "secret" ? { SecretRename: body } : { EnvKeyRename: body },
      );
      this.store.toastResult(renameResponse, t`renomeado: ${from} → ${to}`);
      if (renameResponse.ok) {
        await this.store.fetchServiceDetail(this.svc.id);
        await this.store.refreshNow();
        await this.loadServiceSecrets();
      }
    },

    async serviceMove(op, name, doneMessage) {
      const secretMoveResponse = await this.store.api.rpcChecked({
        SecretMove: { project_id: this.svc.spec.project_id, service_id: this.svc.id, name, op },
      });
      this.store.toastResult(secretMoveResponse, doneMessage);
      if (secretMoveResponse.ok) {
        await this.store.fetchServiceDetail(this.svc.id);
        await this.store.refreshNow();
        await this.loadServiceSecrets();
      }
    },
    /** 🔓: volta a ser variável em claro (o valor deixa de ser cifrado). */
    serviceSecretToEnv(name) {
      if (!confirm(t`O valor de "${name}" deixa de ser cifrado e passa a aparecer em claro na variável (e no estado versionado). Continuar?`)) return;
      return this.serviceMove("SecretToEnv", name, t`secret ${name} movido para variável`);
    },
    /** ↑: do serviço para o projeto (todos os serviços do projeto passam a enxergar). */
    serviceSecretToProject(name) {
      if (!confirm(t`O secret "${name}" passa a valer para todos os serviços deste projeto. Continuar?`)) return;
      return this.serviceMove("ServiceSecretToProject", name, t`secret ${name} movido para o projeto`);
    },
    envToProject(key) {
      if (!confirm(t`A variável "${key}" passa a valer para todos os serviços deste projeto. Continuar?`)) return;
      return this.serviceMove("EnvServiceToProject", key, t`variável ${key} movida para o projeto`);
    },

    async deleteEnvVar(key) {
      // JSON round-trip (não structuredClone): this.svc.spec é um Proxy
      // reativo do Alpine — o clonador estrutural nativo do browser não
      // sabe copiá-lo (DataCloneError). O JSON round-trip descarta a
      // reatividade de forma segura, já que o ServiceSpec é sempre dados
      // planos (sem funções/Date/referências circulares).
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.env_vars = spec.env_vars.filter((envVar) => envVar.key !== key);
      await this.store.saveServiceSpec(spec, t("variável removida"));
    },

    // ── Editor .env de texto (toggle "Exportar"/".env") ───────────────
    envTextOpen: false,
    envText: "",
    openEnvText() {
      this.envText = dotenvFromVars(this.svc.spec.env_vars, this.svc.spec.env_comments);
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
      // JSON round-trip (não structuredClone): this.svc.spec é um Proxy
      // reativo do Alpine — o clonador estrutural nativo do browser não
      // sabe copiá-lo (DataCloneError). O JSON round-trip descarta a
      // reatividade de forma segura, já que o ServiceSpec é sempre dados
      // planos (sem funções/Date/referências circulares).
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.env_vars = vars;
      spec.env_comments = comments;
      const saveServiceSpecResult = await this.store.saveServiceSpec(spec, t("variáveis salvas"));
      if (saveServiceSpecResult.ok) this.envTextOpen = false;
    },

    get domains() {
      const service = this.svc;
      if (!service) return [];
      return (service.spec.domains || []).map((domain) => ({
        domain: domain.domain,
        port: domain.port ?? service.spec.port,
        tls: !!domain.tls,
      }));
    },

    async addDomain() {
      if (!this.newDomain.trim()) return;
      // JSON round-trip (não structuredClone): this.svc.spec é um Proxy
      // reativo do Alpine — o clonador estrutural nativo do browser não
      // sabe copiá-lo (DataCloneError). O JSON round-trip descarta a
      // reatividade de forma segura, já que o ServiceSpec é sempre dados
      // planos (sem funções/Date/referências circulares).
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.domains = spec.domains || [];
      spec.domains.push({
        domain: this.newDomain.trim(),
        port: this.newDomainPort ? Number(this.newDomainPort) : null,
        tls: this.newDomainTls,
      });
      const saveServiceSpecResult = await this.store.saveServiceSpec(spec, t("domínio adicionado"));
      if (saveServiceSpecResult.ok) {
        this.newDomain = "";
        this.newDomainPort = "";
        this.newDomainTls = false;
      }
    },

    async deleteDomain(domain) {
      // JSON round-trip (não structuredClone): this.svc.spec é um Proxy
      // reativo do Alpine — o clonador estrutural nativo do browser não
      // sabe copiá-lo (DataCloneError). O JSON round-trip descarta a
      // reatividade de forma segura, já que o ServiceSpec é sempre dados
      // planos (sem funções/Date/referências circulares).
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.domains = (spec.domains || []).filter((route) => route.domain !== domain);
      await this.store.saveServiceSpec(spec, t("domínio removido"));
    },

    // ── Healthcheck ───────────────────────────────────────────────────
    healthcheckKind: "none", // "none" | "tcp" | "http" | "docker"
    healthcheckPath: "",
    healthcheckStatus: "200",
    healthcheckInterval: "5",
    healthcheckTimeout: "3",
    healthcheckRetries: "10",
    healthcheckStart: "5",

    /** Popula o form local a partir do spec atual — chamado ao abrir a aba
     * (o form é editável, não reativo direto ao spec, então precisa de um
     * ponto explícito de sincronização). */
    initHealthcheckForm() {
      const healthcheck = this.svc?.spec?.healthcheck;
      if (!healthcheck) return;
      if (typeof healthcheck.kind === "string") {
        this.healthcheckKind = healthcheck.kind === "DockerNative" ? "docker" : healthcheck.kind.toLowerCase();
      } else if (healthcheck.kind?.Http) {
        this.healthcheckKind = "http";
        this.healthcheckPath = healthcheck.kind.Http.path;
        this.healthcheckStatus = String(healthcheck.kind.Http.expected_status);
      }
      this.healthcheckInterval = String(healthcheck.interval_secs);
      this.healthcheckTimeout = String(healthcheck.timeout_secs);
      this.healthcheckRetries = String(healthcheck.retries);
      this.healthcheckStart = String(healthcheck.start_period_secs);
    },

    async saveHealthcheck() {
      let kind;
      if (this.healthcheckKind === "http") {
        kind = { Http: { path: this.healthcheckPath.trim() || "/", expected_status: Number(this.healthcheckStatus) || 200 } };
      } else if (this.healthcheckKind === "docker") {
        kind = "DockerNative";
      } else if (this.healthcheckKind === "tcp") {
        kind = "Tcp";
      } else {
        kind = "None";
      }
      const currentHealthcheck = this.svc.spec.healthcheck;
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.healthcheck = {
        kind,
        interval_secs: Number(this.healthcheckInterval) || currentHealthcheck.interval_secs,
        timeout_secs: Number(this.healthcheckTimeout) || currentHealthcheck.timeout_secs,
        retries: Number(this.healthcheckRetries) || currentHealthcheck.retries,
        start_period_secs: Number(this.healthcheckStart) || currentHealthcheck.start_period_secs,
      };
      await this.store.saveServiceSpec(spec, t("healthcheck salvo"));
    },

    // ── Advanced ──────────────────────────────────────────────────────
    advancedReplicas: "1",
    advancedRunCommand: "",
    advancedAutostart: true,
    advancedDependencyAddId: "",

    initAdvancedForm() {
      const spec = this.svc?.spec;
      if (!spec) return;
      this.advancedReplicas = String(spec.replicas || 1);
      this.advancedRunCommand = spec.run_command || "";
      this.advancedAutostart = spec.autostart !== false;
      this.advancedDependencyAddId = "";
      this.advancedPreDeployCheckAddJobId = "";
    },

    get runArgsText() {
      const args = this.svc?.spec?.run_args || [];
      return args.length ? args.join(" ") : "(nenhum)";
    },

    async saveAdvanced() {
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      let replicas = Number(this.advancedReplicas) || 1;
      if (replicas < 1) replicas = 1;
      spec.replicas = Math.floor(replicas);
      const runCommand = this.advancedRunCommand.trim();
      spec.run_command = runCommand || null;
      spec.autostart = this.advancedAutostart;
      await this.store.saveServiceSpec(spec, t("advanced salvo"));
    },

    // ── Dependências (spec.depends_on: ids de serviços do MESMO projeto) ───────
    // Efeito imediato (como a fila de pré-deploy): cada add/remove já salva. O daemon valida (sem ciclo,
    // só serviços do mesmo projeto) e a recusa aparece no toast.
    get projectServicesForDeps() {
      const projectId = this.svc?.spec?.project_id;
      return ((this.store.snap && this.store.snap.services) || [])
        .map((entry) => entry.service)
        .filter((service) => service.spec.project_id === projectId);
    },
    get dependencyRows() {
      const nameById = new Map(this.projectServicesForDeps.map((service) => [service.id, service.spec.name]));
      return (this.svc?.spec?.depends_on || []).map((dependencyId, index) => ({
        id: dependencyId,
        index: index + 1,
        name: nameById.get(dependencyId) || t`(serviço removido: ${dependencyId})`,
      }));
    },
    get dependencyOptions() {
      const already = new Set(this.svc?.spec?.depends_on || []);
      return this.projectServicesForDeps
        .filter((service) => service.id !== this.svc?.id && !already.has(service.id))
        .map((service) => ({ id: service.id, name: service.spec.name }));
    },
    async dependencyAdd() {
      const dependencyId = this.advancedDependencyAddId;
      if (!dependencyId) return;
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.depends_on = [...(spec.depends_on || []).filter((existingId) => existingId !== dependencyId), dependencyId];
      const saveServiceSpecResult = await this.store.saveServiceSpec(spec, t("dependência adicionada"));
      if (saveServiceSpecResult.ok) this.advancedDependencyAddId = "";
    },
    async dependencyDelete(dependencyId) {
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.depends_on = (spec.depends_on || []).filter((existingId) => existingId !== dependencyId);
      await this.store.saveServiceSpec(spec, t("dependência removida"));
    },

    // ── Fila de pré-deploy check ────────────────────────────────────────
    // Efeito imediato (como Domains), não passa pelo Save acima — cada
    // add/remove/reorder já salva na hora. Ver docs/plano-pre-deploy-gate.md.

    // Fila efetiva de ids: `pre_deploy_job_ids` quando não vazia, senão cai
    // no `pre_deploy_job_id` legado (retrocompat) — mesmo idioma de
    // `ServiceSpec::pre_deploy_checks` no lado Rust.
    get preDeployQueueIds() {
      const spec = this.svc?.spec;
      if (!spec) return [];
      if (spec.pre_deploy_job_ids && spec.pre_deploy_job_ids.length > 0) {
        return spec.pre_deploy_job_ids;
      }
      if (spec.pre_deploy_job_id) return [spec.pre_deploy_job_id];
      return [];
    },

    // Fila com nome resolvido a partir dos Jobs do projeto (vem do snapshot
    // já em memória, mesmo padrão de `servicesFiltered` em schedules.js). Um
    // job apagado por fora mas ainda referenciado aparece com rótulo
    // "(job removido)" em vez de sumir, pra dar pra removê-lo da fila.
    get preDeployChecks() {
      const byId = new Map(
        (this.store.snap?.jobs || []).map((job) => [job.job.id, job.job.name])
      );
      return this.preDeployQueueIds.map((preDeployQueueId, index) => ({
        id: preDeployQueueId,
        index: index + 1,
        name: byId.get(preDeployQueueId) || t`(job removido: ${preDeployQueueId})`,
      }));
    },

    // Jobs do projeto que AINDA NÃO estão na fila — opções do seletor
    // "adicionar à fila" (evita duplicar o mesmo job na sequência).
    get preDeployAvailableJobs() {
      const projectId = this.svc?.spec?.project_id;
      if (!projectId) return [];
      const inQueue = new Set(this.preDeployQueueIds);
      return (this.store.snap?.jobs || [])
        .filter((job) => job.job.project_id === projectId && !inQueue.has(job.job.id))
        .map((job) => ({ id: job.job.id, name: job.job.name }));
    },

    advancedPreDeployCheckAddJobId: "",

    async preDeployCheckAdd() {
      const jobId = this.advancedPreDeployCheckAddJobId;
      if (!jobId) return;
      const jobIds = this.preDeployQueueIds.slice();
      if (!jobIds.includes(jobId)) jobIds.push(jobId);
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.pre_deploy_job_ids = jobIds;
      spec.pre_deploy_job_id = null;
      const saveServiceSpecResult = await this.store.saveServiceSpec(spec, t("check adicionado à fila"));
      if (saveServiceSpecResult.ok) this.advancedPreDeployCheckAddJobId = "";
    },

    async preDeployCheckDelete(jobId) {
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.pre_deploy_job_ids = this.preDeployQueueIds.filter((preDeployQueueId) => preDeployQueueId !== jobId);
      spec.pre_deploy_job_id = null;
      await this.store.saveServiceSpec(spec, t("check removido da fila"));
    },

    // Sem drag-and-drop na web UI (sem lib de DnD): reordena com botões
    // mover-pra-cima/baixo — mesmo resultado final do arraste na GUI iced.
    async preDeployCheckMove(jobId, delta) {
      const jobIds = this.preDeployQueueIds.slice();
      const fromIndex = jobIds.indexOf(jobId);
      const toIndex = fromIndex + delta;
      if (fromIndex < 0 || toIndex < 0 || toIndex >= jobIds.length) return;
      [jobIds[fromIndex], jobIds[toIndex]] = [jobIds[toIndex], jobIds[fromIndex]];
      const spec = JSON.parse(JSON.stringify(this.svc.spec));
      spec.pre_deploy_job_ids = jobIds;
      spec.pre_deploy_job_id = null;
      await this.store.saveServiceSpec(spec, t("fila reordenada"));
    },

    get deployments() {
      return (this.store.serviceDeployments || []).map((serviceDeployment) => {
        const [label, kind] = stateLabelKind(serviceDeployment.state);
        return {
          id: serviceDeployment.id,
          image: serviceDeployment.image,
          stateLabel: label,
          stateKind: kind,
          duration: formatDuration(serviceDeployment),
          start: dateDayMonthHourMinuteSecond(serviceDeployment.started_at),
          terminal: serviceDeployment.state === "Live" || serviceDeployment.state === "Failed" || serviceDeployment.state === "Stopped",
        };
      });
    },

    async viewBuildLog(deploymentId) {
      const getBuildLogsResponse = await this.store.api.rpc({ GetBuildLogs: { deployment_id: deploymentId } });
      if (getBuildLogsResponse.ok && getBuildLogsResponse.value?.BuildLogs) {
        // `docker build` também manda cores ANSI — mesmo tratamento dos logs
        // de runtime (ver app.js::cleanLogEntry).
        this.buildLogText = getBuildLogsResponse.value.BuildLogs.map((buildLog) => stripAnsi(buildLog.line)).join("\n");
        this.buildLogFor = deploymentId;
      }
    },
    closeBuildLog() {
      this.buildLogFor = null;
      this.buildLogText = "";
    },

    async abortDeployment(deploymentId) {
      await this.store.deployAbort(deploymentId);
    },
    async removeDeployment(deploymentId) {
      await this.store.deleteDeployment(deploymentId);
    },
  }));
});
