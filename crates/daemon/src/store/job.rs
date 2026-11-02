//! Tabela `job`: jobs one-shot (Schedules), incluindo quais estão vencidos para
//! o agendador.

use super::Db;
use super::infra::model::JobInfra;
use anyhow::Result;
use chrono::{DateTime, Utc};
use shared::{EnvComment, EnvVar, Job, JobGitSource, Recurrence};
use std::collections::HashMap;
use ulid::Ulid;

// Desde a Fase 4d do plano `docs/plano-backup-estado-em-git.md` a **definição** do job mora no
// YAML de infra (`store::infra`, dentro do projeto); o que muda a cada execução (`last_run_at`,
// `next_run_at`) é runtime e mora no SQLite, em `job_runtime`. O histórico (`job_run`, `job_log`)
// segue no SQL. A tabela antiga `job` não é mais lida nem escrita.

type Runtime = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);
type RuntimeRow = (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

async fn runtimes(db: &Db) -> Result<HashMap<String, Runtime>> {
    let rows: Vec<RuntimeRow> =
        sqlx::query_as("SELECT job_id, last_run_at, next_run_at FROM job_runtime")
            .fetch_all(db.pool())
            .await?;
    Ok(rows.into_iter().map(|(i, l, n)| (i, (l, n))).collect())
}

async fn put_runtime(
    db: &Db,
    id: &str,
    last: Option<DateTime<Utc>>,
    next: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO job_runtime (job_id, last_run_at, next_run_at) VALUES (?, ?, ?)
         ON CONFLICT(job_id) DO UPDATE SET last_run_at = excluded.last_run_at,
            next_run_at = excluded.next_run_at",
    )
    .bind(id)
    .bind(last)
    .bind(next)
    .execute(db.pool())
    .await?;
    Ok(())
}

fn assemble(project_id: &str, j: &JobInfra, rt: Option<&Runtime>) -> Job {
    Job {
        id: j.id.clone(),
        project_id: project_id.to_string(),
        trigger_service_id: j.trigger_service_id.clone(),
        name: j.name.clone(),
        compose: j.compose.clone(),
        git_source: j.git_source.clone(),
        main_service: j.main_service.clone(),
        env_vars: j.env_vars.clone(),
        env_comments: j.env_comments.clone(),
        enabled: j.enabled,
        recurrence: j.recurrence,
        last_run_at: rt.and_then(|r| r.0),
        next_run_at: rt.and_then(|r| r.1),
        created_at: j.created_at,
    }
}

async fn all_jobs(db: &Db) -> Result<Vec<Job>> {
    let rts = runtimes(db).await?;
    let st = db.infra().snapshot();
    let mut out: Vec<Job> = st
        .projects
        .iter()
        .flat_map(|p| p.jobs.iter().map(move |j| (p.id.as_str(), j)))
        .map(|(pid, j)| assemble(pid, j, rts.get(&j.id)))
        .collect();
    out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub async fn create(
    db: &Db,
    project_id: &str,
    trigger_service_id: Option<&str>,
    name: &str,
    compose: &str,
    git_source: Option<&JobGitSource>,
    main_service: &str,
    env_vars: &[EnvVar],
    env_comments: &[EnvComment],
    recurrence: Option<Recurrence>,
) -> Result<Job> {
    let id = format!("job_{}", Ulid::new());
    let now = Utc::now();
    let next_run_at = recurrence.map(|r| r.next_after(now));
    let rec = JobInfra {
        id: id.clone(),
        trigger_service_id: trigger_service_id.map(String::from),
        name: name.to_string(),
        compose: compose.to_string(),
        git_source: git_source.cloned(),
        main_service: main_service.to_string(),
        env_vars: env_vars.to_vec(),
        env_comments: env_comments.to_vec(),
        enabled: true,
        recurrence,
        created_at: now,
    };
    // A validação do repositório recusa gatilho que não existe.
    db.infra().update(|st| {
        let Some(p) = st.projects.iter_mut().find(|p| p.id == project_id) else {
            anyhow::bail!("projeto {project_id} não encontrado");
        };
        p.jobs.push(rec.clone());
        Ok(())
    })?;
    put_runtime(db, &id, None, next_run_at).await?;
    Ok(assemble(project_id, &rec, Some(&(None, next_run_at))))
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Job>> {
    let st = db.infra().snapshot();
    let Some((pid, j)) = st
        .projects
        .iter()
        .flat_map(|p| p.jobs.iter().map(move |j| (p.id.as_str(), j)))
        .find(|(_, j)| j.id == id)
    else {
        return Ok(None);
    };
    let rt = runtimes(db).await?.remove(id);
    Ok(Some(assemble(pid, j, rt.as_ref())))
}

pub async fn list(db: &Db, project_id: &str) -> Result<Vec<Job>> {
    Ok(all_jobs(db)
        .await?
        .into_iter()
        .filter(|j| j.project_id == project_id)
        .collect())
}

pub async fn list_all(db: &Db) -> Result<Vec<Job>> {
    all_jobs(db).await
}

/// Jobs vencidos: habilitados, com agendamento configurado, e cujo
/// `next_run_at` já passou — usado só pelo `scheduler_loop`.
pub async fn list_due(db: &Db, now: DateTime<Utc>) -> Result<Vec<Job>> {
    Ok(all_jobs(db)
        .await?
        .into_iter()
        .filter(|j| j.enabled && j.recurrence.is_some() && j.next_run_at.is_some_and(|n| n <= now))
        .collect())
}

#[allow(clippy::too_many_arguments)]
pub async fn update(
    db: &Db,
    id: &str,
    name: &str,
    compose: &str,
    git_source: Option<&JobGitSource>,
    main_service: &str,
    env_vars: &[EnvVar],
    env_comments: &[EnvComment],
    enabled: bool,
    recurrence: Option<Recurrence>,
) -> Result<Option<Job>> {
    let next_run_at = recurrence.map(|r| r.next_after(Utc::now()));
    let found = db.infra().update(|st| {
        let Some(j) = st
            .projects
            .iter_mut()
            .flat_map(|p| p.jobs.iter_mut())
            .find(|j| j.id == id)
        else {
            return Ok(false);
        };
        j.name = name.to_string();
        j.compose = compose.to_string();
        j.main_service = main_service.to_string();
        j.enabled = enabled;
        j.recurrence = recurrence;
        j.git_source = git_source.cloned();
        j.env_vars = env_vars.to_vec();
        j.env_comments = env_comments.to_vec();
        Ok(true)
    })?;
    if !found {
        return Ok(None);
    }
    // Editar recalcula o próximo disparo a partir de agora; o último disparo não muda.
    let last = runtimes(db).await?.remove(id).and_then(|r| r.0);
    put_runtime(db, id, last, next_run_at).await?;
    get(db, id).await
}

/// Registra o disparo de um job: `last_run_at` = agora, `next_run_at` já
/// avançado (`None` se o job não tem agendamento — só `JobRunNow`).
pub async fn mark_fired(
    db: &Db,
    id: &str,
    last_run_at: DateTime<Utc>,
    next_run_at: Option<DateTime<Utc>>,
) -> Result<()> {
    put_runtime(db, id, Some(last_run_at), next_run_at).await
}

/// Apaga o job e o histórico junto (`job_run` + `job_log` + runtime) — não há FK com
/// cascade no schema, então a limpeza é feita aqui, numa transação.
pub async fn delete(db: &Db, id: &str) -> Result<bool> {
    let removed = db.infra().update(|st| {
        let mut removed = false;
        for p in &mut st.projects {
            let before = p.jobs.len();
            p.jobs.retain(|j| j.id != id);
            removed |= p.jobs.len() != before;
        }
        Ok(removed)
    })?;
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM job_log WHERE job_run_id IN (SELECT id FROM job_run WHERE job_id = ?)",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM job_run WHERE job_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM job_runtime WHERE job_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(removed)
}

pub async fn count_by_project(db: &Db, project_id: &str) -> Result<i64> {
    Ok(db
        .infra()
        .snapshot()
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map(|p| p.jobs.len() as i64)
        .unwrap_or(0))
}

/// Remove todos os jobs que usam o serviço como gatilho (cascade do
/// `service_delete` — a GUI avisa no confirm antes). Cada job leva o próprio
/// histórico junto via `delete`. Retorna quantos jobs foram removidos.
pub async fn delete_by_trigger_service(db: &Db, service_id: &str) -> Result<u64> {
    let ids: Vec<String> = db
        .infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|p| p.jobs.iter())
        .filter(|j| j.trigger_service_id.as_deref() == Some(service_id))
        .map(|j| j.id.clone())
        .collect();
    let mut removed = 0u64;
    for id in ids {
        if delete(db, &id).await? {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_db() -> Db {
        let db = Db::memory_for_test().await;
        for p in ["prj_1", "prj_2", "p"] {
            db.seed_project_for_test(p);
        }
        db.seed_service_for_test("prj_1", "svc_1");
        db.seed_service_for_test("p", "s");
        db
    }

    #[tokio::test]
    async fn create_get_update_delete_round_trip() {
        let db = mem_db().await;
        let job = create(
            &db,
            "prj_1",
            Some("svc_1"),
            "backup",
            "version: '3'\nservices:\n  backup:\n    image: busybox\n",
            None,
            "backup",
            &[],
            &[],
            Some(Recurrence::IntervalHours(6)),
        )
        .await
        .unwrap();
        assert!(job.next_run_at.is_some());
        assert_eq!(job.trigger_service_id, Some("svc_1".to_string()));

        let got = get(&db, &job.id).await.unwrap().unwrap();
        assert_eq!(got.recurrence, Some(Recurrence::IntervalHours(6)));

        let updated = update(
            &db,
            &job.id,
            "backup2",
            &job.compose,
            None,
            "backup",
            &[],
            &[],
            false,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.name, "backup2");
        assert!(!updated.enabled);
        assert_eq!(updated.recurrence, None);
        assert_eq!(updated.next_run_at, None);

        assert!(delete(&db, &job.id).await.unwrap());
        assert!(get(&db, &job.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_cascateia_runs_e_logs() {
        let db = mem_db().await;
        let job = create(&db, "prj_1", None, "backup", "c", None, "m", &[], &[], None)
            .await
            .unwrap();
        let run = super::super::job_run::create(&db, &job.id).await.unwrap();
        super::super::job_log::append(
            &db,
            &run.id,
            &shared::protocol::LogStream::Stdout,
            "oi",
            Utc::now(),
        )
        .await
        .unwrap();

        assert!(delete(&db, &job.id).await.unwrap());
        assert!(
            super::super::job_run::get(&db, &run.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            super::super::job_log::get_for_run(&db, &run.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn contagem_por_projeto_e_cascade_por_gatilho() {
        let db = mem_db().await;
        let gatilhado = create(
            &db,
            "prj_1",
            Some("svc_1"),
            "a",
            "c",
            None,
            "m",
            &[],
            &[],
            None,
        )
        .await
        .unwrap();
        let autonomo = create(&db, "prj_1", None, "b", "c", None, "m", &[], &[], None)
            .await
            .unwrap();

        assert_eq!(count_by_project(&db, "prj_1").await.unwrap(), 2);
        assert_eq!(count_by_project(&db, "prj_2").await.unwrap(), 0);

        // Só o job gatilhado por svc_1 cai; o autônomo (sentinel "") fica.
        assert_eq!(delete_by_trigger_service(&db, "svc_2").await.unwrap(), 0);
        assert_eq!(delete_by_trigger_service(&db, "svc_1").await.unwrap(), 1);
        assert!(get(&db, &gatilhado.id).await.unwrap().is_none());
        assert!(get(&db, &autonomo.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn job_sem_servico_gatilho_ida_e_volta_none() {
        let db = mem_db().await;
        let job = create(
            &db,
            "prj_1",
            None,
            "autonomo",
            "c",
            None,
            "m",
            &[],
            &[],
            None,
        )
        .await
        .unwrap();
        assert_eq!(job.trigger_service_id, None);

        let got = get(&db, &job.id).await.unwrap().unwrap();
        assert_eq!(got.trigger_service_id, None);
    }

    #[tokio::test]
    async fn list_due_only_returns_enabled_scheduled_and_overdue() {
        let db = mem_db().await;
        let past = create(
            &db,
            "p",
            Some("s"),
            "past",
            "c",
            None,
            "m",
            &[],
            &[],
            Some(Recurrence::IntervalHours(1)),
        )
        .await
        .unwrap();
        // força next_run_at pro passado, direto no runtime (create já teria calculado no futuro).
        put_runtime(
            &db,
            &past.id,
            None,
            Some(Utc::now() - chrono::Duration::hours(1)),
        )
        .await
        .unwrap();

        let _future = create(
            &db,
            "p",
            Some("s"),
            "future",
            "c",
            None,
            "m",
            &[],
            &[],
            Some(Recurrence::IntervalHours(6)),
        )
        .await
        .unwrap();
        let _manual = create(
            &db,
            "p",
            Some("s"),
            "manual",
            "c",
            None,
            "m",
            &[],
            &[],
            None,
        )
        .await
        .unwrap();

        let due = list_due(&db, Utc::now()).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, past.id);
    }
}
