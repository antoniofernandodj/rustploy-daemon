//! **Auditoria de valores em claro** no estado de infra (plano `docs/plano-backup-estado-em-git.md`,
//! R5/R6). A sincronização com git só pode ser ligada se esta lista vier vazia, e o que chega de
//! fora (`pull`) passa pela mesma checagem — senão o histórico do git conteria um segredo, e apagar
//! segredo de histórico git não funciona de verdade.
//!
//! Duas categorias, porque a certeza é diferente:
//! - [`Certainty::Definite`]: o campo **é** sensível por natureza e deveria estar cifrado
//!   (`ENC[age:…]`) — senha de banco gerenciado, token de webhook, valor de secret, `client_secret`
//!   de provider.
//! - [`Certainty::Suspect`]: um valor em texto puro que **parece** segredo pelo nome da variável,
//!   por ser uma URL com usuário:senha, ou por uma linha de compose. É heurística: pode haver falso
//!   positivo (resolve-se movendo para secret) e **falso negativo** (um segredo com nome banal, ou
//!   dentro de um Dockerfile/script, não é visto). Por isso a auditoria é uma trava, não uma garantia.

use super::model::{InfraState, ProjectInfra};
use crate::platform::secrets::is_enc;
use shared::{EnvVar, EnvVarValue, ServiceSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    Definite,
    Suspect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub certainty: Certainty,
    pub project: String,
    /// Onde: `serviço api`, `job backup`, `projeto`…
    pub scope: String,
    /// O quê: `env DB_PASSWORD`, `senha do banco loja`… **Nunca** o valor.
    pub what: String,
}

/// Fragmentos de nome que denunciam um segredo (comparados em maiúsculas).
const SENSITIVE_NAME_PARTS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "API_KEY",
    "APIKEY",
    "PRIVATE_KEY",
    "ACCESS_KEY",
    "CREDENTIAL",
    "JWT",
    "SALT",
    "_DSN",
    "AUTH_KEY",
    "ENCRYPTION_KEY",
    "SIGNING_KEY",
];

/// O nome da variável denuncia um segredo? (heurística — ver o aviso no topo do módulo)
pub(crate) fn name_looks_sensitive(key: &str) -> bool {
    let up = key.to_uppercase();
    // `PASS`/`PWD` como palavra inteira (`RABBITMQ_DEFAULT_PASS`, `DB_PWD`): só como fragmento
    // pegaria `PASSENGER` ou `COMPASS`.
    let word = |w: &str| {
        up == w
            || up.ends_with(&format!("_{w}"))
            || up.starts_with(&format!("{w}_"))
            || up.contains(&format!("_{w}_"))
    };
    SENSITIVE_NAME_PARTS.iter().any(|p| up.contains(p))
        || up == "KEY"
        || up.ends_with("_KEY")
        || word("PASS")
        || word("PWD")
}

/// `scheme://usuario:senha@host` — credencial embutida numa URL.
fn url_with_credentials(value: &str) -> bool {
    let Some(rest) = value.split_once("://").map(|(_, r)| r) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    match authority.split_once('@') {
        Some((userinfo, _)) => userinfo.contains(':') && !userinfo.ends_with(':'),
        None => false,
    }
}

/// Valor que é só uma referência a ser resolvida depois (`${VAR}`), não um segredo literal.
fn is_reference(value: &str) -> bool {
    let v = value.trim();
    v.starts_with("${") && v.ends_with('}') && v.matches("${").count() == 1
}

fn check_env(out: &mut Vec<Finding>, project: &str, scope: &str, vars: &[EnvVar]) {
    for v in vars {
        let EnvVarValue::Plain(value) = &v.value else {
            continue; // `Secret(nome)` guarda só o nome do secret
        };
        if value.is_empty() || is_reference(value) {
            continue;
        }
        let reason = if name_looks_sensitive(&v.key) {
            Some(format!("env {}", v.key))
        } else if url_with_credentials(value) {
            Some(format!("env {} (URL com usuário e senha)", v.key))
        } else {
            None
        };
        if let Some(what) = reason {
            out.push(Finding {
                certainty: Certainty::Suspect,
                project: project.into(),
                scope: scope.into(),
                what,
            });
        }
    }
}

/// Linhas de compose do tipo `POSTGRES_PASSWORD: valor` / `- API_TOKEN=valor` com valor literal.
fn compose_suspect_lines(content: &str) -> Vec<String> {
    let mut keys = vec![];
    for raw in content.lines() {
        let line = raw.trim().trim_start_matches('-').trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once([':', '=']) else {
            continue;
        };
        let (k, v) = (
            k.trim().trim_matches('"'),
            v.trim().trim_matches(['"', '\'']),
        );
        if v.is_empty() || v.starts_with("${") || v.starts_with('$') {
            continue;
        }
        if name_looks_sensitive(k) && !k.contains(' ') {
            keys.push(k.to_string());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

fn audit_project(out: &mut Vec<Finding>, p: &ProjectInfra) {
    let name = p.name.as_str();
    let push = |out: &mut Vec<Finding>, c, scope: &str, what: String| {
        out.push(Finding {
            certainty: c,
            project: name.into(),
            scope: scope.into(),
            what,
        })
    };
    check_env(out, name, "projeto", &p.env_vars);
    for s in &p.secrets {
        if !is_enc(&s.value) {
            push(
                out,
                Certainty::Definite,
                "projeto",
                format!("secret {} sem cifra", s.key),
            );
        }
    }
    for m in &p.managed_databases {
        // A senha nasce em claro hoje; só some quando o usuário a move para um secret (Fase 5).
        push(
            out,
            Certainty::Definite,
            "projeto",
            format!("senha do banco {}", m.name),
        );
    }
    for svc in &p.services {
        let scope = format!("serviço {}", svc.spec.name);
        if svc.webhook_token.as_deref().is_some_and(|t| !is_enc(t)) {
            push(
                out,
                Certainty::Definite,
                &scope,
                "token de webhook sem cifra".into(),
            );
        }
        check_env(out, name, &scope, &svc.spec.env_vars);
        for sec in &svc.secrets {
            if !is_enc(&sec.value) {
                push(
                    out,
                    Certainty::Definite,
                    &scope,
                    format!("secret {} sem cifra", sec.key),
                );
            }
        }
        if let ServiceSource::Compose(c) = &svc.spec.source {
            for k in compose_suspect_lines(&c.content) {
                push(out, Certainty::Suspect, &scope, format!("compose: {k}"));
            }
        }
    }
    for j in &p.jobs {
        let scope = format!("job {}", j.name);
        check_env(out, name, &scope, &j.env_vars);
        for k in compose_suspect_lines(&j.compose) {
            push(out, Certainty::Suspect, &scope, format!("compose: {k}"));
        }
    }
}

/// Tudo o que está em claro (ou parece estar), em ordem estável. Vazio = pode ligar o git.
pub fn plaintext_findings(st: &InfraState) -> Vec<Finding> {
    let mut out = vec![];
    for prov in &st.providers {
        if prov
            .oauth_client_secret_enc
            .as_deref()
            .is_some_and(|v| !is_enc(v))
        {
            out.push(Finding {
                certainty: Certainty::Definite,
                project: String::new(),
                scope: format!("provider {}", prov.name),
                what: "client_secret sem cifra".into(),
            });
        }
    }
    for p in &st.projects {
        audit_project(&mut out, p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::infra::model::{ManagedDatabaseInfra, SecretInfra};
    use chrono::Utc;

    #[test]
    fn pass_e_pwd_como_palavra_inteira_sao_segredo_mas_passenger_nao() {
        for k in [
            "RABBITMQ_DEFAULT_PASS",
            "RABBITMQ_PASS",
            "PASS",
            "DB_PWD",
            "PASS_FILE",
            "MY_PASS_X",
            "rabbitmq_pass",
        ] {
            assert!(name_looks_sensitive(k), "{k} deveria ser sensível");
        }
        for k in [
            "COMPASS",
            "PASSENGER_COUNT",
            "BYPASS",
            "MODE",
            "RABBITMQ_USER",
            "DEFAULT_PASSIVE",
        ] {
            assert!(!name_looks_sensitive(k), "{k} não deveria ser sensível");
        }
    }

    fn project() -> ProjectInfra {
        ProjectInfra {
            id: "p".into(),
            name: "App".into(),
            description: None,
            env_vars: vec![],
            env_comments: vec![],
            network_name: None,
            created_at: Utc::now(),
            services: vec![],
            jobs: vec![],
            secrets: vec![],
            managed_databases: vec![],
            shared_access: vec![],
        }
    }

    fn plain(k: &str, v: &str) -> EnvVar {
        EnvVar {
            key: k.into(),
            value: EnvVarValue::Plain(v.into()),
        }
    }

    fn state(p: ProjectInfra) -> InfraState {
        InfraState {
            projects: vec![p],
            ..Default::default()
        }
    }

    #[test]
    fn estado_limpo_nao_tem_achados() {
        let mut p = project();
        p.env_vars = vec![
            plain("NODE_ENV", "production"),
            plain("PORT", "3000"),
            plain("API_URL", "https://api.exemplo.com/v1"),
            EnvVar {
                key: "DB_PASSWORD".into(),
                value: EnvVarValue::Secret("DB_PASSWORD".into()),
            },
            plain("TOKEN_FROM_OUTSIDE", "${TOKEN}"),
            plain("EMPTY_PASSWORD", ""),
        ];
        p.secrets = vec![SecretInfra {
            id: "s".into(),
            key: "K".into(),
            value: "ENC[age:abc]".into(),
        }];
        assert_eq!(plaintext_findings(&state(p)), vec![]);
    }

    #[test]
    fn acha_env_suspeita_por_nome_e_por_url_com_senha() {
        let mut p = project();
        p.env_vars = vec![
            plain("STRIPE_SECRET_KEY", "sk_live_x"),
            plain("DATABASE_URL", "postgres://app:hunter2@db:5432/app"),
            plain("DATABASE_URL_SEM_SENHA", "postgres://app@db:5432/app"),
            plain("SESSION_KEY", "abc"),
        ];
        let f = plaintext_findings(&state(p));
        let whats: Vec<_> = f.iter().map(|x| x.what.as_str()).collect();
        assert_eq!(f.len(), 3, "{whats:?}");
        assert!(whats.contains(&"env STRIPE_SECRET_KEY"));
        assert!(whats.iter().any(|w| w.starts_with("env DATABASE_URL (URL")));
        assert!(whats.contains(&"env SESSION_KEY"));
        assert!(f.iter().all(|x| x.certainty == Certainty::Suspect));
    }

    #[test]
    fn o_achado_nunca_contem_o_valor() {
        let mut p = project();
        p.env_vars = vec![plain("API_TOKEN", "valor-ultra-secreto")];
        p.managed_databases = vec![ManagedDatabaseInfra {
            id: "m".into(),
            server_service_id: "s".into(),
            name: "loja".into(),
            password: "senha-do-banco".into(),
            env_var: "DATABASE_URL".into(),
            connection_limit: None,
            statement_timeout_ms: None,
            created_at: Utc::now(),
        }];
        // o banco aponta para um servidor; o validate não é chamado aqui
        let f = plaintext_findings(&state(p));
        let tudo = format!("{f:?}");
        assert!(!tudo.contains("valor-ultra-secreto") && !tudo.contains("senha-do-banco"));
        assert!(
            f.iter()
                .any(|x| x.certainty == Certainty::Definite && x.what == "senha do banco loja")
        );
    }

    #[test]
    fn secret_e_webhook_sem_cifra_sao_certezas() {
        let mut p = project();
        p.secrets = vec![SecretInfra {
            id: "s".into(),
            key: "K".into(),
            value: "em-claro".into(),
        }];
        let f = plaintext_findings(&state(p));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].certainty, Certainty::Definite);
    }

    #[test]
    fn linhas_de_compose_com_valor_literal_sao_suspeitas() {
        let compose = "services:\n  db:\n    environment:\n      POSTGRES_PASSWORD: hunter2\n      POSTGRES_USER: app\n      - API_TOKEN=abc\n      OK_PASSWORD: ${DB_PASSWORD}\n    # SENHA_TOKEN: comentado\n";
        let k = compose_suspect_lines(compose);
        assert_eq!(k, vec!["API_TOKEN", "POSTGRES_PASSWORD"]);
    }
}
