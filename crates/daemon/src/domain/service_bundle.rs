use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Uma variável listada na tela de checkboxes do export.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanVar {
    pub key: String,
    /// `secret:NOME` — só a referência viaja, nunca o valor.
    pub is_secret: bool,
    /// Pré-marcar: o serviço cita esta variável por nome (`${VAR}`/`$VAR`).
    /// Sempre `false` nas variáveis do serviço (essas vão todas).
    pub suggested: bool,
}

/// Resposta de `ServiceExportPlan`: o que dá para levar, para a tela marcar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceExportPlan {
    pub service_name: String,
    pub project_name: String,
    pub service_env: Vec<PlanVar>,
    pub project_env: Vec<PlanVar>,
    /// Origem por zip (ou outro motivo): não exportável. A tela mostra o texto
    /// em vez do botão.
    pub blocked: Option<String>,
}

/// O que fazer, no destino, com UMA variável de projeto trazida no pacote.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProjectEnvChoice {
    /// Nova → cria no projeto; em conflito → mantém a do destino (padrão).
    #[default]
    Keep,
    /// Em conflito → troca a do projeto de destino pela do arquivo.
    Overwrite,
    /// Grava no env do serviço novo (que vence a do projeto), sem tocar no projeto.
    ServiceOnly,
    /// Não traz.
    Ignore,
}

/// Pedido de `ServiceImport`. Tudo opcional que não for o YAML.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceImportReq {
    pub yaml: String,
    /// Projeto de destino: aquele em que o usuário está (sempre já existe — o
    /// import é uma criação de serviço dentro do projeto, não cria projeto).
    pub project_id: String,
    /// Nome do serviço novo (padrão: o do pacote).
    #[serde(default)]
    pub name: Option<String>,
    /// Não traz as rotas de domínio.
    #[serde(default)]
    pub drop_domains: bool,
    /// Git provider do destino a usar (vence o referenciado por nome no pacote).
    #[serde(default)]
    pub git_provider_id: Option<String>,
    /// Valores dos `${CHAVE}` do env do SERVIÇO (pacote exportado sem valores).
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Idem, das variáveis do PROJETO.
    #[serde(default)]
    pub project_vars: BTreeMap<String, String>,
    /// Valores para secrets que não existem no projeto de destino.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// Escolha por variável de projeto (ausente = [`ProjectEnvChoice::Keep`]).
    #[serde(default)]
    pub project_env: BTreeMap<String, ProjectEnvChoice>,
    /// Deployar logo depois de criar (padrão: não).
    #[serde(default)]
    pub deploy: bool,
    /// Só analisa e devolve o relatório; nada é criado nem gravado.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportWarning {
    /// Identificador estável (`data_not_copied`, `domains_dns`, …).
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProjectEnvState {
    /// Não existe no projeto de destino.
    New,
    /// Já existe, com o mesmo valor.
    Same,
    /// Já existe, com outro valor.
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectEnvStatus {
    pub key: String,
    pub state: ProjectEnvState,
    pub is_secret: bool,
    /// A escolha efetivamente aplicada (a do pedido, ou o padrão).
    pub choice: ProjectEnvChoice,
}

/// Resposta de `ServiceImport` (também no `dry_run`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceImportReport {
    pub dry_run: bool,
    /// `Some` só quando o serviço foi de fato criado.
    pub service_id: Option<String>,
    /// Nome final do serviço (o do pedido, ou o do pacote).
    pub service_name: String,
    pub project_name: String,
    /// Já existe serviço com esse nome no projeto (bloqueia a criação).
    pub name_conflict: bool,
    pub warnings: Vec<ImportWarning>,
    pub missing_service_vars: Vec<String>,
    pub missing_project_vars: Vec<String>,
    pub missing_secrets: Vec<String>,
    /// Provider git do pacote que não existe no destino (nem foi escolhido outro).
    pub missing_git_provider: Option<String>,
    pub project_env: Vec<ProjectEnvStatus>,
    pub deployed: bool,
}
