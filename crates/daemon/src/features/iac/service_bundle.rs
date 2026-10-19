//! Pacote de serviço: UM serviço (mais as variáveis do projeto que o usuário
//! escolheu levar) num arquivo YAML, para copiar de um servidor para outro.
//!
//! Diferente do IaC (`manifest.rs`), que descreve o servidor inteiro e
//! reconcilia, o pacote é aditivo e de escopo um-serviço: quem importa decide
//! projeto, nome e o que fazer com cada conflito. Ver
//! `docs/plano-copiar-servico-entre-servidores.md`.
//!
//! Este módulo é o FORMATO, as conversões puras (spec ⇄ pacote) e os tipos de
//! pedido/resposta dos comandos de export/import; a lógica deles vive no daemon.

#![allow(dead_code)] // API de quando o módulo vivia no `shared`; parte dela só os testes usam.

use super::manifest::{API_VERSION, SECRET_PREFIX, env_map_to_vars, env_vars_to_map};
use serde::{Deserialize, Serialize};
use shared::*;
use std::collections::{BTreeMap, BTreeSet};

/// Valor de `kind` num pacote de serviço (o IaC do servidor não tem `kind`).
pub const BUNDLE_KIND: &str = "Service";

/// De onde o pacote saiu. Só informativo: o import não depende de nada daqui.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BundleOrigin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
}

/// O arquivo. Estrito no topo (`deny_unknown_fields`): um campo que este
/// daemon não conhece é erro, não algo ignorado em silêncio.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceBundle {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub origin: BundleOrigin,
    pub service: super::manifest::ServiceManifest,
    /// Variáveis do PROJETO que o usuário marcou para levar. Valor literal
    /// (export com valores), `${CHAVE}` (export só com nomes) ou `secret:NOME`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_env: BTreeMap<String, String>,
}

impl ServiceBundle {
    /// Monta o pacote a partir de um serviço do banco.
    ///
    /// - `project_env`: as variáveis do projeto já FILTRADAS pelo usuário (só as
    ///   marcadas); as demais nunca entram no arquivo.
    /// - `include_values`: `false` troca todo valor `Plain` por `${CHAVE}`
    ///   (o arquivo leva só os nomes); referências `secret:` ficam como estão —
    ///   o segredo nunca é decifrado para o cliente.
    ///
    /// Falha para origem por zip: o arquivo mora no daemon de origem e não
    /// viaja (a v1 recusa em vez de exportar um serviço quebrado).
    pub fn from_service(
        svc: &Service,
        providers: &BTreeMap<String, GitProvider>,
        project_env: &[EnvVar],
        include_values: bool,
        origin: BundleOrigin,
    ) -> Result<Self, String> {
        if matches!(svc.spec.source, ServiceSource::Archive(_)) {
            return Err(format!(
                "o serviço '{}' vem de um upload .zip, guardado no daemon de origem — \
                 não dá para exportá-lo; reenvie o zip no servidor de destino",
                svc.spec.name
            ));
        }

        let mut service = super::manifest::ServiceManifest::from_spec(svc, providers);
        // A porta externa de cá não vale lá: quem tinha uma pede "alocar" (0, o
        // mesmo sentinela do `ServiceCreate`); quem não tinha continua sem.
        service.host_port = service.host_port.map(|_| 0);
        let mut project_env = env_vars_to_map(project_env);

        if !include_values {
            redact_values(&mut service.env);
            redact_values(&mut project_env);
        }

        Ok(ServiceBundle {
            api_version: API_VERSION.to_string(),
            kind: BUNDLE_KIND.to_string(),
            origin,
            service,
            project_env,
        })
    }

    /// Confere um pacote JÁ desserializado (o parse do YAML é de quem chama —
    /// o `rustploy-shared` não depende de um parser de YAML). Rejeita `kind`
    /// diferente de `Service` e `apiVersion` desconhecida, para um YAML do IaC
    /// (ou de uma versão futura) não ser importado como se fosse isto.
    pub fn validate(&self) -> Result<(), String> {
        if self.kind != BUNDLE_KIND {
            return Err(format!(
                "kind '{}' não é um pacote de serviço (esperado '{BUNDLE_KIND}')",
                self.kind
            ));
        }
        if self.api_version != API_VERSION {
            return Err(format!(
                "apiVersion '{}' não suportada (esta versão lê '{API_VERSION}')",
                self.api_version
            ));
        }
        if self.service.name.trim().is_empty() {
            return Err("o serviço do pacote não tem nome".to_string());
        }
        Ok(())
    }

    /// Variáveis do projeto do pacote, já como `EnvVar` (para gravar no destino).
    pub fn project_env_vars(&self) -> Vec<EnvVar> {
        env_map_to_vars(&self.project_env)
    }
}

/// Todo valor `Plain` vira `${CHAVE}`; `secret:` passa intacto.
fn redact_values(map: &mut BTreeMap<String, String>) {
    for (k, v) in map.iter_mut() {
        if !v.starts_with(SECRET_PREFIX) {
            *v = format!("${{{k}}}");
        }
    }
}

/// Nomes de variáveis citados em `text` como `${NOME}`, `${NOME:-padrão}` ou
/// `$NOME` (sintaxe de shell/compose). `$$` é um `$` literal do compose e não
/// conta.
fn referenced_names(text: &str, out: &mut BTreeSet<String>) {
    let b = text.as_bytes();
    let is_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let is_cont = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some(b'$') => i += 2,
            Some(b'{') => {
                let start = i + 2;
                let mut j = start;
                while j < b.len() && is_cont(b[j]) {
                    j += 1;
                }
                if j > start && is_start(b[start]) {
                    out.insert(text[start..j].to_string());
                }
                i = j.max(i + 2);
            }
            Some(&c) if is_start(c) => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && is_cont(b[j]) {
                    j += 1;
                }
                out.insert(text[start..j].to_string());
                i = j;
            }
            _ => i += 1,
        }
    }
}

/// Quais variáveis DO PROJETO este serviço provavelmente usa: as citadas como
/// `${VAR}`/`$VAR` nos valores do próprio env do serviço, no compose, no comando
/// e nos argumentos. É só uma SUGESTÃO para a tela de checkboxes: em runtime o
/// container recebe todas as variáveis do projeto e o rustploy não sabe o que o
/// código do app lê.
pub fn suggested_project_vars(spec: &ServiceSpec, project_keys: &[String]) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for ev in &spec.env_vars {
        if let EnvVarValue::Plain(v) = &ev.value {
            referenced_names(v, &mut refs);
        }
    }
    if let ServiceSource::Compose(c) = &spec.source {
        referenced_names(&c.content, &mut refs);
    }
    if let Some(cmd) = &spec.run_command {
        referenced_names(cmd, &mut refs);
    }
    for a in &spec.run_args {
        referenced_names(a, &mut refs);
    }
    project_keys
        .iter()
        .filter(|k| refs.contains(*k))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            name: "api".into(),
            project_id: "prj_a".into(),
            source: ServiceSource::Git(GitSource {
                url: "https://github.com/acme/flow.git".into(),
                branch: "development".into(),
                root_path: ".".into(),
                watch_paths: vec!["src".into()],
                submodules: false,
                dockerfile_path: "dockerfiles/api.dockerfile".into(),
                build_context: ".".into(),
                build_stage: None,
                credentials: None,
                username: None,
                provider_id: None,
            }),
            port: 8888,
            host_port: Some(30123),
            domain: Some("api.a.tech".into()),
            tls_enabled: true,
            env_vars: vec![
                EnvVar {
                    key: "DATABASE_URL".into(),
                    value: EnvVarValue::Plain("postgres://u:p@db/x".into()),
                },
                EnvVar {
                    key: "API_KEY".into(),
                    value: EnvVarValue::Secret("API_KEY".into()),
                },
                EnvVar {
                    key: "REDIS".into(),
                    value: EnvVarValue::Plain("${REDIS_URL}".into()),
                },
            ],
            env_comments: vec![EnvComment {
                text: "# banco".into(),
                before_key: Some("DATABASE_URL".into()),
            }],
            volumes: vec![],
            healthcheck: Healthcheck::default(),
            replicas: 1,
            resources: ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            db_kind: None,
            domains: vec![
                DomainRoute {
                    domain: "api.a.tech".into(),
                    port: None,
                    tls: true,
                },
                DomainRoute {
                    domain: "admin.a.tech".into(),
                    port: Some(9000),
                    tls: false,
                },
            ],
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec!["job_1".into()],
            shared: None,
        }
    }

    fn service(spec: ServiceSpec) -> Service {
        Service {
            id: "svc_m".into(),
            spec,
            compose_project: None,
            status: ServiceStatus::Stopped,
            live_container_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn to_yaml(b: &ServiceBundle) -> String {
        serde_yaml::to_string(b).unwrap()
    }

    fn from_yaml(text: &str) -> Result<ServiceBundle, String> {
        let b: ServiceBundle = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        b.validate()?;
        Ok(b)
    }

    fn envs(pairs: &[(&str, &str)]) -> Vec<EnvVar> {
        pairs
            .iter()
            .map(|(k, v)| EnvVar {
                key: (*k).into(),
                value: EnvVarValue::Plain((*v).into()),
            })
            .collect()
    }

    #[test]
    fn ida_e_volta_preserva_o_spec_menos_o_que_nao_viaja() {
        let original = spec();
        let b = ServiceBundle::from_service(
            &service(original.clone()),
            &BTreeMap::new(),
            &[],
            true,
            BundleOrigin::default(),
        )
        .unwrap();
        let yaml = to_yaml(&b);
        let lido = from_yaml(&yaml).unwrap();
        assert_eq!(b, lido);

        let volta = lido.service.to_spec("prj_b", &BTreeMap::new());
        assert_eq!(volta.project_id, "prj_b");
        assert_eq!(volta.name, original.name);
        assert_eq!(volta.source, original.source);
        assert_eq!(volta.port, original.port);
        assert_eq!(volta.domains, original.domains, "várias rotas de domínio");
        assert_eq!(volta.domain, original.domain);
        assert_eq!(volta.env_comments, original.env_comments);
        assert_eq!(volta.env_vars.len(), 3);
        // O que não viaja:
        assert_eq!(
            volta.host_port,
            Some(0),
            "porta externa é do servidor de origem: o destino aloca outra"
        );
        assert!(
            volta.pre_deploy_job_ids.is_empty(),
            "ids de job são do origem"
        );
    }

    #[test]
    fn sem_valores_so_os_nomes_e_secret_intacto() {
        let b = ServiceBundle::from_service(
            &service(spec()),
            &BTreeMap::new(),
            &envs(&[("REDIS_URL", "redis://cache:6379")]),
            false,
            BundleOrigin::default(),
        )
        .unwrap();
        assert_eq!(b.service.env["DATABASE_URL"], "${DATABASE_URL}");
        assert_eq!(b.service.env["API_KEY"], "secret:API_KEY");
        assert_eq!(b.project_env["REDIS_URL"], "${REDIS_URL}");
        let yaml = to_yaml(&b);
        assert!(!yaml.contains("postgres://"), "valor vazou: {yaml}");
        assert!(!yaml.contains("redis://"), "valor vazou: {yaml}");
    }

    #[test]
    fn com_valores_leva_o_literal_e_secret_do_projeto_continua_referencia() {
        let mut proj = envs(&[("REDIS_URL", "redis://cache:6379")]);
        proj.push(EnvVar {
            key: "SENTRY".into(),
            value: EnvVarValue::Secret("SENTRY_DSN".into()),
        });
        let b = ServiceBundle::from_service(
            &service(spec()),
            &BTreeMap::new(),
            &proj,
            true,
            BundleOrigin::default(),
        )
        .unwrap();
        assert_eq!(b.service.env["DATABASE_URL"], "postgres://u:p@db/x");
        assert_eq!(b.project_env["REDIS_URL"], "redis://cache:6379");
        assert_eq!(b.project_env["SENTRY"], "secret:SENTRY_DSN");
        assert!(matches!(
            b.project_env_vars()
                .iter()
                .find(|e| e.key == "SENTRY")
                .unwrap()
                .value,
            EnvVarValue::Secret(_)
        ));
    }

    #[test]
    fn origem_por_zip_e_recusada() {
        let mut s = spec();
        s.source = ServiceSource::Archive(ArchiveSource::default());
        let e = ServiceBundle::from_service(
            &service(s),
            &BTreeMap::new(),
            &[],
            true,
            BundleOrigin::default(),
        )
        .unwrap_err();
        assert!(e.contains(".zip"), "{e}");
    }

    #[test]
    fn leitura_estrita() {
        let ok = "apiVersion: rustploy/v1\nkind: Service\nservice:\n  name: x\n  source: {registry: nginx}\n";
        assert!(from_yaml(ok).is_ok());

        let kind = "apiVersion: rustploy/v1\nkind: Server\nservice:\n  name: x\n  source: {registry: nginx}\n";
        assert!(from_yaml(kind).unwrap_err().contains("kind"));

        let ver = "apiVersion: rustploy/v9\nkind: Service\nservice:\n  name: x\n  source: {registry: nginx}\n";
        assert!(from_yaml(ver).unwrap_err().contains("apiVersion"));

        let extra = "apiVersion: rustploy/v1\nkind: Service\nsurpresa: 1\nservice:\n  name: x\n  source: {registry: nginx}\n";
        assert!(from_yaml(extra).is_err(), "campo desconhecido no topo");

        let sem_nome = "apiVersion: rustploy/v1\nkind: Service\nservice:\n  name: \"  \"\n  source: {registry: nginx}\n";
        assert!(from_yaml(sem_nome).unwrap_err().contains("nome"));

        // O YAML do IaC não é um pacote de serviço.
        let iac = "apiVersion: rustploy/v1\nproject:\n  name: p\nservices: []\n";
        assert!(from_yaml(iac).is_err());
    }

    #[test]
    fn sugestao_de_variaveis_do_projeto() {
        let mut s = spec();
        s.run_command = Some("run --token $TOKEN".into());
        s.source = ServiceSource::Compose(ComposeSource {
            content: "image: x\nenvironment:\n  A: ${DB_PASS:-pw}\n  B: $$LITERAL\n".into(),
            ingress_service: None,
        });
        let keys: Vec<String> = ["REDIS_URL", "DB_PASS", "TOKEN", "LITERAL", "OUTRA"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = suggested_project_vars(&s, &keys);
        let got: Vec<&str> = got.iter().map(String::as_str).collect();
        // REDIS_URL vem do env do serviço (`${REDIS_URL}`), DB_PASS do compose
        // (com `:-padrão`), TOKEN do comando; `$$LITERAL` é literal do compose.
        assert_eq!(got, vec!["DB_PASS", "REDIS_URL", "TOKEN"]);
    }
}
