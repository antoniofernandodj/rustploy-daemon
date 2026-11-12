// screens/job_env.js — janela "Variáveis do job": as envs e os secrets de UM job, com os mesmos
// botões da aba Environment do serviço e das variáveis do projeto (renomear, editar no lugar,
// cifrar, voltar para claro, subir para o projeto, ver/copiar o valor, editor `.env`).
//
// O escopo de job é `job_id` nos comandos de secret/env (EnvKeyRename, EnvValueSet, SecretSet,
// SecretMove…). A lista de envs viaja no próprio `Job` do snapshot; adicionar/remover/colar o
// `.env` reenvia o Job inteiro (`JobUpdate`), como o `jobToggle` do store.
//
// Toda operação que muda ou expõe uma env pede confirmação, nos três lugares (projeto, serviço e
// job) — exceto reordenar.
import {
  dotenvFromVars,
  parseDotenv,
  dotenvErrorsMessage,
  confirmEnvKey,
  envRowsWithComments,
  envValueFromDraft,
} from "../format.js";
import { t } from "../i18n.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("jobEnv", () => ({
    get store() {
      return Alpine.store("app");
    },

    newEnvKey: "",
    newEnvValue: "",
    envTextOpen: false,
    envText: "",
    envEdit: null,
    // Nomes dos secrets DESTE job (o valor nunca volta do daemon) e os que o usuário pediu para ver.
    jobSecrets: [],
    revealedSecrets: {},
    newSecretName: "",
    newSecretValue: "",

    init() {
      this.$watch(
        () => this.store.jobEnvJobId,
        () => {
          this.revealedSecrets = {};
          this.envEdit = null;
          this.envTextOpen = false;
          this.loadSecrets();
        }
      );
    },

    get job() {
      const jobId = this.store.jobEnvJobId;
      return jobId ? this.store.jobsById[jobId] || null : null;
    },
    get envVars() {
      const job = this.job;
      return job ? envRowsWithComments(job.env_vars, job.env_comments) : [];
    },
    get envCount() {
      return (this.job?.env_vars || []).length;
    },
    get scope() {
      return { project_id: this.job.project_id, job_id: this.job.id };
    },

    async afterChange() {
      await this.store.refreshNow();
      await this.loadSecrets();
    },

    /** Reenvia o Job inteiro com as envs novas (o daemon não tem PATCH parcial). */
    async saveEnv(vars, comments, doneMessage) {
      const job = this.job;
      const response = await this.store.api.rpcChecked({
        JobUpdate: {
          id: job.id,
          name: job.name,
          compose: job.compose,
          git_source: job.git_source ?? null,
          main_service: job.main_service,
          env_vars: vars,
          env_comments: comments,
          enabled: job.enabled,
          recurrence: job.recurrence,
        },
      });
      this.store.toastResult(response, doneMessage);
      if (response.ok) await this.afterChange();
      return response;
    },

    async addEnvVar() {
      const key = this.newEnvKey.trim();
      if (!key) return;
      const job = this.job;
      if (!confirmEnvKey(this.store, key, job.env_vars)) return;
      const vars = (job.env_vars || []).filter((envVar) => envVar.key !== key);
      vars.push({ key, value: { Plain: this.newEnvValue } });
      const result = await this.saveEnv(vars, job.env_comments || [], t("variável salva"));
      if (result.ok) {
        this.newEnvKey = "";
        this.newEnvValue = "";
      }
    },
    async deleteEnvVar(key) {
      if (!confirm(t`Remover a variável "${key}" deste job?`)) return;
      const job = this.job;
      const vars = (job.env_vars || []).filter((envVar) => envVar.key !== key);
      const comments = (job.env_comments || []).filter((envComment) => envComment.before_key !== key);
      await this.saveEnv(vars, comments, t("variável removida"));
    },

    // ── Editor .env de texto ───────────────────────────────────────────
    openEnvText() {
      this.envText = dotenvFromVars(this.job?.env_vars, this.job?.env_comments);
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
      if (!confirm(t("Substituir todas as variáveis deste job pelo texto do editor?"))) return;
      const result = await this.saveEnv(vars, comments, t("variáveis salvas"));
      if (result.ok) this.envTextOpen = false;
    },

    // ── Edição no lugar da chave/valor ─────────────────────────────────
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
      let response;
      if (edit.field === "key") {
        const to = edit.draft.trim();
        if (!to || to === row.key) return;
        response = await this.store.api.rpcChecked({ EnvKeyRename: { ...this.scope, from: row.key, to } });
        this.store.toastResult(response, t`renomeado: ${row.key} → ${to}`);
      } else {
        const current = row.isSecret ? row.secretName : row.value;
        if (edit.draft === current) return;
        response = await this.store.api.rpcChecked({
          EnvValueSet: { ...this.scope, key: row.key, value: envValueFromDraft(edit.draft, row.isSecret) },
        });
        this.store.toastResult(response, t`valor de ${row.key} salvo`);
      }
      if (response.ok) await this.afterChange();
    },

    /** ✏️ num secret/variável: renomeia sem o valor passar pela tela. */
    async rename(kind, from) {
      const answer = prompt(t`Renomear "${from}" para:`, from);
      const to = answer === null ? "" : answer.trim();
      if (!to || to === from) return;
      const body = { ...this.scope, from, to };
      const response = await this.store.api.rpcChecked(kind === "secret" ? { SecretRename: body } : { EnvKeyRename: body });
      this.store.toastResult(response, t`renomeado: ${from} → ${to}`);
      if (response.ok) await this.afterChange();
    },

    // ── Movimentos (SecretMove) ────────────────────────────────────────
    async move(op, name, doneMessage) {
      const response = await this.store.api.rpcChecked({ SecretMove: { ...this.scope, name, op } });
      this.store.toastResult(response, doneMessage);
      if (response.ok) await this.afterChange();
    },
    /** 🔒: a variável em claro passa a ser guardada cifrada. */
    envToSecret(key) {
      if (!confirm(t`Mover a variável "${key}" para secret? O valor passa a ser guardado cifrado.`)) return;
      return this.move("EnvToSecret", key, t`variável ${key} movida para secret`);
    },
    /** 🔓: volta a ser variável em claro. */
    secretToEnv(name) {
      if (!confirm(t`O valor de "${name}" deixa de ser cifrado e passa a aparecer em claro na variável (e no estado versionado). Continuar?`)) return;
      return this.move("SecretToEnv", name, t`secret ${name} movido para variável`);
    },
    /** ↑ numa variável: sobe para o projeto (todos os serviços e jobs do projeto passam a herdar). */
    envToProject(key) {
      if (!confirm(t`A variável "${key}" passa a valer para todo o projeto (serviços e jobs). Continuar?`)) return;
      return this.move("EnvJobToProject", key, t`variável ${key} movida para o projeto`);
    },
    /** ↑ num secret: sobe para o projeto. */
    secretToProject(name) {
      if (!confirm(t`O secret "${name}" passa a valer para todo o projeto (serviços e jobs). Continuar?`)) return;
      return this.move("JobSecretToProject", name, t`secret ${name} movido para o projeto`);
    },

    // ── Secrets do job ─────────────────────────────────────────────────
    async loadSecrets() {
      const job = this.job;
      if (!job) {
        this.jobSecrets = [];
        return;
      }
      try {
        const response = await this.store.api.rpc({ SecretList: { project_id: job.project_id, job_id: job.id } });
        if (this.job?.id === job.id) this.jobSecrets = response.ok ? response.value?.SecretNames || [] : [];
      } catch (_) {
        this.jobSecrets = [];
      }
    },
    async addSecret() {
      const name = this.newSecretName.trim();
      if (!name || !this.newSecretValue) {
        this.store.toastError(t("Preencha nome e valor do secret"));
        return;
      }
      const response = await this.store.api.rpcChecked({ SecretSet: { ...this.scope, name, value: this.newSecretValue } });
      this.store.toastResult(response, t`secret ${name} salvo — vale na próxima execução`);
      if (response.ok) {
        this.newSecretName = "";
        this.newSecretValue = "";
        await this.loadSecrets();
      }
    },
    async deleteSecret(name) {
      if (!confirm(t`Remover o secret "${name}"? Variáveis que o referenciam vão falhar na próxima execução.`)) return;
      const response = await this.store.api.rpcChecked({ SecretDelete: { ...this.scope, name } });
      this.store.toastResult(response, t`secret ${name} removido`);
      if (response.ok) await this.loadSecrets();
    },
    /** 👁: pede o valor ao daemon (SecretReveal grava auditoria: quem, qual, quando; nunca o valor). */
    async revealSecret(name) {
      if (!confirm(t`Ver o valor do secret "${name}"? O valor vai aparecer na tela e o acesso fica registrado na auditoria do servidor (quem, qual secret e quando; nunca o valor).`)) return;
      const response = await this.store.api.rpc({ SecretReveal: { ...this.scope, name } });
      const value = response.ok ? response.value?.SecretValue : undefined;
      if (value === undefined) {
        this.store.toastError(response.error || t("não foi possível ler o secret"));
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
  }));
});
