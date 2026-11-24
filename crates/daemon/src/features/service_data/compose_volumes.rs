//! O que um arquivo Compose diz sobre volumes: quais a stack monta, onde, e quais são **externos**
//! (`external: true`: o volume já existia e a stack só o usa — "emprestado"). E a inserção do bloco
//! `external` no texto, para o "usar volume existente" das telas. Ver
//! `docs/plano-volumes-tela-e-emprestados.md`.

use std::collections::BTreeMap;

/// Um volume nomeado que a stack monta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeVolumeUse {
    /// O nome real do volume no motor (`<stack>_<chave>`, o `name:` declarado, ou o nome do externo).
    pub volume_name: String,
    /// Onde ele entra no container (`/var/lib/postgresql/data`).
    pub container_path: String,
    /// O serviço da stack que monta (`rp_db`).
    pub compose_service: String,
    /// Declarado com `external: true`.
    pub external: bool,
}

/// Uma declaração do topo `volumes:`: o nome real e se é externo.
fn declared_volumes(
    doc: &serde_yaml::Value,
    compose_project: &str,
) -> BTreeMap<String, (String, bool)> {
    let mut out = BTreeMap::new();
    let Some(volumes) = doc.get("volumes").and_then(|v| v.as_mapping()) else {
        return out;
    };
    for (key, value) in volumes {
        let Some(key) = key.as_str() else { continue };
        let external = match value.get("external") {
            Some(serde_yaml::Value::Bool(flag)) => *flag,
            // Formato antigo: `external: {name: x}`.
            Some(serde_yaml::Value::Mapping(_)) => true,
            _ => false,
        };
        let old_external_name = value
            .get("external")
            .and_then(|e| e.get("name"))
            .and_then(|n| n.as_str());
        let explicit_name = value
            .get("name")
            .and_then(|n| n.as_str())
            .or(old_external_name);
        let real = match (explicit_name, external) {
            (Some(name), _) => name.to_string(),
            (None, true) => key.to_string(),
            (None, false) => format!("{compose_project}_{key}"),
        };
        out.insert(key.to_string(), (real, external));
    }
    out
}

/// Os volumes nomeados que a stack monta. Um arquivo que não é YAML válido devolve vazio (quem
/// chama trata como "não sei", nunca como erro: o deploy é que valida o arquivo).
pub fn compose_volume_uses(content: &str, compose_project: &str) -> Vec<ComposeVolumeUse> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(content) else {
        return Vec::new();
    };
    let declared = declared_volumes(&doc, compose_project);
    let mut uses = Vec::new();
    let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else {
        return uses;
    };
    for (service_name, service) in services {
        let Some(service_name) = service_name.as_str() else {
            continue;
        };
        let Some(mounts) = service.get("volumes").and_then(|v| v.as_sequence()) else {
            continue;
        };
        for mount in mounts {
            // Curto: `chave:/caminho[:ro]`. Longo: `{type: volume, source: chave, target: /caminho}`.
            let (source, target) = match mount {
                serde_yaml::Value::String(text) => {
                    let mut parts = text.splitn(3, ':');
                    (
                        parts.next().unwrap_or_default().to_string(),
                        parts.next().unwrap_or_default().to_string(),
                    )
                }
                serde_yaml::Value::Mapping(_) => {
                    if mount
                        .get("type")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t != "volume")
                    {
                        continue;
                    }
                    (
                        mount
                            .get("source")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        mount
                            .get("target")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    )
                }
                _ => continue,
            };
            // Pasta do servidor (`/srv`, `./x`, `~/x`) não é volume nomeado.
            let Some((real, external)) = declared.get(&source) else {
                continue;
            };
            uses.push(ComposeVolumeUse {
                volume_name: real.clone(),
                container_path: target,
                compose_service: service_name.to_string(),
                external: *external,
            });
        }
    }
    uses
}

/// Os nomes dos volumes **externos** que a stack usa (montados ou só declarados).
pub fn external_volume_names(content: &str, compose_project: &str) -> Vec<String> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(content) else {
        return Vec::new();
    };
    let mut names: Vec<String> = declared_volumes(&doc, compose_project)
        .into_values()
        .filter(|(_, external)| *external)
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A chave do volume no arquivo, derivada do nome real: só letras, dígitos, `_` e `-`.
fn key_for(volume_name: &str) -> String {
    let key: String = volume_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if key.is_empty() { "dados".into() } else { key }
}

/// Insere a declaração `external` de um volume que já existe no texto do Compose, sem reformatar o
/// resto (comentários e ordem ficam). Se já houver um `volumes:` no topo, entra nele; senão, um bloco
/// novo no fim. Devolve o texto novo e a **chave** a usar na montagem (`- chave:/caminho`), ou o motivo
/// de não ter inserido. Se o volume já estiver declarado como externo, não mexe.
pub fn insert_external_volume(
    content: &str,
    volume_name: &str,
) -> Result<(String, String), String> {
    if !crate::domain::is_named_volume(volume_name) {
        return Err(format!("nome de volume inválido: {volume_name:?}"));
    }
    if !content.trim().is_empty() && serde_yaml::from_str::<serde_yaml::Value>(content).is_err() {
        return Err("o arquivo atual não é um YAML válido: corrija-o antes".into());
    }
    if let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(content)
        && let Some((key, _)) = declared_volumes(&doc, "")
            .into_iter()
            .find(|(_, (name, external))| *external && name == volume_name)
    {
        return Ok((content.to_string(), key));
    }
    let mut key = key_for(volume_name);
    if let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(content) {
        let taken = declared_volumes(&doc, "");
        let base = key.clone();
        let mut counter = 2;
        while taken.contains_key(&key) {
            key = format!("{base}_{counter}");
            counter += 1;
        }
    }
    let entry = format!(
        "  # volume existente (adicionado pelo Rustploy): monte com `- {key}:/caminho` num serviço\n  {key}:\n    external: true\n    name: {volume_name}\n"
    );
    let lines: Vec<&str> = content.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.trim_end() == "volumes:" && !line.starts_with([' ', '\t']));
    let text = match top {
        Some(index) => {
            let mut out: Vec<String> = lines[..=index].iter().map(|l| l.to_string()).collect();
            out.push(entry.trim_end_matches('\n').to_string());
            out.extend(lines[index + 1..].iter().map(|l| l.to_string()));
            let mut joined = out.join("\n");
            if content.ends_with('\n') {
                joined.push('\n');
            }
            joined
        }
        None => {
            let mut text = content.trim_end_matches('\n').to_string();
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("volumes:\n");
            text.push_str(&entry);
            text
        }
    };
    if serde_yaml::from_str::<serde_yaml::Value>(&text).is_err() {
        return Err("não consegui inserir sem quebrar o YAML: adicione o bloco à mão".into());
    }
    Ok((text, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = "services:\n  rp_db:\n    image: mysql:8\n    volumes:\n      - mysql_data:/var/lib/mysql\n\nvolumes:\n  mysql_data:\n    external: true\n    name: flow_mysql_data\n";

    #[test]
    fn le_proprio_externo_longo_e_ignora_pasta() {
        let content = "services:\n  app:\n    volumes:\n      - data:/data\n      - ./cfg:/cfg\n      - /srv/x:/x:ro\n      - type: volume\n        source: velho\n        target: /old\n  db:\n    volumes:\n      - pg:/var/lib/postgresql\nvolumes:\n  data: {}\n  velho:\n    external: true\n  pg:\n    name: meu_pg\n";
        let uses = compose_volume_uses(content, "rp_x_app");
        let summary: Vec<(&str, &str, bool)> = uses
            .iter()
            .map(|u| {
                (
                    u.volume_name.as_str(),
                    u.container_path.as_str(),
                    u.external,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("rp_x_app_data", "/data", false),
                ("velho", "/old", true),
                ("meu_pg", "/var/lib/postgresql", false),
            ]
        );
        assert_eq!(external_volume_names(content, "rp_x_app"), vec!["velho"]);
        assert_eq!(
            external_volume_names(FLOW, "rp_flow"),
            vec!["flow_mysql_data"]
        );
        assert!(compose_volume_uses("isto: [nao fecha", "p").is_empty());
    }

    #[test]
    fn insere_no_volumes_existente_ou_cria_o_bloco() {
        let with_top =
            "# meu banco\nservices:\n  rp_db:\n    image: postgres\nvolumes:\n  pgdata: {}\n";
        let (text, key) = insert_external_volume(with_top, "rp_abc_db_pgdata").unwrap();
        assert_eq!(key, "rp_abc_db_pgdata");
        assert!(text.starts_with("# meu banco\n"), "comentário preservado");
        assert_eq!(external_volume_names(&text, "p"), vec!["rp_abc_db_pgdata"]);
        assert!(text.contains("  pgdata: {}"), "volume anterior preservado");

        let without = "services:\n  rp_db:\n    image: postgres\n";
        let (text, _) = insert_external_volume(without, "dados_velhos").unwrap();
        assert_eq!(external_volume_names(&text, "p"), vec!["dados_velhos"]);

        // Já declarado: não duplica.
        let (same, key) = insert_external_volume(FLOW, "flow_mysql_data").unwrap();
        assert_eq!(same, FLOW);
        assert_eq!(key, "mysql_data");

        // Chave ocupada: ganha sufixo.
        let busy = "services: {}\nvolumes:\n  pgdata: {}\n";
        let (text, key) = insert_external_volume(busy, "pgdata").unwrap();
        assert_eq!(key, "pgdata_2");
        assert_eq!(external_volume_names(&text, "p"), vec!["pgdata"]);

        assert!(insert_external_volume("x: [", "v").is_err());
        assert!(insert_external_volume(without, "../x").is_err());
    }
}
