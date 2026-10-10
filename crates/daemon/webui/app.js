// app.js — único <script type="module"> carregado por index.html. Orquestra
// a ordem de boot do Alpine "na mão" (em vez do CDN clássico com `defer`):
// um `<script defer>` do Alpine e módulos ES separados entram na mesma fila
// de execução em ordem, mas plugins/CDNs alternativos podem correr fora de
// ordem — e um `Alpine.store`/`Alpine.data` registrado DEPOIS que o Alpine já
// disparou `alpine:init` nunca é visto (x-show/x-data leem `undefined` e a
// tela fica em branco). Aqui a ordem é garantida pelos próprios `import`
// (sempre resolvidos antes do corpo do módulo que os declara):
//   1. screens/*.js rodam primeiro — só REGISTRAM listeners de `alpine:init`.
//   2. Alpine é importado como módulo (não via `<script>` solto).
//   3. este módulo registra o `Alpine.store('app', …)` (a "ctx" global).
//   4. só então `Alpine.start()` — dispara `alpine:init`, todos os
//      listeners acima já registrados.
import "./screens/login.js";
import "./screens/dashboard.js";
import "./screens/deploy_engine.js";
import "./screens/monitoring.js";
import "./screens/ingress.js";
import "./screens/docker.js";
import "./screens/schedules.js";
import "./screens/settings.js";
import "./screens/projects.js";
import "./screens/project_detail.js";
import "./screens/new_service.js";
import "./screens/service_detail.js";
import "./screens/service_bundle.js";
import Alpine from "https://cdn.jsdelivr.net/npm/alpinejs@3.x.x/dist/module.esm.js";
import { Api } from "./net/api.js";
import { openStream } from "./net/sse.js";
import { registerDirectives } from "./directives.js";
import { registerWindows } from "./wm.js";
import { registerIcons } from "./icons.js";
import { registerBusy } from "./busy.js";
import {
  formatUptime,
  formatBytes,
  stripAnsi,
  oauthRedirectUri,
  timeHms,
  dateDayMonthHourMinuteSecond,
  dateDayMonthHourMinute,
  parseDotenv,
  dotenvFromVars,
  dockerCleanupLastRunSummary,
  shortReason,
  hourMinuteJoin,
  hourMinuteSplit,
} from "./format.js";

window.Alpine = Alpine;
registerDirectives(Alpine);
registerWindows(Alpine);
registerIcons(Alpine);
registerBusy(Alpine);

const PREFS_KEY = "rustploy.prefs";

function loadPrefs() {
  try {
    return JSON.parse(localStorage.getItem(PREFS_KEY) || "{}");
  } catch {
    return {};
  }
}

function savePrefs(preferences) {
  localStorage.setItem(PREFS_KEY, JSON.stringify(preferences));
}

document.addEventListener("alpine:init", () => {
  const prefs = loadPrefs();

  Alpine.store("app", {
    // ── Sessão / navegação (≈ ctx.screen/ctx.view do glacier) ────────────
    screen: "login",
    view: "deployments",
    menuOpen: true,
    connected: false,
    statusLine: "pronto para conectar",
    error: "",

    // ── Formulário de login ───────────────────────────────────────────
    // Sem campo de servidor: a webui é sempre servida pelo próprio daemon
    // (ver net/api.js), então o servidor é a própria origem da página.
    token: prefs.rememberToken && prefs.token ? prefs.token : "",
    rememberToken: !!prefs.rememberToken,

    // ── Sessão ativa ─────────────────────────────────────────────────
    api: null,
    stream: null,
    search: "",

    // ── Dados do snapshot (≈ State.snap) ────────────────────────────
    snap: null,
    dataLoading: true,
    daemonVersion: "",
    daemonUptime: "…",
    servicesLabel: "…",
    deploymentsMessage: "",

    // ── Eventos ao vivo do bus (≈ State.metrics_by_id / ctx.sys_*) ─────
    // Só chegam por /api/events kind="bus" — o snapshot periódico (2s) não
    // carrega métricas por container nem do host (ver onStreamEvent).
    metricsById: {}, // service_id -> ContainerMetricsPoint
    hostCpu: "—",
    hostMemory: "—",
    hostDisk: "—",
    hostLoad: "—",

    // ── Docker / Registry (aba Docker) ──────────────────────────────────
    dockerTab: "containers",
    onlyUsedImages: false,
    onlyUsedVolumes: false,
    onlyUsedNetworks: false,
    pruneAllImages: false,
    pruneAllVolumes: false,
    registrySelectedRepo: "",
    registryTags: [],
    registryTagsLoading: false,
    registryTokens: [],

    // ── Schedules (jobs one-shot) ────────────────────────────────────────
    jobsById: {}, // id -> Job cru (porta de State.jobs_by_id, precisa do
    // record completo pra reenviar em JobUpdate — só `enabled` muda)
    jobLogLines: [],
    jobLogStream: null,
    jobsInflight: {}, // id -> true entre o click em "Rodar agora" e o
    // refreshNow() que segue (trava double-click durante o round-trip da
    // RPC — depois disso o botão passa a refletir `running` do snapshot).

    // Wizard "novo job" (3 passos: projeto → serviço gatilho → form) — mora
    // no store (não num componente de tela) porque duas entradas abrem o
    // MESMO wizard: a tela global "Schedules" e a aba "Jobs" do projeto
    // (equivalente ao new_job_window do cliente iced, aberto por ambos
    // handlers/jobs.luau::open_new_job_window e a aba do projeto). Modal
    // renderizado uma vez em index.html, fora do escopo de qualquer tela.
    showNewJob: false,
    // Janelas (wm.js) "Novo projeto/Editar projeto" e "Novo serviço": estado no
    // store porque várias telas as abrem (grid de projetos, tela do projeto).
    // `projectWin` = null (fechada) | {} (novo) | {id,name,description} (editar).
    projectWin: null,
    showNewService: false,
    exportWin: null, // janela "Exportar serviço" (screens/service_bundle.js): null | { serviceId }
    logWin: false, // janela de logs ao vivo do serviço aberto (wm.js)
    newJobStep: "pick_project", // "pick_project" | "pick_service" | "form"
    // Modo edição: aberto por openEditJob(id) em vez do fluxo normal — pula
    // pro passo "form" (project_id/trigger_service_id não são editáveis via
    // JobUpdate, então não há passos 1/2). `null` = criando um job novo.
    newJobEditId: null,
    newJobEnabled: true,
    newJobProjectId: "",
    newJobProjectName: "",
    newJobServiceId: "",
    newJobServiceName: "",
    newJobName: "",
    newJobCompose: "",
    // Fonte do compose: "compose" (colado, default) | "git" (clona um repo a
    // cada execução, mesmo picker conta→repo→branch da aba Git de serviço —
    // ver newJobSetSourceTab/newJobGitProviderPick/newJobGitRepoPick).
    newJobSourceTab: "compose",
    newJobGitProviderId: "",
    newJobGitProviders: [],
    newJobGitRepos: [],
    newJobGitRepoFullName: "",
    newJobGitBranches: [],
    newJobGitBranch: "",
    newJobComposePath: "docker-compose.yml",
    newJobGitMessage: "",
    newJobMainService: "",
    // Env vars próprias do job — maior precedência (por cima de projeto +
    // serviço gatilho). Blob .env colado, parseado em newJobCreate.
    newJobEnvText: "",
    newJobKind: "manual", // "manual" | "interval" | "daily" | "weekly"
    newJobHours: "6",
    // Uma chave só, "HH:MM": quem edita agora é o <input type="time">.
    // hourMinuteJoin/hourMinuteSplit fazem a ponte com o {hour, minute} do daemon.
    newJobTime: "03:00",
    newJobWeekday: "0",
    newJobError: "",
    newJobSubmitting: false,

    // Modal de logs ao vivo de uma execução de job — mesma razão do wizard
    // acima (aberto tanto da tela Schedules quanto da aba Jobs do projeto).
    jobLogsFor: null,

    // ── Settings (Web Server / Git / Infra as Code) ─────────────────────
    settingsTab: "web",
    serverSettingsPublicBase: "",
    serverSettingsEmail: "",
    serverSettingsRegistryDomain: "",
    settingsMessage: "",
    gitProviders: [],
    gitProviderKind: "gitea", // "gitea" | "github"
    gitProviderMode: "oauth", // "oauth" | "pat"
    gitProviderName: "",
    gitProviderBaseUrl: "",
    gitProviderClientId: "",
    gitProviderClientSecret: "",
    gitProviderPersonalAccessToken: "",
    gitProviderMessage: "",
    gitProviderOauthUrl: "",
    manifestYaml: "",
    manifestDotenv: "",
    manifestHasExport: false,
    manifestExportMessage: "",
    manifestImportYaml: "",
    manifestImportDotenv: "",
    manifestPrune: false,
    manifestDeploy: false,
    manifestHasMissing: false,
    manifestMissingVars: "",
    manifestHasReport: false,
    manifestReportLines: [],
    manifestImportMessage: "",

    // Settings → Manutenção (limpeza automática de Docker) — ver
    // docs/plano-limpeza-automatica-docker.md. Porta de handlers/settings.luau
    // (dc_*). `dockerCleanupLastRunAtRaw` é um campo "mudo" (não exibido, só round-trip):
    // preserva o `last_run_at` real entre saves pra um agendamento
    // IntervalHours não reiniciar a contagem toda vez que o usuário salva.
    dockerCleanupEnabled: false,
    dockerCleanupKind: "daily", // "interval" | "daily" | "weekly"
    dockerCleanupHours: "6",
    dockerCleanupTime: "03:00",
    dockerCleanupWeekday: "0",
    dockerCleanupContainers: false,
    dockerCleanupImages: false,
    dockerCleanupImagesAll: false,
    dockerCleanupVolumes: false,
    dockerCleanupVolumesAll: false,
    dockerCleanupNetworks: false,
    dockerCleanupBuildCache: false,
    dockerCleanupNextRunLabel: "—",
    dockerCleanupLastRunAtRaw: null,
    dockerCleanupLastRunText: "ainda não rodou",
    dockerCleanupRunning: false,
    dockerCleanupMessage: "",

    get gitProviderRedirect() {
      return oauthRedirectUri(this.serverSettingsPublicBase, this.gitProviderKind);
    },

    // ── Navegação (≈ ctx.view do glacier) ───────────────────────────
    // "deployments" | "projects" | "project" | "service" | "new_service"
    view: "deployments",
    selectedProjectId: null,
    selectedServiceId: null,

    // Serviço cujo deploy foi disparado DESTA aba (ver applyBusEvent):
    // sobrevive à navegação, ao contrário de `selectedServiceId`.
    deployTrackId: null,

    // ── Detalhe de serviço aberto ────────────────────────────────────
    serviceDetail: null, // Service (ServiceGet)
    serviceDeployments: [], // Vec<Deployment> (DeployHistory)
    serviceTab: "general",
    serviceMessage: "",
    serviceLoading: false,
    serviceLogLines: [],
    serviceLogStream: null,

    // ── Toasts ────────────────────────────────────────────────────────
    // O par do `toast{ message, kind }` do glacier-ui na GUI desktop: mesmos
    // quatro kinds, mesma duração de 4s, empilhados no canto inferior
    // direito. Existe porque metade das ações da webui só escrevia `*Msg`
    // numa linha da tela — e em quase toda ação bem-sucedida essa linha
    // recebia `""`, ou seja, silêncio total: o único jeito de saber se o
    // clique funcionou era ver a lista mudar.
    //
    // Mensagem de PROGRESSO ("salvando…", "carregando repositórios…")
    // continua inline no `*Msg` da tela. Toast some sozinho; estado, não.
    toasts: [],
    toastSeq: 0,

    toast(message, kind = "info", durationMs = 4000) {
      if (!message) return null;
      const toastId = ++this.toastSeq;
      this.toasts.push({ id: toastId, kind, message: String(message), ms: durationMs });
      setTimeout(() => this.dismissToast(toastId), durationMs);
      return toastId;
    },

    toastOk(message) {
      return this.toast(message, "success");
    },
    toastError(message) {
      return this.toast(message, "error");
    },
    toastWarn(message) {
      return this.toast(message, "warning");
    },

    // Desfecho de um rpc num passo só: `okMessage` omitido = sucesso
    // silencioso (a tela já mostra o resultado, um toast seria ruído).
    // Devolve `r.ok` para caber num `if`.
    toastResult(result, okMessage) {
      if (result.ok) {
        if (okMessage) this.toastOk(okMessage);
      } else {
        this.toastError("erro: " + result.error);
      }
      return result.ok;
    },

    dismissToast(toastId) {
      const toastIndex = this.toasts.findIndex((toast) => toast.id === toastId);
      if (toastIndex !== -1) this.toasts.splice(toastIndex, 1);
    },

    serviceNameById(serviceId) {
      const entry = (this.snap?.services || []).find((service) => service.service.id === serviceId);
      return entry?.service?.spec?.name || "serviço";
    },

    nav(view) {
      this.stopServiceLogs();
      this.view = view;
    },

    persistPrefs() {
      savePrefs({
        rememberToken: this.rememberToken,
        token: this.rememberToken ? this.token : undefined,
      });
    },

    async connect() {
      // Sem servidor pra digitar: a webui só existe servida pelo próprio
      // daemon, então a origem da página É o servidor.
      const base = window.location.origin;
      this.persistPrefs();
      this.statusLine = "conectando…";
      this.error = "";

      const client = new Api(base, this.token);
      const daemonStatusResponse = await client.rpc("DaemonStatus");
      if (!daemonStatusResponse.ok) {
        this.error = daemonStatusResponse.error;
        this.statusLine = "falha na conexão";
        this.connected = false;
        return;
      }

      this.api = client;
      this.connected = true;
      this.statusLine = "conectado";
      this.screen = "shell";
      this.dataLoading = true;
      await this.loadSettings();
      this.openStream();
    },

    disconnect() {
      this.stopServiceLogs();
      this.stream?.close();
      this.stream = null;
      this.api = null;
      this.snap = null;
      this.connected = false;
      this.screen = "login";
      this.statusLine = "desconectado";
      this.view = "deployments";
      this.selectedProjectId = null;
      this.selectedServiceId = null;
      this.serviceDetail = null;
    },

    openStream() {
      this.stream?.close();
      this.stream = openStream(this.api.baseUrl, this.api.token, "/api/events", {
        onEvent: (kind, data) => this.onStreamEvent(data),
        onError: (message) => {
          this.statusLine = "stream: " + message;
        },
        onClose: () => {
          this.stream = null;
          if (this.connected) {
            this.statusLine = "conexão encerrada";
            this.connected = false;
            this.screen = "login";
          }
        },
      });
    },

    onStreamEvent(streamMessage) {
      if (!streamMessage || typeof streamMessage !== "object") return;
      if (streamMessage.kind === "snapshot") this.applySnapshot(streamMessage);
      else if (streamMessage.kind === "bus") this.applyBusEvent(streamMessage.event);
    },

    /** Um evento do bus (mesmo formato que stream.luau::apply_bus trata) —
     * unit variants chegam como string crua (ex. "DeployQueueChanged"). */
    applyBusEvent(busEvent) {
      if (busEvent === "DeployQueueChanged") {
        this.refreshNow();
        return;
      }
      if (!busEvent || typeof busEvent !== "object") return;
      if (busEvent.DockerCleanupCompleted) {
        const cleanupEvent = busEvent.DockerCleanupCompleted;
        this.dockerCleanupRunning = false;
        this.dockerCleanupLastRunText = dockerCleanupLastRunSummary({ at: cleanupEvent.at, results: cleanupEvent.results });
        this.dockerCleanupLastRunAtRaw = cleanupEvent.at;
      } else if (busEvent.ContainerMetrics) {
        const containerMetrics = busEvent.ContainerMetrics;
        if (containerMetrics.service_id) this.metricsById[containerMetrics.service_id] = containerMetrics;
      } else if (busEvent.SystemMetrics) {
        const systemMetrics = busEvent.SystemMetrics;
        this.hostCpu = `${(systemMetrics.cpu_percent || 0).toFixed(0)}%`;
        this.hostMemory = `${formatBytes(systemMetrics.mem_used_bytes)} / ${formatBytes(systemMetrics.mem_total_bytes)}`;
        this.hostDisk = `${formatBytes(systemMetrics.disk_used_bytes)} / ${formatBytes(systemMetrics.disk_total_bytes)}`;
        this.hostLoad = `${(systemMetrics.load_avg_1 || 0).toFixed(2)} ${(systemMetrics.load_avg_5 || 0).toFixed(2)} ${(systemMetrics.load_avg_15 || 0).toFixed(2)}`;
      } else if (busEvent.DeployStateChanged) {
        // O evento sempre carregou `message` — o MOTIVO da falha (texto do
        // docker build, healthcheck que não passou…) — e nenhum dos dois
        // clientes o lia. Sem isto, a tela do serviço só via a linha do
        // deployment virar `Failed` no snapshot de 2s, sem hipótese nenhuma
        // do porquê. Ver docs/plano-erro-de-deploy-invisivel.md.
        const deployEvent = busEvent.DeployStateChanged;
        const terminal =
          deployEvent.state === "Live" || deployEvent.state === "Failed" || deployEvent.state === "Stopped";
        const motivo = shortReason(deployEvent.message);
        if (deployEvent.service_id && deployEvent.service_id === this.selectedServiceId) {
          if (terminal) {
            this.serviceMessage =
              deployEvent.state === "Live"
                ? "deploy concluído"
                : motivo
                  ? `deploy falhou: ${motivo}`
                  : `deploy: ${deployEvent.state}`;
            this.fetchServiceDetail(deployEvent.service_id);
          } else {
            this.serviceMessage = `deploy · ${deployEvent.state}`;
          }
        }
        // Desfecho do deploy que ESTE usuário disparou, mesmo que ele já
        // tenha saído da tela do serviço — quem manda deployar vai fazer
        // outra coisa enquanto o build roda. A GUI desktop resolve isso com
        // notificação do SO mais toast; no browser ficamos no toast (uma
        // Notification exigiria pedir permissão, que ninguém pediu).
        if (terminal && deployEvent.service_id && deployEvent.service_id === this.deployTrackId) {
          this.deployTrackId = null;
          const nome = this.serviceNameById(deployEvent.service_id);
          if (deployEvent.state === "Live") {
            this.toastOk(`${nome}: deploy concluído`);
          } else {
            this.toastError(`${nome}: ${motivo || "deploy " + deployEvent.state}`);
          }
        }
      }
    },

    applySnapshot(snapshot) {
      this.snap = snapshot;
      const daemonStatus = snapshot.status;
      if (daemonStatus) {
        this.daemonVersion = daemonStatus.version || "";
        this.daemonUptime = formatUptime(daemonStatus.uptime_secs);
        this.servicesLabel = `${daemonStatus.services_running || 0}/${daemonStatus.services_total || 0}`;
      }
      const byId = {};
      for (const job of snapshot.jobs || []) byId[job.job.id] = job.job;
      this.jobsById = byId;
      this.dataLoading = false;
    },

    searchChanged(searchText) {
      this.search = searchText || "";
    },

    async stopAll() {
      if (!confirm("Parar todos os serviços? Todos os serviços em execução serão parados agora.")) {
        return;
      }
      if (!this.api) return;
      this.statusLine = "parando todos…";
      const stopAllManagedResponse = await this.api.rpcChecked("StopAllManaged");
      this.statusLine = stopAllManagedResponse.ok ? "todos os serviços parados" : "falha ao parar";
      this.toastResult(stopAllManagedResponse, "todos os serviços parados");
    },

    async clearFinished() {
      if (!this.snap) return;
      const deploymentIds = (this.snap.deployments || [])
        .map((deployment) => deployment.deployment)
        .filter((deployment) => deployment.state === "Stopped" || deployment.state === "Failed")
        .map((deployment) => deployment.id);
      if (deploymentIds.length === 0) {
        this.toastWarn("nada para limpar");
        return;
      }
      if (
        !confirm(
          `Remove do histórico ${deploymentIds.length} deployment(s) em estado Stopped ou Failed, e seus build logs. Ação irreversível.`
        )
      ) {
        return;
      }
      this.deploymentsMessage = `removendo ${deploymentIds.length} deployment(s)…`;
      let removed = 0,
        failed = 0;
      for (const deploymentId of deploymentIds) {
        const deployDeleteResponse = await this.api.rpcChecked({ DeployDelete: { deployment_id: deploymentId } });
        if (deployDeleteResponse.ok) removed++;
        else failed++;
      }
      this.deploymentsMessage = "";
      if (failed === 0) this.toastOk(`${removed} deployment(s) removido(s)`);
      else this.toastError(`${removed} removido(s), ${failed} falharam`);
    },

    async refreshNow() {
      if (!this.api) return;
      const snapshotResponse = await this.api.rpc("Snapshot");
      if (snapshotResponse.ok && snapshotResponse.value && typeof snapshotResponse.value.Snapshot === "string") {
        try {
          this.applySnapshot(JSON.parse(snapshotResponse.value.Snapshot));
        } catch {
          /* snapshot malformado — ignora, o próximo tick de 2s corrige */
        }
      }
    },

    // ── Projects ─────────────────────────────────────────────────────

    async createProject(name, description) {
      if (!name || !name.trim()) return { ok: false, error: "nome obrigatório" };
      const projectCreateResponse = await this.api.rpcChecked({
        ProjectCreate: { name: name.trim(), description: description?.trim() || null },
      });
      if (projectCreateResponse.ok) {
        this.toastOk("projeto criado");
        await this.refreshNow();
      }
      return projectCreateResponse;
    },

    async updateProject(projectId, name, description) {
      const projectUpdateResponse = await this.api.rpcChecked({
        ProjectUpdate: { id: projectId, name: name.trim(), description: description?.trim() || null },
      });
      this.toastResult(projectUpdateResponse, "projeto atualizado");
      if (projectUpdateResponse.ok) await this.refreshNow();
      return projectUpdateResponse;
    },

    async deleteProject(projectId) {
      if (!confirm("Remover este projeto? Só funciona se não houver serviços nele.")) {
        return { ok: false, error: "cancelado" };
      }
      const projectDeleteResponse = await this.api.rpcChecked({ ProjectDelete: { id: projectId } });
      this.toastResult(projectDeleteResponse, "projeto removido");
      if (projectDeleteResponse.ok) {
        await this.refreshNow();
        this.nav("projects");
      }
      return projectDeleteResponse;
    },

    openProject(projectId) {
      this.selectedProjectId = projectId;
      this.nav("project");
    },

    async saveProjectEnv(envVars, envComments) {
      const projectEnvSetResponse = await this.api.rpcChecked({
        ProjectEnvSet: {
          project_id: this.selectedProjectId,
          env_vars: envVars,
          env_comments: envComments,
        },
      });
      this.toastResult(projectEnvSetResponse, "variáveis salvas");
      if (projectEnvSetResponse.ok) await this.refreshNow();
      return projectEnvSetResponse;
    },

    async addSecret(name, value) {
      if (!name.trim()) return { ok: false, error: "nome obrigatório" };
      const secretSetResponse = await this.api.rpcChecked({
        SecretSet: { project_id: this.selectedProjectId, name: name.trim(), value },
      });
      this.toastResult(secretSetResponse, `secret ${name.trim()} salvo`);
      if (secretSetResponse.ok) await this.refreshNow();
      return secretSetResponse;
    },

    async deleteSecret(name) {
      if (!confirm(`Remover o secret "${name}"? Serviços que o referenciam vão falhar no próximo deploy.`)) {
        return;
      }
      const secretDeleteResponse = await this.api.rpcChecked({
        SecretDelete: { project_id: this.selectedProjectId, name },
      });
      this.toastResult(secretDeleteResponse, `secret ${name} removido`);
      if (secretDeleteResponse.ok) await this.refreshNow();
    },

    // ── Services ─────────────────────────────────────────────────────

    /** `step` opcional: "import" abre direto no passo Importar (ícone do card). */
    openNewService(step) {
      window.dispatchEvent(new CustomEvent("newservice-reset", { detail: { step } }));
      this.showNewService = true;
    },
    closeNewService() {
      this.showNewService = false;
    },
    openExportWin(serviceId) {
      this.exportWin = { serviceId };
    },
    closeExportWin() {
      this.exportWin = null;
    },
    openProjectWin(project) {
      this.projectWin = project ? { id: project.id, name: project.name, description: project.description || "" } : {};
    },
    closeProjectWin() {
      this.projectWin = null;
    },

    /** Catálogos do wizard (bancos/brokers/templates) — buscados uma vez ao
     * abrir a tela "Novo serviço" (ver screens/new_service.js). */
    async fetchWizardCatalog(search) {
      const wizardCatalogResponse = await this.api.rpc({ WizardCatalog: { search: search || "" } });
      if (!wizardCatalogResponse.ok || !wizardCatalogResponse.value?.WizardCatalog) return { dbs: [], brokers: [], templates: [] };
      const catalog = wizardCatalogResponse.value.WizardCatalog;
      try {
        return {
          dbs: JSON.parse(catalog.dbs),
          brokers: JSON.parse(catalog.brokers),
          templates: JSON.parse(catalog.templates),
        };
      } catch {
        return { dbs: [], brokers: [], templates: [] };
      }
    },

    /** `req` é o `WizardCreateReq` completo (ver screens/new_service.js) — o
     * daemon (shared::wizard::build_spec) monta o ServiceSpec certo conforme
     * `req.kind`. Usado por Compose/Database/Broker/Template: o backend gera
     * o compose e as env vars corretas (mesma lógica do client iced). */
    async wizardCreate(createRequest) {
      const wizardCreateResponse = await this.api.rpcChecked({ WizardCreate: createRequest });
      if (wizardCreateResponse.ok) {
        this.toastOk("serviço criado");
        await this.refreshNow();
        this.closeNewService();
        const created = wizardCreateResponse.value?.Service;
        if (created) this.openService(created.id);
        else this.nav("project");
      }
      return wizardCreateResponse;
    },

    /** `source` já é o `ServiceSource` externally-tagged. Usado só pelo tipo
     * "Application" do wizard: diferente dos outros 4 tipos, o
     * `WizardCreateReq` não carrega URL de Git nem imagem de registry — o
     * wizard iced cria um serviço vazio (`Registry{image:""}`) e deixa o
     * usuário preencher depois na aba General. Aqui coletamos a origem já na
     * criação (mais direto), então vamos por `ServiceCreate` puro. */
    async createServiceDirect(name, source, port, domain) {
      const spec = {
        name: name.trim(),
        project_id: this.selectedProjectId,
        source,
        port: Number(port) || 80,
        host_port: null,
        domain: domain?.trim() || null,
        tls_enabled: false,
        env_vars: [],
        env_comments: [],
        volumes: [],
        healthcheck: {
          kind: "None",
          interval_secs: 5,
          timeout_secs: 3,
          retries: 10,
          start_period_secs: 5,
        },
        replicas: 1,
        resources: { cpu_shares: 0, mem_limit_bytes: 0 },
        run_command: null,
        run_args: [],
        db_kind: null,
        domains: [],
      };
      const serviceCreateResponse = await this.api.rpcChecked({ ServiceCreate: spec });
      if (serviceCreateResponse.ok) {
        this.toastOk("serviço criado");
        await this.refreshNow();
        this.closeNewService();
        const created = serviceCreateResponse.value?.Service;
        if (created) this.openService(created.id);
        else this.nav("project");
      }
      return serviceCreateResponse;
    },

    async openService(serviceId) {
      this.selectedServiceId = serviceId;
      this.serviceTab = "general";
      this.serviceMessage = "";
      this.serviceDeployments = [];
      this.serviceLoading = true;
      this.nav("service");
      await this.fetchServiceDetail(serviceId);
    },

    async fetchServiceDetail(serviceId) {
      const serviceGetResponse = await this.api.rpc({ ServiceGet: { id: serviceId } });
      if (!serviceGetResponse.ok || !serviceGetResponse.value?.Service) {
        this.serviceMessage = serviceGetResponse.ok ? "serviço não encontrado" : serviceGetResponse.error;
        this.serviceLoading = false;
        return;
      }
      this.serviceDetail = serviceGetResponse.value.Service;
      const deployHistoryResponse = await this.api.rpc({ DeployHistory: { service_id: serviceId, limit: 30 } });
      this.serviceDeployments = (deployHistoryResponse.ok && deployHistoryResponse.value?.Deployments) || [];
      this.serviceLoading = false;
    },

    async saveServiceSpec(spec, okMessage) {
      const serviceId = this.selectedServiceId;
      const serviceUpdateResponse = await this.api.rpcChecked({ ServiceUpdate: { id: serviceId, spec } });
      if (serviceUpdateResponse.ok) {
        this.toastOk(okMessage || "salvo");
        await this.fetchServiceDetail(serviceId);
        await this.refreshNow();
      } else {
        this.toastError("erro: " + serviceUpdateResponse.error);
      }
      return serviceUpdateResponse;
    },

    async deleteService(serviceId) {
      if (!confirm("Remover este serviço? Para o container e apaga o histórico. Ação irreversível.")) {
        return;
      }
      const serviceDeleteResponse = await this.api.rpcChecked({ ServiceDelete: { id: serviceId } });
      if (serviceDeleteResponse.ok) {
        await this.refreshNow();
        this.nav("project");
        this.toastOk("serviço removido");
      } else {
        this.toastError("erro ao remover: " + serviceDeleteResponse.error);
      }
    },

    /** Botão "Parar" do card de serviço (grid do projeto) — porta de
     * handlers/projects.luau::svc_stop_id. */
    async stopService(serviceId) {
      if (!confirm("Parar serviço? O tráfego para ele será interrompido até um novo deploy.")) {
        return;
      }
      const serviceStopResponse = await this.api.rpcChecked({ ServiceStop: { service_id: serviceId } });
      this.toastResult(serviceStopResponse, "serviço parado");
      await this.refreshNow();
    },

    /** Botão "Remover" do card de serviço (grid do projeto) — porta de
     * handlers/projects.luau::stop_delete_service. `ServiceDelete` sozinho
     * NÃO para o container (o handler só apaga linhas do DB e rotas de
     * ingress — ver service_delete.rs), então parar primeiro evita deixar
     * um container órfão rodando fora do controle do rustploy. */
    async stopAndDeleteService(serviceId) {
      const deploymentJobs = (this.snap?.jobs || [])
        .filter((job) => job.job.trigger_service_id === serviceId)
        .map((job) => job.job.name)
        .sort();
      let message = "O serviço será parado e removido.";
      if (deploymentJobs.length > 0) {
        message += ` Os jobs de Schedules que dependem dele também serão removidos, com histórico: ${deploymentJobs.join(", ")}.`;
      }
      message += " Essa ação não pode ser desfeita.";
      if (!confirm(message)) return;

      const serviceStopResponse = await this.api.rpcChecked({ ServiceStop: { service_id: serviceId } });
      if (!serviceStopResponse.ok) {
        this.toastError("erro ao parar: " + serviceStopResponse.error);
        return;
      }
      const serviceDeleteResponse = await this.api.rpcChecked({ ServiceDelete: { id: serviceId } });
      if (serviceDeleteResponse.ok) this.toastOk("serviço parado e removido");
      else this.toastError("erro ao remover: " + serviceDeleteResponse.error);
      await this.refreshNow();
    },

    async deployStart() {
      const serviceId = this.selectedServiceId;
      this.serviceMessage = "iniciando deploy…";
      const deployStartResponse = await this.api.rpcChecked({ DeployStart: { service_id: serviceId } });
      this.serviceMessage = "";
      // Guarda quem o USUÁRIO mandou deployar: o desfecho (DeployStateChanged)
      // vira toast mesmo que ele já tenha navegado para outra tela — é o par
      // do `State.deploy_track` da GUI desktop.
      if (deployStartResponse.ok) this.deployTrackId = serviceId;
      this.toastResult(deployStartResponse, "deploy iniciado");
      await this.fetchServiceDetail(serviceId);
      await this.refreshNow();
    },

    async deployAbort(deploymentId) {
      const deployAbortResponse = await this.api.rpcChecked({ DeployAbort: { deployment_id: deploymentId } });
      this.toastResult(deployAbortResponse, "deploy cancelado");
      await this.fetchServiceDetail(this.selectedServiceId);
    },

    async deployRollback() {
      if (!confirm("Reverter para o deploy anterior?")) return;
      const serviceId = this.selectedServiceId;
      const deployRollbackResponse = await this.api.rpcChecked({ DeployRollback: { service_id: serviceId } });
      if (deployRollbackResponse.ok) this.deployTrackId = serviceId;
      this.toastResult(deployRollbackResponse, "rollback iniciado");
      await this.fetchServiceDetail(serviceId);
    },

    async deleteDeployment(deploymentId) {
      const deployDeleteResponse = await this.api.rpcChecked({ DeployDelete: { deployment_id: deploymentId } });
      this.toastResult(deployDeleteResponse, "deployment removido");
      if (deployDeleteResponse.ok) await this.fetchServiceDetail(this.selectedServiceId);
      return deployDeleteResponse;
    },

    // ── Deploy Engine (fila global) ─────────────────────────────────────
    // Porta de handlers/deploy_queue.luau — sem drag-and-drop (só glacier);
    // "↑ topo" e "✕" cobrem furar fila / desistir de um item.

    /** Cancela um deploy que ainda espera na fila — reaproveita DeployAbort
     * (o daemon já trata remoção da fila como caso do abort). */
    async queueCancel(deploymentId) {
      const deployAbortResponse = await this.api.rpcChecked({ DeployAbort: { deployment_id: deploymentId } });
      this.toastResult(deployAbortResponse, "deploy cancelado");
      await this.refreshNow();
    },

    async queuePromote(deploymentId) {
      const deployQueuePromoteResponse = await this.api.rpcChecked({ DeployQueuePromote: { deployment_id: deploymentId } });
      this.toastResult(deployQueuePromoteResponse, "deploy movido para o topo da fila");
      await this.refreshNow();
    },

    async queueReorder(order) {
      const deployQueueReorderResponse = await this.api.rpcChecked({ DeployQueueReorder: { order } });
      this.toastResult(deployQueueReorderResponse, "fila reordenada");
      await this.refreshNow();
    },

    async queueTogglePause() {
      const paused = !!this.snap?.engine?.paused;
      const deployQueuePauseResponse = await this.api.rpcChecked({ DeployQueuePause: { paused: !paused } });
      this.toastResult(deployQueuePauseResponse, paused ? "fila retomada" : "fila pausada");
      await this.refreshNow();
    },

    // ── Docker (host-wide) ──────────────────────────────────────────────
    // Porta de handlers/docker.luau — cada prune/remove segue o mesmo padrão:
    // rpcChecked + toast do desfecho + refreshNow() pra refletir na hora
    // (o snapshot de 2s pegaria de qualquer jeito, mas assim fica imediato).

    /** Toast de um prune, com o Response::PruneResult{count,reclaimed_bytes}
     * quando o daemon o devolve; sem esse payload (algum prune que responde
     * só Ok), mensagem genérica. */
    toastPrune(result) {
      if (!result.ok) return this.toastError("erro: " + result.error);
      const pruneResult = result.value?.PruneResult;
      return this.toastOk(
        pruneResult
          ? `removidos: ${pruneResult.count} · ${formatBytes(pruneResult.reclaimed_bytes)} liberados`
          : "limpeza concluída"
      );
    },

    async dockerPruneContainers() {
      const pruneContainersResponse = await this.api.rpcChecked("PruneContainers");
      this.toastPrune(pruneContainersResponse);
      await this.refreshNow();
    },
    async dockerPruneImages() {
      const pruneImagesResponse = await this.api.rpcChecked({ PruneImages: { all: this.pruneAllImages } });
      this.toastPrune(pruneImagesResponse);
      await this.refreshNow();
    },
    async dockerPruneVolumes() {
      const pruneVolumesResponse = await this.api.rpcChecked({ PruneVolumes: { all: this.pruneAllVolumes } });
      this.toastPrune(pruneVolumesResponse);
      await this.refreshNow();
    },
    async dockerPruneNetworks() {
      const pruneNetworksResponse = await this.api.rpcChecked("PruneNetworks");
      this.toastPrune(pruneNetworksResponse);
      await this.refreshNow();
    },

    async dockerRemoveContainer(containerId) {
      const removeContainerResponse = await this.api.rpcChecked({ RemoveContainer: { id: containerId } });
      this.toastResult(removeContainerResponse, "removido");
      if (removeContainerResponse.ok) await this.refreshNow();
    },
    async dockerRemoveImage(imageId) {
      const removeImageResponse = await this.api.rpcChecked({ RemoveImage: { id: imageId } });
      this.toastResult(removeImageResponse, "removido");
      if (removeImageResponse.ok) await this.refreshNow();
    },
    async dockerRemoveVolume(name) {
      const removeVolumeResponse = await this.api.rpcChecked({ RemoveVolume: { name } });
      this.toastResult(removeVolumeResponse, "removido");
      if (removeVolumeResponse.ok) await this.refreshNow();
    },
    async dockerRemoveNetwork(networkId) {
      const removeNetworkResponse = await this.api.rpcChecked({ RemoveNetwork: { id: networkId } });
      this.toastResult(removeNetworkResponse, "removido");
      if (removeNetworkResponse.ok) await this.refreshNow();
    },

    /** Troca a sub-aba Docker; ao entrar em "registry" busca os tokens (não
     * vêm no snapshot periódico, diferente de repos/containers/imagens). */
    async dockerSetTab(tab) {
      this.dockerTab = tab;
      if (tab === "registry") await this.registryRefreshTokens();
    },

    // ── Registry OCI embutido ────────────────────────────────────────────

    async registryOpenRepo(name) {
      this.registrySelectedRepo = name;
      this.registryTagsLoading = true;
      const registryTagListResponse = await this.api.rpc({ RegistryTagList: { repo: name } });
      this.registryTags = (registryTagListResponse.ok && registryTagListResponse.value?.RegistryTags) || [];
      this.registryTagsLoading = false;
    },
    registryCloseRepo() {
      this.registrySelectedRepo = "";
      this.registryTags = [];
    },

    /** Sem rpcChecked de propósito (mesmo comportamento silencioso do Luau —
     * falha aqui não é acionável pelo usuário, só deixa a lista vazia). */
    async registryRefreshTokens() {
      const registryTokenListResponse = await this.api.rpc("RegistryTokenList");
      this.registryTokens = (registryTokenListResponse.ok && registryTokenListResponse.value?.RegistryTokens) || [];
    },

    async registryRemoveTag(tag) {
      const repo = this.registrySelectedRepo;
      const registryTagDeleteResponse = await this.api.rpcChecked({ RegistryTagDelete: { repo, tag } });
      this.toastResult(registryTagDeleteResponse, `tag ${tag} removida`);
      if (registryTagDeleteResponse.ok) {
        await this.registryOpenRepo(repo);
        await this.refreshNow();
      }
    },
    async registryRemoveRepo(name) {
      const registryRepoDeleteResponse = await this.api.rpcChecked({ RegistryRepoDelete: { repo: name } });
      this.toastResult(registryRepoDeleteResponse, `repositório ${name} removido`);
      if (registryRepoDeleteResponse.ok) {
        if (this.registrySelectedRepo === name) this.registryCloseRepo();
        await this.refreshNow();
      }
    },
    async registryGc() {
      const registryGcResponse = await this.api.rpcChecked("RegistryGc");
      if (registryGcResponse.ok) {
        const gcResult = registryGcResponse.value?.RegistryGcResult;
        this.toastOk(
          gcResult
            ? `GC: ${gcResult.blobs_removed} arquivo(s) removido(s) · ${formatBytes(gcResult.bytes_freed)} liberados`
            : "GC concluído"
        );
        await this.refreshNow();
      } else {
        this.toastError("erro: " + registryGcResponse.error);
      }
    },
    async registryRemoveToken(name) {
      const registryTokenRevokeResponse = await this.api.rpcChecked({ RegistryTokenRevoke: { name } });
      this.toastResult(registryTokenRevokeResponse, `token ${name} revogado`);
      if (registryTokenRevokeResponse.ok) await this.registryRefreshTokens();
    },

    /** Devolve {ok, secret} pro modal de "novo token" — o segredo só existe
     * nesta resposta, nunca mais é recuperável depois. */
    async registryCreateToken(name, scope) {
      const registryTokenCreateResponse = await this.api.rpcChecked({ RegistryTokenCreate: { name, scope } });
      if (!registryTokenCreateResponse.ok) return { ok: false, error: registryTokenCreateResponse.error };
      await this.registryRefreshTokens();
      return { ok: true, secret: registryTokenCreateResponse.value.RegistryTokenCreated.secret };
    },

    // ── Schedules (jobs one-shot) ────────────────────────────────────────
    // Porta de handlers/jobs.luau.

    async jobRunNow(jobId) {
      if (this.jobsInflight[jobId]) return;
      this.jobsInflight[jobId] = true;
      try {
        const jobRunNowResponse = await this.api.rpcChecked({ JobRunNow: { id: jobId } });
        this.toastResult(jobRunNowResponse, "job disparado");
        await this.refreshNow();
      } finally {
        delete this.jobsInflight[jobId];
      }
    },

    /** Cancela um job_run em execução (ver docs/plano-cancelamento-de-jobs.md):
     * mata o processo `docker compose up` de verdade no daemon, não só a UI.
     * `jobRunId` vem de `j.lastRunId` — enquanto `j.running` só reflete o
     * estado otimista de `jobsInflight` (antes do primeiro refresh confirmar
     * o job_run novo), pode ainda apontar pro run ANTERIOR; o daemon devolve
     * NotFound nesse caso (inofensivo, só a mensagem de erro). */
    async jobRunCancel(jobRunId) {
      if (!jobRunId) return;
      const jobRunCancelResponse = await this.api.rpcChecked({ JobRunCancel: { job_run_id: jobRunId } });
      this.toastResult(jobRunCancelResponse, "cancelamento solicitado");
      await this.refreshNow();
    },

    /** Reenvia o Job inteiro (só `enabled` inverte) — o daemon não tem um
     * PATCH parcial; mesma limitação do cliente iced. */
    async jobToggle(jobId) {
      const job = this.jobsById[jobId];
      if (!job) {
        this.toastWarn("job não encontrado no snapshot atual");
        return;
      }
      const jobUpdateResponse = await this.api.rpcChecked({
        JobUpdate: {
          id: job.id,
          name: job.name,
          compose: job.compose,
          git_source: job.git_source ?? null,
          main_service: job.main_service,
          env_vars: job.env_vars ?? [],
          env_comments: job.env_comments ?? [],
          enabled: !job.enabled,
          recurrence: job.recurrence,
        },
      });
      this.toastResult(jobUpdateResponse, job.enabled ? "job desativado" : "job ativado");
      await this.refreshNow();
    },

    async jobDelete(jobId) {
      if (!confirm("Remover este job? Ação irreversível.")) return;
      const jobDeleteResponse = await this.api.rpcChecked({ JobDelete: { id: jobId } });
      this.toastResult(jobDeleteResponse, "job removido");
      await this.refreshNow();
    },

    /** `payload` = { project_id, trigger_service_id, name, compose,
     * main_service, recurrence }. */
    async jobCreate(payload) {
      const jobCreateResponse = await this.api.rpcChecked({ JobCreate: payload });
      if (jobCreateResponse.ok) await this.refreshNow();
      return jobCreateResponse;
    },

    // ── Wizard "novo job" (ver campos no bloco de estado acima) ──────────
    // Porta de new_job_window.luau — mesmos passos e mesma validação.

    get newJobProjects() {
      return this.snap?.projects || [];
    },
    /** Serviços do projeto escolhido — já em memória (snap.services), sem
     * re-fetch (diferente do Luau, que precisa pré-semear a janela isolada). */
    get newJobServicesFiltered() {
      return (this.snap?.services || []).filter(
        (service) => service.service.spec.project_id === this.newJobProjectId
      );
    },

    openNewJob() {
      this.showNewJob = true;
      this.newJobStep = "pick_project";
      this.newJobEditId = null;
      this.newJobEnabled = true;
      this.newJobProjectId = "";
      this.newJobProjectName = "";
      this.newJobServiceId = "";
      this.newJobServiceName = "";
      this.newJobName = "";
      this.newJobCompose = "";
      this.newJobSourceTab = "compose";
      this.newJobGitProviderId = "";
      this.newJobGitProviders = [];
      this.newJobGitRepos = [];
      this.newJobGitRepoFullName = "";
      this.newJobGitBranches = [];
      this.newJobGitBranch = "";
      this.newJobComposePath = "docker-compose.yml";
      this.newJobGitMessage = "";
      this.newJobMainService = "";
      this.newJobEnvText = "";
      this.newJobKind = "manual";
      this.newJobHours = "6";
      this.newJobTime = "03:00";
      this.newJobWeekday = "0";
      this.newJobError = "";
    },
    closeNewJob() {
      this.showNewJob = false;
    },
    newJobPickProject(projectId, name) {
      this.newJobProjectId = projectId;
      this.newJobProjectName = name;
      this.newJobStep = "pick_service";
    },
    newJobPickService(serviceId, name) {
      this.newJobServiceId = serviceId;
      this.newJobServiceName = name;
      this.newJobStep = "form";
    },
    newJobPickNoService() {
      this.newJobServiceId = "";
      this.newJobServiceName = "nenhum (autônomo)";
      this.newJobStep = "form";
    },
    newJobBack() {
      // Modo edição: não há passos 1/2 pra voltar (project_id/
      // trigger_service_id não são editáveis via JobUpdate) — fecha o modal.
      if (this.newJobEditId) {
        this.closeNewJob();
        return;
      }
      if (this.newJobStep === "form") this.newJobStep = "pick_service";
      else if (this.newJobStep === "pick_service") this.newJobStep = "pick_project";
    },

    // ── Fonte do compose: aba "Compose" x aba "Git" (picker conta→repo→branch) ──
    // Porta de njob_source/njob_git_provider_pick/njob_git_repo_pick do
    // cliente iced (new_job_window.luau) — mesma resolução, sem a limitação
    // de janela isolada (aqui é só um fetch preguiçoso na primeira troca de aba).
    async newJobSetSourceTab(kind) {
      this.newJobSourceTab = kind;
      if (kind === "git" && this.newJobGitProviders.length === 0) {
        this.newJobGitMessage = "carregando contas…";
        const gitProviderListResponse = await this.api.rpc("GitProviderList");
        if (gitProviderListResponse.ok && gitProviderListResponse.value?.GitProviders) {
          this.newJobGitProviders = gitProviderListResponse.value.GitProviders;
          this.newJobGitMessage = "";
        } else {
          this.newJobGitMessage = "erro ao listar contas conectadas";
        }
      }
    },
    async newJobGitProviderPick(providerId) {
      this.newJobGitProviderId = providerId || "";
      this.newJobGitRepoFullName = "";
      this.newJobGitRepos = [];
      this.newJobGitBranches = [];
      if (!this.newJobGitProviderId) return;
      this.newJobGitMessage = "carregando repositórios…";
      const gitRepoListResponse = await this.api.rpc({ GitRepoList: { provider_id: this.newJobGitProviderId } });
      if (gitRepoListResponse.ok && gitRepoListResponse.value?.GitRepos) {
        this.newJobGitRepos = gitRepoListResponse.value.GitRepos;
        this.newJobGitMessage = `${gitRepoListResponse.value.GitRepos.length} repositório(s)`;
      } else {
        this.newJobGitMessage = "erro ao listar repositórios";
      }
    },
    async newJobGitRepoPick(fullName) {
      if (!fullName) return;
      this.newJobGitRepoFullName = fullName;
      const repo = this.newJobGitRepos.find((newJobGitRepo) => newJobGitRepo.full_name === fullName);
      if (repo?.default_branch) this.newJobGitBranch = repo.default_branch;
      this.newJobGitBranches = [];
      if (!this.newJobGitProviderId) return;
      this.newJobGitMessage = "carregando branches…";
      const gitBranchListResponse = await this.api.rpc({
        GitBranchList: { provider_id: this.newJobGitProviderId, repo_full_name: fullName },
      });
      if (gitBranchListResponse.ok && gitBranchListResponse.value?.GitBranches) {
        this.newJobGitBranches = gitBranchListResponse.value.GitBranches;
        this.newJobGitMessage = "";
      } else {
        this.newJobGitMessage = "erro ao listar branches";
      }
    },

    /** Monta `recurrence` (Option<Recurrence>, externally-tagged) a partir
     * de newJobKind. */
    buildNewJobRecurrence() {
      if (this.newJobKind === "interval") {
        return { IntervalHours: Math.max(1, Number(this.newJobHours) || 1) };
      }
      const [hora, minuto] = hourMinuteSplit(this.newJobTime);
      if (this.newJobKind === "daily") {
        return { Daily: { hour: hora, minute: minuto } };
      }
      if (this.newJobKind === "weekly") {
        return {
          Weekly: {
            weekday: Number(this.newJobWeekday) || 0,
            hour: hora,
            minute: minuto,
          },
        };
      }
      return null;
    },

    async newJobCreate() {
      if (!this.newJobName.trim() || !this.newJobMainService.trim()) {
        this.newJobError = "nome e main service são obrigatórios";
        return;
      }
      let compose = "";
      let gitSource = null;
      if (this.newJobSourceTab === "git") {
        const branch = this.newJobGitBranch.trim();
        if (!this.newJobGitRepoFullName || !branch) {
          this.newJobError = "selecione repositório e branch";
          return;
        }
        const composePath = this.newJobComposePath.trim() || "docker-compose.yml";
        const repo = this.newJobGitRepos.find((newJobGitRepo) => newJobGitRepo.full_name === this.newJobGitRepoFullName);
        if (!repo?.clone_url) {
          this.newJobError = "não foi possível resolver a URL do repositório — selecione de novo";
          return;
        }
        gitSource = {
          provider_id: this.newJobGitProviderId || null,
          url: repo.clone_url,
          branch,
          username: null,
          credentials: null,
          compose_path: composePath,
        };
      } else {
        compose = this.newJobCompose;
        if (!compose.trim()) {
          this.newJobError = "cole o docker-compose.yml do job";
          return;
        }
      }
      this.newJobError = "";
      this.newJobSubmitting = true;
      const { vars: envVars, comments: envComments } = parseDotenv(this.newJobEnvText);
      let saveResponse;
      if (this.newJobEditId) {
        saveResponse = await this.api.rpcChecked({
          JobUpdate: {
            id: this.newJobEditId,
            name: this.newJobName.trim(),
            compose,
            git_source: gitSource,
            main_service: this.newJobMainService.trim(),
            env_vars: envVars,
            env_comments: envComments,
            enabled: this.newJobEnabled,
            recurrence: this.buildNewJobRecurrence(),
          },
        });
        if (saveResponse.ok) await this.refreshNow();
      } else {
        saveResponse = await this.jobCreate({
          project_id: this.newJobProjectId,
          // "" (não null) = job autônomo — Command::JobCreate::trigger_service_id
          // é String simples no protocolo (não Option<String>); o handler no
          // daemon é quem converte "" → None (job_create.rs).
          trigger_service_id: this.newJobServiceId || "",
          name: this.newJobName.trim(),
          compose,
          git_source: gitSource,
          main_service: this.newJobMainService.trim(),
          env_vars: envVars,
          env_comments: envComments,
          recurrence: this.buildNewJobRecurrence(),
        });
      }
      this.newJobSubmitting = false;
      if (saveResponse.ok) {
        this.toastOk(this.newJobEditId ? "job atualizado" : "job criado");
        this.closeNewJob();
      } else {
        this.newJobError = saveResponse.error;
      }
    },

    /** Abre o mesmo modal do wizard, mas em modo edição: pula pro passo
     * "form" já preenchido com o job existente (project_id/trigger_service_id
     * não são editáveis via JobUpdate, então não há passos 1/2 aqui). Só
     * webui — sem a limitação de janela isolada do cliente iced, então
     * resolve a conta/repo/branch do git_source direto, sem pré-fetch. */
    async openEditJob(jobId) {
      const job = this.jobsById[jobId];
      if (!job) {
        this.toastWarn("job não encontrado no snapshot atual");
        return;
      }
      this.showNewJob = true;
      this.newJobEditId = jobId;
      this.newJobStep = "form";
      this.newJobError = "";
      this.newJobName = job.name || "";
      this.newJobMainService = job.main_service || "";
      this.newJobEnabled = !!job.enabled;
      this.newJobEnvText = dotenvFromVars(job.env_vars, job.env_comments);

      const recurrence = job.recurrence;
      if (recurrence?.IntervalHours != null) {
        this.newJobKind = "interval";
        this.newJobHours = String(recurrence.IntervalHours);
      } else if (recurrence?.Daily) {
        this.newJobKind = "daily";
        this.newJobTime = hourMinuteJoin(recurrence.Daily.hour, recurrence.Daily.minute);
      } else if (recurrence?.Weekly) {
        this.newJobKind = "weekly";
        this.newJobWeekday = String(recurrence.Weekly.weekday);
        this.newJobTime = hourMinuteJoin(recurrence.Weekly.hour, recurrence.Weekly.minute);
      } else {
        this.newJobKind = "manual";
      }

      if (job.git_source) {
        this.newJobSourceTab = "git";
        this.newJobCompose = "";
        this.newJobGitBranch = job.git_source.branch || "";
        this.newJobComposePath = job.git_source.compose_path || "docker-compose.yml";
        this.newJobGitProviderId = job.git_source.provider_id || "";
        if (this.newJobGitProviderId) {
          // O modal já está visível neste ponto (showNewJob=true lá em cima)
          // — sem esta mensagem os selects de repo/branch ficam vazios,
          // sem nenhuma explicação, durante os 2-3 round-trips abaixo.
          this.newJobGitMessage = "carregando repositórios…";
          const gitProviderListResponse = await this.api.rpc("GitProviderList");
          if (gitProviderListResponse.ok && gitProviderListResponse.value?.GitProviders) this.newJobGitProviders = gitProviderListResponse.value.GitProviders;
          const gitRepoListResponse = await this.api.rpc({ GitRepoList: { provider_id: this.newJobGitProviderId } });
          if (gitRepoListResponse.ok && gitRepoListResponse.value?.GitRepos) {
            this.newJobGitRepos = gitRepoListResponse.value.GitRepos;
            const match = this.newJobGitRepos.find((repo) => repo.clone_url === job.git_source.url);
            this.newJobGitRepoFullName = match?.full_name || "";
            if (this.newJobGitRepoFullName) {
              this.newJobGitMessage = "carregando branches…";
              const gitBranchListResponse = await this.api.rpc({
                GitBranchList: {
                  provider_id: this.newJobGitProviderId,
                  repo_full_name: this.newJobGitRepoFullName,
                },
              });
              if (gitBranchListResponse.ok && gitBranchListResponse.value?.GitBranches) this.newJobGitBranches = gitBranchListResponse.value.GitBranches;
            }
          }
          this.newJobGitMessage = "";
        }
      } else {
        this.newJobSourceTab = "compose";
        this.newJobCompose = job.compose || "";
      }
    },

    openJobLogs(jobRunId) {
      this.jobLogsFor = jobRunId;
      this.startJobLogs(jobRunId);
    },
    closeJobLogs() {
      this.jobLogsFor = null;
      this.stopJobLogs();
    },

    /** Logs ao vivo de UMA execução de job — mesmo par seed+SSE de
     * startServiceLogs/stopServiceLogs, apontando pro endpoint dedicado de
     * job run (gêmeo do de serviço: mesmo framing bus_batch). */
    async startJobLogs(jobRunId) {
      this.stopJobLogs();
      const seed = await this.api.rpc({ GetJobLogs: { job_run_id: jobRunId } });
      this.jobLogLines = ((seed.ok && seed.value?.JobLogs) || []).map(this.cleanLogEntry);
      this.jobLogStream = openStream(
        this.api.baseUrl,
        this.api.token,
        `/api/jobs/runs/${jobRunId}/logs`,
        {
          onEvent: (_kind, data) => {
            if (data?.kind === "bus_batch" && Array.isArray(data.events)) {
              for (const event of data.events) {
                const line = event?.JobLogLine;
                if (line) this.jobLogLines.push(this.cleanLogEntry(line));
              }
              const MAX = 2000;
              if (this.jobLogLines.length > MAX) {
                this.jobLogLines.splice(0, this.jobLogLines.length - MAX);
              }
            }
          },
        }
      );
    },
    stopJobLogs() {
      this.jobLogStream?.close();
      this.jobLogStream = null;
    },

    // ── Settings ──────────────────────────────────────────────────────────
    // Porta de handlers/settings.luau + a load_settings() de connection.luau.
    // Buscado uma vez na conexão (não a cada snapshot — senão apagaria o que
    // o usuário está digitando nos formulários).

    async loadSettings() {
      const getDaemonSettingsResponse = await this.api.rpc("GetDaemonSettings");
      if (getDaemonSettingsResponse.ok && getDaemonSettingsResponse.value?.DaemonSettings) {
        const settings = getDaemonSettingsResponse.value.DaemonSettings;
        this.serverSettingsPublicBase = settings.public_base_url || "";
        this.serverSettingsEmail = settings.acme_email || "";
        this.serverSettingsRegistryDomain = settings.registry_domain || "";
      }
      await this.gitProviderRefresh();
      await this.dockerCleanupLoad();
    },

    async settingsSave() {
      const email = this.serverSettingsEmail.trim();
      const registryDomain = this.serverSettingsRegistryDomain.trim();
      this.settingsMessage = "salvando…";
      const setDaemonSettingsResponse = await this.api.rpcChecked({
        SetDaemonSettings: {
          acme_email: email || null,
          registry_domain: registryDomain || null,
        },
      });
      this.settingsMessage = "";
      this.toastResult(setDaemonSettingsResponse, "configurações salvas");
    },

    async gitProviderRefresh() {
      const gitProviderListResponse = await this.api.rpc("GitProviderList");
      this.gitProviders = (gitProviderListResponse.ok && gitProviderListResponse.value?.GitProviders) || [];
    },

    /** Mesmas validações de handlers/settings.luau::gp_connect: GitHub cai
     * pro github.com se a Base URL vier vazia (só existe pra Enterprise);
     * Gitea sempre exige Base URL. Client id+secret OU PAT, conforme gitProviderMode. */
    async gitProviderConnect() {
      this.gitProviderOauthUrl = "";
      const isGithub = this.gitProviderKind === "github";
      const kindWire = isGithub ? "Github" : "Gitea";
      const label = isGithub ? "GitHub" : "Gitea";
      let base = this.gitProviderBaseUrl.trim();
      if (!base) {
        if (isGithub) base = "https://github.com";
        else {
          this.toastWarn("informe a Base URL do Gitea");
          return;
        }
      }
      const name = this.gitProviderName.trim() || label;
      const isOauth = this.gitProviderMode !== "pat";
      let providerCommand;
      if (isOauth) {
        const clientId = this.gitProviderClientId.trim();
        const csec = this.gitProviderClientSecret || "";
        if (!clientId || !csec.trim()) {
          this.toastWarn("Client ID e Client Secret são obrigatórios");
          return;
        }
        providerCommand = {
          GitProviderCreate: {
            kind: kindWire,
            name,
            base_url: base,
            auth_mode: "OAuth",
            oauth_client_id: clientId,
            oauth_client_secret: csec,
            pat: null,
          },
        };
      } else {
        const personalAccessToken = this.gitProviderPersonalAccessToken || "";
        if (!personalAccessToken.trim()) {
          this.toastWarn("informe o Personal Access Token");
          return;
        }
        providerCommand = {
          GitProviderCreate: {
            kind: kindWire,
            name,
            base_url: base,
            auth_mode: "Pat",
            oauth_client_id: null,
            oauth_client_secret: null,
            pat: personalAccessToken,
          },
        };
      }
      this.gitProviderMessage = "conectando…";
      const rpcResponse = await this.api.rpc(providerCommand);
      if (!rpcResponse.ok || !rpcResponse.value?.GitProviderInfo) {
        this.gitProviderMessage = "";
        this.toastError("erro: " + (rpcResponse.ok ? "resposta inesperada" : rpcResponse.error));
        return;
      }
      const providerId = rpcResponse.value.GitProviderInfo.id;
      if (isOauth) {
        // OAuth precisa de round-trip no navegador — diferente do Luau (que só
        // linkava por não saber abrir o browser), aqui abrimos direto.
        const gitOAuthStartResponse = await this.api.rpc({ GitOAuthStart: { provider_id: providerId } });
        if (gitOAuthStartResponse.ok && gitOAuthStartResponse.value?.OAuthUrl) {
          this.gitProviderOauthUrl = gitOAuthStartResponse.value.OAuthUrl;
          this.gitProviderMessage = "autorize a janela aberta e depois clique em Atualizar";
          window.open(gitOAuthStartResponse.value.OAuthUrl, "_blank");
        } else {
          this.gitProviderMessage = "provider criado; inicie o OAuth manualmente";
        }
      } else {
        this.gitProviderMessage = "";
        this.toastOk(`conta ${label} conectada`);
      }
      this.gitProviderName = "";
      this.gitProviderBaseUrl = "";
      this.gitProviderClientId = "";
      this.gitProviderClientSecret = "";
      this.gitProviderPersonalAccessToken = "";
      await this.gitProviderRefresh();
    },

    async gitProviderDelete(providerId) {
      const gitProviderDeleteResponse = await this.api.rpcChecked({ GitProviderDelete: { id: providerId } });
      this.gitProviderMessage = "";
      this.toastResult(gitProviderDeleteResponse, "provider removido");
      await this.gitProviderRefresh();
    },

    async manifestExport() {
      this.manifestExportMessage = "exportando…";
      const manifestExportAllResponse = await this.api.rpcChecked("ManifestExportAll");
      if (!manifestExportAllResponse.ok || !manifestExportAllResponse.value?.ManifestBundle) {
        this.manifestExportMessage = "";
        this.toastError("erro: " + (manifestExportAllResponse.ok ? "resposta inesperada" : manifestExportAllResponse.error));
        return;
      }
      this.manifestYaml = manifestExportAllResponse.value.ManifestBundle.yaml;
      this.manifestDotenv = manifestExportAllResponse.value.ManifestBundle.dotenv;
      this.manifestHasExport = true;
      this.manifestExportMessage = "";
      this.toastOk("manifesto exportado");
    },

    /** 3 formas de resposta possíveis (mesma distinção de handlers/
     * settings.luau::iac_import): MissingEnvVars (nada aplicado), Err
     * (rpc-level), ou ManifestReport (sucesso). */
    async manifestImport() {
      this.manifestHasMissing = false;
      this.manifestHasReport = false;
      this.manifestMissingVars = "";
      this.manifestReportLines = [];

      const yaml = this.manifestImportYaml || "";
      if (!yaml.trim()) {
        this.toastWarn("cole o YAML do manifesto");
        return;
      }

      this.manifestImportMessage = "importando…";
      const manifestImportResponse = await this.api.rpc({
        ManifestImport: {
          yaml,
          dotenv: this.manifestImportDotenv || "",
          prune: this.manifestPrune,
          deploy: this.manifestDeploy,
        },
      });
      if (!manifestImportResponse.ok) {
        this.manifestImportMessage = "";
        this.toastError("erro: " + manifestImportResponse.error);
        return;
      }
      const applyResult = manifestImportResponse.value;
      if (applyResult?.MissingEnvVars) {
        this.manifestHasMissing = true;
        this.manifestMissingVars = applyResult.MissingEnvVars.join(", ");
        this.manifestImportMessage = "faltam variáveis — nada foi aplicado";
        this.toastError("faltam variáveis — nada foi aplicado");
        return;
      }
      if (applyResult?.Err) {
        this.manifestImportMessage = "";
        this.toastError(`erro: ${applyResult.Err.code}: ${applyResult.Err.message}`);
        return;
      }
      if (!applyResult?.ManifestReport) {
        this.manifestImportMessage = "";
        this.toastError("resposta inesperada do daemon");
        return;
      }
      const lines = (applyResult.ManifestReport.actions || []).map(
        (action) => `[${action.action}] ${action.kind} ${action.name}`
      );
      if ((applyResult.ManifestReport.deployed || []).length > 0) {
        lines.push("deploy disparado: " + applyResult.ManifestReport.deployed.join(", "));
      }
      this.manifestReportLines = lines;
      this.manifestHasReport = true;
      this.manifestImportMessage = "";
      this.toastOk("import concluído");
      await this.refreshNow();
    },

    // ── Settings → Manutenção (limpeza automática de Docker) ────────────
    // Porta de handlers/settings.luau (dc_*) — ver
    // docs/plano-limpeza-automatica-docker.md. Config é um singleton do
    // daemon, carregada/salva inteira a cada Get/Set.

    /** Recorrência (Option<Recurrence>, externally-tagged) → campos do
     * formulário — mesmo formato do unpack usado ao editar um Job (acima,
     * perto de `newJobKind`). */
    dockerCleanupApplyConfig(config) {
      this.dockerCleanupEnabled = !!config.enabled;
      this.dockerCleanupContainers = !!config.containers;
      this.dockerCleanupImages = !!config.images;
      this.dockerCleanupImagesAll = !!config.images_all;
      this.dockerCleanupVolumes = !!config.volumes;
      this.dockerCleanupVolumesAll = !!config.volumes_all;
      this.dockerCleanupNetworks = !!config.networks;
      this.dockerCleanupBuildCache = !!config.build_cache;
      const recurrence = config.recurrence;
      if (recurrence && recurrence.IntervalHours != null) {
        this.dockerCleanupKind = "interval";
        this.dockerCleanupHours = String(recurrence.IntervalHours);
      } else if (recurrence && recurrence.Weekly) {
        this.dockerCleanupKind = "weekly";
        this.dockerCleanupTime = hourMinuteJoin(recurrence.Weekly.hour, recurrence.Weekly.minute);
        this.dockerCleanupWeekday = String(recurrence.Weekly.weekday);
      } else if (recurrence && recurrence.Daily) {
        this.dockerCleanupKind = "daily";
        this.dockerCleanupTime = hourMinuteJoin(recurrence.Daily.hour, recurrence.Daily.minute);
      } else {
        this.dockerCleanupKind = "daily";
      }
      this.dockerCleanupNextRunLabel = config.next_run_at ? dateDayMonthHourMinute(config.next_run_at) : "—";
      this.dockerCleanupLastRunAtRaw = config.last_run_at ?? null;
    },

    async dockerCleanupLoad() {
      const dockerCleanupConfigGetResponse = await this.api.rpc("DockerCleanupConfigGet");
      if (dockerCleanupConfigGetResponse.ok && dockerCleanupConfigGetResponse.value?.DockerCleanupConfig) {
        this.dockerCleanupApplyConfig(dockerCleanupConfigGetResponse.value.DockerCleanupConfig.config);
        this.dockerCleanupLastRunText = dockerCleanupLastRunSummary(dockerCleanupConfigGetResponse.value.DockerCleanupConfig.last_run);
      }
    },

    dockerCleanupBuildRecurrence() {
      if (this.dockerCleanupKind === "interval") {
        return { IntervalHours: Math.max(1, parseInt(this.dockerCleanupHours, 10) || 1) };
      }
      const [hora, minuto] = hourMinuteSplit(this.dockerCleanupTime);
      if (this.dockerCleanupKind === "weekly") {
        return {
          Weekly: {
            weekday: parseInt(this.dockerCleanupWeekday, 10) || 0,
            hour: hora,
            minute: minuto,
          },
        };
      }
      return {
        Daily: {
          hour: hora,
          minute: minuto,
        },
      };
    },

    async dockerCleanupSave() {
      this.dockerCleanupMessage = "salvando…";
      const dockerCleanupConfigSetResponse = await this.api.rpcChecked({
        DockerCleanupConfigSet: {
          config: {
            enabled: this.dockerCleanupEnabled,
            recurrence: this.dockerCleanupBuildRecurrence(),
            containers: this.dockerCleanupContainers,
            images: this.dockerCleanupImages,
            images_all: this.dockerCleanupImagesAll,
            volumes: this.dockerCleanupVolumes,
            volumes_all: this.dockerCleanupVolumesAll,
            networks: this.dockerCleanupNetworks,
            build_cache: this.dockerCleanupBuildCache,
            last_run_at: this.dockerCleanupLastRunAtRaw,
          },
        },
      });
      if (dockerCleanupConfigSetResponse.ok && dockerCleanupConfigSetResponse.value?.DockerCleanupConfig) {
        this.dockerCleanupApplyConfig(dockerCleanupConfigSetResponse.value.DockerCleanupConfig.config);
        this.dockerCleanupLastRunText = dockerCleanupLastRunSummary(dockerCleanupConfigSetResponse.value.DockerCleanupConfig.last_run);
        this.dockerCleanupMessage = "";
        this.toastOk("configurações salvas");
      } else {
        this.dockerCleanupMessage = "";
        this.toastError("erro: " + dockerCleanupConfigSetResponse.error);
      }
    },

    /** Botão "Executar agora": roda os recursos marcados fora do horário
     * agendado, independente do interruptor geral. Roda em background no
     * daemon — `dockerCleanupRunning` volta a `false` quando `Event::DockerCleanupCompleted`
     * chega (applyBusEvent), não pela resposta deste RPC. */
    async dockerCleanupRunNow() {
      const anySelected =
        this.dockerCleanupContainers || this.dockerCleanupImages || this.dockerCleanupVolumes || this.dockerCleanupNetworks || this.dockerCleanupBuildCache;
      if (!anySelected) {
        this.toastWarn("marque pelo menos um recurso");
        return;
      }
      if (
        !confirm(
          "Roda a limpeza dos recursos marcados imediatamente, fora do horário agendado. Ação irreversível."
        )
      ) {
        return;
      }
      this.dockerCleanupRunning = true;
      this.dockerCleanupMessage = "executando…";
      const dockerCleanupRunNowResponse = await this.api.rpcChecked("DockerCleanupRunNow");
      if (!dockerCleanupRunNowResponse.ok) {
        this.dockerCleanupRunning = false;
        this.dockerCleanupMessage = "";
        this.toastError("erro: " + dockerCleanupRunNowResponse.error);
      }
    },

    async serviceStop() {
      const serviceId = this.selectedServiceId;
      const serviceStopResponse = await this.api.rpcChecked({ ServiceStop: { service_id: serviceId } });
      this.toastResult(serviceStopResponse, "serviço parado");
      await this.fetchServiceDetail(serviceId);
      await this.refreshNow();
    },

    async serviceReload() {
      const serviceId = this.selectedServiceId;
      const serviceReloadResponse = await this.api.rpcChecked({ ServiceReload: { service_id: serviceId } });
      this.toastResult(serviceReloadResponse, "serviço recarregado");
      await this.fetchServiceDetail(serviceId);
    },

    // ── Logs (aba Logs do serviço) ───────────────────────────────────
    // Mesmo endpoint SSE dedicado do client iced (crates/daemon/src/api/
    // http_api.rs::service_logs), com o mesmo histórico inicial via LogsGet
    // (ver startServiceLogs).

    /** LogEntry/LogLine cru → { stream, line, timestamp } com ANSI limpo e
     * truncado (defesa contra uma linha absurda — ex. um dump binário sem
     * quebra — quebrar o layout inteiro da aba, como aconteceu antes de
     * filtrar ANSI: a barra de escape crua virava glifos de caixa e o texto
     * "empilhava" visualmente). Mesmo teto de format/service_detail.luau::
     * log_rows (LINE_MAX). */
    cleanLogEntry(logEntry) {
      const LINE_MAX = 4000;
      let line = stripAnsi(logEntry.line || "");
      if (line.length > LINE_MAX) line = line.slice(0, LINE_MAX) + "…";
      return { stream: logEntry.stream, line, timestamp: logEntry.timestamp };
    },

    /** Exposta no store (não só no módulo) porque o modal de logs de job é
     * global — ver bloco "Modais globais de Jobs" em index.html. */
    timeHms,

    async startServiceLogs() {
      this.stopServiceLogs();
      const serviceId = this.selectedServiceId;
      // Semeia o histórico ANTES de abrir o stream ao vivo — mesma ordem do
      // client iced (handlers/services.luau::open_logs_window): sem isso, um
      // serviço já rodando há tempo (sem stdout novo desde então) mostra a
      // aba vazia para sempre, mesmo tendo logs de sobra.
      const seed = await this.api.rpc({ LogsGet: { service_id: serviceId, tail: 500 } });
      this.serviceLogLines = ((seed.ok && seed.value?.Logs) || []).map(this.cleanLogEntry);
      // A troca de aba pode ter acontecido enquanto o fetch estava em voo —
      // se o usuário já saiu da aba Logs (ou do serviço), não abre a stream.
      if (this.serviceTab !== "logs" || this.selectedServiceId !== serviceId) return;
      this.serviceLogStream = openStream(
        this.api.baseUrl,
        this.api.token,
        `/api/services/${serviceId}/logs`,
        {
          onEvent: (_kind, data) => {
            if (data?.kind === "bus_batch" && Array.isArray(data.events)) {
              for (const event of data.events) {
                const line = event?.LogLine;
                if (line) this.serviceLogLines.push(this.cleanLogEntry(line));
              }
              const MAX = 2000;
              if (this.serviceLogLines.length > MAX) {
                this.serviceLogLines.splice(0, this.serviceLogLines.length - MAX);
              }
            }
          },
        }
      );
    },

    stopServiceLogs() {
      this.serviceLogStream?.close();
      this.serviceLogStream = null;
    },

    setServiceTab(tab) {
      this.serviceTab = tab;
      if (tab === "logs") this.startServiceLogs();
      else this.stopServiceLogs();
    },
  });
});

// Dispara `alpine:init` só agora — todos os listeners acima (login.js,
// dashboard.js, o Alpine.store deste arquivo) já estão registrados.
Alpine.start();

if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {
      /* PWA opcional — a app funciona normalmente sem o SW */
    });
  });
}
