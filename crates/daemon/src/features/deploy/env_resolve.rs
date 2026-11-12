//! Resolução de env vars com secrets decifradas — extraído de
//! `DeployExecutor::resolve_env` pra ser reaproveitado também pelo `JobRunner`
//! (`crates/daemon/src/features/jobs/runner.rs`), que precisa das mesmas env vars de
//! base (projeto + serviço gatilho, secrets incluídas) sem instanciar um
//! `DeployExecutor` inteiro.

use crate::platform::secrets::SecretsManager;
use crate::store::Db;
use anyhow::Result;
use shared::{EnvVarValue, Job, Service};
use std::collections::HashMap;
use tracing::{debug, error, warn};

/// Resolve o valor de uma env `Secret(nome)`. **Secret ausente é erro** (antes virava string
/// vazia em silêncio e o container subia com a variável vazia). A mensagem diz qual secret
/// falta, para qual env e onde foi procurado, e some junto com o deploy/job que a usou.
///
/// Escopo: com `service_id`, o secret **do serviço** vale antes do **do projeto**; sem ele (env de
/// projeto, job autônomo), só o do projeto.
async fn secret_value(
    secrets: &SecretsManager,
    project_id: &str,
    service_id: Option<&str>,
    secret_name: &str,
    env_key: &str,
    owner: &str,
) -> Result<String> {
    secrets
        .resolve(project_id, service_id, secret_name)
        .await
        .map_err(|e| {
            let onde = match service_id {
                Some(sid) => format!("nos secrets do serviço '{sid}' e do projeto '{project_id}'"),
                None => format!("nos secrets do projeto '{project_id}'"),
            };
            anyhow::anyhow!(
                "a env '{env_key}' ({owner}) aponta para o secret '{secret_name}', que não pôde ser resolvido \
                 (procurado {onde}): {e}"
            )
        })
}

/// Só as env vars do projeto (base), secrets decifradas — sem nenhuma
/// específica de serviço. Reaproveitado por `resolve` (que soma as do
/// serviço por cima) e por `resolve_project_only` (jobs autônomos, sem
/// serviço gatilho).
async fn resolve_project_env(
    db: &Db,
    secrets: &SecretsManager,
    project_id: &str,
) -> Result<HashMap<String, String>> {
    let project_env = match crate::store::projects::get(db, project_id).await {
        Ok(Some(project)) => project.env_vars,
        Ok(None) => {
            warn!(
                project_id,
                "resolve_env: projeto não encontrado no banco — env vars de projeto não serão injetadas"
            );
            vec![]
        }
        Err(e) => {
            error!(
                project_id,
                error = %e,
                "resolve_env: falha ao carregar projeto (possível erro de desserialização do JSON env_vars) — env vars de projeto não serão injetadas"
            );
            vec![]
        }
    };

    let mut env_map: HashMap<String, String> = HashMap::new();
    for ev in &project_env {
        let value = match &ev.value {
            EnvVarValue::Plain(v) => v.clone(),
            EnvVarValue::Secret(name) => {
                debug!(project_id, secret = %name, "resolve_env: desencriptando secret do projeto");
                secret_value(secrets, project_id, None, name, &ev.key, "projeto").await?
            }
        };
        env_map.insert(ev.key.clone(), value);
    }
    Ok(env_map)
}

/// Funde env vars do projeto (base) com as do serviço (sobrescreve por
/// chave), decifrando `EnvVarValue::Secret` via `secrets`. Mesma precedência
/// de `shared::resolve_env_vars`, mas com secrets resolvidas em texto puro
/// (`shared::resolve_env_vars` deixa `EnvVarValue` intacto).
pub async fn resolve(
    db: &Db,
    secrets: &SecretsManager,
    svc: &Service,
) -> Result<Vec<(String, String)>> {
    let mut env_map = resolve_project_env(db, secrets, &svc.spec.project_id).await?;

    for ev in &svc.spec.env_vars {
        let value = match &ev.value {
            EnvVarValue::Plain(v) => v.clone(),
            EnvVarValue::Secret(name) => {
                debug!(service_id = %svc.id, secret = %name, "resolve_env: desencriptando secret do serviço");
                secret_value(
                    secrets,
                    &svc.spec.project_id,
                    Some(&svc.id),
                    name,
                    &ev.key,
                    "serviço",
                )
                .await?
            }
        };
        env_map.insert(ev.key.clone(), value);
    }

    let keys: Vec<&str> = env_map.keys().map(|k| k.as_str()).collect();
    tracing::info!(
        service_id = %svc.id,
        service_vars = svc.spec.env_vars.len(),
        total = env_map.len(),
        keys = ?keys,
        "resolve_env: vars resolvidas (projeto + serviço)"
    );

    Ok(env_map.into_iter().collect())
}

/// Só as env vars do projeto — usado por jobs sem serviço gatilho
/// (`Job::trigger_service_id: None`, job 100% autônomo). Sem acesso a env
/// vars/secrets exclusivas de um serviço específico.
pub async fn resolve_project_only(
    db: &Db,
    secrets: &SecretsManager,
    project_id: &str,
) -> Result<Vec<(String, String)>> {
    let env_map = resolve_project_env(db, secrets, project_id).await?;
    tracing::info!(
        project_id,
        total = env_map.len(),
        "resolve_env: vars resolvidas (só projeto, job autônomo sem serviço gatilho)"
    );
    Ok(env_map.into_iter().collect())
}

/// Env vars completas de um `Job`: base (projeto, + serviço gatilho quando
/// houver) por baixo, `job.env_vars` sobrescrevendo por cima — maior
/// precedência, pensado pra overrides específicos do job (ex.:
/// `FLOW_MYENV=test`) sem precisar duplicar toda a config do
/// projeto/serviço. Centraliza aqui a lógica que antes vivia inline em
/// `jobs::runner` (o `match trigger_service_id` pra escolher `resolve` vs
/// `resolve_project_only`).
pub async fn resolve_job(
    db: &Db,
    secrets: &SecretsManager,
    job: &Job,
) -> Result<Vec<(String, String)>> {
    let base = match &job.trigger_service_id {
        Some(sid) => {
            let svc = crate::store::services::get(db, sid)
                .await?
                .ok_or_else(|| anyhow::anyhow!("serviço gatilho não encontrado: {sid}"))?;
            resolve(db, secrets, &svc).await?
        }
        None => resolve_project_only(db, secrets, &job.project_id).await?,
    };
    let mut env_map: HashMap<String, String> = base.into_iter().collect();

    for ev in &job.env_vars {
        let value = match &ev.value {
            EnvVarValue::Plain(v) => v.clone(),
            EnvVarValue::Secret(name) => {
                debug!(job_id = %job.id, secret = %name, "resolve_env: desencriptando secret do job");
                secrets
                    .resolve_for_job(
                        &job.project_id,
                        &job.id,
                        job.trigger_service_id.as_deref(),
                        name,
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "a env '{}' (job) aponta para o secret '{name}', que não pôde ser resolvido \
                             (procurado nos secrets do job '{}', do serviço gatilho e do projeto '{}'): {e}",
                            ev.key,
                            job.id,
                            job.project_id
                        )
                    })?
            }
        };
        env_map.insert(ev.key.clone(), value);
    }

    tracing::info!(
        job_id = %job.id,
        job_vars = job.env_vars.len(),
        total = env_map.len(),
        "resolve_env: vars resolvidas (projeto [+ serviço] + overrides do job)"
    );

    Ok(env_map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn secret_ausente_vira_erro_que_diz_qual_e_onde() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        let secrets = SecretsManager::new(db).unwrap();

        let err = secret_value(
            &secrets,
            "proj_1",
            None,
            "DB_PASSWORD",
            "DATABASE_PASSWORD",
            "serviço",
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("DB_PASSWORD"), "{err}");
        assert!(err.contains("DATABASE_PASSWORD"), "{err}");
        assert!(err.contains("proj_1"), "{err}");
    }

    #[tokio::test]
    async fn secret_existente_continua_resolvendo() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        db.seed_project_for_test("proj_1");
        let secrets = SecretsManager::new(db).unwrap();
        secrets.set("proj_1", "TOKEN", "abc123").await.unwrap();

        let v = secret_value(&secrets, "proj_1", None, "TOKEN", "API_TOKEN", "projeto")
            .await
            .unwrap();
        assert_eq!(v, "abc123");
    }

    /// A env do serviço enxerga o secret do serviço antes do de projeto; o job com serviço gatilho
    /// enxerga os dois; o job autônomo só o do projeto.
    #[tokio::test]
    async fn escopos_de_secret_na_resolucao_de_servico_e_job() {
        use shared::{EnvVar, EnvVarValue};
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s1");
        let secrets = SecretsManager::new(db.clone()).unwrap();
        secrets.set("p1", "SENHA", "do-projeto").await.unwrap();
        secrets.set("p1", "SO_PROJETO", "pp").await.unwrap();
        secrets
            .set_scoped("p1", Some("s1"), "SENHA", "do-servico")
            .await
            .unwrap();
        secrets
            .set_scoped("p1", Some("s1"), "SO_SERVICO", "ss")
            .await
            .unwrap();
        let sec = |k: &str, n: &str| EnvVar {
            key: k.into(),
            value: EnvVarValue::Secret(n.into()),
        };
        // env do serviço: SENHA (serviço vence), SO_PROJETO (cai no projeto), SO_SERVICO
        db.infra()
            .update(|st| {
                st.projects[0].services[0].spec.env_vars = vec![
                    sec("A", "SENHA"),
                    sec("B", "SO_PROJETO"),
                    sec("C", "SO_SERVICO"),
                ];
                Ok(())
            })
            .unwrap();
        let svc = crate::store::services::get(&db, "s1")
            .await
            .unwrap()
            .unwrap();
        let map: std::collections::HashMap<_, _> = resolve(&db, &secrets, &svc)
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(map["A"], "do-servico");
        assert_eq!(map["B"], "pp");
        assert_eq!(map["C"], "ss");

        // job COM gatilho: o override do job pode apontar para um secret do serviço gatilho
        let mk_job = |trigger: Option<&str>| shared::Job {
            id: "j".into(),
            project_id: "p1".into(),
            trigger_service_id: trigger.map(String::from),
            name: "j".into(),
            compose: String::new(),
            git_source: None,
            main_service: "m".into(),
            env_vars: vec![sec("X", "SO_SERVICO")],
            env_comments: vec![],
            enabled: true,
            recurrence: None,
            last_run_at: None,
            next_run_at: None,
            created_at: chrono::Utc::now(),
        };
        let com: std::collections::HashMap<_, _> = resolve_job(&db, &secrets, &mk_job(Some("s1")))
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(com["X"], "ss");
        // job autônomo: o secret do serviço NÃO é visível → erro de deploy claro
        let e = resolve_job(&db, &secrets, &mk_job(None))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("SO_SERVICO") && e.contains("job"), "{e}");
    }
}
