//! **Ambiente**: o mesmo projeto, outra instância (homologação, preview…). Um ambiente é um projeto
//! comum (rede, nomes de container e env próprios) com um vínculo para o projeto de origem e para
//! cada serviço de origem. Ver `docs/plano-releases-ambientes-rotas-volumes.md` §2.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::ReleaseChange;

/// O vínculo de um projeto-ambiente com o projeto de onde ele foi copiado.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct EnvironmentOf {
    /// O projeto de origem (o "produção").
    pub origin_project_id: String,
    /// O nome do ambiente (`homologação`, `staging`, `preview-42`).
    pub name: String,
    /// Serviço deste ambiente → o serviço irmão na origem. Um serviço criado depois, só aqui, não
    /// aparece (não tem irmão).
    #[serde(default)]
    pub service_origins: BTreeMap<String, String>,
}

/// Um par de serviços irmãos na comparação de um ambiente com a origem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvironmentServicePair {
    pub service_name: String,
    pub origin_service_id: String,
    pub environment_service_id: String,
    /// O número da release no ar na origem (`None` = nada no ar ou nunca houve release).
    #[serde(default)]
    pub origin_release_number: Option<u32>,
    #[serde(default)]
    pub environment_release_number: Option<u32>,
    /// A release no ar em cada lado (o id), para promover.
    #[serde(default)]
    pub origin_release_id: Option<String>,
    #[serde(default)]
    pub environment_release_id: Option<String>,
    /// Os dois lados rodam a mesma imagem (referência fixa igual).
    pub same_image: bool,
    /// Dá para promover (a fonte não é Compose e há release no ar com imagem fixada).
    pub can_promote_to_origin: bool,
    pub can_bring_from_origin: bool,
    /// Por que não há botão (ou o que o botão vai fazer), para a tela explicar. Códigos:
    /// `compose` (stack Compose: não há imagem única para levar), `no_release_anywhere` (nenhum
    /// lado tem imagem: faça um deploy), `no_release_in_origin` / `no_release_here` (só um lado
    /// tem), `running_without_release` (o lado de onde se leva está no ar desde antes das releases:
    /// a imagem do container é registrada como release ao levar). `None` = nada a dizer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// O que difere na configuração do serviço (origem → ambiente). Domínios e portas externas,
    /// que são diferentes por definição, ficam de fora.
    pub changes: Vec<ReleaseChange>,
}

/// Resposta de `EnvironmentCompare`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvironmentComparison {
    pub origin_project_id: String,
    pub origin_project_name: String,
    pub environment_project_id: String,
    pub environment_name: String,
    pub services: Vec<EnvironmentServicePair>,
    /// Variáveis do projeto que diferem (origem → ambiente).
    pub project_env_changes: Vec<ReleaseChange>,
    /// Serviços que só existem num dos lados (nome).
    #[serde(default)]
    pub only_in_origin: Vec<String>,
    #[serde(default)]
    pub only_in_environment: Vec<String>,
}

/// Um ambiente de um projeto, na lista da origem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvironmentSummary {
    pub project_id: String,
    pub project_name: String,
    pub name: String,
}

/// O nome do projeto-ambiente: `loja (homologação)`.
pub fn environment_project_name(origin_name: &str, environment_name: &str) -> String {
    format!("{origin_name} ({environment_name})")
}

/// Nome de ambiente aceito: 1–40 caracteres, sem parênteses (que entram no nome do projeto) nem
/// quebra de linha.
pub fn is_valid_environment_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= 40
        && !trimmed.contains(['(', ')', '\n', '\r'])
}

/// Campos do spec que são diferentes por definição entre ambientes e não entram na comparação:
/// domínios, TLS, portas do host e o zip enviado (cada lado tem o seu; a imagem é que conta).
pub const ENVIRONMENT_FIELDS_IGNORED: [&str; 8] = [
    "domain",
    "domains",
    "tls_enabled",
    "host_port",
    "tcp_ports",
    "project_id",
    "source.Archive.archive_id",
    "source.Archive.original_filename",
];

/// Filtra das mudanças de um diff de spec os campos que diferem por definição entre ambientes.
pub fn environment_relevant(changes: Vec<ReleaseChange>) -> Vec<ReleaseChange> {
    changes
        .into_iter()
        .filter(|change| {
            change.scope != "service"
                || !ENVIRONMENT_FIELDS_IGNORED.iter().any(|ignored| {
                    change.field == *ignored || change.field.starts_with(&format!("{ignored}."))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nome_de_ambiente() {
        assert!(is_valid_environment_name("homologação"));
        assert!(is_valid_environment_name("preview-42"));
        assert!(!is_valid_environment_name("  "));
        assert!(!is_valid_environment_name("a (b)"));
        assert!(!is_valid_environment_name(&"x".repeat(41)));
        assert_eq!(
            environment_project_name("loja", "homologação"),
            "loja (homologação)"
        );
    }

    #[test]
    fn comparacao_ignora_dominio_e_porta() {
        let change = |field: &str| ReleaseChange {
            scope: "service".into(),
            field: field.into(),
            before: Some("a".into()),
            after: Some("b".into()),
        };
        let kept = environment_relevant(vec![
            change("domains"),
            change("tcp_ports"),
            change("replicas"),
            change("domain"),
            change("source.Git.branch"),
        ]);
        assert_eq!(
            kept.iter()
                .map(|change| change.field.as_str())
                .collect::<Vec<_>>(),
            vec!["replicas", "source.Git.branch"]
        );
    }
}
