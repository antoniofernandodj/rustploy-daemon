//! Quais volumes pertencem a um serviço. O nome real do volume é um detalhe interno: para fora
//! só sai o nome lógico (`Part::name`).

use anyhow::{Result, anyhow};
use shared::{Service, ServiceSource};

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Dump lógico do banco do serviço (precisa estar no ar).
    Database(super::database::DbTarget),
    /// Volume nomeado do motor (`rp_xxx_pgdata`, ou o nome que o spec declara).
    Named(String),
    /// Pasta do servidor que o spec monta (já passou por [`bind_allowed`]).
    Bind(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    /// Nome lógico: é o que viaja no pacote e o que casa na importação.
    pub name: String,
    pub source: Source,
}

/// Caminhos do servidor que nunca entram, mesmo se o serviço os montar: o estado do próprio
/// Rustploy e pastas do sistema.
const BIND_DENYLIST: &[&str] = &[
    "/etc/rustploy",
    "/var/lib/rustploy",
    "/var/run",
    "/run",
    "/proc",
    "/sys",
    "/dev",
    "/root",
    "/boot",
];

/// `path` pode virar parte de um pacote? Só absoluto, nunca `/`, `..`, nem dentro/acima da denylist.
pub fn bind_allowed(path: &str) -> bool {
    if !path.starts_with('/') || path.split('/').any(|c| c == "..") {
        return false;
    }
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        return false;
    }
    !BIND_DENYLIST
        .iter()
        .any(|d| p == *d || p.starts_with(&format!("{d}/")))
        // um ancestral de pasta proibida (ex.: `/etc`, `/var/lib`) também expõe o conteúdo dela
        && !BIND_DENYLIST.iter().any(|d| d.starts_with(&format!("{p}/")))
}

/// Nome lógico de um volume da stack: tira o prefixo `<projeto>_`.
pub fn logical_name(compose_project: &str, volume: &str) -> String {
    volume
        .strip_prefix(&format!("{compose_project}_"))
        .filter(|s| !s.is_empty())
        .unwrap_or(volume)
        .to_string()
}

/// Nome lógico de um bind: `bind:<destino no container>`.
fn bind_name(container_path: &str) -> String {
    format!("bind:{container_path}")
}

/// Uma parte de **volume** que o pacote traz e o serviço ainda não tem (servidor recém-recuperado: o
/// volume só nasce no primeiro deploy). Só devolve o que o spec do serviço justifica, nunca inventa:
/// - `bind:<destino>`: a pasta que o spec monta nesse destino (passa pela denylist);
/// - Compose: o volume da stack, `<projeto>_<nome>`;
/// - demais: o volume nomeado que o spec declara com esse nome.
pub fn synthesize(svc: &Service, name: &str) -> Option<Part> {
    if let Some(dest) = name.strip_prefix("bind:") {
        return svc
            .spec
            .volumes
            .iter()
            .find(|v| !v.read_only && v.container_path == dest && v.host_path.starts_with('/'))
            .filter(|v| bind_allowed(&v.host_path))
            .map(|v| Part {
                name: name.to_string(),
                source: Source::Bind(v.host_path.clone()),
            });
    }
    if matches!(svc.spec.source, ServiceSource::Compose(_)) {
        return Some(Part {
            name: name.to_string(),
            source: Source::Named(format!("{}_{name}", svc.compose_project_name())),
        });
    }
    svc.spec
        .volumes
        .iter()
        .any(|v| !v.read_only && v.host_path == name)
        .then(|| Part {
            name: name.to_string(),
            source: Source::Named(name.to_string()),
        })
}

/// As partes do serviço, em ordem estável (por nome lógico). Banco suportado = um dump lógico
/// (e **nenhum** volume: o tar de um banco no ar sai corrompido); o resto, os volumes.
pub async fn resolve(docker: &crate::engine::DockerClient, svc: &Service) -> Result<Vec<Part>> {
    if super::database::engine_of(svc).is_some() {
        let t = super::database::target(docker, svc).await?;
        return Ok(vec![Part {
            name: format!("db:{}", t.db),
            source: Source::Database(t),
        }]);
    }
    let mut parts: Vec<Part> = Vec::new();

    // Volumes nomeados da stack (Compose): o motor os etiqueta com o projeto.
    if matches!(svc.spec.source, ServiceSource::Compose(_)) {
        let project = svc.compose_project_name();
        for vol in list_project_volumes(&project).await? {
            parts.push(Part {
                name: logical_name(&project, &vol),
                source: Source::Named(vol),
            });
        }
    }

    // O que o spec declara montar. Somente leitura não entra: importar nele não faria sentido.
    for v in svc.spec.volumes.iter().filter(|v| !v.read_only) {
        if v.host_path.starts_with('/') {
            if bind_allowed(&v.host_path) {
                parts.push(Part {
                    name: bind_name(&v.container_path),
                    source: Source::Bind(v.host_path.clone()),
                });
            }
        } else if !v.host_path.is_empty() {
            parts.push(Part {
                name: v.host_path.clone(),
                source: Source::Named(v.host_path.clone()),
            });
        }
    }

    parts.sort_by(|a, b| a.name.cmp(&b.name));
    parts.dedup_by(|a, b| a.name == b.name);
    Ok(parts)
}

async fn list_project_volumes(project: &str) -> Result<Vec<String>> {
    let out = crate::engine::cli()
        .args([
            "volume",
            "ls",
            "-q",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
        ])
        .output()
        .await
        .map_err(|e| anyhow!("falha ao listar volumes: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "listar volumes falhou: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_recusa_o_que_e_do_rustploy_ou_do_sistema() {
        assert!(bind_allowed("/srv/uploads"));
        assert!(bind_allowed("/var/lib/outra-coisa"));
        for ruim in [
            "/",
            "",
            "relativo",
            "/srv/../etc",
            "/etc/rustploy",
            "/etc/rustploy/age.key",
            "/var/lib/rustploy/db",
            "/var/lib",
            "/etc",
            "/root/.ssh",
            "/proc/1",
        ] {
            assert!(!bind_allowed(ruim), "{ruim} deveria ser recusado");
        }
    }

    #[test]
    fn nome_logico_tira_o_prefixo_da_stack() {
        assert_eq!(logical_name("rp_abc_db", "rp_abc_db_pgdata"), "pgdata");
        assert_eq!(logical_name("rp_abc_db", "outro"), "outro");
        assert_eq!(logical_name("rp_abc_db", "rp_abc_db_"), "rp_abc_db_");
    }
}

#[cfg(test)]
mod synth_tests {
    use super::*;
    use shared::{ComposeSource, VolumeMount};

    fn svc(source: ServiceSource, volumes: Vec<VolumeMount>) -> Service {
        let mut s = crate::features::wizard::app_spec("svc".into(), "p1".into());
        s.source = source;
        s.volumes = volumes;
        Service {
            id: "svc_1".into(),
            spec: s,
            compose_project: Some("minha_stack".into()),
            status: shared::ServiceStatus::Stopped,
            live_container_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn vol(host: &str, dest: &str, ro: bool) -> VolumeMount {
        VolumeMount {
            host_path: host.into(),
            container_path: dest.into(),
            read_only: ro,
        }
    }

    #[test]
    fn volume_nomeado_so_se_o_spec_o_declara() {
        let s = svc(
            ServiceSource::Registry { image: "x".into() },
            vec![vol("pgdata", "/data", false), vol("cache", "/c", true)],
        );
        assert_eq!(
            synthesize(&s, "pgdata").unwrap().source,
            Source::Named("pgdata".into())
        );
        assert!(
            synthesize(&s, "cache").is_none(),
            "somente leitura não entra"
        );
        assert!(
            synthesize(&s, "invento").is_none(),
            "nunca inventa um volume"
        );
    }

    #[test]
    fn compose_usa_o_prefixo_da_stack() {
        let s = svc(ServiceSource::Compose(ComposeSource::default()), vec![]);
        assert_eq!(
            synthesize(&s, "dados").unwrap().source,
            Source::Named("minha_stack_dados".into())
        );
    }

    #[test]
    fn bind_so_se_o_spec_monta_e_fora_da_denylist() {
        let s = svc(
            ServiceSource::Registry { image: "x".into() },
            vec![
                vol("/srv/uploads", "/app/up", false),
                vol("/etc/rustploy", "/x", false),
            ],
        );
        assert_eq!(
            synthesize(&s, "bind:/app/up").unwrap().source,
            Source::Bind("/srv/uploads".into())
        );
        assert!(synthesize(&s, "bind:/x").is_none(), "denylist");
        assert!(synthesize(&s, "bind:/outro").is_none());
    }
}
