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

Na ferramenta `rpc`, o argumento `command` pode ir como valor JSON (o nome
`ProjectList`, ou o objeto) **ou** como texto JSON (`"\"ProjectList\""`,
`"{\"ServiceGet\":…}"`): o daemon decodifica o texto antes de interpretar. Se o
seu cliente MCP mandar o nome com aspas, funciona igual.

Comandos, por área (campos entre chaves):

- **Projetos**: `ProjectList` · `ProjectCreate{name, description?}` ·
  `ProjectUpdate{id, name, description?}` · `ProjectDelete{id}` ·
  `ProjectServicesReorder{project_id, service_ids}` (ordem de exibição dos serviços; vai no `project.yml`) ·
  `ProjectEnvSet{project_id, env_vars, env_comments}`
- **Serviços**: `ServiceList{project_id}` · `ServiceGet{id}` ·
  `ServiceCreate(ServiceSpec)` · `ServiceUpdate{id, spec}` ·
  `ServiceVolumeAttach{service_id, volume:{host_path, container_path, read_only}}` (Application: `host_path` = nome de volume — um órfão, para adotar os dados de um serviço apagado, ou um nome novo — ou pasta absoluta; recusa volume de outro serviço; vale no próximo deploy) · `ServiceVolumeDetach{service_id, container_path}` (os dados ficam) · `ServiceDelete{id, delete_volumes?}` (sem `delete_volumes: true` os volumes nomeados — os dados — ficam e aparecem como órfãos em `DockerVolumes`) · `ServiceStop{service_id}` · `ServiceReload{service_id}` ·
  `ServiceConnectionInfo{service_id}` · `GetWebhookUrl{service_id}` ·
  `RegenerateWebhookToken{service_id}`
- **Healthcheck** (campo `healthcheck` do `ServiceSpec`): `{kind, interval_secs, timeout_secs, retries,
  start_period_secs}`, com `kind` = `"None"` · `"Tcp"` (a `port` do serviço aceita conexão) ·
  `{"Http":{"path":"/health","expected_status":200}}` (os dois campos são obrigatórios) ·
  `"DockerNative"` (o `HEALTHCHECK` da imagem). O deploy só promove o container novo depois que o
  healthcheck passa; prefira `Http` num endpoint que confira as dependências (banco, cache).
- **Vínculos** (campo `links` do `ServiceSpec`, editado pelo `ServiceUpdate`): `links:
  [{service_id, env_prefix?, resource?, env_names?}]` diz quais serviços este **usa**. Cada vínculo vira dependência
  (ordem do deploy em árvore e do boot) e, no deploy, entrega `<PREFIXO>_HOST`, `_PORT` e `_URL`
  do outro (prefixo padrão: o nome dele em maiúsculas), com a **menor** precedência: as variáveis
  do projeto e do serviço ganham. De outro projeto, só um banco compartilhado, e só se a trava dele
  (`shared.accept_links`) aceitar; o daemon concede o acesso entre projetos sozinho. Recusas:
  `LinkNotAllowed`, `LinkRefused`, `DependencyCycle`. Num serviço **Compose**, as variáveis só
  chegam ao container se o YAML as referenciar (`${DB_URL}`), como qualquer variável. Jobs com
  serviço gatilho também recebem as variáveis dos vínculos dele.
  **Recurso** (`resource: {name, public_read?}` no vínculo, só no mesmo projeto): num S3 (`product`
  `rustfs`/`minio`) é um bucket, num `postgres`/`mysql`/`mariadb` um database. O daemon cria no deploy
  do consumidor, antes dos pré-deploy checks, se não existir (idempotente; teto de 60 s), e entrega
  `<PREFIXO>_BUCKET` ou `<PREFIXO>_DATABASE` (a `_URL` passa a apontar para o database).
  `public_read` (só bucket) aplica a política de leitura anônima. Desfazer o vínculo não apaga o recurso.
  **Nomes das variáveis** (`env_names: {SUFIXO: NOME}` no vínculo): troca o nome com que cada variável
  entra no container (`{"URL":"S3_ENDPOINT","BUCKET":"S3_BUCKET_NAME_1"}`); valor `""` = não entregar;
  sufixo ausente = o nome padrão (prefixo + sufixo). Sufixos: `HOST`, `PORT`, `URL`, `BUCKET`,
  `DATABASE`. Um nome trocado não pode repetir outro nome entregue pelos vínculos do serviço
  (`InvalidInput`). A precedência não muda: uma variável digitada com o mesmo nome ganha.
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
  `DeployQueuePause{paused}`. A fila também leva **execuções de job** (`jrun_…`) e **aquecimentos**
  (`jwarm_…`): em `DeployEngineStatus` cada item traz `kind` (`Deploy`, `Job` ou `Warm`) e o `deployment_id`
  é o id do item, que serve a `DeployQueuePromote`/`Reorder` e a `DeployAbort` (cancela o job, na fila ou
  rodando; `jwarm_…` só sai da fila antes de começar). `DeployRollback` só **consulta** o deploy Live
  anterior; para voltar de verdade, use as releases (abaixo).
- **Releases** (o que rodou em cada deploy que chegou a Live: imagem fixada por digest/id + o spec e as
  variáveis do projeto daquele momento): `ReleaseList{service_id}` (da mais nova para a mais velha;
  `number` é o "v12", `is_current` marca a que está no ar, `image_status` = `Local`, `Pullable` (será
  baixada pelo digest), `Missing` (não dá para voltar) ou `NotApplicable` (Compose)) ·
  `ReleaseDiff{release_id, against_release_id?}` (sem `against_release_id`: o que muda indo do que está
  declarado **agora** para a release, ou seja, o que um rollback mudaria; secrets aparecem como
  `secret:NOME`) · `ReleaseRollback{release_id}` (**sobrescreve** o spec do serviço com o da release,
  mantendo nome e projeto, e dispara um deploy **sem build** com a imagem dela; resposta `Deployment`).
  O rollback **não** volta as variáveis do projeto e usa o valor **atual** de cada secret. Confira o
  `ReleaseDiff` antes. As imagens das 3 releases mais novas de cada serviço são poupadas pela limpeza
  do Docker.
- **Rotas próprias** (o domínio fora do serviço): `RouteList` (de todos os projetos; a senha nunca volta) ·
  `RouteSave{route}` (cria com `id` vazio, atualiza com o id) · `RouteDelete{route_id}`. Uma rota tem `project_id`,
  `domain`, `tls`, `basic_auth: {user, password}` (senha em claro na ida, `null` para manter a gravada) e `action`:
  `{"Forward":{"rules":[{"path_prefix":"/api","service_id":"svc_…","port":null,"strip_prefix":true}]}}` (vence o
  prefixo mais longo; serviços do mesmo projeto) ou `{"Redirect":{"to":"https://app.com","permanent":true}}` (o
  caminho pedido vai junto). Vale na hora, sem redeploy, e aparece no `ingress_routes` (`app.com/api`). Um domínio
  não pode estar numa rota e num serviço ao mesmo tempo (os dois lados recusam).
- **Ambientes** (homologação, preview…: o mesmo projeto, outra instância): `EnvironmentCreate{project_id, name}`
  cria o projeto `<projeto> (<name>)` com cópias dos serviços, jobs, variáveis e secrets (rede e nomes de container
  próprios), **sem** domínios, portas do host, push-to-deploy, backup nem acesso aos bancos da origem; jobs agendados
  nascem desligados; nada é implantado (resposta `Project`; o projeto traz `environment: {origin_project_id, name,
  service_origins}`). `EnvironmentList{project_id}` lista os ambientes de uma origem. `EnvironmentCompare{project_id}`
  (o id do **ambiente**) compara com a origem serviço a serviço: release no ar de cada lado, `same_image`, o que difere
  na configuração (sem domínios/portas) e `can_promote_to_origin`/`can_bring_from_origin`.
  `EnvironmentPromote{from_service_id, to_service_id}` (serviços irmãos, nos dois sentidos) faz o deploy da **mesma
  imagem** da release no ar de `from` em `to`, sem build e **sem mudar a configuração** do destino; a release nova diz
  `promoted_from`. Para apagar um ambiente, `ProjectDelete` no projeto dele.
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
  `SecretGenerate{project_id, service_id?, job_id?, name, kind?, length?, on_exists?}` — **o daemon sorteia
  o valor** e grava o secret cifrado; a resposta é `SecretGenerated{name, created, length, fingerprint}`,
  **nunca** o valor. `kind`: `Password` (padrão, `A-Za-z0-9`, 32 caracteres; seguro em URL, SQL e shell),
  `Hex` (64; segredo de JWT/HMAC) ou `Base64Url` (43; tokens). `length` em caracteres, de 16 a 512.
  `on_exists`: `Fail` (padrão: recusa com `SecretExists`), `Keep` (mantém o atual e responde
  `created: false`: o "garanta que exista") ou `Replace` (troca o valor; quem usa só recebe o novo no próximo
  **deploy**, e o banco/broker **não** é avisado). `fingerprint` = 8 hex do SHA-256 do valor guardado, para
  conferir "é o mesmo?". Cada geração entra na auditoria de secrets. É **assim** que você cria senhas e
  chaves: uma pessoa lê o valor depois pelo 👁 da GUI/webui (auditado) ·
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
  `service_id` omitido = escopo do projeto. Todos esses comandos (`SecretSet/Delete/List/Reveal/Move/Rename`,
  `EnvKeyRename`, `EnvValueSet`) aceitam também `job_id` (no lugar de `service_id`, não os dois) = escopo
  **de um job**: as envs do job (`Job.env_vars`) e os secrets dele, que valem antes dos do serviço gatilho e
  dos do projeto. Moves extras de job: `EnvJobToProject`, `JobSecretToProject`.
- **Infra as code**: `InfraDownload` (os arquivos que o `state_git` grava no repositório, com
  os secrets como `ENC[age:…]`; recusado enquanto houver valor em claro — veja `InfraAudit`) ·
  `ServiceExportPlan{service_id}` · `ServiceExport{service_id, include_values, project_env_keys}` ·
  `ServiceImport(…)`
- **Dados do serviço (volumes)**: `ServiceDataPlan{service_id}` (só metadados: quais volumes
  e se está no ar). Exportar e importar os bytes (`GET /api/services/<id>/data/export?stop=`,
  `POST /api/services/<id>/data/import?confirm=`) é **só da chave mestra**, nunca do agente.
- **Portas TCP do host**: no spec, `tcp_ports: [{host_port, container_port, label}]` — várias portas do
  host, cada uma para uma porta do container (`container_port` vazio = a `port` do serviço; `host_port: 0`
  = escolher uma livre ao salvar; no máximo 8). O `host_port` antigo é só espelho: ao editar a lista, mande
  `host_port: null`. `ServicePortSuggestions{service_id}` diz quais portas o container provavelmente tem
  (`sources`: `listening`, `image`, `compose`, `product`; `internal: true` = não exponha).
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
  manda `password` vazio (ou omitido) e a guarda como secret do serviço: a resposta traz só a
  referência, nunca o valor. É o jeito de criar infra sem ver credencial. Só `kind`, `project_id`
  e `name` são obrigatórios; `id` é o motor (`postgres`, `mysql`, `mariadb`, `mongodb`, `redis`;
  brokers `rabbitmq`, `kafka`, `nats`; storages `rustfs`, `minio`; ou o id do template). O que
  faltar recebe o padrão da GUI: usuário do motor, banco `app`, imagem do catálogo. Mínimo que
  funciona: `{"WizardCreate":{"kind":"database","id":"postgres","project_id":"prj_…","name":"db"}}`.
  Opcionais: `db_name`, `user`, `password`, `root_password`, `image`, `app_name`, `use_replica`,
  `template_values`, `expose_external` (porta TCP do host escolhida pelo daemon).
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
- **Docker**: `DockerContainers` · `DockerImages` · `DockerVolumes` (cada volume traz o dono — `service_id`, `service`, `project` —, `orphan_of` quando o serviço dono foi apagado e o volume ficou, e `has_backup_policy`) ·
  `DockerNetworks` · `StopContainer{id}` (só container **órfão**: o de um serviço se para pelo serviço) · `RemoveContainer{id}` · `RemoveImage{id}` ·
  `RemoveVolume{name}` · `RemoveNetwork{id}` · `StopAllManaged` ·
  `PruneContainers` · `PruneImages{all}` · `PruneVolumes{all}` ·
  `PruneBuildCache` · `PruneNetworks` · `DockerCleanupConfigGet` ·
  `DockerCleanupConfigSet{config}` · `DockerCleanupRunNow`
- **Ingress**: `IngressRoutes` · `IngressReconcile{service_id?}`
- **Registry embutido**: `RegistryStatus` · `RegistryRepoList` ·
  `RegistryTagList{repo}` · `RegistryTagDelete{repo, tag}` ·
  `RegistryRepoDelete{repo}` · `RegistryGc` · `RegistryTokenList` ·
  `RegistryTokenCreate{name, scope}` · `RegistryTokenRevoke{name}` ·
  `RegistryRetentionSet{config:{enabled, keep_last}}` (retenção de tags: antes de cada
  GC apaga as tags mais antigas de cada repositório e deixa as `keep_last` mais recentes;
  nunca apaga a tag que um serviço usa; desligada por padrão; a leitura vem em
  `RegistryStatus.retention`)
- **Backup de dados para S3** (destino único por servidor; plano em `docs/plano-backup-de-dados-para-s3.md`):
  `BackupDestinationGet` (o destino **sem** a chave secreta: `configured`, `endpoint`, `bucket`,
  `secret_set`, `recovery_configured`) · `BackupDestinationSet{destination}` · `BackupDestinationClear` ·
  `BackupDestinationTest{destination?}` (grava, lê e apaga um objeto pequeno). Só o `Get` é seu: o
  `Set` e o `Test` levam a chave secreta e são **proibidos ao agente** (peça a uma pessoa em Settings → Backups).
  Todos exigem o nível `admin`.
  **Backup dos dados de um serviço** (volumes e bancos, sempre cifrado com a chave de recuperação):
  `ServiceBackupList{service_id}` (os backups no S3, a última operação em `last_run` e, se não dá para
  fazer, `blocked`) · `ServiceBackupNow{service_id, stop?, keep?}` (começa **em segundo plano** e responde
  `Ok`; consulte `ServiceBackupList.last_run`: `running`/`ok`/`failed`; `stop` padrão `true` para volume,
  banco não para; `keep` mantém só os N mais novos depois de enviar) · `ServiceBackupRestore{service_id,
  key, confirm}` e `ServiceBackupDelete{service_id, key}`, **proibidos ao agente** (substituem dados /
  apagam o backup) · `ServiceBackupPolicySet{service_id, policy}` (`policy`: `{enabled, recurrence:
  {IntervalHours:N}|{Daily:{hour,minute}}|{Weekly:{weekday,hour,minute}} (UTC), keep (1–365, padrão 7), stop}`;
  `null` apaga; ligar exige o destino S3 e a chave de recuperação; a resposta é o `ServiceBackupList` já com
  `policy` e `next_run_at`). O agendador roda a cada minuto, um backup por vez (na fila de deploy). Níveis:
  `List` observador, `Now` operador, `PolicySet`/`Restore`/`Delete` admin.
  **Recuperar um servidor limpo** (`ServerRecoverStart{remote, token, branch?, recovery_key}` → `ServerRecoverRun` →
  `ServerRecoverStatus` / `ServerRecoverCancel`): traz a infra do git, recifra os segredos para o servidor e
  restaura os dados do S3 subindo na ordem das dependências. **Proibidos ao agente** (levam a chave de
  recuperação e um token): é ação de uma pessoa em Settings → Recuperar.
  **Chave de recuperação** (`RecoveryKeyStatus` → `{configured, public_key}`;
  `RecoveryKeyGenerate{replace?, infra_remote, infra_branch?, provider_id}`): o status só mostra a chave
  **pública**. **`RecoveryKeyGenerate` é proibido ao agente** (devolve a chave privada uma única vez, dentro
  do arquivo de recuperação `.rpkit` com o repositório da infra, a branch e o token da conta escolhida): é
  ação de uma pessoa em Settings → Chave de recuperação, que o baixa na hora. Em Settings → Recuperar,
  "Carregar arquivo de recuperação" preenche os quatro campos.
- **Provedores git**: `GitProviderList` · `GitProviderCreate{…}` ·
  `GitProviderDelete{id, force?}` (**recusa** com `InUse` e a lista enquanto serviços ou jobs ainda usam a conta; `force: true` remove mesmo assim — eles deixam de clonar até serem reapontados) · `GitRepoList{provider_id}` ·
  `GitBranchList{provider_id, repo_full_name}`
- **Daemon**: `Ping` · `DaemonStatus` · `CheckUpdate{force?}` (há release nova? só
  consulta, nunca instala; cache de horas) · `Snapshot` (o painel inteiro, JSON
  dentro de uma string) · `GetDaemonSettings` ·
  `SetDaemonSettings{acme_email?, registry_domain?}` · `EnvBackupList` ·
  `EnvBackupRestore{snapshot}` · `UpdateStatus` (andamento ou resultado da última
  atualização) · `ApplyUpdate{allow_active_deploys?}` e `UpdateRollback` (instalam/
  desfazem uma versão: **recusados** ao agente, a menos que o dono ligue
  `[update] allow_agent_apply`) · `ApplyAppUpdate` (atualiza o app desktop **desta
  máquina** pelo helper root, sem senha; mesma trava; responde `AppUpdated{from,to}`)
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
  `*TOKEN*`, `*KEY*`) vem como `***`; a senha em qualquer `://user:senha@host`,
  **em qualquer campo de qualquer resposta** (a `internal_url` do `ServiceConnectionInfo`, a
  `connection_url` do `ManagedDatabaseList`, `DATABASE_URL`…), vem como `user:***@host`; o `.env` e os YAMLs de exportação seguem a mesma regra. Valor
  `Secret` aparece só como referência. É de propósito generoso (esconde até
  `VAPID_PUBLIC_KEY`). Proibidos ao agente: `SecretReveal`, `SecretMove` com
  `SecretToEnv`, `RegistryTokenCreate` e `ServiceExport` com `include_values`.
  Se você devolver um spec com `***` num `ServiceUpdate`/`ProjectEnvSet`, o
  daemon restaura o valor real; se não houver original, recusa. Para dar um
  segredo a um app, referencie um secret (`<secret:NOME>`), não escreva o valor.
  Isto reduz o risco de engano; não é um cofre (o token é de administrador).
- **`ServiceReload` NÃO relê variáveis nem secrets**: ele só para e inicia o MESMO container, que guarda o
  ambiente de quando foi criado. Trocou um secret/variável? Faça **redeploy** (`deploy`), que cria um container novo.
- **Gerar um segredo SEM ver o valor: `SecretGenerate`.** Nunca escolha você a senha com `SecretSet` (ela
  passaria pela conversa) nem use serviço descartável como gerador. Para uma senha que uma **pessoa** precisa
  digitar (o primeiro login de um app), gere com `SecretGenerate` e diga a ela onde ler: projeto → Secrets →
  👁. **Rotacionar** a senha de um banco ou broker é mais que gerar: `on_exists: Replace` troca só o secret, e
  o banco continua com a senha antiga até alguém rodar o `ALTER USER` (um job) e redeployar quem usa. O
  `SecretRotate` com esses passos ainda não existe: combine com uma pessoa antes.
- **Sem escopo.** O token é o de administrador: não existe modo
  somente-leitura. Comandos destrutivos (`ServiceDelete`, `Prune*`,
  `Remove*`, `StopAllManaged`, `RegistryRepoDelete`) não pedem confirmação —
  peça a quem te deu o token de conexão antes de usá-los.
- **Rotas erradas dão `404`**; corpo que não é um `Command` válido dá `400`
  com a causa do serde.
