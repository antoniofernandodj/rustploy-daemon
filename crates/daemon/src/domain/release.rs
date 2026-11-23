//! **Release**: o que rodou num deploy que chegou a Live — a imagem fixada e a configuração
//! congelada daquele momento. É o que torna o rollback de verdade possível (voltar código e
//! configuração juntos) e o que responde "o que mudou entre a v11 e a v12?".
//! Ver `docs/plano-releases-ambientes-rotas-volumes.md` §1.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::{EnvVar, EnvVarValue, ServiceSource, ServiceSpec};

/// Quantas releases mais recentes de cada serviço têm a imagem protegida da limpeza do Docker.
pub const PROTECTED_RELEASES_PER_SERVICE: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Release {
    /// `rel_<ulid>`.
    pub id: String,
    pub service_id: String,
    /// O deploy que virou esta release.
    pub deployment_id: String,
    /// 1, 2, 3… por serviço: o que a tela mostra como **v12**.
    pub number: u32,
    /// O nome da imagem usado no deploy (`app:latest`, `rp_api:01kd…`). Vazio no Compose.
    pub image: String,
    /// A referência **fixa** da imagem: o `RepoDigest` (`app@sha256:…`) quando veio de um registry,
    /// ou o id (`sha256:…`) quando foi construída aqui. `None` no Compose ou se a inspeção falhou.
    #[serde(default)]
    pub image_ref: Option<String>,
    /// O `ServiceSpec` daquele momento. As variáveis `Secret(nome)` ficam como referência: o valor
    /// do secret **não** é copiado (voltar usa o valor atual de cada secret).
    pub spec: ServiceSpec,
    /// As variáveis do **projeto** daquele momento (só para o diff; voltar não as restaura).
    #[serde(default)]
    pub project_env: Vec<EnvVar>,
    /// Esta release foi criada por um "voltar para a vN": o número da release de origem.
    #[serde(default)]
    pub restored_from_number: Option<u32>,
    pub created_at: DateTime<Utc>,
    /// Calculado na leitura: é a release do deploy que está Live agora.
    #[serde(default)]
    pub is_current: bool,
    /// Calculado na leitura: dá para voltar para ela (e por quê não, se não der).
    #[serde(default)]
    pub image_status: ReleaseImageStatus,
}

/// Se a imagem de uma release ainda está disponível para um "voltar".
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReleaseImageStatus {
    /// A imagem está neste servidor.
    Local,
    /// Não está aqui, mas tem digest de registry: o "voltar" a baixa de novo.
    Pullable,
    /// A imagem foi apagada (pela limpeza do Docker, por exemplo) e não há como baixá-la.
    Missing,
    /// Compose: não há uma imagem; voltar refaz o `compose up` com o conteúdo daquela release.
    #[default]
    NotApplicable,
}

/// Uma diferença entre duas configurações (duas releases, ou uma release e o que está declarado agora).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseChange {
    /// `image`, `service` (um campo do spec), `service_env` (uma variável do serviço) ou
    /// `project_env` (uma variável do projeto).
    pub scope: String,
    /// O nome do campo (`replicas`, `source.Git.branch`) ou da variável (`DATABASE_URL`).
    pub field: String,
    /// O valor antes (`None` = não existia). Secret aparece como `secret:NOME`, nunca o valor.
    pub before: Option<String>,
    /// O valor depois (`None` = deixou de existir).
    pub after: Option<String>,
}

/// Resposta de `ReleaseDiff`: o que muda indo de `from` para `to`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseDiff {
    /// Rótulo do lado de origem: `v9`, ou `current` para o que está declarado agora.
    pub from_label: String,
    /// Rótulo do lado de destino.
    pub to_label: String,
    pub changes: Vec<ReleaseChange>,
}

/// Uma configuração comparável: a imagem, o spec e as variáveis do projeto.
pub struct ReleaseSide<'a> {
    pub image_ref: Option<&'a str>,
    pub spec: &'a ServiceSpec,
    pub project_env: &'a [EnvVar],
}

/// O valor de uma variável como a tela pode mostrar: secret vira `secret:NOME`.
pub fn env_display(value: &EnvVarValue) -> String {
    match value {
        EnvVarValue::Plain(text) => text.clone(),
        EnvVarValue::Secret(name) => format!("secret:{name}"),
    }
}

fn env_map(vars: &[EnvVar]) -> BTreeMap<String, String> {
    vars.iter()
        .map(|var| (var.key.clone(), env_display(&var.value)))
        .collect()
}

/// Achata um valor JSON em `caminho → texto`. Objetos descem (`source.Git.branch`); listas e
/// escalares viram um texto só (uma lista de domínios mudou = uma linha no diff).
fn flatten(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, child, out);
            }
        }
        serde_json::Value::Null => {}
        serde_json::Value::String(text) => {
            out.insert(prefix.to_string(), text.clone());
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// Campos do spec que não entram no diff: as variáveis (comparadas uma a uma, à parte) e os
/// comentários do editor `.env` (não mudam o que roda).
const SPEC_FIELDS_COMPARED_APART: [&str; 2] = ["env_vars", "env_comments"];

fn spec_fields(spec: &ServiceSpec) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(serde_json::Value::Object(mut map)) = serde_json::to_value(spec) {
        for field in SPEC_FIELDS_COMPARED_APART {
            map.remove(field);
        }
        flatten("", &serde_json::Value::Object(map), &mut out);
    }
    out
}

fn diff_maps(
    scope: &str,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    out: &mut Vec<ReleaseChange>,
) {
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let old = before.get(key);
        let new = after.get(key);
        if old != new {
            out.push(ReleaseChange {
                scope: scope.to_string(),
                field: key.clone(),
                before: old.cloned(),
                after: new.cloned(),
            });
        }
    }
}

/// O que muda indo de `from` para `to`: imagem, campos do spec, variáveis do serviço e do projeto.
pub fn diff_releases(from: &ReleaseSide<'_>, to: &ReleaseSide<'_>) -> Vec<ReleaseChange> {
    let mut changes = Vec::new();
    // Imagem: só quando os dois lados a conhecem. "Declarado agora" não tem imagem fixada (é o
    // próximo build que vai dizer), e mostrar "imagem: sha256:… → (nada)" só confundiria.
    if let (Some(old), Some(new)) = (from.image_ref, to.image_ref)
        && old != new
    {
        changes.push(ReleaseChange {
            scope: "image".to_string(),
            field: "image".to_string(),
            before: Some(old.to_string()),
            after: Some(new.to_string()),
        });
    }
    diff_maps(
        "service",
        &spec_fields(from.spec),
        &spec_fields(to.spec),
        &mut changes,
    );
    diff_maps(
        "service_env",
        &env_map(&from.spec.env_vars),
        &env_map(&to.spec.env_vars),
        &mut changes,
    );
    diff_maps(
        "project_env",
        &env_map(from.project_env),
        &env_map(to.project_env),
        &mut changes,
    );
    changes
}

/// A fonte da release dá para voltar só trocando a imagem (pulando o build)? Compose não: refaz o
/// `compose up` com o conteúdo gravado.
pub fn release_skips_build(source: &ServiceSource) -> bool {
    !matches!(source, ServiceSource::Compose(_))
}

/// Separa `repo:tag` (aceitando registry com porta, `host:5000/app:v1`) para o `docker tag`.
/// Sem tag explícita, `latest`. Um `repo@sha256:…` não tem tag: devolve `None`.
pub fn split_repository_tag(image: &str) -> Option<(String, String)> {
    if image.contains('@') {
        return None;
    }
    let last_slash = image.rfind('/').map(|index| index + 1).unwrap_or(0);
    match image[last_slash..].rfind(':') {
        Some(colon) => {
            let colon = last_slash + colon;
            Some((image[..colon].to_string(), image[colon + 1..].to_string()))
        }
        None => Some((image.to_string(), "latest".to_string())),
    }
}

/// A referência fixa de uma imagem, a partir da inspeção dela. Imagem de registry: o `RepoDigest`
/// do mesmo repositório (`app@sha256:…`), que dá para baixar de novo depois de um push na tag;
/// sem digest, o id. Imagem construída aqui: o id (`sha256:…`), que só existe neste servidor.
pub fn pick_image_ref(
    from_registry: bool,
    image: &str,
    image_id: Option<&str>,
    repo_digests: &[String],
) -> Option<String> {
    if from_registry {
        let repository = split_repository_tag(image).map(|(repository, _)| repository);
        let same_repository = repo_digests.iter().find(|digest| {
            digest
                .split_once('@')
                .is_some_and(|(name, _)| Some(name) == repository.as_deref())
        });
        if let Some(digest) = same_repository.or(repo_digests.first()) {
            return Some(digest.clone());
        }
    }
    image_id.map(str::to_string)
}

/// A referência é um digest de registry (`repo@sha256:…`), que dá para baixar de novo?
pub fn image_ref_is_pullable(image_ref: &str) -> bool {
    image_ref.contains('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referencia_fixa_prefere_digest_do_mesmo_repositorio() {
        let digests = vec![
            "mirror.io/app@sha256:111".to_string(),
            "ghcr.io/org/app@sha256:222".to_string(),
        ];
        assert_eq!(
            pick_image_ref(true, "ghcr.io/org/app:v2", Some("sha256:id"), &digests).as_deref(),
            Some("ghcr.io/org/app@sha256:222")
        );
        assert_eq!(
            pick_image_ref(true, "app:latest", Some("sha256:id"), &[]).as_deref(),
            Some("sha256:id")
        );
        assert_eq!(
            pick_image_ref(false, "rp_api:01kd", Some("sha256:id"), &digests).as_deref(),
            Some("sha256:id")
        );
        assert!(image_ref_is_pullable("ghcr.io/org/app@sha256:222"));
        assert!(!image_ref_is_pullable("sha256:id"));
    }

    fn spec_with(replicas: u32, env: Vec<EnvVar>) -> ServiceSpec {
        ServiceSpec {
            name: "api".into(),
            project_id: "prj_1".into(),
            source: ServiceSource::Registry {
                image: "app:latest".into(),
            },
            port: 8080,
            host_port: None,
            tcp_ports: vec![],
            domain: None,
            tls_enabled: false,
            env_vars: env,
            env_comments: vec![],
            volumes: vec![],
            healthcheck: crate::domain::Healthcheck::default(),
            replicas,
            resources: crate::domain::ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            product: None,
            domains: vec![],
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec![],
            shared: None,
            depends_on: vec![],
            registry_auto_deploy: false,
            autostart: false,
            links: Vec::new(),
        }
    }

    fn plain(key: &str, value: &str) -> EnvVar {
        EnvVar {
            key: key.to_string(),
            value: EnvVarValue::Plain(value.to_string()),
        }
    }

    #[test]
    fn diff_lista_campo_variavel_e_imagem_que_mudaram() {
        let old_spec = spec_with(1, vec![plain("MODE", "a"), plain("GONE", "x")]);
        let new_spec = spec_with(
            2,
            vec![
                plain("MODE", "b"),
                EnvVar {
                    key: "TOKEN".into(),
                    value: EnvVarValue::Secret("API_TOKEN".into()),
                },
            ],
        );
        let changes = diff_releases(
            &ReleaseSide {
                image_ref: Some("sha256:aaa"),
                spec: &old_spec,
                project_env: &[plain("REGION", "us")],
            },
            &ReleaseSide {
                image_ref: Some("sha256:bbb"),
                spec: &new_spec,
                project_env: &[plain("REGION", "us")],
            },
        );
        let fields: Vec<(&str, &str)> = changes
            .iter()
            .map(|change| (change.scope.as_str(), change.field.as_str()))
            .collect();
        assert_eq!(
            fields,
            vec![
                ("image", "image"),
                ("service", "replicas"),
                ("service_env", "GONE"),
                ("service_env", "MODE"),
                ("service_env", "TOKEN"),
            ]
        );
        let token = changes
            .iter()
            .find(|change| change.field == "TOKEN")
            .unwrap();
        assert_eq!(token.after.as_deref(), Some("secret:API_TOKEN"));
        assert_eq!(token.before, None);
    }

    #[test]
    fn diff_sem_mudanca_e_vazio_e_ignora_comentarios() {
        let mut left = spec_with(1, vec![plain("A", "1")]);
        let right = left.clone();
        left.env_comments = vec![crate::domain::EnvComment {
            text: "# só um comentário".into(),
            before_key: Some("A".into()),
        }];
        let changes = diff_releases(
            &ReleaseSide {
                image_ref: None,
                spec: &left,
                project_env: &[],
            },
            &ReleaseSide {
                image_ref: Some("sha256:x"),
                spec: &right,
                project_env: &[],
            },
        );
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn diff_pega_variavel_do_projeto() {
        let spec = spec_with(1, vec![]);
        let changes = diff_releases(
            &ReleaseSide {
                image_ref: None,
                spec: &spec,
                project_env: &[plain("REGION", "us")],
            },
            &ReleaseSide {
                image_ref: None,
                spec: &spec,
                project_env: &[plain("REGION", "eu")],
            },
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].scope, "project_env");
    }

    #[test]
    fn separa_repositorio_e_tag() {
        assert_eq!(
            split_repository_tag("app"),
            Some(("app".into(), "latest".into()))
        );
        assert_eq!(
            split_repository_tag("ghcr.io/org/app:v2"),
            Some(("ghcr.io/org/app".into(), "v2".into()))
        );
        assert_eq!(
            split_repository_tag("localhost:5000/app"),
            Some(("localhost:5000/app".into(), "latest".into()))
        );
        assert_eq!(
            split_repository_tag("rp_api:01kd1234"),
            Some(("rp_api".into(), "01kd1234".into()))
        );
        assert_eq!(split_repository_tag("app@sha256:abc"), None);
    }
}
