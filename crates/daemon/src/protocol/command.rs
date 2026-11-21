//! `Command`: tudo o que o cliente pede ao daemon.

use crate::domain::*;
use serde::{Deserialize, Serialize};

/// Como resolver um histórico divergido (`Command::StateGitResolve`). Os dois lados andaram, então
/// escolher um **descarta** o outro — por isso o outro vira uma branch de backup antes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateGitResolveChoice {
    /// "Usar o do remoto": o servidor adota o repositório. Os commits locais que o remoto não tem
    /// ficam numa branch `rustploy-backup/local-<data>` no clone do servidor. Antes de adotar, o
    /// conteúdo do remoto passa pelas mesmas validações de um pull (YAML, ciclos, valor em claro…).
    UseRemote,
    /// "Usar o meu": o repositório adota o servidor. O que o remoto tinha de diferente é copiado
    /// para `rustploy-backup/remote-<data>` **no próprio remoto** antes de ser sobrescrito.
    UseLocal,
}

/// As operações de [`Command::SecretMove`]. "Mover" = criar no destino e apagar na origem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretMoveOp {
    /// Env → secret. `service_id: Some` = env do serviço → secret do serviço; `None` = env do
    /// projeto → secret do projeto. `name` é a chave da env, que passa a `Secret(name)`.
    EnvToSecret,
    /// Secret → env, no mesmo escopo. As envs que apontavam para o secret viram `Plain(valor)`; se
    /// nenhuma apontava, cria-se uma env com chave = nome do secret.
    SecretToEnv,
    /// Secret de **serviço** → secret de projeto. Exige `service_id`.
    ServiceSecretToProject,
    /// Secret de **projeto** → secret de um serviço específico. Exige `service_id` (o destino).
    ProjectSecretToService,
    /// Variável **em claro** de um serviço → variável do projeto (passa a valer para todos os
    /// serviços dele). Exige `service_id`; a env que aponta para um secret não se move aqui (mova o
    /// secret). Falha se o projeto já tem uma variável com esse nome.
    EnvServiceToProject,
    /// Variável **em claro** do projeto → variável de um serviço (`service_id` é o destino); sai do
    /// projeto, então os outros serviços deixam de recebê-la. Falha se o serviço já tem o nome.
    EnvProjectToService,
    /// Variável **em claro** de um job → variável do projeto. Exige `job_id`.
    EnvJobToProject,
    /// Secret de **job** → secret de projeto. Exige `job_id`.
    JobSecretToProject,
}

/// O formato do valor que [`Command::SecretGenerate`] sorteia. `length` é sempre em **caracteres**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SecretKind {
    /// `A-Za-z0-9` (padrão 32): seguro em URL, SQL e shell. Para senhas de banco, admin, broker.
    #[default]
    Password,
    /// `0-9a-f` (padrão 64 = 32 bytes): segredo de HMAC/JWT.
    Hex,
    /// `A-Za-z0-9-_` (padrão 43 = 32 bytes): tokens e chaves de sessão.
    Base64Url,
}

/// O que [`Command::SecretGenerate`] faz quando o secret já existe no escopo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SecretOnExists {
    /// Recusa (`SecretExists`), para ninguém trocar por engano uma senha em uso.
    #[default]
    Fail,
    /// Não faz nada e responde `created: false`: o "garanta que exista", idempotente.
    Keep,
    /// Troca o valor. Quem usa o secret só recebe o novo no próximo **deploy**, e o sistema de fora
    /// (o banco, o broker) não é avisado: trocar a senha lá é outro passo.
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    // Projects
    ProjectCreate {
        name: String,
        description: Option<String>,
    },
    ProjectDelete {
        id: String,
    },
    ProjectUpdate {
        id: String,
        name: String,
        description: Option<String>,
    },
    ProjectList,
    /// Define a ordem de exibição dos serviços do projeto: `service_ids` na ordem
    /// desejada. Guardada no próprio `project.yml` (a posição de cada serviço na
    /// lista `services`), logo vai para o git. Ids que sobrarem (serviço criado
    /// enquanto a tela estava aberta) ficam no fim, na ordem em que já estavam; id
    /// que não é do projeto ou repetido é erro. Response: `Ok`.
    ProjectServicesReorder {
        project_id: String,
        service_ids: Vec<String>,
    },
    ProjectEnvSet {
        project_id: String,
        env_vars: Vec<EnvVar>,
        env_comments: Vec<EnvComment>,
    },

    // Services
    ServiceCreate(ServiceSpec),
    ServiceUpdate {
        id: String,
        spec: ServiceSpec,
    },
    ServiceDelete {
        id: String,
    },
    ServiceList {
        project_id: String,
    },
    ServiceGet {
        id: String,
    },

    // Deployments
    DeployStart {
        service_id: String,
    },
    /// Faz o deploy de um serviço **e da árvore de dependências dele**: enfileira, na ordem certa (quem
    /// não depende de ninguém primeiro), todas as dependências (diretas e indiretas, inclusive o banco
    /// compartilhado a que o projeto tem acesso) e por último o serviço. Se o deploy de uma dependência
    /// falhar, quem depende dela **não roda** (fica "Failed: a dependência … não foi implantada"). Com
    /// `skip_running`, as dependências que já estão no ar não entram na fila. Resposta: `DeployBatch`.
    DeployWithDependencies {
        service_id: String,
        #[serde(default)]
        skip_running: bool,
    },
    /// Faz o deploy de **todos os serviços do projeto**, cada um com as suas dependências, na ordem
    /// necessária para todos funcionarem (o mesmo plano do `DeployWithDependencies`, para o projeto
    /// inteiro). Resposta: `DeployBatch`.
    ProjectDeploy {
        project_id: String,
        #[serde(default)]
        skip_running: bool,
    },
    /// **Só mostra** o plano de `DeployWithDependencies`, sem enfileirar nada: a ordem em que os serviços
    /// entrariam na fila (`Queued` = entraria; `Skipped` = ficaria de fora, com o motivo em `message`).
    /// É o que a tela mostra antes de a pessoa confirmar. Resposta: `DeployBatch` (sem `deployment_id`).
    DeployWithDependenciesPlan {
        service_id: String,
        #[serde(default)]
        skip_running: bool,
    },
    /// Como `DeployWithDependenciesPlan`, para o `ProjectDeploy` (o projeto inteiro).
    ProjectDeployPlan {
        project_id: String,
        #[serde(default)]
        skip_running: bool,
    },
    DeployAbort {
        deployment_id: String,
    },
    DeployRollback {
        service_id: String,
    },
    DeployHistory {
        service_id: String,
        limit: usize,
    },
    DeployDelete {
        deployment_id: String,
    },

    // Service lifecycle
    ServiceStop {
        service_id: String,
    },
    ServiceReload {
        service_id: String,
    },

    // Global views
    RecentDeployments {
        limit: usize,
    },
    GetBuildLogs {
        deployment_id: String,
    },

    // Observability
    LogsGet {
        service_id: String,
        tail: usize,
    },
    /// Log de runtime **persistido** do serviço (sobrevive à troca de container no deploy e ao
    /// rollback). Devolve até `limit` linhas em ordem cronológica, anteriores a `before` (ou as mais
    /// recentes, se omitido); para paginar para trás, repita com o `next_before` da resposta.
    /// `container_id` filtra por um container (id curto). Resposta: `LogsHistory`. Não cobre
    /// serviços Compose (só ao vivo, por `LogsGet`).
    LogsHistory {
        service_id: String,
        #[serde(default)]
        before: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
        #[serde(default)]
        container_id: Option<String>,
        /// Vários containers de uma vez (uma execução com réplicas); soma-se a `container_id`.
        /// Vazio (e sem `container_id`) = o serviço inteiro.
        #[serde(default)]
        container_ids: Vec<String>,
    },
    /// As execuções (um container = uma execução) com log guardado, da mais nova para a mais velha:
    /// início/fim, deploy, imagem, linhas, bytes e desfecho do deploy. Resposta: `LogsExecutions`.
    /// Não cobre serviços Compose.
    LogsExecutions {
        service_id: String,
    },
    /// Apaga o log guardado de execuções (irreversível; vários de uma vez). A execução atual é
    /// recusada (`ExecutionRunning`); id inexistente é ignorado. Resposta: `LogsExecutionsDeleted`.
    LogsExecutionDelete {
        service_id: String,
        container_ids: Vec<String>,
    },
    LogsSubscribe {
        service_id: String,
        tail: usize,
    },
    LogsUnsubscribe {
        service_id: String,
    },
    MetricsSubscribe {
        service_id: String,
    },
    MetricsUnsubscribe {
        service_id: String,
    },

    /// Host e URL de conexão **dentro** da rede Docker do projeto (montada no
    /// daemon por `crate::domain::connection`, para GUI e webui não divergirem).
    ServiceConnectionInfo {
        service_id: String,
    },

    /// Projetos autorizados a alcançar um servidor de banco compartilhado.
    SharedAccessList {
        server_service_id: String,
    },
    /// Autoriza um projeto: conecta o servidor à rede dele sob o alias global.
    SharedAccessGrant {
        server_service_id: String,
        project_id: String,
    },
    SharedAccessRevoke {
        server_service_id: String,
        project_id: String,
    },

    /// Databases gerenciados de um servidor compartilhado.
    ManagedDatabaseList {
        server_service_id: String,
    },
    /// Cria database + usuário, autoriza o projeto e grava a env var nele.
    ManagedDatabaseCreate(crate::domain::ManagedDatabaseCreateReq),
    /// Remove database + usuário do servidor (**apaga os dados**).
    ManagedDatabaseDelete {
        id: String,
    },

    /// Todos os databases gerenciados (de todos os servidores).
    ManagedDatabaseListAll,
    /// Inicia a migração (corre em segundo plano; acompanhe com `MigrationGet`).
    MigrationStart(crate::domain::MigrationStartReq),
    MigrationGet {
        id: String,
    },
    MigrationList {
        project_id: String,
    },
    /// Volta a app para o banco antigo (env var + serviços).
    MigrationRollback {
        id: String,
    },
    /// Remove o banco antigo (**apaga os dados dele**) após o período de observação.
    MigrationDiscard {
        id: String,
    },

    // Webhooks
    GetWebhookUrl {
        service_id: String,
    },
    RegenerateWebhookToken {
        service_id: String,
    },
    GetDaemonSettings,
    SetDaemonSettings {
        acme_email: Option<String>,
        registry_domain: Option<String>,
    },

    // Secrets
    //
    // Escopo: `service_id: None` = secret do **projeto**; `Some(id)` = secret **daquele serviço**
    // (que, na resolução de uma env do serviço, vale antes do de projeto). Clientes antigos, que
    // não mandam o campo, continuam falando só do escopo de projeto.
    SecretSet {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        name: String,
        value: String,
    },
    /// Sorteia um valor **dentro do daemon** e o grava como secret cifrado (do projeto, de um serviço
    /// ou de um job). O valor nunca sai na resposta, só `SecretGenerated` com o tamanho e uma
    /// impressão digital; uma pessoa o lê pelo "ver valor" (auditado). É o jeito de o agente criar
    /// senhas sem vê-las. Grava uma linha `generate` na auditoria de secrets.
    SecretGenerate {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        #[serde(default)]
        job_id: Option<String>,
        name: String,
        #[serde(default)]
        kind: SecretKind,
        /// Em caracteres; vazio = o padrão do `kind`. Mínimo 16, máximo 512.
        #[serde(default)]
        length: Option<u32>,
        #[serde(default)]
        on_exists: SecretOnExists,
    },
    SecretDelete {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        name: String,
    },
    SecretList {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
    },
    /// Devolve o **valor** de um secret (botão "ver valor"). Fica fora do MCP/`rpc` do agente e
    /// grava uma linha de auditoria (quem, qual secret, quando — nunca o valor).
    SecretReveal {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        name: String,
    },
    /// Lista o que está em claro (ou parece estar) no estado de infra — a trava para ligar a
    /// sincronização com git, e o guia de "o que ainda falta mover para secret". Nunca devolve valores.
    InfraAudit,
    /// Baixa a infra: os mesmos arquivos que o `state_git` grava no repositório (`settings.yml`,
    /// `providers.yml`, `projects/<id>/project.yml`…), com os segredos como `ENC[age:…]`. Resposta:
    /// `InfraFiles`. **Recusado** enquanto houver valor em claro que travaria o commit no git
    /// (ver `InfraAudit`): o download não pode vazar o que o git não aceitaria.
    InfraDownload,
    /// Situação da sincronização do estado de infra com o git.
    StateGitStatus,
    /// Liga/desliga/configura a sincronização. `mode`: `off` | `commit` | `sync`. Ligar (`commit`/`sync`)
    /// é **recusado** enquanto houver valor em claro no estado (ver `InfraAudit`). `remote` só em
    /// `https://` (a credencial vem do `provider_id`) ou `file://` (testes/cópia local).
    StateGitConfigure {
        mode: String,
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        provider_id: Option<String>,
    },
    /// "Sincronizar agora": faz o commit pendente e, em `sync`, o `fetch`, a aplicação do que veio de
    /// fora e o `push`.
    StateGitSyncNow,
    /// Retoma a sincronização pausada por histórico divergido (depois de resolver no repositório).
    StateGitResume,
    /// Resolve a divergência **sem usar o terminal**: descarta um dos lados, com cópia de segurança
    /// do que for descartado (ver [`StateGitResolveChoice`]). Só vale com a sincronização pausada.
    StateGitResolve {
        choice: StateGitResolveChoice,
    },
    /// Esquece uma marca de "removido no git" (`id` vem em `StateGitStatus.removed_in_git`).
    StateGitDismissRemoval {
        id: String,
    },
    /// Marca (ou desmarca) uma **suspeita** da auditoria como "não é segredo", para não travar o git.
    /// Certezas (senha de banco, webhook…) não podem ser liberadas.
    InfraAuditAllow {
        project: String,
        scope: String,
        what: String,
        allowed: bool,
    },
    /// Para um container (`id` completo, vindo de `DockerContainers`). **Recusa** container que ainda
    /// pertence a um serviço do estado: esses se param pelo serviço. Serve ao container órfão
    /// ("removido no git"): parar aqui e depois `RemoveContainer`.
    StopContainer {
        id: String,
    },
    /// Move um item entre env e secret, ou entre escopos. Sempre atômico: ou tudo muda, ou nada.
    /// Falha se o destino já tem um item com o mesmo nome. Ver [`SecretMoveOp`].
    SecretMove {
        project_id: String,
        /// Depende da operação (ver [`SecretMoveOp`]).
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        /// Nome do secret — ou a chave da env, em [`SecretMoveOp::EnvToSecret`].
        name: String,
        op: SecretMoveOp,
    },
    /// Renomeia a **chave** de uma env (do projeto, ou do serviço se `service_id` vier), mantendo o
    /// valor — `Plain` ou `Secret` — sem que ele passe pelo chamador, e acompanhando os comentários
    /// do editor `.env` ancorados nela. Falha se `from` não existe ou `to` já existe no escopo.
    /// Resposta: `Ok`.
    EnvKeyRename {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        from: String,
        to: String,
    },
    /// Troca só o **valor** de uma env existente (do projeto, ou do serviço se `service_id` vier),
    /// na mesma posição da lista — edição "no lugar", sem reenviar a lista inteira como o
    /// `ProjectEnvSet`/`ServiceUpdate` (que são substituição total e perdem o que o cliente não
    /// conhece). `value` é `{"Plain":"…"}` ou `{"Secret":"NOME"}`; um `Secret` precisa existir no
    /// escopo (do serviço ou, caindo para ele, do projeto). Falha se a chave não existe, se o
    /// valor é a máscara `***` (cópia de uma tela que escondeu o segredo) ou se o secret não existe.
    /// Resposta: `Ok`.
    EnvValueSet {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        key: String,
        value: EnvVarValue,
    },
    /// Renomeia um **secret** (do projeto, ou do serviço se `service_id` vier) e atualiza, na mesma
    /// transação, toda env `Secret(from)` que resolvia para ele. O valor cifrado não é lido.
    /// Falha se `from` não existe ou `to` já existe no escopo. Resposta: `Ok`.
    SecretRename {
        project_id: String,
        #[serde(default)]
        service_id: Option<String>,
        /// Escopo de **job** (`Some` = secret/env daquele job; exige `service_id: None`). Na resolução de
        /// um secret do job vale o do job, depois o do serviço gatilho, depois o do projeto.
        #[serde(default)]
        job_id: Option<String>,
        from: String,
        to: String,
    },

    // Pacote de serviço (copiar um serviço entre servidores) — ver
    // `docs/plano-copiar-servico-entre-servidores.md`.
    /// O que dá para levar do serviço: suas variáveis e as do projeto (com a
    /// sugestão de pré-marcação). Nunca devolve valores. Resposta:
    /// `ServiceExportPlan`.
    ServiceExportPlan {
        service_id: String,
    },
    /// Gera o pacote (YAML) do serviço. `project_env_keys` são as variáveis do
    /// projeto marcadas pelo usuário (as demais não entram no arquivo);
    /// `include_values: false` leva só os nomes (`${CHAVE}`). Resposta:
    /// `ServiceBundleYaml`.
    ServiceExport {
        service_id: String,
        include_values: bool,
        #[serde(default)]
        project_env_keys: Vec<String>,
    },
    /// Dados do serviço (volumes): o que um export levaria. Só olha, não muda nada. Os bytes
    /// vão por `GET /api/services/<id>/data/export` e `POST /api/services/<id>/data/import`.
    /// Resposta: `ServiceDataPlan`.
    ServiceDataPlan {
        service_id: String,
    },
    /// Portas do container que o serviço provavelmente tem (o que o container escuta, o que a imagem e o
    /// compose declaram, e uma tabela por produto), para sugerir ao configurar as portas TCP. Só olha.
    /// Resposta: `ServicePortSuggestions`.
    ServicePortSuggestions {
        service_id: String,
    },
    /// O destino S3 dos backups de dados (sem a chave secreta). Resposta: `BackupDestination`.
    BackupDestinationGet,
    /// Grava o destino S3. `secret_key: None` mantém a chave já gravada. Confere os campos, mas não
    /// conecta (use `BackupDestinationTest`). Resposta: `BackupDestination`.
    BackupDestinationSet {
        destination: BackupDestination,
    },
    /// Apaga o destino gravado (os backups já enviados ficam no S3). Resposta: `Ok`.
    BackupDestinationClear,
    /// Testa a conexão: grava, lê e apaga um objeto pequeno. `destination: None` testa o gravado; com
    /// valor, testa o rascunho da tela (e `secret_key: None` usa a chave gravada). Resposta: `Ok`.
    BackupDestinationTest {
        #[serde(default)]
        destination: Option<BackupDestination>,
    },
    /// Os backups de dados de um serviço no S3 e a última operação. Resposta: `ServiceBackups`.
    ServiceBackupList {
        service_id: String,
    },
    /// Faz o backup dos dados do serviço para o S3 **em segundo plano** (volumes e bancos, cifrado com a
    /// chave de recuperação). Responde `Ok` assim que começa; o andamento e o resultado vêm em
    /// `ServiceBackupList(.last_run)`. `stop`: parar o serviço durante a cópia de volume (padrão
    /// `true`; banco não para). `keep`: depois de enviar, manter só os `keep` mais novos.
    ServiceBackupNow {
        service_id: String,
        #[serde(default = "backup_stop_default")]
        stop: bool,
        #[serde(default)]
        keep: Option<u32>,
    },
    /// Restaura um backup do S3 **substituindo** os dados atuais do serviço, em segundo plano (o mesmo
    /// caminho e a mesma rede de segurança da importação). Exige `confirm: true`. Resposta: `Ok`.
    ServiceBackupRestore {
        service_id: String,
        key: String,
        confirm: bool,
    },
    /// Apaga um backup do S3. Resposta: `Ok`.
    ServiceBackupDelete {
        service_id: String,
        key: String,
    },
    /// Grava (ou, com `None`, apaga) a política de backup automático do serviço. Ligar exige o destino S3
    /// configurado e a chave de recuperação. Resposta: `ServiceBackups` (já com a política e o próximo
    /// disparo).
    ServiceBackupPolicySet {
        service_id: String,
        policy: Option<BackupPolicy>,
    },
    /// **Recuperar este servidor** (passo 1): num servidor **limpo**, traz a infra do repositório git e
    /// recifra os segredos para este servidor, **sem** mexer nos dados nem subir nada. A chave privada de
    /// recuperação fica só na memória (até `ServerRecoverRun`, `ServerRecoverCancel` ou 1 hora). Resposta:
    /// `ServerRecover` (`phase: "ready"`, com o que foi achado e os backups por serviço).
    ServerRecoverStart {
        /// Endereço do repositório da infra: `https://…`, `file://…` ou um caminho absoluto.
        remote: String,
        /// Token de acesso ao repositório (vira um provider git deste servidor). Dispensável num remoto
        /// local (`file://…` ou caminho absoluto).
        #[serde(default)]
        token: String,
        #[serde(default)]
        branch: Option<String>,
        /// A chave privada de recuperação (`AGE-SECRET-KEY-1…`).
        recovery_key: String,
    },
    /// Em que ponto a recuperação está. Resposta: `ServerRecover`.
    ServerRecoverStatus,
    /// Recuperação (passo 2): restaura os dados e sobe cada serviço na ordem das dependências (volumes
    /// antes de subir, bancos depois). Em segundo plano; acompanhe por `ServerRecoverStatus`.
    /// Resposta: `Ok`.
    ServerRecoverRun,
    /// Desiste: esquece a chave de recuperação da memória. A infra restaurada continua no servidor.
    /// Resposta: `ServerRecover`.
    ServerRecoverCancel,
    /// A chave **pública** de recuperação deste servidor (`[secrets] recovery_recipient`), se houver.
    /// Nunca devolve a privada. Resposta: `RecoveryKey`.
    /// Plano: `docs/plano-chave-de-recuperacao.md`.
    RecoveryKeyStatus,
    /// **Gera** a chave de recuperação: grava só a pública no `config.toml` e devolve a privada **uma única
    /// vez** (o servidor não a guarda; quem chama deve baixá-la na hora). Já havendo chave, recusa — a não
    /// ser com `replace: true` (a antiga deixa de cifrar o que for exportado daí em diante). Resposta:
    /// `RecoveryKeyGenerated`.
    ///
    /// A privada sai num **arquivo de recuperação** (`.rpkit`, `features::data_backup::recovery_kit`)
    /// junto com o que a tela Recuperar pede: `infra_remote` (obrigatório), `infra_branch` (opcional) e o
    /// token da conta Git `provider_id` (obrigatório para `https://`; o token só entra se a URL for do
    /// host da conta). Tudo é conferido **antes** de gerar: um erro aqui não troca a chave.
    /// Plano: `docs/plano-arquivo-de-recuperacao.md`.
    RecoveryKeyGenerate {
        #[serde(default)]
        replace: bool,
        #[serde(default)]
        infra_remote: String,
        #[serde(default)]
        infra_branch: Option<String>,
        #[serde(default)]
        provider_id: Option<String>,
    },
    /// Cria UM serviço novo a partir de um pacote, sem sobrescrever nada.
    /// `dry_run` só devolve o relatório. Resposta: `ServiceImportReport`.
    ServiceImport(ServiceImportReq),

    // Jobs (tarefas one-shot via docker-compose, agendadas ou manuais)
    JobCreate {
        project_id: String,
        trigger_service_id: String,
        name: String,
        compose: String,
        /// `Some` = clona esse repo a cada execução em vez de usar `compose`
        /// (cliente manda `compose: ""` nesse caso). `#[serde(default)]`: os
        /// clientes só incluem esta chave no modo Git (Luau/JS table
        /// constructor com valor Lua/JS `nil`/`undefined` não gera a chave).
        #[serde(default)]
        git_source: Option<JobGitSource>,
        main_service: String,
        #[serde(default)]
        env_vars: Vec<EnvVar>,
        #[serde(default)]
        env_comments: Vec<EnvComment>,
        recurrence: Option<Recurrence>,
    },
    JobUpdate {
        id: String,
        name: String,
        compose: String,
        #[serde(default)]
        git_source: Option<JobGitSource>,
        main_service: String,
        #[serde(default)]
        env_vars: Vec<EnvVar>,
        #[serde(default)]
        env_comments: Vec<EnvComment>,
        enabled: bool,
        recurrence: Option<Recurrence>,
    },
    JobDelete {
        id: String,
    },
    /// Jobs de um único projeto (aba "Jobs" do projeto).
    JobList {
        project_id: String,
    },
    /// Todos os jobs, de todos os projetos (tela global da sidebar "Schedules").
    JobListAll,
    /// Dispara o job imediatamente, fora do agendamento.
    JobRunNow {
        id: String,
    },
    /// Cancela um `job_run` em andamento: mata o processo `docker compose
    /// up` de verdade (não só a task) — ver `AppState::active_jobs`. Erro
    /// `NotFound` se o run já não estiver mais registrado como em execução
    /// (terminou, ou o id nunca existiu).
    JobRunCancel {
        job_run_id: String,
    },
    JobRunHistory {
        job_id: String,
        limit: usize,
    },
    GetJobLogs {
        job_run_id: String,
    },
    /// Fonte única da verdade de "este job está quente?": confere se as imagens
    /// e o cache de build dele ainda estão no host. Resposta: `JobWarmStatus`.
    JobWarmStatus {
        job_id: String,
    },
    /// Aquece o job (baixa as imagens que faltam e popula o cache de build) sem
    /// executá-lo. Já quente: não faz nada e avisa. Senão a vaga única da fila
    /// é disputada em segundo plano e o fim chega em `Event::JobWarmChanged`.
    ///
    /// `force = true` **reconstrói do zero** mesmo com o job quente: rebaixa as
    /// imagens de registry e roda o build sem cache (`--no-cache --pull`), para
    /// pegar imagem base nova ou refazer um `RUN` que buscou algo externo.
    /// Ausente no JSON (clientes antigos) vale `false`.
    /// Resposta: `JobWarmRequested`.
    JobWarm {
        job_id: String,
        #[serde(default)]
        force: bool,
    },

    /// O status de aquecimento (`ServiceWarmStatus`) de todos os serviços com
    /// fonte Git ou zip. Resposta: `ServiceWarmStatuses`.
    ServiceWarmStatusList,
    /// Aquece o serviço: constrói a imagem dele na vaga única da fila, para o
    /// próximo deploy achar o cache quente. Já quente: não faz nada e avisa
    /// (a menos que `force`, que reconstrói sem cache). O fim chega em
    /// `Event::ServiceWarmChanged`. Resposta: `ServiceWarmRequested`.
    ServiceWarm {
        service_id: String,
        #[serde(default)]
        force: bool,
    },

    // Docker cleanup
    PruneContainers,
    /// `all=true` remove volumes mesmo que não sejam anônimos (equivalente ao
    /// `docker volume prune --all`); `false` é o padrão do Docker.
    PruneVolumes {
        all: bool,
    },
    /// `all=true` remove toda imagem sem uso, não só as dangling/untagged
    /// (equivalente ao `docker image prune -a`); `false` é o padrão do Docker.
    PruneImages {
        all: bool,
    },
    PruneBuildCache,
    PruneNetworks,

    // Limpeza automática (agendada) dos recursos acima — ver
    // docs/plano-limpeza-automatica-docker.md. `RunNow` dispara o mesmo
    // caminho de código do scheduler, fora do horário agendado (botão
    // "Executar agora" da tela).
    DockerCleanupConfigGet,
    DockerCleanupConfigSet {
        config: DockerCleanupConfig,
    },
    DockerCleanupRunNow,

    // Docker inventory (every image/volume/network on the host, not just
    // rustploy-managed ones — see `crate::domain::DockerImageInfo` etc.)
    DockerImages,
    DockerVolumes,
    DockerNetworks,
    /// Todo container do host (rodando + parado), para a sub-aba Containers.
    DockerContainers,

    // Remoção INDIVIDUAL de um recurso Docker (o par por-item dos `Prune*`).
    // O Docker recusa remover recursos em uso (sem force) — o erro é propagado.
    RemoveContainer {
        id: String,
    },
    RemoveImage {
        id: String,
    },
    RemoveVolume {
        name: String,
    },
    RemoveNetwork {
        id: String,
    },
    /// Stops every container labeled `rustploy.managed=true`, regardless of
    /// what the DB's service status currently says (more robust than
    /// looping over `Service` rows one `ServiceStop` at a time — see
    /// `Command::ServiceStop`). Scoped to rustploy's own containers; never
    /// touches unrelated containers on the same Docker host.
    StopAllManaged,

    // Ingress proxy — a tabela de rotas viva do proxy reverso (em memória, não
    // no banco). É ela que decide para onde uma requisição HTTP vai agora, e
    // até aqui não havia como olhá-la nem corrigi-la sem um redeploy.
    /// Lê a tabela de rotas do ingress (domínios + portas). Diagnóstico de
    /// 502: a rota existe? para qual `ip:porta` ela aponta?
    IngressRoutes,
    /// Recalcula as rotas a partir dos containers que realmente existem no
    /// Docker, sem redeployar. `service_id = None` varre todos os serviços.
    /// É o mesmo caminho do reconcile de boot, exposto sob demanda.
    IngressReconcile {
        #[serde(default)]
        service_id: Option<String>,
    },

    // Env var backup / restore
    /// Lista os snapshots disponíveis (retorna Vec<String> com nomes de ficheiro).
    EnvBackupList,
    /// Restaura o snapshot com o nome dado (caminho relativo ao backup_dir).
    EnvBackupRestore {
        snapshot: String,
    },

    // Infrastructure
    Ping,
    DaemonStatus,
    DeployEngineStatus,
    /// Pergunta ao GitHub se há release mais nova que o daemon. Só leitura: não instala nada.
    /// O resultado fica em cache no daemon (o GitHub limita a API anônima a 60 req/h por IP);
    /// `force` ignora o cache, mas o daemon ainda espaça consultas forçadas em ~1 min.
    /// Resposta: `UpdateInfo` (ou `Err` com `UpdateCheckFailed` se o GitHub não responder).
    CheckUpdate {
        #[serde(default)]
        force: bool,
    },
    /// Atualiza o daemon para a última release **estável** (a que o `CheckUpdate` vê): baixa o
    /// pacote e a assinatura, tira um snapshot do banco e pede a instalação ao helper root, que
    /// só instala o que estiver assinado. O daemon reinicia no meio; acompanhe com `UpdateStatus`.
    /// Recusa se há deploy em andamento ou na fila, a menos que `allow_active_deploys`.
    /// O agente MCP só pode usar se `[update] allow_agent_apply = true`. Resposta: `Ok`.
    ApplyUpdate {
        #[serde(default)]
        allow_active_deploys: bool,
    },
    /// Estado da atualização em curso, ou o resultado da última. Resposta: `UpdateProgress`.
    UpdateStatus,
    /// Volta os binários para os do backup da última atualização (o único caminho que aceita
    /// uma versão menor). Mesma trava do agente MCP que `ApplyUpdate`. Resposta: `Ok`.
    UpdateRollback,
    /// Atualiza o **app desktop** (`rustploy-gui`) desta máquina para o da última release, pelo
    /// helper root do daemon (sem a senha do `pkexec`). Recusa se o app não está instalado aqui ou
    /// já está na versão mais nova. Espera a instalação terminar. Mesma trava do agente MCP que
    /// `ApplyUpdate`. O app aberto segue na versão antiga até ser reaberto. Resposta: `AppUpdated`.
    ApplyAppUpdate,

    // Git providers (Gitea OAuth2 / PAT)
    GitProviderList,
    GitProviderCreate {
        kind: GitProviderKind,
        name: String,
        base_url: String,
        auth_mode: GitAuthMode,
        oauth_client_id: Option<String>,
        oauth_client_secret: Option<String>,
        /// Personal Access Token, when `auth_mode == Pat`.
        pat: Option<String>,
    },
    /// Remove uma conta git. **Recusa** (`InUse`, com a lista) enquanto algum serviço ou job ainda
    /// aponta para ela — sem a conta eles não conseguem mais clonar —, a menos que `force` seja `true`.
    /// Os clientes avisam o usuário antes e só então mandam `force`. Resposta: `Ok`.
    GitProviderDelete {
        id: String,
        #[serde(default)]
        force: bool,
    },
    /// Returns the Gitea authorization URL for the client to open in a browser.
    GitOAuthStart {
        provider_id: String,
    },
    GitRepoList {
        provider_id: String,
    },
    GitBranchList {
        provider_id: String,
        repo_full_name: String,
    },

    // Wizard "Novo serviço" (catálogos + criação server-side). O cliente Luau
    // só dirige a UI; o daemon monta o ServiceSpec via `crate::domain::wizard` (que tem
    // acesso aos blueprints de `templates`).
    WizardCatalog {
        search: String,
    },
    WizardCreate(WizardCreateReq),
    /// Snapshot completo do dashboard como JSON (o mesmo que o SSE `/api/events`
    /// empurra a cada 2s). O cliente usa após uma mutação para refletir a
    /// mudança na hora, sem esperar o próximo tick. Resposta: `Snapshot(String)`.
    Snapshot,

    // Registry OCI embutido — sub-aba Docker > Registry (somente leitura +
    // delete; criar conteúdo só acontece via `docker push` externo).
    RegistryStatus,
    RegistryRepoList,
    RegistryTagList {
        repo: String,
    },
    /// Remove a tag; se outra tag apontar pro mesmo digest (mesmo manifest),
    /// ela também é removida — mesma semântica do DELETE da OCI spec (por
    /// digest, não por tag).
    RegistryTagDelete {
        repo: String,
        tag: String,
    },
    RegistryRepoDelete {
        repo: String,
    },
    /// Garbage collection do registry: remove manifests pendurados (sem tag e
    /// não referenciados por nenhum index), blobs órfãos (metadados + arquivos
    /// do CAS) e uploads abandonados há mais de 24 h. Resposta:
    /// `RegistryGcResult`.
    RegistryGc,
    /// Grava a política de retenção de tags (ligar/desligar e quantas manter).
    /// A leitura vem em `RegistryStatus(.retention)`. Resposta: `Ok`.
    RegistryRetentionSet {
        config: crate::domain::RegistryRetentionConfig,
    },
    /// Cria um token de acesso (Basic auth) — `scope` é `"pull"` ou `"push"`.
    /// Resposta traz o segredo em texto plano UMA ÚNICA VEZ.
    RegistryTokenCreate {
        name: String,
        scope: String,
    },
    RegistryTokenList,
    RegistryTokenRevoke {
        name: String,
    },
    /// Move um deploy enfileirado para o início da fila ("furar fila"). Sem
    /// efeito se o id não estiver na fila (ex.: já rodando/terminado).
    /// Resposta: `Ok`.
    DeployQueuePromote {
        deployment_id: String,
    },
    /// Reordena a fila para exatamente a ordem dada (ids de deployment). Ids
    /// ausentes/desconhecidos são ignorados; enfileirados omitidos ficam ao fim
    /// preservando a ordem relativa. Usado pelo drag-and-drop da GUI.
    /// Resposta: `Ok`.
    DeployQueueReorder {
        order: Vec<String>,
    },
    /// Pausa (`true`) ou retoma (`false`) a fila global. Pausada, o worker não
    /// puxa o próximo deploy; o que já estiver rodando segue até o fim.
    /// Resposta: `Ok`.
    DeployQueuePause {
        paused: bool,
    },
}

fn backup_stop_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::Command;

    #[test]
    fn job_warm_sem_force_vale_false() {
        // Cliente antigo (GUI/webui/agente) não manda `force`.
        let cmd: Command = serde_json::from_str(r#"{"JobWarm":{"job_id":"job_1"}}"#).unwrap();
        assert!(matches!(cmd, Command::JobWarm { force: false, .. }));
    }

    #[test]
    fn check_update_sem_force_vale_false() {
        let cmd: Command = serde_json::from_str(r#"{"CheckUpdate":{}}"#).unwrap();
        assert!(matches!(cmd, Command::CheckUpdate { force: false }));
    }

    #[test]
    fn apply_update_sem_flag_nao_ignora_deploys_ativos() {
        let cmd: Command = serde_json::from_str(r#"{"ApplyUpdate":{}}"#).unwrap();
        assert!(matches!(
            cmd,
            Command::ApplyUpdate {
                allow_active_deploys: false
            }
        ));
    }

    #[test]
    fn job_warm_com_force() {
        let cmd: Command =
            serde_json::from_str(r#"{"JobWarm":{"job_id":"job_1","force":true}}"#).unwrap();
        assert!(matches!(cmd, Command::JobWarm { force: true, .. }));
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn secret_generate_so_com_nome_usa_os_padroes() {
        let c: Command =
            serde_json::from_str(r#"{"SecretGenerate":{"project_id":"p","name":"JWT_SECRET"}}"#)
                .unwrap();
        assert!(matches!(
            c,
            Command::SecretGenerate {
                service_id: None,
                job_id: None,
                kind: SecretKind::Password,
                length: None,
                on_exists: SecretOnExists::Fail,
                ..
            }
        ));
        let c: Command = serde_json::from_str(
            r#"{"SecretGenerate":{"project_id":"p","service_id":"s","name":"K","kind":"Hex","length":64,"on_exists":"Keep"}}"#,
        )
        .unwrap();
        assert!(matches!(
            c,
            Command::SecretGenerate {
                kind: SecretKind::Hex,
                length: Some(64),
                on_exists: SecretOnExists::Keep,
                ..
            }
        ));
    }

    #[test]
    fn clientes_antigos_sem_service_id_continuam_validos() {
        let c: Command =
            serde_json::from_str(r#"{"SecretSet":{"project_id":"p","name":"K","value":"v"}}"#)
                .unwrap();
        assert!(matches!(
            c,
            Command::SecretSet {
                service_id: None,
                ..
            }
        ));
        let c: Command = serde_json::from_str(r#"{"SecretList":{"project_id":"p"}}"#).unwrap();
        assert!(matches!(
            c,
            Command::SecretList {
                service_id: None,
                ..
            }
        ));
        let c: Command =
            serde_json::from_str(r#"{"SecretDelete":{"project_id":"p","name":"K"}}"#).unwrap();
        assert!(matches!(
            c,
            Command::SecretDelete {
                service_id: None,
                ..
            }
        ));
    }

    #[test]
    fn comandos_novos_de_secret_ida_e_volta() {
        let c: Command = serde_json::from_str(
            r#"{"SecretMove":{"project_id":"p","service_id":"s","name":"K","op":"ServiceSecretToProject"}}"#,
        )
        .unwrap();
        assert!(matches!(
            c,
            Command::SecretMove {
                op: SecretMoveOp::ServiceSecretToProject,
                ..
            }
        ));
        let c: Command =
            serde_json::from_str(r#"{"SecretReveal":{"project_id":"p","name":"K"}}"#).unwrap();
        assert!(matches!(
            c,
            Command::SecretReveal {
                service_id: None,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_string(&Command::InfraAudit).unwrap(),
            "\"InfraAudit\""
        );
    }

    #[test]
    fn env_value_set_ida_e_volta() {
        let c: Command = serde_json::from_str(
            r#"{"EnvValueSet":{"project_id":"p","key":"A","value":{"Plain":"x"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            &c,
            Command::EnvValueSet { service_id: None, key, value: EnvVarValue::Plain(v), .. }
                if key == "A" && v == "x"
        ));
        let c: Command = serde_json::from_str(
            r#"{"EnvValueSet":{"project_id":"p","service_id":"s","key":"A","value":{"Secret":"S"}}}"#,
        )
        .unwrap();
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["EnvValueSet"]["value"]["Secret"], "S");
    }

    #[test]
    fn comandos_de_renomear_ida_e_volta() {
        let c: Command =
            serde_json::from_str(r#"{"EnvKeyRename":{"project_id":"p","from":"A","to":"B"}}"#)
                .unwrap();
        assert!(matches!(
            &c,
            Command::EnvKeyRename { service_id: None, from, to, .. } if from == "A" && to == "B"
        ));
        let c: Command = serde_json::from_str(
            r#"{"SecretRename":{"project_id":"p","service_id":"s","from":"A","to":"B"}}"#,
        )
        .unwrap();
        assert!(matches!(
            &c,
            Command::SecretRename { service_id: Some(s), .. } if s == "s"
        ));
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["SecretRename"]["to"], "B");
    }
}

#[cfg(test)]
mod state_git_tests {
    use super::*;

    #[test]
    fn configurar_so_com_o_modo_e_o_resto_opcional() {
        let c: Command = serde_json::from_str(r#"{"StateGitConfigure":{"mode":"off"}}"#).unwrap();
        assert!(matches!(
            c,
            Command::StateGitConfigure {
                remote: None,
                branch: None,
                provider_id: None,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_string(&Command::StateGitSyncNow).unwrap(),
            "\"StateGitSyncNow\""
        );
        let c: Command = serde_json::from_str(
            r#"{"InfraAuditAllow":{"project":"p","scope":"projeto","what":"env X","allowed":true}}"#,
        )
        .unwrap();
        assert!(matches!(c, Command::InfraAuditAllow { allowed: true, .. }));
    }

    #[test]
    fn achado_antigo_sem_allowed_continua_legivel() {
        let f: PlaintextFinding =
            serde_json::from_str(r#"{"definite":false,"project":"p","scope":"s","what":"w"}"#)
                .unwrap();
        assert!(!f.allowed);
    }
}

#[cfg(test)]
mod pull_tests {
    use super::*;

    #[test]
    fn comandos_do_pull_e_status_antigo_sem_campos_novos() {
        assert_eq!(
            serde_json::to_string(&Command::StateGitResume).unwrap(),
            "\"StateGitResume\""
        );
        let c: Command =
            serde_json::from_str(r#"{"StateGitDismissRemoval":{"id":"serviço:s1"}}"#).unwrap();
        assert!(matches!(c, Command::StateGitDismissRemoval { .. }));
        // um status gravado/enviado por uma versão anterior (sem os campos da Fase 7) segue legível
        let antigo = r#"{"mode":"sync","remote":null,"branch":"main","provider_id":null,
            "git_available":true,"recovery_configured":false,"last_commit":null,
            "last_push_ok":null,"last_push_at":null,"last_error":null,
            "blocked_by_plaintext":0,"needs_push":false}"#;
        let s: StateGitStatus = serde_json::from_str(antigo).unwrap();
        assert!(
            s.paused_reason.is_none() && s.removed_in_git.is_empty() && s.pending_deploy.is_empty()
        );
        assert_eq!((s.ahead, s.behind), (0, 0));
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn resolver_divergencia_e_parar_container() {
        let c: Command =
            serde_json::from_str(r#"{"StateGitResolve":{"choice":"UseRemote"}}"#).unwrap();
        assert!(matches!(
            c,
            Command::StateGitResolve {
                choice: StateGitResolveChoice::UseRemote
            }
        ));
        let c: Command = serde_json::from_str(r#"{"StopContainer":{"id":"abc"}}"#).unwrap();
        assert!(matches!(c, Command::StopContainer { .. }));
        // inventário gravado por uma versão anterior (sem `removed_in_git`) segue legível
        let antigo = r#"{"id":"a","name":"n","image":"i","state":"running","status":"Up","managed":true,"project":null,"service":null}"#;
        let ci: DockerContainerInfo = serde_json::from_str(antigo).unwrap();
        assert_eq!(ci.removed_in_git, None);
    }
}

#[cfg(test)]
mod deploy_batch_tests {
    use super::*;

    #[test]
    fn deploy_em_lote_ida_e_volta_e_skip_running_e_opcional() {
        let c: Command =
            serde_json::from_str(r#"{"DeployWithDependencies":{"service_id":"s"}}"#).unwrap();
        assert!(matches!(
            &c,
            Command::DeployWithDependencies { service_id, skip_running: false } if service_id == "s"
        ));
        let c: Command =
            serde_json::from_str(r#"{"ProjectDeploy":{"project_id":"p","skip_running":true}}"#)
                .unwrap();
        assert!(matches!(
            &c,
            Command::ProjectDeploy {
                skip_running: true,
                ..
            }
        ));
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["ProjectDeploy"]["project_id"], "p");
    }
}

#[cfg(test)]
mod recovery_key_tests {
    use super::*;

    #[test]
    fn gerar_sem_replace_assume_falso() {
        let c: Command = serde_json::from_str(r#"{"RecoveryKeyGenerate":{}}"#).unwrap();
        assert!(matches!(
            c,
            Command::RecoveryKeyGenerate { replace: false, .. }
        ));
        let c: Command =
            serde_json::from_str(r#"{"RecoveryKeyGenerate":{"replace":true}}"#).unwrap();
        assert!(matches!(
            c,
            Command::RecoveryKeyGenerate { replace: true, .. }
        ));
        let c: Command = serde_json::from_str(
            r#"{"RecoveryKeyGenerate":{"infra_remote":"https://g.io/i.git","provider_id":"p1"}}"#,
        )
        .unwrap();
        assert!(matches!(
            c,
            Command::RecoveryKeyGenerate { ref infra_remote, provider_id: Some(_), infra_branch: None, .. }
                if infra_remote == "https://g.io/i.git"
        ));
    }

    #[test]
    fn status_e_um_comando_sem_campos() {
        let c: Command = serde_json::from_str(r#""RecoveryKeyStatus""#).unwrap();
        assert!(matches!(c, Command::RecoveryKeyStatus));
    }
}
