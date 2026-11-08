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
  `ProjectServicesReorder{project_id, service_ids}` (ordem de exibição dos serviços; vai no `project.yml`) ·
  `ProjectEnvSet{project_id, env_vars, env_comments}`
- **Serviços**: `ServiceList{project_id}` · `ServiceGet{id}` ·
  `ServiceCreate(ServiceSpec)` · `ServiceUpdate{id, spec}` ·
  `ServiceDelete{id}` · `ServiceStop{service_id}` · `ServiceReload{service_id}` ·
  `ServiceConnectionInfo{service_id}` · `GetWebhookUrl{service_id}` ·
  `RegenerateWebhookToken{service_id}`
- **Deploys**: `DeployStart{service_id}` (só aquele serviço) ·
  `DeployWithDependencies{service_id, skip_running?}` (o serviço **e a árvore de dependências dele**,
  na ordem: quem não depende de ninguém primeiro; resposta `DeployBatch` com cada serviço e seu
  `deployment_id`; se o deploy de uma dependência falhar, quem depende dela **não roda**) ·
  `DeployWithDependenciesPlan{service_id, skip_running?}` e `ProjectDeployPlan{project_id, skip_running?}`
  (só mostram a ordem que o deploy acima usaria, **sem enfileirar**) ·
  `ProjectDeploy{project_id, skip_running?}` (todos os serviços do projeto, cada um com as suas
  dependências, na ordem necessária; `skip_running` pula o que já está no ar) · `DeployAbort{deployment_id}` ·
  `DeployRollback{service_id}` · `DeployHistory{service_id, limit}` ·
  `DeployDelete{deployment_id}` · `RecentDeployments{limit}` ·
  `GetBuildLogs{deployment_id}` · `DeployEngineStatus` ·
  `DeployQueuePromote{deployment_id}` · `DeployQueueReorder{order}` ·
  `DeployQueuePause{paused}`
- **Logs de container**: `LogsGet{service_id, tail}` (ao vivo; para acompanhar,
  use o SSE `/api/services/<id>/logs`) · `LogsHistory{service_id, before?, limit,
  container_id?, container_ids?}` (o log **persistido**, que atravessa deploys e
  rollbacks; pagine para trás repetindo com o `next_before` da resposta) ·
  `LogsExecutions{service_id}` (as **execuções** com log guardado, uma por
  container, da mais nova: início/fim, `deployment_id`, imagem, linhas, bytes,
  `current`, `partial` e o `outcome` do deploy) · `LogsExecutionDelete{service_id,
  container_ids}` (**apaga** o log dessas execuções — irreversível, vários de uma
  vez; a execução atual é recusada com `ExecutionRunning`)
- **Segredos**: `SecretList{project_id}` · `SecretSet{project_id, name, value}` ·
  `SecretDelete{project_id, name}` ·
  `SecretMove{project_id, service_id?, name, op}` (`EnvToSecret`, escopos…) ·
  `EnvKeyRename{project_id, service_id?, from, to}` — troca só a **chave** de uma
  variável, mantendo o valor (que você não vê) ·
  `EnvValueSet{project_id, service_id?, key, value}` — troca só o **valor** de uma
  variável existente, na mesma posição (`value` é `{"Plain":"…"}` ou
  `{"Secret":"NOME"}`, e o secret precisa existir no escopo); é a forma segura de
  editar um valor, sem o `ServiceUpdate`/`ProjectEnvSet` que substituem tudo. O valor
  `***` é recusado ·
  `SecretRename{project_id, service_id?, from, to}` — renomeia o secret e reaponta
  as variáveis que o usam. Use estes dois para alinhar nomes entre serviços (ex.:
  `RABBITMQ_DEFAULT_PASS` do broker × `RABBITMQ_PASS` do código) sem tocar em senha.
  `service_id` omitido = escopo do projeto.
- **Infra as code**: `InfraDownload` (os arquivos que o `state_git` grava no repositório, com
  os secrets como `ENC[age:…]`; recusado enquanto houver valor em claro — veja `InfraAudit`) ·
  `ServiceExportPlan{service_id}` · `ServiceExport{service_id, include_values, project_env_keys}` ·
  `ServiceImport(…)`
- **Jobs one-shot**: `JobListAll` · `JobList{project_id}` · `JobCreate{…}` ·
  `JobUpdate{…}` · `JobDelete{id}` · `JobRunNow{id}` · `JobRunCancel{job_run_id}` ·
  `JobRunHistory{job_id, limit}` · `GetJobLogs{job_run_id}` ·
  `JobWarmStatus{job_id}` (o job está quente? o cache e as imagens já estão no host) ·
  `JobWarm{job_id}` (aquece sem executar; o fim chega em `Event::JobWarmChanged`)
- **Bancos compartilhados**: `SharedAccessList{server_service_id}` ·
  `SharedAccessGrant{server_service_id, project_id}` ·
  `SharedAccessRevoke{…}` · `ManagedDatabaseList{server_service_id}` ·
  `ManagedDatabaseListAll` · `ManagedDatabaseCreate(…)` · `ManagedDatabaseDelete{id}`
- **Migrações**: `MigrationStart(…)` · `MigrationGet{id}` · `MigrationList{project_id}` ·
  `MigrationRollback{id}` · `MigrationDiscard{id}`
- **Templates e wizard**: `WizardCatalog{search}` · `WizardCreate(…)`. O `WizardCreate` com
  `kind` = `database`, `broker` ou `storage` (RustFS, MinIO) **gera a senha** quando você
  manda `password` vazio e a guarda como secret do serviço: a resposta traz só a referência,
  nunca o valor. É o jeito de criar infra sem ver credencial.
  **Bucket num Storage:** o Storage não cria bucket sozinho, e a aplicação não deve criá-lo na subida. O jeito certo é um
  **passo de provisionamento do próprio repositório da app** (idempotente: cria o bucket se faltar), executado por um job
  `git_source` (`JobCreate` com `git_source` + `compose_path`, e `build:` no Compose do job) que herda as variáveis e os
  secrets do projeto, e ligado ao serviço em `pre_deploy_job_ids`. Migrations do banco seguem o mesmo padrão.
  **Sem código próprio**, um job inline resolve: crie-o com `trigger_service_id` = o serviço do Storage (herda o ambiente
  dele, com os secrets decifrados só em memória) e `JobRunNow`. Receita testada num servidor real (RustFS, Podman):
  `compose` = `services:\n  bucket:\n    image: docker.io/curlimages/curl\n    entrypoint: ["/bin/sh", "-c"]\n    command: ["curl -sS -f -X PUT --aws-sigv4 aws:amz:us-east-1:s3 --user \"$$AK:$$SK\" -o /dev/null -w 'HTTP %{http_code}\\n' http://rp_<nome-do-storage>:9000/<bucket>"]\n    environment:\n      AK: ${RUSTFS_ACCESS_KEY}\n      SK: ${RUSTFS_SECRET_KEY}`
  (MinIO: `${MINIO_ROOT_USER}` / `${MINIO_ROOT_PASSWORD}`), `main_service` = `bucket`. `HTTP 200` = existe (repetir é
  seguro). **Não use `aws-cli`** com o nome interno: o botocore rejeita hostname com `_` (`Invalid endpoint:
  http://rp_s3:9000`) — o `curl --aws-sigv4` aceita. Não use `minio/mc`: a imagem não é mais puxável.
- **Docker**: `DockerContainers` · `DockerImages` · `DockerVolumes` ·
  `DockerNetworks` · `StopContainer{id}` (só container **órfão**: o de um serviço se para pelo serviço) · `RemoveContainer{id}` · `RemoveImage{id}` ·
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
- **Daemon**: `Ping` · `DaemonStatus` · `CheckUpdate{force?}` (há release nova? só
  consulta, nunca instala; cache de horas) · `Snapshot` (o painel inteiro, JSON
  dentro de uma string) · `GetDaemonSettings` ·
  `SetDaemonSettings{acme_email?, registry_domain?}` · `EnvBackupList` ·
  `EnvBackupRestore{snapshot}` · `UpdateStatus` (andamento ou resultado da última
  atualização) · `ApplyUpdate{allow_active_deploys?}` e `UpdateRollback` (instalam/
  desfazem uma versão: **recusados** ao agente, a menos que o dono ligue
  `[update] allow_agent_apply`)
- **Estado em git (`state_git`)**: `StateGitStatus` (situação da sincronização) ·
  `StateGitConfigure{mode, remote?, branch?, provider_id?}` (`off` | `commit` |
  `sync`; ligar é recusado enquanto houver valor em claro, veja `InfraAudit`) ·
  `StateGitSyncNow` (commit pendente e, em `sync`, fetch + push) · `StateGitResume`
  (retoma depois de uma divergência resolvida no repositório) ·
  `StateGitDismissRemoval{id}` (esquece uma marca "removido no git") ·
  `InfraAuditAllow{project, scope, what, allowed}` (marca uma **suspeita** da
  auditoria como "não é segredo"; certezas não podem ser liberadas). `StateGitResolve`
  descarta um dos lados e é **proibido** ao agente: peça a uma pessoa na GUI/webui.
- **Aquecimento**: `ServiceWarmStatusList` (status de todos os serviços Git/zip) ·
  `ServiceWarm{service_id, force?}` (constrói a imagem na vaga da fila para o próximo
  deploy achar o cache quente; o fim vem em `Event::ServiceWarmChanged`)

Quando não souber a forma de um tipo (`ServiceSpec`, `JobCreate`…), leia um
existente primeiro (`ServiceGet`, `JobListAll`) e use-o de molde.

## 6. Armadilhas

- **Chave de visitante e níveis.** Se o dono gerou uma chave **mestra**, a chave que você recebeu é a de
  **visitante** e tem um nível (`observer` só lê; `operator` opera o que já existe; `admin` também cria e
  apaga). Um comando acima do seu nível volta como `forbidden`, com o nível exigido e o seu. O `operator`
  **não** muda o que um serviço ou job **roda** (`source`, `run_command`, `run_args`, `volumes`, `product`,
  `shared`; num job, `compose`, `git_source`, `main_service`), mas edita variáveis, domínios, réplicas,
  healthcheck, recursos e liga/desliga jobs. Em qualquer nível não há `SecretReveal`, `InfraAuditAllow`,
  token do registry nem export com valores, e `GET /api/events` é recusado. Sem mestra configurada, vale o
  que está neste manual, sem níveis.

- **`ServiceUpdate` é substituição total, não patch.** Faça `ServiceGet`, mude
  o campo, devolva o spec inteiro. Campo omitido é campo apagado: pelo `/api/rpc`
  o daemon recusa (400) um `ServiceUpdate` que omite campos que o serviço já tem
  preenchidos; para limpar de propósito, mande a chave vazia (`"domains": []`).
- **Log de container: `LogsGet` é ao vivo, `LogsHistory` é o histórico.** O swap de
  deploy destrói o container antigo, mas o daemon grava o log de cada container
  (14 dias / 50 MiB por serviço, `[logs]` no `config.toml`) e guarda o que sobrou
  antes de remover o antigo ou o de staging que falhou. Para "o que aconteceu
  antes do último deploy", use `LogsHistory`; para o log de **uma** execução (um
  container), liste com `LogsExecutions` e passe o id dela em `container_ids`.
  `LogsExecutionDelete` é irreversível: só use a pedido. Serviços Compose seguem só ao vivo.
  Build log (`GetBuildLogs`/`build_logs`) também fica guardado.
- **Você não vê senhas — por desenho.** O daemon mascara o que o agente recebe:
  env `Plain` cuja chave parece segredo (`*PASSWORD*`, `*PASS*`, `*SECRET*`,
  `*TOKEN*`, `*KEY*`) vem como `***`; a senha em qualquer `://user:senha@host`
  (a `internal_url` do `ServiceConnectionInfo`, `DATABASE_URL`…) vem como
  `user:***@host`; o `.env` e os YAMLs de exportação seguem a mesma regra. Valor
  `Secret` aparece só como referência. É de propósito generoso (esconde até
  `VAPID_PUBLIC_KEY`). Proibidos ao agente: `SecretReveal`, `SecretMove` com
  `SecretToEnv`, `RegistryTokenCreate` e `ServiceExport` com `include_values`.
  Se você devolver um spec com `***` num `ServiceUpdate`/`ProjectEnvSet`, o
  daemon restaura o valor real; se não houver original, recusa. Para dar um
  segredo a um app, referencie um secret (`<secret:NOME>`), não escreva o valor.
  Isto reduz o risco de engano; não é um cofre (o token é de administrador).
- **`ServiceReload` NÃO relê variáveis nem secrets**: ele só para e inicia o MESMO container, que guarda o
  ambiente de quando foi criado. Trocou um secret/variável? Faça **redeploy** (`deploy`), que cria um container novo.
- **Gerar ou rotacionar um segredo SEM ver o valor: ainda não existe comando para isso.** Só dois caminhos servem hoje: o
  `WizardCreate` de banco/broker/storage, que gera a senha do serviço novo dentro do daemon, e uma pessoa gerando/trocando o
  secret na GUI/webui. **Não improvise** (nem com serviço descartável, nem escolhendo você a senha com `SecretSet`): isso
  tira o valor do cofre ou o deixa passar por você. O comando certo está planejado (`SecretGenerate`/`SecretRotate`, ainda não existem); até lá, peça a uma pessoa.
- **Sem escopo.** O token é o de administrador: não existe modo
  somente-leitura. Comandos destrutivos (`ServiceDelete`, `Prune*`,
  `Remove*`, `StopAllManaged`, `RegistryRepoDelete`) não pedem confirmação —
  peça a quem te deu o token de conexão antes de usá-los.
- **Rotas erradas dão `404`**; corpo que não é um `Command` válido dá `400`
  com a causa do serde.
