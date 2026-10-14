# Rustploy — manual do agente

Você está falando direto com o **daemon** do rustploy (um PaaS single-node:
projetos, serviços em container, deploys, proxy reverso com TLS, registry,
jobs). Tudo o que as telas da GUI e da webui fazem passa pela mesma API que
você vai usar aqui — não existe porta dos fundos que só humanos alcançam.

## 1. Entrar

Você recebeu um **token de conexão**: o base64 de `host:porta::token`, sem
esquema. Decodifique e você tem o endereço do servidor e o token da API —
`echo '<token de conexão>' | base64 -d`. Esse token da API já é o Bearer;
se preferir, o `/api/login` faz a troca e devolve as URLs do MCP e do manual:

```bash
curl -s https://HOST:PORTA/api/login -d '{"key":"<token de conexão>"}'
```

```json
{"access_token":"…","token_type":"Bearer","server":"https://HOST:PORTA", "ai_agent":{…}}
```

O esquema não está no token: tente `https` e, se o TLS falhar, `http`.

Guarde `access_token` e mande em **toda** chamada seguinte:
`Authorization: Bearer <access_token>`. Ele não expira; trocar o token exige
editar o `config.toml` do daemon.

Se o token de conexão estiver malformado, o `/api/login` responde `400` dizendo
qual parte falhou e qual é o formato certo. Token errado → `401`.

## 2. Duas portas para as mesmas ferramentas

### 2a. MCP (se o seu cliente fala MCP)

`POST /mcp` é um servidor MCP (transporte Streamable HTTP, respostas em JSON,
sem SSE). Exemplo no Claude Code:

```bash
claude mcp add --transport http rustploy https://SERVIDOR:PORTA/mcp \
  --header "Authorization: Bearer <access_token>"
```

Atalho para humanos: na GUI ou na webui, Settings → Web Server → "Conectar
um agente (Claude Code)" → Copiar já dá esse comando com o token resolvido.

As ferramentas aparecem com descrição e esquema. Este manual é também as
`instructions` do handshake e o recurso `rustploy://manual`.

### 2b. JSON-RPC à mão (se você só tem `curl`)

O MCP é só JSON-RPC 2.0 num POST — dá para chamar sem cliente MCP nenhum:

```bash
export RP=https://SERVIDOR:PORTA RPT=<access_token>
alias rp='curl -s -H "Authorization: Bearer $RPT" -H "Content-Type: application/json"'

rp $RP/mcp -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
rp $RP/mcp -d '{"jsonrpc":"2.0","id":2,"method":"tools/call",
               "params":{"name":"list_services","arguments":{}}}'
```

O resultado de `tools/call` vem em `result.content[0].text` (JSON em texto) e
também já decodificado em `result.structuredContent`. Erro da ferramenta vem
com `result.isError: true` e a causa no texto — não como erro JSON-RPC.

## 3. As ferramentas

| Ferramenta | Argumentos | Para quê |
|---|---|---|
| `status` | — | versão/uptime do daemon + fila de deploys |
| `list_services` | — | índice projeto→serviço: `service_id`, nome, status, origem, porta |
| `list_deploys` | `limit` (1..200, padrão 20) | últimos deploys **com o desfecho resolvido** |
| `deploy` | `service_id` **ou** `service` (nome), `wait`, `timeout_s`, `log_tail` | dispara e **espera** o resultado |
| `build_logs` | `deployment_id`, `after`, `limit` | build log paginado por cursor |
| `ingress_routes` | — | tabela de rotas viva do proxy — o diagnóstico de um 502 |
| `ingress_reconcile` | `service_id` (opcional) | recalcula as rotas sem redeploy |
| `rpc` | `command` | **qualquer** `Command` do protocolo (seção 5) |
| `manual` | — | este texto |

### `deploy` — a que você mais vai usar

```json
{"name":"deploy","arguments":{"service":"api","wait":true}}
```

```json
{
  "deployment_id": "dep_01M11Z99…", "state": "Failed", "ok": false,
  "error": "docker build error: Cannot locate specified Dockerfile: Dockerfile",
  "log_tail": ["…"], "log_cursor": 6, "waited": true, "timed_out": false
}
```

- `ok`: `true` = Live, `false` = Failed, `null` = ainda rodando **ou** terminal
  sem desfecho (`Stopped` = parado de propósito, `Pruning` = substituído por
  um deploy mais novo). Não chame esses dois de falha.
- Nome de serviço é único **por projeto**: com ambiguidade a ferramenta recusa
  e lista os candidatos `projeto=service_id`.
- `wait:false` volta na hora; acompanhe com `list_deploys`/`build_logs`.
- `timeout_s` padrão 900, teto 3600. `log_tail` padrão 40, teto 500.
- Para continuar lendo o log depois: `build_logs` com `after = log_cursor`, e
  depois com o `next_after` de cada resposta.

### `ingress_routes` — domínio respondendo 502

O 502 vem do proxy do rustploy: a rota existe e o backend não respondeu.

- domínio **ausente** → nenhum deploy registrou rota (serviço parado, ou o
  deploy falhou antes de promover);
- `backends` **vazio** → rota sem destino, 502 garantido;
- `backends` na **porta errada** → caso clássico de stack Compose: o alvo de
  ingress do serviço aponta para o container/porta errados.

`ingress_reconcile` relê os containers que existem e reescreve as rotas sem
recriar nada. Se continuar errado depois dele, o problema está no spec (alvo
de ingress, porta do domínio), não na tabela.

## 4. Fora do MCP: rotas HTTP diretas

Todas com o mesmo `Authorization: Bearer`.

| Rota | Para quê |
|---|---|
| `POST /api/rpc` | um `Command` em JSON → a `Response` em JSON (o mesmo que a ferramenta `rpc`) |
| `POST /api/services/<id>/archive` | sobe o `.zip` de um serviço com fonte Archive: **corpo binário**, header opcional `x-rustploy-filename`, teto 100 MiB |
| `GET /api/events` | SSE: snapshot do painel a cada 2 s + eventos ao vivo (`data` traz `kind`) |
| `GET /api/services/<id>/logs` | SSE: log de runtime de um serviço |
| `GET /api/deployments/<id>/build-logs` | SSE: build ao vivo |
| `GET /api/jobs/runs/<id>/logs` | SSE: saída de uma execução de job |
| `GET /api/agent/manual` | este manual |
| `GET /api/access-key` | o token de conexão deste servidor (para repassar o acesso) |
| `GET /api/version` | versão em texto puro (sem token) |

Upload de zip:

```bash
curl -s -H "Authorization: Bearer $RPT" -H "x-rustploy-filename: app.zip" \
  --data-binary @app.zip $RP/api/services/svc_…/archive
```

## 5. O protocolo (`rpc`)

Codificação serde **externally-tagged**, em `Command` **e** em `Response`:
variante com campos é objeto de uma chave (`{"ServiceGet":{"id":"svc_…"}}`),
variante sem campos é a string nua (`"ProjectList"`). Variante tupla leva o
valor direto (`{"ServiceCreate":{…spec…}}`). Falha do daemon vem como
`{"Err":{"code":"…","message":"…"}}` (a ferramenta `rpc` devolve isso como
`isError`).

Comandos, por área (campos entre chaves):

- **Projetos**: `ProjectList` · `ProjectCreate{name, description?}` ·
  `ProjectUpdate{id, name, description?}` · `ProjectDelete{id}` ·
  `ProjectEnvSet{project_id, env_vars, env_comments}`
- **Serviços**: `ServiceList{project_id}` · `ServiceGet{id}` ·
  `ServiceCreate(ServiceSpec)` · `ServiceUpdate{id, spec}` ·
  `ServiceDelete{id}` · `ServiceStop{service_id}` · `ServiceReload{service_id}` ·
  `ServiceConnectionInfo{service_id}` · `GetWebhookUrl{service_id}` ·
  `RegenerateWebhookToken{service_id}`
- **Deploys**: `DeployStart{service_id}` · `DeployAbort{deployment_id}` ·
  `DeployRollback{service_id}` · `DeployHistory{service_id, limit}` ·
  `DeployDelete{deployment_id}` · `RecentDeployments{limit}` ·
  `GetBuildLogs{deployment_id}` · `DeployEngineStatus` ·
  `DeployQueuePromote{deployment_id}` · `DeployQueueReorder{order}` ·
  `DeployQueuePause{paused}`
- **Logs de container**: `LogsGet{service_id, tail}` (para acompanhar ao vivo,
  use o SSE `/api/services/<id>/logs`)
- **Segredos**: `SecretList{project_id}` · `SecretSet{project_id, name, value}` ·
  `SecretDelete{project_id, name}`
- **Infra as code**: `ManifestExport{project_id}` · `ManifestExportAll` ·
  `ManifestImport{yaml, dotenv, prune, deploy}` · `ManifestApply{manifests, prune, deploy}` ·
  `ServiceExportPlan{service_id}` · `ServiceExport{service_id, include_values, project_env_keys}` ·
  `ServiceImport(…)`
- **Jobs one-shot**: `JobListAll` · `JobList{project_id}` · `JobCreate{…}` ·
  `JobUpdate{…}` · `JobDelete{id}` · `JobRunNow{id}` · `JobRunCancel{job_run_id}` ·
  `JobRunHistory{job_id, limit}` · `GetJobLogs{job_run_id}`
- **Bancos compartilhados**: `SharedAccessList{server_service_id}` ·
  `SharedAccessGrant{server_service_id, project_id}` ·
  `SharedAccessRevoke{…}` · `ManagedDatabaseList{server_service_id}` ·
  `ManagedDatabaseListAll` · `ManagedDatabaseCreate(…)` · `ManagedDatabaseDelete{id}`
- **Migrações**: `MigrationStart(…)` · `MigrationGet{id}` · `MigrationList{project_id}` ·
  `MigrationRollback{id}` · `MigrationDiscard{id}`
- **Templates**: `WizardCatalog{search}` · `WizardCreate(…)`
- **Docker**: `DockerContainers` · `DockerImages` · `DockerVolumes` ·
  `DockerNetworks` · `RemoveContainer{id}` · `RemoveImage{id}` ·
  `RemoveVolume{name}` · `RemoveNetwork{id}` · `StopAllManaged` ·
  `PruneContainers` · `PruneImages{all}` · `PruneVolumes{all}` ·
  `PruneBuildCache` · `PruneNetworks` · `DockerCleanupConfigGet` ·
  `DockerCleanupConfigSet{config}` · `DockerCleanupRunNow`
- **Ingress**: `IngressRoutes` · `IngressReconcile{service_id?}`
- **Registry embutido**: `RegistryStatus` · `RegistryRepoList` ·
  `RegistryTagList{repo}` · `RegistryTagDelete{repo, tag}` ·
  `RegistryRepoDelete{repo}` · `RegistryGc` · `RegistryTokenList` ·
  `RegistryTokenCreate{name, scope}` · `RegistryTokenRevoke{name}`
- **Provedores git**: `GitProviderList` · `GitProviderCreate{…}` ·
  `GitProviderDelete{id}` · `GitRepoList{provider_id}` ·
  `GitBranchList{provider_id, repo_full_name}`
- **Daemon**: `Ping` · `DaemonStatus` · `Snapshot` (o painel inteiro, JSON
  dentro de uma string) · `GetDaemonSettings` ·
  `SetDaemonSettings{acme_email?, registry_domain?}` · `EnvBackupList` ·
  `EnvBackupRestore{snapshot}`

Quando não souber a forma de um tipo (`ServiceSpec`, `JobCreate`…), leia um
existente primeiro (`ServiceGet`, `JobListAll`) e use-o de molde.

## 6. Armadilhas

- **`ServiceUpdate` é substituição total, não patch.** Faça `ServiceGet`, mude
  o campo, devolva o spec inteiro. Campo omitido é campo apagado.
- **Log de container não é persistido.** `LogsGet` lê do Docker ao vivo, e o
  swap de deploy destrói o container antigo junto com o log. Build log
  (`GetBuildLogs`/`build_logs`) esse sim fica guardado.
- **Sem escopo.** O token é o de administrador: não existe modo
  somente-leitura. Comandos destrutivos (`ServiceDelete`, `Prune*`,
  `Remove*`, `StopAllManaged`, `RegistryRepoDelete`) não pedem confirmação —
  peça a quem te deu o token de conexão antes de usá-los.
- **Rotas erradas dão `404`**; corpo que não é um `Command` válido dá `400`
  com a causa do serde.
