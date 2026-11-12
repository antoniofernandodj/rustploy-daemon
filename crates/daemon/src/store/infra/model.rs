//! O **estado de infra tipado** (Fase 4 do plano `docs/plano-backup-estado-em-git.md`, R2/R3) e
//! como ele vira arquivos YAML e volta.
//!
//! Layout no disco (um arquivo pequeno por assunto → diffs pequenos no git):
//! ```text
//! MANIFEST.json                    {"schema": 1}
//! settings.yml                     as 3 chaves de configuração do daemon_settings
//! providers.yml                    git providers, SEM tokens de login (esses ficam no SQL)
//! projects/<id>/project.yml        projeto + serviços + jobs + secrets + bancos + acessos
//! ```
//! O diretório do projeto usa o **id** (e não o nome): o nome pode ter espaço/acento, e renomear
//! o projeto não pode virar "arquivo apagado + arquivo novo" no histórico do git.
//!
//! O que **não** entra aqui, de propósito, por ser runtime (muda toda hora e só geraria diff):
//! `service.status`/`live_container_id` (`service_runtime`), `service.updated_at`,
//! `job.last_run_at`/`next_run_at`, os tokens de login dos providers e as chaves `*_last_run`/`warm`
//! do `daemon_settings`.

use super::txn::FileTree;
use super::yaml::{from_yaml, to_yaml};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::{EnvComment, EnvVar, JobGitSource, Recurrence, ServiceSpec};
use std::collections::BTreeMap;

/// Versão do formato dos arquivos. Sobe quando o YAML mudar de forma incompatível.
///
/// - v2 (2026-11): `ServiceSpec.db_kind` virou `product`. Arquivos v1 são convertidos **na leitura**
///   (ver [`upgrade_project_v1`]); o disco só muda na próxima gravação (ou em `Repo::open`).
pub const SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InfraState {
    pub settings: SettingsInfra,
    pub providers: Vec<ProviderInfra>,
    pub projects: Vec<ProjectInfra>,
}

/// As chaves de configuração do `daemon_settings` (R2.1). `webhook_base_url` está descartada.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SettingsInfra {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_domain: Option<String>,
    /// O JSON de `docker_cleanup_config`, como valor estruturado (fica legível no YAML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_cleanup_config: Option<serde_json::Value>,
    /// O JSON de `shared::RegistryRetentionConfig` (retenção de tags do registry embutido).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_retention: Option<serde_json::Value>,
    /// O destino S3 dos backups de dados (JSON; a chave secreta vai como `ENC[age:…]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_destination: Option<serde_json::Value>,
}

/// Config de um git provider. `oauth_client_secret_enc` já é cifrado hoje; os tokens de login
/// (`access_token_enc`/`refresh_token_enc`) ficam só no SQL (decisão da Pergunta 9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfra {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub base_url: String,
    pub auth_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_secret_enc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_avatar: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectInfra {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_comments: Vec<EnvComment>,
    /// Nome da rede Docker, gravado na criação (`docs/plano-nome-gravado-rede-e-stack.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_name: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceInfra>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobInfra>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretInfra>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_databases: Vec<ManagedDatabaseInfra>,
    /// Servidores de banco compartilhado que **este projeto** está autorizado a alcançar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_access: Vec<SharedAccessInfra>,
    /// Ordem de exibição dos serviços (ids), escolhida à mão. A lista `services` fica
    /// ordenada por nome (bytes estáveis no git); esta diz como mostrar. Vazia = ordem
    /// de criação. Ids de serviços que não existem mais são podados em `normalize`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_order: Vec<String>,
}

impl ProjectInfra {
    /// Serviços na ordem de exibição: primeiro os de `service_order`, na ordem dela; os
    /// que ela não cita (criados depois) vêm em seguida, por data de criação.
    pub fn services_in_order(&self) -> Vec<&ServiceInfra> {
        let mut out: Vec<&ServiceInfra> = self.services.iter().collect();
        out.sort_by_key(|s| {
            (
                self.service_order
                    .iter()
                    .position(|id| id == &s.id)
                    .unwrap_or(usize::MAX),
                s.created_at,
                s.id.clone(),
            )
        });
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceInfra {
    pub id: String,
    pub spec: ServiceSpec,
    /// Nome da stack Compose, gravado na criação — os volumes se chamam `<stack>_<volume>`, então
    /// restaurar sem isso renomearia a stack e os dados "sumiriam" (R2.1, ponto 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_project: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Segredo do webhook de push, cifrado (`ENC[age:…]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_token: Option<String>,
    /// Secrets **deste serviço** (escopo de serviço, R5). Valor sempre `ENC[age:…]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretInfra>,
    /// Política de backup automático dos dados (Fase 3 de `docs/plano-backup-de-dados-para-s3.md`).
    /// Fora do `ServiceSpec` de propósito: é decisão do servidor, não viaja no "Exportar serviço".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupInfra>,
}

/// A política de backup e **desde quando** vale (referência do primeiro disparo: um `Daily 03:00`
/// ligado às 10h roda às 03:00 do dia seguinte, não na hora).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupInfra {
    pub policy: shared::BackupPolicy,
    pub since: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobInfra {
    pub id: String,
    /// `None` = job autônomo (no SQL é a string vazia).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_service_id: Option<String>,
    pub name: String,
    pub compose: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_source: Option<JobGitSource>,
    pub main_service: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_comments: Vec<EnvComment>,
    /// Secrets **deste job** (escopo de job). Valor sempre `ENC[age:…]`; valem antes dos do serviço
    /// gatilho e dos do projeto.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretInfra>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
    pub created_at: DateTime<Utc>,
}

/// Hoje o `value` já vem cifrado pelo `SecretsManager` (passphrase). A Fase 5 troca para
/// `ENC[age:…]`; até lá o YAML carrega o mesmo texto cifrado que o SQL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretInfra {
    pub id: String,
    pub key: String,
    pub value: String,
}

/// `password` é gravada **cifrada** (`ENC[age:…]`); quem precisa do valor abre com
/// `store::managed_database::open`. Um registro antigo, em claro, é cifrado no boot (`infra::seal`)
/// e até lá o `InfraAudit` o aponta como certeza.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedDatabaseInfra {
    pub id: String,
    pub server_service_id: String,
    pub name: String,
    pub password: String,
    pub env_var: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout_ms: Option<u32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedAccessInfra {
    pub server_service_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct ProvidersFile {
    providers: Vec<ProviderInfra>,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    schema: u32,
}

const MANIFEST: &str = "MANIFEST.json";

/// Versão do formato gravada na pasta (`None` = sem `MANIFEST.json`).
pub fn disk_schema(tree: &FileTree) -> Result<Option<u32>> {
    let Some(m) = tree.read(MANIFEST)? else {
        return Ok(None);
    };
    let m: ManifestFile = serde_json::from_slice(&m).context("lendo MANIFEST.json")?;
    Ok(Some(m.schema))
}
const SETTINGS: &str = "settings.yml";
const PROVIDERS: &str = "providers.yml";

/// O caminho é um arquivo do estado (e não um `README`, `.gitignore`…)?
pub fn is_state_file(rel: &str) -> bool {
    rel == MANIFEST
        || rel == SETTINGS
        || rel == PROVIDERS
        || (rel.starts_with("projects/") && rel.ends_with("/project.yml"))
}

fn project_path(id: &str) -> String {
    format!("projects/{id}/project.yml")
}

impl InfraState {
    /// Põe tudo em ordem estável. Dois estados iguais passam a gerar bytes idênticos.
    pub fn normalize(&mut self) {
        self.providers
            .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        self.projects
            .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        for p in &mut self.projects {
            p.services
                .sort_by(|a, b| (&a.spec.name, &a.id).cmp(&(&b.spec.name, &b.id)));
            p.jobs
                .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
            p.secrets.sort_by(|a, b| a.key.cmp(&b.key));
            // Ordem manual: sem repetidos e sem ids de serviços que já não existem.
            let existing: std::collections::HashSet<&str> =
                p.services.iter().map(|s| s.id.as_str()).collect();
            let mut seen = std::collections::HashSet::new();
            p.service_order
                .retain(|id| existing.contains(id.as_str()) && seen.insert(id.clone()));
            for s in &mut p.services {
                s.secrets.sort_by(|a, b| a.key.cmp(&b.key));
            }
            for j in &mut p.jobs {
                j.secrets.sort_by(|a, b| a.key.cmp(&b.key));
            }
            p.managed_databases.sort_by(|a, b| {
                (&a.server_service_id, &a.name).cmp(&(&b.server_service_id, &b.name))
            });
            p.shared_access
                .sort_by(|a, b| a.server_service_id.cmp(&b.server_service_id));
        }
    }

    /// Valida as referências entre entidades — o "valida tudo" do passo 2 da transação (R3).
    pub fn validate(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for p in &self.projects {
            if !ids.insert(p.id.as_str()) {
                bail!("id de projeto repetido: {}", p.id);
            }
            if !names.insert(p.name.as_str()) {
                bail!("nome de projeto repetido: {}", p.name);
            }
        }
        let mut service_ids = std::collections::BTreeSet::new();
        for p in &self.projects {
            for s in &p.services {
                if !service_ids.insert(s.id.as_str()) {
                    bail!("id de serviço repetido: {}", s.id);
                }
            }
        }
        for p in &self.projects {
            for j in &p.jobs {
                if let Some(t) = &j.trigger_service_id
                    && !service_ids.contains(t.as_str())
                {
                    bail!(
                        "o job '{}' aponta para o serviço gatilho {t}, que não existe",
                        j.name
                    );
                }
            }
            let mut dbs = std::collections::BTreeSet::new();
            for m in &p.managed_databases {
                if !service_ids.contains(m.server_service_id.as_str()) {
                    bail!(
                        "o banco '{}' do projeto '{}' aponta para o servidor {}, que não existe",
                        m.name,
                        p.name,
                        m.server_service_id
                    );
                }
                if !dbs.insert((m.server_service_id.as_str(), m.name.as_str())) {
                    bail!(
                        "o servidor {} já tem um banco chamado '{}'",
                        m.server_service_id,
                        m.name
                    );
                }
            }
            for a in &p.shared_access {
                if !service_ids.contains(a.server_service_id.as_str()) {
                    bail!(
                        "o projeto '{}' tem acesso ao servidor {}, que não existe",
                        p.name,
                        a.server_service_id
                    );
                }
            }
        }
        Ok(())
    }

    /// O estado como mapa `caminho → bytes`, na ordem estável.
    pub fn to_files(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut s = self.clone();
        s.normalize();
        let mut out = BTreeMap::new();
        out.insert(
            MANIFEST.to_string(),
            serde_json::to_vec_pretty(&ManifestFile { schema: SCHEMA })
                .map(|mut v| {
                    v.push(b'\n');
                    v
                })
                .context("serializando MANIFEST.json")?,
        );
        out.insert(SETTINGS.into(), to_yaml(&s.settings)?);
        out.insert(
            PROVIDERS.into(),
            to_yaml(&ProvidersFile {
                providers: s.providers,
            })?,
        );
        for p in &s.projects {
            out.insert(project_path(&p.id), to_yaml(p)?);
        }
        Ok(out)
    }

    /// Lê o estado de uma árvore de arquivos. `Ok(None)` = não há infra gravada (sem `MANIFEST.json`).
    pub fn read(tree: &FileTree) -> Result<Option<InfraState>> {
        if tree.read(MANIFEST)?.is_none() {
            return Ok(None);
        }
        let mut files = BTreeMap::new();
        for rel in tree.list()? {
            if is_state_file(&rel) {
                let b = tree.read(&rel)?.with_context(|| format!("{rel} sumiu"))?;
                files.insert(rel, b);
            }
        }
        Self::from_files(&files)
    }

    /// Monta o estado a partir de `caminho → bytes` (de um disco **ou de uma árvore do git**, sem
    /// checkout). É a mesma função para a leitura local, a migração e o que chega por `pull`.
    /// Arquivos que não são do estado (um `README.md` no repositório, por exemplo) são ignorados.
    /// `Ok(None)` = não há `MANIFEST.json`.
    pub fn from_files(files: &BTreeMap<String, Vec<u8>>) -> Result<Option<InfraState>> {
        let Some(m) = files.get(MANIFEST) else {
            return Ok(None);
        };
        let m: ManifestFile = serde_json::from_slice(m).context("lendo MANIFEST.json")?;
        if m.schema > SCHEMA {
            bail!(
                "o estado de infra está no formato v{}, mas este daemon só entende até v{SCHEMA}; atualize o rustployd",
                m.schema
            );
        }
        let settings = match files.get(SETTINGS) {
            Some(b) => from_yaml(b).context(SETTINGS)?,
            None => SettingsInfra::default(),
        };
        let providers = match files.get(PROVIDERS) {
            Some(b) => from_yaml::<ProvidersFile>(b).context(PROVIDERS)?.providers,
            None => vec![],
        };
        let mut projects = vec![];
        for (rel, b) in files {
            if rel.starts_with("projects/") && rel.ends_with("/project.yml") {
                let project = if m.schema < 2 {
                    upgrade_project_v1(b).with_context(|| rel.clone())?
                } else {
                    from_yaml::<ProjectInfra>(b).with_context(|| rel.clone())?
                };
                projects.push(project);
            }
        }
        let mut st = InfraState {
            settings,
            providers,
            projects,
        };
        st.normalize();
        Ok(Some(st))
    }

    /// Grava o estado inteiro numa transação: escreve o que mudou e **apaga** os projetos que
    /// deixaram de existir.
    pub fn write(&self, tree: &FileTree) -> Result<()> {
        self.validate()?;
        let files = self.to_files()?;
        let mut tx = super::txn::Tx::new();
        for (rel, bytes) in &files {
            if tree.read(rel)?.as_deref() != Some(bytes.as_slice()) {
                tx.write(rel.clone(), bytes.clone());
            }
        }
        for rel in tree.list()? {
            if rel.starts_with("projects/") && !files.contains_key(&rel) {
                tx.delete(rel);
            }
        }
        if tx.is_empty() {
            return Ok(());
        }
        tree.commit(tx)
    }
}

/// Lê um `project.yml` do schema 1: renomeia `db_kind` → `product` em cada `services[].spec`
/// antes de tipar. Sem isto o `serde(default)` engoliria a chave velha e todo banco/broker/storage
/// viraria serviço comum, em silêncio.
fn upgrade_project_v1(bytes: &[u8]) -> Result<ProjectInfra> {
    let mut doc: serde_yaml::Value = from_yaml(bytes)?;
    let services = doc
        .get_mut("services")
        .and_then(serde_yaml::Value::as_sequence_mut);
    for svc in services.into_iter().flatten() {
        let Some(spec) = svc
            .get_mut("spec")
            .and_then(serde_yaml::Value::as_mapping_mut)
        else {
            continue;
        };
        if let Some(v) = spec.remove("db_kind") {
            // Só copia se `product` ainda não existir (um arquivo misto não perde o dado novo).
            if !spec.contains_key("product") {
                spec.insert("product".into(), v);
            }
        }
    }
    serde_yaml::from_value(doc).context("lendo YAML (schema 1)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn project(id: &str, name: &str) -> ProjectInfra {
        ProjectInfra {
            id: id.into(),
            name: name.into(),
            description: None,
            env_vars: vec![],
            env_comments: vec![],
            network_name: Some(format!("rp_{id}")),
            created_at: t("2026-01-01T10:00:00.123456789Z"),
            services: vec![],
            jobs: vec![],
            secrets: vec![SecretInfra {
                id: "s1".into(),
                key: "TOKEN".into(),
                value: "cifrado".into(),
            }],
            managed_databases: vec![],
            shared_access: vec![],
            service_order: vec![],
        }
    }

    #[test]
    fn ida_e_volta_pelo_disco_e_bytes_estaveis() {
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        let st = InfraState {
            settings: SettingsInfra {
                acme_email: Some("a@b.c".into()),
                registry_domain: None,
                docker_cleanup_config: Some(serde_json::json!({"enabled": true, "days": 7})),
                registry_retention: Some(serde_json::json!({"enabled": true, "keep_last": 5})),
                backup_destination: None,
            },
            providers: vec![],
            projects: vec![project("p2", "Segundo App"), project("p1", "Primeiro")],
        };
        assert!(InfraState::read(&tree).unwrap().is_none());
        st.write(&tree).unwrap();
        let back = InfraState::read(&tree).unwrap().unwrap();
        let mut esperado = st.clone();
        esperado.normalize();
        assert_eq!(back, esperado);
        // escrever de novo o mesmo estado não muda nada (nenhuma transação)
        let antes = tree.list().unwrap();
        st.write(&tree).unwrap();
        assert_eq!(antes, tree.list().unwrap());
        // dois estados iguais em ordens diferentes geram os mesmos bytes
        let mut inv = st.clone();
        inv.projects.reverse();
        assert_eq!(st.to_files().unwrap(), inv.to_files().unwrap());
    }

    #[test]
    fn projeto_removido_apaga_o_arquivo() {
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        let mut st = InfraState {
            projects: vec![project("p1", "A"), project("p2", "B")],
            ..Default::default()
        };
        st.write(&tree).unwrap();
        st.projects.pop();
        st.write(&tree).unwrap();
        let back = InfraState::read(&tree).unwrap().unwrap();
        assert_eq!(back.projects.len(), 1);
        assert!(tree.read("projects/p2/project.yml").unwrap().is_none());
    }

    #[test]
    fn validacao_recusa_referencia_quebrada() {
        let mut st = InfraState {
            projects: vec![project("p1", "A")],
            ..Default::default()
        };
        st.projects[0].shared_access.push(SharedAccessInfra {
            server_service_id: "fantasma".into(),
            created_at: t("2026-01-01T00:00:00Z"),
        });
        assert!(st.validate().is_err());
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        assert!(st.write(&tree).is_err());
        assert!(
            InfraState::read(&tree).unwrap().is_none(),
            "nada pode ter sido gravado"
        );
    }

    #[test]
    fn schema_futuro_e_recusado() {
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        let mut tx = super::super::txn::Tx::new();
        tx.write("MANIFEST.json", br#"{"schema": 99}"#.to_vec());
        tree.commit(tx).unwrap();
        let e = InfraState::read(&tree).unwrap_err().to_string();
        assert!(e.contains("v99"), "{e}");
    }
}

#[cfg(test)]
mod schema_v1_tests {
    use super::*;

    /// Fixture fiel: o YAML que o daemon grava hoje, com a chave trocada de volta para a do schema 1.
    fn files_v1() -> (InfraState, BTreeMap<String, Vec<u8>>) {
        let mut spec = crate::features::wizard::app_spec("db".into(), "p1".into());
        spec.product = Some("postgres".into());
        let comum = crate::features::wizard::app_spec("web".into(), "p1".into());
        let svc = |id: &str, spec| ServiceInfra {
            id: id.into(),
            spec,
            compose_project: None,
            created_at: Utc::now(),
            webhook_token: None,
            secrets: vec![],
            backup: None,
        };
        let st = InfraState {
            settings: SettingsInfra::default(),
            providers: vec![],
            projects: vec![ProjectInfra {
                id: "p1".into(),
                name: "App".into(),
                description: None,
                env_vars: vec![],
                env_comments: vec![],
                network_name: None,
                created_at: Utc::now(),
                services: vec![svc("s1", spec), svc("s2", comum)],
                jobs: vec![],
                secrets: vec![],
                managed_databases: vec![],
                shared_access: vec![],
                service_order: vec![],
            }],
        };
        let mut f = st.to_files().unwrap();
        f.insert(MANIFEST.into(), b"{\"schema\": 1}\n".to_vec());
        let p = project_path("p1");
        let yml = String::from_utf8(f[&p].clone()).unwrap();
        assert!(yml.contains("product: postgres"));
        f.insert(
            p,
            yml.replace("product: postgres", "db_kind: postgres")
                .into_bytes(),
        );
        (st, f)
    }

    #[test]
    fn schema_1_converte_db_kind_em_product_na_leitura() {
        let (esperado, files) = files_v1();
        let st = InfraState::from_files(&files).unwrap().unwrap();
        let svcs = &st.projects[0].services;
        assert_eq!(svcs[0].spec.product.as_deref(), Some("postgres"));
        assert_eq!(svcs[1].spec.product, None);
        assert_eq!(
            st.projects[0].services.len(),
            esperado.projects[0].services.len()
        );
        // regravado, o YAML não tem mais a chave velha e o schema sobe
        let out = st.to_files().unwrap();
        let yml = String::from_utf8(out[&project_path("p1")].clone()).unwrap();
        assert!(yml.contains("product: postgres"));
        assert!(!yml.contains("db_kind"));
        assert!(String::from_utf8_lossy(&out[MANIFEST]).contains("\"schema\": 2"));
    }

    #[test]
    fn open_atualiza_a_pasta_do_schema_1_e_guarda_backup() {
        let (_, files) = files_v1();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("infra");
        std::fs::create_dir_all(dir.join("projects/p1")).unwrap();
        for (rel, b) in &files {
            std::fs::write(dir.join(rel), b).unwrap();
        }
        let repo = super::super::repo::Repo::open(&dir).unwrap();
        let yml = std::fs::read_to_string(dir.join(project_path("p1"))).unwrap();
        assert!(yml.contains("product: postgres") && !yml.contains("db_kind"));
        let bak = std::fs::read_to_string(
            tmp.path()
                .join("infra.bak-schema1")
                .join(project_path("p1")),
        )
        .unwrap();
        assert!(bak.contains("db_kind: postgres"));
        let st = repo.snapshot();
        assert_eq!(
            st.projects[0].services[0].spec.product.as_deref(),
            Some("postgres")
        );
    }

    #[test]
    fn pull_de_arvore_do_schema_1_regrava_o_disco_no_formato_novo() {
        let (_, files) = files_v1();
        let repo = super::super::repo::Repo::temp();
        repo.apply_files(&files, |_, _, _| Ok(()), |_| Ok(()))
            .unwrap();
        let root = repo.tree().root().to_path_buf();
        let yml = std::fs::read_to_string(root.join(project_path("p1"))).unwrap();
        assert!(
            yml.contains("product: postgres") && !yml.contains("db_kind"),
            "{yml}"
        );
        assert!(
            std::fs::read_to_string(root.join(MANIFEST))
                .unwrap()
                .contains("\"schema\": 2")
        );
        assert_eq!(
            repo.snapshot().projects[0].services[0]
                .spec
                .product
                .as_deref(),
            Some("postgres")
        );
    }
}
