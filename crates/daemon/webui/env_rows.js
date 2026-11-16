// env_rows.js — a linha de variável editável no lugar, nas abas Variáveis do projeto e Environment
// do serviço e na janela Variáveis do job. Porta de rustploy-gui/views/scripts/handlers/env_rows.luau (o mesmo desenho nos dois
// clientes).
//
// A chave e o valor são campos sem borda: clicar já edita. Nada grava sozinho — digitar só muda um
// RASCUNHO da linha; aí aparecem o OK e o ✕ (desfazer). O OK (ou Enter) pede confirmação e grava:
// EnvValueSet para o valor, EnvKeyRename para a chave. O "usar secret" troca o campo do valor por
// uma lista com os secrets que a variável pode usar (no serviço: os dele e os do projeto; no
// projeto: os do projeto; no job: os dele, os do serviço gatilho e os do projeto) — ninguém precisa escrever `secret:NOME` à mão.
//
// O rascunho mora em `envDrafts`, por escopo + chave, e não no snapshot: o snapshot que chega a cada
// segundo não apaga o que a pessoa está digitando.
import { validEnvKey } from "./format.js";
import { t } from "./i18n.js";

/** O que a linha mostra: o rascunho, se houver, senão o que está gravado. */
export function envRowState(row, draft) {
  return {
    key: draft?.key ?? row.key,
    value: draft?.value ?? (row.isSecret ? "" : row.value),
    useSecret: draft?.useSecret ?? !!row.isSecret,
    secret: draft?.secret ?? (row.secretName || ""),
  };
}

/** O que mudou em relação ao gravado: `renameTo` (chave nova ou null) e `newValue` (o
 * EnvVarValue novo ou null). */
export function envRowChanges(row, state) {
  const key = (state.key || "").trim();
  const renameTo = key !== row.key ? key : null;
  let newValue = null;
  if (state.useSecret) {
    if (!row.isSecret || state.secret !== row.secretName) newValue = { Secret: state.secret || "" };
  } else if (row.isSecret || state.value !== row.value) {
    newValue = { Plain: state.value ?? "" };
  }
  return { renameTo, newValue };
}

/**
 * Os métodos da linha editável, para espalhar no `Alpine.data` da tela.
 *  - `scope()`: `{ project_id, service_id?, job_id? }` do EnvValueSet/EnvKeyRename (com `this` = a
 *    tela);
 *  - `secretOptions()`: `[{ name, label }]` dos secrets que a variável pode usar;
 *  - `afterSave()`: recarrega o que a tela mostra depois de gravar;
 *  - `footer`: a última linha da confirmação (padrão: "Vale no próximo deploy.").
 */
export function envRowEditor({ scope, secretOptions, afterSave, footer }) {
  return {
    envDrafts: {},

    envDraftId(row) {
      const where = scope.call(this);
      return `${where.project_id}|${where.service_id || ""}|${where.job_id || ""}|${row.key}`;
    },
    envRow(row) {
      return envRowState(row, this.envDrafts[this.envDraftId(row)]);
    },
    setEnvDraft(row, field, value) {
      const id = this.envDraftId(row);
      const next = { ...envRowState(row, this.envDrafts[id]), [field]: value };
      this.envDrafts = { ...this.envDrafts, [id]: next };
    },
    envRowDirty(row) {
      const draft = this.envDrafts[this.envDraftId(row)];
      if (!draft) return false;
      const changes = envRowChanges(row, draft);
      return changes.renameTo !== null || changes.newValue !== null;
    },
    resetEnvRow(row) {
      const { [this.envDraftId(row)]: _dropped, ...rest } = this.envDrafts;
      this.envDrafts = rest;
    },
    /** As opções da lista "usar secret"; um secret que a variável usa mas sumiu continua na lista,
     * marcado, para a pessoa ver o que está gravado. */
    envSecretOptions(row) {
      const options = secretOptions.call(this);
      const current = this.envRow(row).secret;
      if (current && !options.some((option) => option.name === current)) {
        return [...options, { name: current, label: t`${current} (não encontrado)` }];
      }
      return options;
    },

    async saveEnvRow(row) {
      const state = this.envRow(row);
      const { renameTo, newValue } = envRowChanges(row, state);
      if (renameTo === null && newValue === null) return;
      if (renameTo !== null && !validEnvKey(renameTo)) {
        this.store.toastError(t`Nome inválido: "${renameTo}" — use letras, dígitos e _, sem começar por dígito`);
        return;
      }
      if (newValue?.Secret !== undefined && !newValue.Secret) {
        this.store.toastWarn(t("escolha um secret"));
        return;
      }
      const lines = [];
      if (newValue?.Secret !== undefined) lines.push(t`passa a usar o secret "${newValue.Secret}"`);
      else if (newValue && row.isSecret) lines.push(t`deixa de usar o secret "${row.secretName}" e passa a ter o valor digitado`);
      else if (newValue) lines.push(t("o valor muda para o que foi digitado"));
      if (renameTo !== null) lines.push(t`a chave muda de "${row.key}" para "${renameTo}"`);
      if (!confirm(t`Salvar a variável "${row.key}"?` + "\n\n• " + lines.join("\n• ") + "\n\n" + (footer ? footer() : t("Vale no próximo deploy.")))) return;

      const where = scope.call(this);
      // Valor antes da chave: o EnvValueSet ainda acha a variável pelo nome antigo.
      if (newValue) {
        const valueResponse = await this.store.api.rpcChecked({ EnvValueSet: { ...where, key: row.key, value: newValue } });
        if (!valueResponse.ok) {
          this.store.toastResult(valueResponse);
          return;
        }
      }
      if (renameTo !== null) {
        const renameResponse = await this.store.api.rpcChecked({ EnvKeyRename: { ...where, from: row.key, to: renameTo } });
        if (!renameResponse.ok) {
          // O valor (se mudou) já foi gravado; o rascunho fica, com a chave nova ainda por gravar.
          this.store.toastResult(renameResponse);
          await afterSave.call(this);
          return;
        }
      }
      this.resetEnvRow(row);
      this.store.toastOk(t`variável ${renameTo ?? row.key} salva`);
      await afterSave.call(this);
    },
  };
}
