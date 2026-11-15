use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::*;

/// Uma tarefa one-shot: sobe um stack docker-compose próprio (efêmero — roda
/// até terminar e é removido, nunca fica de pé como um `Service`), anexado à
/// rede do projeto do `trigger_service_id` (que também empresta as env vars de
/// base). Disparado por agendamento (`recurrence`) e/ou manualmente
/// (`Command::JobRunNow`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub project_id: String,
    /// Serviço do projeto que empresta env vars de base ao job (o job não
    /// roda dentro do container dele — sobe um stack novo; a rede Docker já
    /// vem só do `project_id` acima). `None` = job autônomo: só as env vars
    /// do projeto são injetadas, sem nenhuma específica de serviço.
    pub trigger_service_id: Option<String>,
    pub name: String,
    /// Conteúdo de um `docker-compose.yml` — mesma UX/formato de
    /// `ComposeSource.content`. Vazio/ignorado quando `git_source` é `Some`
    /// (o YAML real vem do clone a cada execução, nunca fica salvo aqui —
    /// mesma filosofia do deploy de serviço git-sourced).
    pub compose: String,
    /// Quando presente, o job clona este repositório a cada execução e roda
    /// o `docker-compose.yml` encontrado nele (`compose_path`), em vez do
    /// conteúdo colado em `compose`.
    #[serde(default)]
    pub git_source: Option<JobGitSource>,
    /// Nome do serviço, dentro do compose (colado ou clonado), cujo exit
    /// code decide sucesso/falha do job (`docker compose up
    /// --exit-code-from`).
    pub main_service: String,
    /// Env vars próprias do job — maior precedência na resolução (por cima
    /// de projeto + serviço gatilho, ver `deploy::env_resolve::resolve_job`).
    /// Pensado pra overrides específicos do job que não fazem sentido herdar
    /// de um serviço "de verdade" (ex.: `FLOW_MYENV=test` num job de CI que
    /// roda testes de um repo, sem precisar duplicar as vars do serviço real).
    #[serde(default)]
    pub env_vars: Vec<EnvVar>,
    /// Comentários do editor `.env` do job — mesmo esquema de
    /// `Project.env_comments`/`ServiceSpec.env_comments`.
    #[serde(default)]
    pub env_comments: Vec<EnvComment>,
    pub enabled: bool,
    /// `None` = sem agendamento automático, só `Command::JobRunNow`.
    pub recurrence: Option<Recurrence>,
    pub last_run_at: Option<DateTime<Utc>>,
    /// `None` quando `recurrence` é `None` (nada a agendar).
    pub next_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Fonte git de um `Job` — irmã mais enxuta de `GitSource` (sem
/// `dockerfile_path`/`build_context`/`build_stage`/`submodules`/
/// `watch_paths`, que não fazem sentido pra um `docker-compose.yml` inteiro
/// clonado do repo em vez de um único Dockerfile buildado pelo daemon).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobGitSource {
    /// Conta git conectada (Gitea/GitHub) — quando presente, tem precedência
    /// sobre `credentials` na resolução do token de clone (mesma regra de
    /// `GitSource.provider_id`). `#[serde(default)]`: os clientes (Luau/JS)
    /// omitem a chave inteira pra "sem valor" em vez de mandar `null`.
    #[serde(default)]
    pub provider_id: Option<String>,
    pub url: String,
    pub branch: String,
    /// Auth manual (sem `provider_id`): usuário do clone HTTPS.
    #[serde(default)]
    pub username: Option<String>,
    /// Nome de um secret do projeto, usado como token quando não há
    /// `provider_id` (mesma semântica de `GitSource.credentials`).
    #[serde(default)]
    pub credentials: Option<String>,
    /// Caminho do `docker-compose.yml` dentro do repo clonado.
    pub compose_path: String,
}

impl Default for JobGitSource {
    fn default() -> Self {
        Self {
            provider_id: None,
            url: String::new(),
            branch: "main".into(),
            username: None,
            credentials: None,
            compose_path: "docker-compose.yml".into(),
        }
    }
}

/// Recorrência estruturada (sem expressão cron — ver decisão em
/// docs/planejamento; `chrono` já é dependência, sem precisar de crate nova).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Recurrence {
    IntervalHours(u32),
    Daily {
        hour: u8,
        minute: u8,
    },
    /// `weekday`: 0=segunda .. 6=domingo (`chrono::Weekday::num_days_from_monday`).
    Weekly {
        weekday: u8,
        hour: u8,
        minute: u8,
    },
}

impl Recurrence {
    /// Próximo disparo estritamente depois de `from`.
    pub fn next_after(&self, from: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Recurrence::IntervalHours(hours) => {
                from + chrono::Duration::hours((*hours).max(1) as i64)
            }
            Recurrence::Daily { hour, minute } => {
                let mut candidate = Self::at_time(from, *hour, *minute);
                if candidate <= from {
                    candidate += chrono::Duration::days(1);
                }
                candidate
            }
            Recurrence::Weekly {
                weekday,
                hour,
                minute,
            } => {
                let mut candidate = Self::at_time(from, *hour, *minute);
                let target = (*weekday % 7) as i64;
                let current = from.weekday().num_days_from_monday() as i64;
                let mut delta_days = target - current;
                if delta_days < 0 {
                    delta_days += 7;
                }
                candidate += chrono::Duration::days(delta_days);
                if candidate <= from {
                    candidate += chrono::Duration::days(7);
                }
                candidate
            }
        }
    }

    /// `from`'s calendar date at the given UTC hour:minute (seconds zeroed).
    fn at_time(from: DateTime<Utc>, hour: u8, minute: u8) -> DateTime<Utc> {
        from.date_naive()
            .and_hms_opt(hour.min(23) as u32, minute.min(59) as u32, 0)
            .unwrap_or_else(|| from.date_naive().and_hms_opt(0, 0, 0).unwrap())
            .and_utc()
    }
}

/// Uma execução de um `Job`. `success: None` enquanto ainda está rodando.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub success: Option<bool>,
}

/// `Job` + nomes resolvidos, pra listagem cross-project (sidebar Schedules) —
/// mesmo espírito de `DeploymentSummary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub job: Job,
    pub project_name: String,
    /// `None` quando `job.trigger_service_id` é `None` (job autônomo).
    pub trigger_service_name: Option<String>,
    pub last_run: Option<JobRun>,
    /// `#[serde(default)]`: um daemon mais antigo não manda a chave.
    #[serde(default)]
    pub warm: JobWarmStatus,
}

/// Se as imagens e o cache de build de um job já estão no host, de modo que a
/// próxima execução não baixe nem recompile do zero. Fonte única da verdade
/// (`Command::JobWarmStatus`, também embutida em `JobSummary`): as telas só
/// exibem este valor, nunca o deduzem.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobWarmStatus {
    pub job_id: String,
    pub warm: bool,
    /// Há um aquecimento em andamento (ou esperando a vaga da fila) agora.
    pub warming: bool,
    /// Por que está frio (`warm == false`): nunca aquecido, imagem ausente,
    /// cache de build limpo, definição do job alterada. `None` quando quente.
    pub reason: Option<String>,
    /// Quando o job foi aquecido pela última vez (aquecimento explícito ou
    /// execução bem-sucedida).
    pub warmed_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod job_tests {
    use super::*;

    #[test]
    fn docker_cleanup_config_saved_before_warm_after_cleanup_warms_by_default() {
        let mut json = serde_json::to_value(DockerCleanupConfig::default()).unwrap();
        json.as_object_mut().unwrap().remove("warm_after_cleanup");

        let loaded: DockerCleanupConfig = serde_json::from_value(json).unwrap();

        assert!(loaded.warm_after_cleanup);
    }

    #[test]
    fn docker_cleanup_config_saved_before_retention_fields_keeps_its_values() {
        let config = DockerCleanupConfig {
            enabled: true,
            build_cache: true,
            ..DockerCleanupConfig::default()
        };
        let mut json = serde_json::to_value(&config).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("build_cache_older_than_hours");
        object.remove("images_older_than_hours");

        let loaded: DockerCleanupConfig = serde_json::from_value(json).unwrap();

        assert!(
            loaded.enabled && loaded.build_cache,
            "config antiga não pode resetar"
        );
        assert_eq!(
            loaded.build_cache_older_than_hours,
            DEFAULT_CLEANUP_RETENTION_HOURS
        );
        assert_eq!(
            loaded.images_older_than_hours,
            DEFAULT_CLEANUP_RETENTION_HOURS
        );
    }

    #[test]
    fn job_summary_from_older_daemon_has_cold_default_warm_status() {
        let status: JobWarmStatus = serde_json::from_str(
            r#"{"job_id":"j","warm":false,"warming":false,"reason":null,"warmed_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            status,
            JobWarmStatus {
                job_id: "j".into(),
                ..JobWarmStatus::default()
            }
        );
    }

    #[test]
    fn interval_advances_by_hours() {
        let from = "2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next = Recurrence::IntervalHours(6).next_after(from);
        assert_eq!(next, from + chrono::Duration::hours(6));
    }

    #[test]
    fn daily_before_time_today_fires_today() {
        let from = "2026-01-01T02:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next = Recurrence::Daily { hour: 3, minute: 0 }.next_after(from);
        assert_eq!(
            next,
            "2026-01-01T03:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn daily_after_time_today_fires_tomorrow() {
        let from = "2026-01-01T04:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next = Recurrence::Daily { hour: 3, minute: 0 }.next_after(from);
        assert_eq!(
            next,
            "2026-01-02T03:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn weekly_picks_next_matching_weekday() {
        // 2026-01-01 é uma quinta-feira (weekday 3, 0=segunda).
        let from = "2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(from.weekday().num_days_from_monday(), 3);
        // Próxima segunda (weekday 0) às 03:00 -> 2026-01-05.
        let next = Recurrence::Weekly {
            weekday: 0,
            hour: 3,
            minute: 0,
        }
        .next_after(from);
        assert_eq!(
            next,
            "2026-01-05T03:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn weekly_same_weekday_but_time_passed_rolls_to_next_week() {
        // 2026-01-01 é quinta (weekday 3); pede quinta às 03:00, mas já são 10:00 -> semana que vem.
        let from = "2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next = Recurrence::Weekly {
            weekday: 3,
            hour: 3,
            minute: 0,
        }
        .next_after(from);
        assert_eq!(
            next,
            "2026-01-08T03:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn recurrence_json_round_trip() {
        for r in [
            Recurrence::IntervalHours(6),
            Recurrence::Daily {
                hour: 3,
                minute: 30,
            },
            Recurrence::Weekly {
                weekday: 6,
                hour: 0,
                minute: 0,
            },
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: Recurrence = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }
}
