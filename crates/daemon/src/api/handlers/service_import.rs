//! `Command::ServiceImport`: cria UM serviço novo a partir de um
//! [`shared::ServiceBundle`] (ver `docs/plano-copiar-servico-entre-servidores.md`).
//!
//! O destino é sempre um projeto que JÁ existe (aquele em que o usuário está
//! com a tela aberta): importar é criar um serviço dentro dele, como qualquer
//! outro. Aditivo por construção: nunca atualiza nem apaga serviço, e só mexe no
//! projeto (variável nova, ou sobrescrita explícita) por escolha do usuário. Duas fases, na ordem:
//!
//! 1. [`analyze`] — lê o pacote e compara com o destino. Não grava NADA e
//!    devolve tudo o que o relatório precisa; é o que o `dry_run` mostra.
//! 2. [`apply`] — só roda sem `dry_run` e sem pendência que bloqueie.

use crate::api::AppState;
use shared::{
    EnvVar, EnvVarValue, ImportWarning, Project, ProjectEnvChoice, ProjectEnvState,
    ProjectEnvStatus, Response as RpResponse, Service, ServiceBundle, ServiceImportReport,
    ServiceImportReq, ServiceSource, ServiceSpec, normalize_name,
};
use std::collections::{BTreeMap, BTreeSet};
use tracing::info;

/// Um pacote maior que isto não é um pacote de serviço.
const MAX_YAML_BYTES: usize = 1024 * 1024;
const SECRET_PREFIX: &str = "secret:";

/// Erro de pedido: `(code, mensagem)` já no formato de `Response::err`.
type Fail = (&'static str, String);

fn warn_item(code: &str, message: impl Into<String>) -> ImportWarning {
    ImportWarning {
        code: code.to_string(),
        message: message.into(),
    }
}

/// Tudo o que a análise concluiu — o relatório e o necessário para aplicar.
struct Analysis {
    report: ServiceImportReport,
    project: Project,
    /// O spec pronto para criar (com `project_id` em branco se o projeto é novo).
    spec: ServiceSpec,
    /// Variáveis do projeto a gravar (já com a escolha do usuário aplicada).
    project_env_writes: Vec<EnvVar>,
    /// Secrets a criar no projeto de destino (`nome → valor`).
    secrets_to_set: BTreeMap<String, String>,
}

/// `${CHAVE}` sob a PRÓPRIA chave é o marcador de "valor não exportado". Um
/// valor `${OUTRA}` é texto do usuário (referência de compose/shell) e passa
/// intacto — senão um export COM valores acusaria falta de variável.
fn is_placeholder(key: &str, value: &str) -> bool {
    value == format!("${{{key}}}")
}

/// Resolve os marcadores de `map` com `provided`; devolve as chaves sem valor.
fn resolve_placeholders(
    map: &mut BTreeMap<String, String>,
    provided: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for (k, v) in map.iter_mut() {
        if is_placeholder(k, v) {
            match provided.get(k) {
                Some(val) => *v = val.clone(),
                None => missing.push(k.clone()),
            }
        }
    }
    missing
}

fn secret_names(map: &BTreeMap<String, String>) -> BTreeSet<String> {
    map.values()
        .filter_map(|v| v.strip_prefix(SECRET_PREFIX).map(str::to_string))
        .collect()
}

/// Em um serviço Compose de banco/broker o nome do serviço interno é
/// `rp_<nome>` (é o DNS da connection string). Se o import renomeia o serviço,
/// o nome interno muda junto; troca só ocorrências inteiras (`rp_db` não pode
/// pegar `rp_db_admin`).
fn rename_compose_alias(content: &str, old: &str, new: &str) -> String {
    let from = format!("rp_{}", normalize_name(old));
    let to = format!("rp_{}", normalize_name(new));
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(pos) = rest.find(&from) {
        let before_ok = rest[..pos].chars().next_back().is_none_or(|c| !is_word(c));
        let after = &rest[pos + from.len()..];
        let after_ok = after.chars().next().is_none_or(|c| !is_word(c));
        out.push_str(&rest[..pos]);
        out.push_str(if before_ok && after_ok { &to } else { &from });
        rest = after;
    }
    out.push_str(rest);
    out
}

async fn analyze(state: &AppState, req: &ServiceImportReq) -> Result<Analysis, Fail> {
    if req.yaml.len() > MAX_YAML_BYTES {
        return Err((
            "InvalidBundle",
            "o arquivo é grande demais para ser um pacote de serviço".into(),
        ));
    }
    let bundle: ServiceBundle =
        serde_yaml::from_str(&req.yaml).map_err(|e| ("InvalidBundle", e.to_string()))?;
    bundle.validate().map_err(|e| ("InvalidBundle", e))?;

    // ── destino: o projeto em que o usuário está ───────────────────────────
    let project = match crate::db::projects::get(&state.db, &req.project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err(("NotFound", "projeto de destino não encontrado".into())),
        Err(e) => return Err(("DatabaseError", e.to_string())),
    };

    let mut warnings: Vec<ImportWarning> = Vec::new();
    let mut manifest = bundle.service.clone();
    let orig_name = manifest.name.clone();
    let final_name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&orig_name)
        .to_string();
    manifest.name = final_name.clone();

    // ── conflito de nome ─────────────────────
    let existing_services: Vec<Service> = crate::db::services::list(&state.db, &project.id)
        .await
        .map_err(|e| ("DatabaseError", e.to_string()))?;
    let name_conflict = existing_services
        .iter()
        .any(|s| normalize_name(&s.spec.name) == normalize_name(&final_name));

    // ── variáveis: marcadores `${CHAVE}` e secrets ─────────────────────────
    let missing_service_vars = resolve_placeholders(&mut manifest.env, &req.vars);
    let mut project_vals = bundle.project_env.clone();
    let missing_project_vars = resolve_placeholders(&mut project_vals, &req.project_vars);
    let unresolved: BTreeSet<&String> = missing_project_vars.iter().collect();

    // Variáveis de projeto: estado no destino + escolha do usuário.
    let dest_env: Vec<EnvVar> = project.env_vars.clone();
    let mut statuses = Vec::new();
    let mut project_env_writes = dest_env.clone();
    for (key, raw) in &project_vals {
        if unresolved.contains(key) {
            continue; // sem valor: não dá para comparar nem gravar
        }
        let incoming = EnvVar {
            key: key.clone(),
            value: match raw.strip_prefix(SECRET_PREFIX) {
                Some(name) => EnvVarValue::Secret(name.to_string()),
                None => EnvVarValue::Plain(raw.clone()),
            },
        };
        let state_in_dest = match dest_env.iter().find(|e| &e.key == key) {
            None => ProjectEnvState::New,
            Some(cur) if cur.value == incoming.value => ProjectEnvState::Same,
            Some(_) => ProjectEnvState::Conflict,
        };
        let choice = req.project_env.get(key).copied().unwrap_or_default();
        statuses.push(ProjectEnvStatus {
            key: key.clone(),
            state: state_in_dest,
            is_secret: matches!(incoming.value, EnvVarValue::Secret(_)),
            choice,
        });

        match (state_in_dest, choice) {
            (_, ProjectEnvChoice::Ignore) | (ProjectEnvState::Same, _) => {}
            (ProjectEnvState::Conflict, ProjectEnvChoice::Keep) => {}
            (ProjectEnvState::New, ProjectEnvChoice::Keep | ProjectEnvChoice::Overwrite) => {
                project_env_writes.push(incoming);
            }
            (ProjectEnvState::Conflict, ProjectEnvChoice::Overwrite) => {
                if let Some(slot) = project_env_writes.iter_mut().find(|e| &e.key == key) {
                    slot.value = incoming.value;
                }
            }
            (_, ProjectEnvChoice::ServiceOnly) => {
                // O env do serviço vence o do projeto; não sobrescreve o que o
                // próprio serviço já define.
                manifest
                    .env
                    .entry(key.clone())
                    .or_insert_with(|| raw.clone());
            }
        }
    }

    // Secrets citados (serviço + os que vão para o projeto) que o destino não tem.
    let mut referenced = secret_names(&manifest.env);
    for s in &statuses {
        let applied = !matches!(s.choice, ProjectEnvChoice::Ignore);
        if applied
            && s.is_secret
            && s.state != ProjectEnvState::Same
            && let Some(n) = project_vals
                .get(&s.key)
                .and_then(|v| v.strip_prefix(SECRET_PREFIX))
        {
            referenced.insert(n.to_string());
        }
    }
    let have: BTreeSet<String> = state
        .secrets
        .list_names(&project.id)
        .await
        .map_err(|e| ("SecretListFailed", e.to_string()))?
        .into_iter()
        .collect();
    let mut missing_secrets = Vec::new();
    let mut secrets_to_set = BTreeMap::new();
    for name in referenced.difference(&have) {
        match req.secrets.get(name) {
            Some(value) => {
                secrets_to_set.insert(name.clone(), value.clone());
            }
            None => missing_secrets.push(name.clone()),
        }
    }

    // ── git provider (por nome) ────────────────────────────────────────────
    let providers = crate::db::git_providers::list(&state.db)
        .await
        .map_err(|e| ("DatabaseError", e.to_string()))?;
    let provider_ids: BTreeMap<String, String> = providers
        .iter()
        .map(|p| (p.name.clone(), p.id.clone()))
        .collect();
    let mut missing_git_provider = None;
    if let Some(git) = &manifest.source.git
        && req.git_provider_id.is_none()
        && let Some(name) = &git.provider
        && !provider_ids.contains_key(name)
    {
        missing_git_provider = Some(name.clone());
        warnings.push(warn_item(
            "git_provider_missing",
            format!(
                "O Git provider '{name}' não existe neste servidor: o serviço fica sem \
                 provider (repo público ou credenciais manuais). Escolha outro provider ou \
                 conecte '{name}' antes."
            ),
        ));
    }

    // ── o que viaja diferente / o que não viaja ────────────────────────────
    if manifest.shared {
        manifest.shared = false;
        warnings.push(warn_item(
            "shared_dropped",
            "Este serviço era um servidor de banco compartilhado; aqui ele vira um serviço \
             comum (o compartilhamento depende dos projetos e usuários do servidor de origem).",
        ));
    }
    if !manifest.volumes.is_empty() || manifest.db.is_some() {
        warnings.push(warn_item(
            "data_not_copied",
            "Só a configuração foi copiada: os DADOS (volumes, conteúdo do banco) não vêm junto.",
        ));
    }
    if manifest.host_port.is_some() {
        warnings.push(warn_item(
            "host_port_new",
            "A porta externa do serviço será alocada de novo neste servidor.",
        ));
    }
    if req.drop_domains {
        let had = manifest.domain.is_some() || !manifest.domains.is_empty();
        manifest.domain = None;
        manifest.tls = false;
        manifest.domains.clear();
        if had {
            warnings.push(warn_item(
                "domains_dropped",
                "Os domínios do pacote não foram trazidos.",
            ));
        }
    } else {
        let mut domains: Vec<String> = manifest.domains.iter().map(|d| d.domain.clone()).collect();
        domains.extend(manifest.domain.clone());
        domains.sort();
        domains.dedup();
        if !domains.is_empty() {
            warnings.push(warn_item(
                "domains_dns",
                format!(
                    "Domínios trazidos: {}. O DNS precisa apontar para ESTE servidor.",
                    domains.join(", ")
                ),
            ));
            let others = crate::db::services::list_all(&state.db)
                .await
                .map_err(|e| ("DatabaseError", e.to_string()))?;
            let clash: Vec<String> = domains
                .iter()
                .filter(|d| {
                    others.iter().any(|s| {
                        s.spec
                            .domain_routes()
                            .iter()
                            .any(|r| r.domain.eq_ignore_ascii_case(d))
                    })
                })
                .cloned()
                .collect();
            if !clash.is_empty() {
                warnings.push(warn_item(
                    "domain_in_use",
                    format!(
                        "Já há serviço neste servidor usando: {}. Dois serviços no mesmo \
                         domínio disputam a rota; use \"não trazer domínios\" ou troque o domínio.",
                        clash.join(", ")
                    ),
                ));
            }
        }
    }

    // ── spec final ─────────────────────────────────────────────────────────
    let mut spec = manifest.to_spec(&project.id, &provider_ids);
    if let (Some(id), ServiceSource::Git(g)) = (&req.git_provider_id, &mut spec.source) {
        g.provider_id = Some(id.clone());
    }
    if final_name != orig_name
        && let (ServiceSource::Compose(c), Some(_)) = (&mut spec.source, &spec.db_kind)
    {
        let renamed = rename_compose_alias(&c.content, &orig_name, &final_name);
        if renamed != c.content {
            c.content = renamed;
            warnings.push(warn_item(
                "compose_alias_renamed",
                format!(
                    "O nome interno do banco/broker mudou de rp_{} para rp_{}. Variáveis de \
                     OUTROS serviços que apontam para o nome antigo não foram alteradas.",
                    normalize_name(&orig_name),
                    normalize_name(&final_name)
                ),
            ));
        }
    }

    let report = ServiceImportReport {
        dry_run: req.dry_run,
        service_id: None,
        service_name: final_name,
        project_name: project.name.clone(),
        name_conflict,
        warnings,
        missing_service_vars,
        missing_project_vars,
        missing_secrets,
        missing_git_provider,
        project_env: statuses,
        deployed: false,
    };
    Ok(Analysis {
        report,
        project,
        spec,
        project_env_writes,
        secrets_to_set,
    })
}

/// Grava o que a análise decidiu.
async fn apply(state: &AppState, req: &ServiceImportReq, mut a: Analysis) -> RpResponse {
    match write_all(state, req, &mut a).await {
        Ok(()) => RpResponse::ServiceImportReport(a.report),
        Err(r) => r,
    }
}

async fn write_all(
    state: &AppState,
    req: &ServiceImportReq,
    a: &mut Analysis,
) -> Result<(), RpResponse> {
    let project_id = a.project.id.clone();
    let project_id = project_id.as_str();
    for (name, value) in &a.secrets_to_set {
        state
            .secrets
            .set(project_id, name, value)
            .await
            .map_err(|e| RpResponse::err("SecretSetFailed", e.to_string()))?;
    }

    if a.project_env_writes != a.project.env_vars {
        crate::db::projects::update_env_vars(
            &state.db,
            project_id,
            a.project_env_writes.clone(),
            a.project.env_comments.clone(),
        )
        .await
        .map_err(|e| RpResponse::err("DatabaseError", e.to_string()))?;
    }

    a.spec.project_id = project_id.to_string();
    let svc = match super::service_create::handle(state.clone(), a.spec.clone()).await {
        RpResponse::Service(s) => s,
        err @ RpResponse::Err { .. } => return Err(err),
        _ => {
            return Err(RpResponse::err(
                "Unexpected",
                "resposta inesperada do ServiceCreate",
            ));
        }
    };
    a.report.service_id = Some(svc.id.clone());
    info!(service_id = %svc.id, name = %svc.spec.name, project_id, "service_import: serviço criado");

    if req.deploy {
        match super::deploy_start::handle(state.clone(), svc.id.clone()).await {
            RpResponse::Err { message, .. } => {
                // O serviço existe; só o deploy não começou. Vira aviso, não falha.
                a.report.warnings.push(warn_item(
                    "deploy_not_started",
                    format!("Serviço criado, mas o deploy não iniciou: {message}"),
                ));
            }
            _ => a.report.deployed = true,
        }
    }
    Ok(())
}

pub async fn handle(state: AppState, req: ServiceImportReq) -> RpResponse {
    info!(
        dry_run = req.dry_run,
        deploy = req.deploy,
        "service_import: recebido"
    );

    let analysis = match analyze(&state, &req).await {
        Ok(a) => a,
        Err((code, msg)) => return RpResponse::err(code, msg),
    };
    if req.dry_run {
        return RpResponse::ServiceImportReport(analysis.report);
    }

    // Pendências que impedem criar (as demais viram só aviso no relatório).
    let r = &analysis.report;
    if r.name_conflict {
        return RpResponse::err(
            "NameConflict",
            format!(
                "já existe um serviço '{}' no projeto '{}'; escolha outro nome",
                r.service_name, r.project_name
            ),
        );
    }
    if !r.missing_service_vars.is_empty() || !r.missing_project_vars.is_empty() {
        return RpResponse::err(
            "MissingVars",
            format!(
                "faltam valores para: {}",
                r.missing_service_vars
                    .iter()
                    .chain(r.missing_project_vars.iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    apply(&state, &req, analysis).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marcador_so_vale_sob_a_propria_chave() {
        assert!(is_placeholder("DB", "${DB}"));
        assert!(!is_placeholder("DB", "${OUTRA}"));
        assert!(!is_placeholder("DB", "valor"));
    }

    #[test]
    fn resolve_marcadores_e_lista_os_que_faltam() {
        let mut m = BTreeMap::from([
            ("A".to_string(), "${A}".to_string()),
            ("B".to_string(), "${B}".to_string()),
            ("C".to_string(), "literal".to_string()),
            ("D".to_string(), "${A}".to_string()),
        ]);
        let missing = resolve_placeholders(&mut m, &BTreeMap::from([("A".into(), "1".into())]));
        assert_eq!(missing, vec!["B"]);
        assert_eq!(m["A"], "1");
        assert_eq!(
            m["D"], "${A}",
            "referência de texto do usuário fica intacta"
        );
    }

    #[test]
    fn renomeia_so_o_alias_inteiro_do_compose() {
        let c = "services:\n  rp_db:\n    image: x\n  rp_db_admin:\n    image: y\n  other: rp_db\n";
        let r = rename_compose_alias(c, "db", "db2");
        assert!(r.contains("  rp_db2:\n"), "{r}");
        assert!(r.contains("rp_db_admin"), "não pode pegar prefixo: {r}");
        assert!(r.contains("other: rp_db2"), "{r}");
        assert!(!r.contains("rp_db2_admin"), "{r}");
    }
}

/// Fim-a-fim: dois daemons de mentira (A exporta, B importa), com os handlers
/// reais e bancos de verdade — nada de lógica replicada no teste.
#[cfg(test)]
mod e2e {
    use super::*;
    use shared::{DomainRoute, EnvVarValue, Healthcheck, ResourceLimits, Response, ServiceStatus};
    use ulid::Ulid;

    async fn daemon() -> AppState {
        let tmp = std::env::temp_dir().join(format!("rustploy-test-state-{}", Ulid::new()));
        let db = crate::db::connect(&tmp.join("data")).await.unwrap();
        let config = shared::RustployConfig::default();
        let docker = std::sync::Arc::new(
            crate::docker::DockerClient::connect(&config.docker.socket_path).unwrap(),
        );
        let db = std::sync::Arc::new(db);
        let bus = std::sync::Arc::new(crate::event_bus::EventBus::new());
        let ingress = std::sync::Arc::new(crate::ingress::IngressController::new());
        let secrets = std::sync::Arc::new(
            crate::secrets::SecretsManager::new(&tmp.join("master.key"), db.clone()).unwrap(),
        );
        let tls = std::sync::Arc::new(
            crate::ingress::TlsManager::new(tmp.join("certs"), config.ingress.acme.clone())
                .unwrap(),
        );
        AppState::new(
            db,
            docker,
            ingress,
            bus,
            secrets,
            tls,
            tmp.join("db"),
            tmp.join("backup"),
            30,
            config.api,
            None,
            None,
        )
    }

    fn plain(k: &str, v: &str) -> EnvVar {
        EnvVar {
            key: k.into(),
            value: EnvVarValue::Plain(v.into()),
        }
    }

    fn secret(k: &str, name: &str) -> EnvVar {
        EnvVar {
            key: k.into(),
            value: EnvVarValue::Secret(name.into()),
        }
    }

    fn spec(project_id: &str) -> ServiceSpec {
        ServiceSpec {
            name: "api".into(),
            project_id: project_id.into(),
            source: ServiceSource::Registry {
                image: "nginx:1".into(),
            },
            port: 8888,
            host_port: None,
            domain: None,
            tls_enabled: false,
            env_vars: vec![plain("CACHE", "${REDIS_URL}"), plain("OWN", "mine")],
            env_comments: vec![],
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
            pre_deploy_job_ids: vec![],
            shared: None,
        }
    }

    /// O servidor A: projeto "Flow" com 3 variáveis (uma é secret) e o serviço api.
    async fn server_a() -> (AppState, String) {
        let a = daemon().await;
        let p = crate::db::projects::create(&a.db, "Flow".into(), None)
            .await
            .unwrap();
        crate::db::projects::update_env_vars(
            &a.db,
            &p.id,
            vec![
                plain("REDIS_URL", "redis://cache:6379"),
                plain("NAO_LEVAR", "segredo-que-fica-em-A"),
                secret("SENTRY", "SENTRY_DSN"),
            ],
            vec![],
        )
        .await
        .unwrap();
        let svc = crate::db::services::create(&a.db, spec(&p.id))
            .await
            .unwrap();
        (a, svc.id)
    }

    async fn export(a: &AppState, id: &str, values: bool, keys: &[&str]) -> String {
        match service_export_export(a, id, values, keys).await {
            Response::ServiceBundleYaml { yaml, .. } => yaml,
            other => panic!("export falhou: {other:?}"),
        }
    }

    async fn service_export_export(
        a: &AppState,
        id: &str,
        values: bool,
        keys: &[&str],
    ) -> Response {
        super::super::service_export::export(
            a.clone(),
            id.into(),
            values,
            keys.iter().map(|k| k.to_string()).collect(),
        )
        .await
    }

    fn report(r: Response) -> ServiceImportReport {
        match r {
            Response::ServiceImportReport(r) => r,
            other => panic!("esperava relatório: {other:?}"),
        }
    }

    fn err_code(r: Response) -> String {
        match r {
            Response::Err { code, .. } => code,
            other => panic!("esperava erro: {other:?}"),
        }
    }

    fn req(yaml: &str, project_id: &str) -> ServiceImportReq {
        ServiceImportReq {
            yaml: yaml.into(),
            project_id: project_id.into(),
            ..Default::default()
        }
    }

    /// O projeto do servidor B em que o usuário está com a tela aberta.
    async fn project_b(b: &AppState) -> String {
        crate::db::projects::create(&b.db, "Flow B".into(), None)
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn plano_sugere_so_o_que_o_servico_cita_e_nunca_traz_valor() {
        let (a, id) = server_a().await;
        let Response::ServiceExportPlan(plan) =
            super::super::service_export::plan(a.clone(), id).await
        else {
            panic!("plano")
        };
        assert_eq!(plan.project_name, "Flow");
        let sug: Vec<&str> = plan
            .project_env
            .iter()
            .filter(|v| v.suggested)
            .map(|v| v.key.as_str())
            .collect();
        assert_eq!(sug, vec!["REDIS_URL"], "CACHE=${{REDIS_URL}} cita só ela");
        assert!(
            plan.project_env
                .iter()
                .any(|v| v.key == "SENTRY" && v.is_secret)
        );
        assert_eq!(plan.service_env.len(), 2);
        assert!(plan.blocked.is_none());
    }

    #[tokio::test]
    async fn copia_de_a_para_b_dentro_do_projeto() {
        let (a, id) = server_a().await;
        let yaml = export(&a, &id, true, &["REDIS_URL", "SENTRY"]).await;
        assert!(
            !yaml.contains("segredo-que-fica-em-A"),
            "variável não marcada vazou no arquivo"
        );

        let b = daemon().await;
        let pid = project_b(&b).await;

        // Pré-visualização: não grava nada.
        let mut r = req(&yaml, &pid);
        r.dry_run = true;
        let rep = report(handle_resp(&b, r.clone()).await);
        assert!(rep.service_id.is_none());
        assert_eq!(rep.project_name, "Flow B");
        // `CACHE=${REDIS_URL}` é texto do usuário, não marcador: nada "falta".
        assert!(rep.missing_service_vars.is_empty() && rep.missing_project_vars.is_empty());
        assert_eq!(rep.missing_secrets, vec!["SENTRY_DSN"]);
        assert!(rep.warnings.iter().any(|w| w.code == "domains_dns"));
        assert_eq!(rep.project_env.len(), 2);
        assert!(
            rep.project_env
                .iter()
                .all(|s| s.state == ProjectEnvState::New)
        );
        let antes = crate::db::projects::get(&b.db, &pid)
            .await
            .unwrap()
            .unwrap();
        assert!(
            antes.env_vars.is_empty(),
            "dry_run não pode gravar variável"
        );
        assert!(
            crate::db::services::list(&b.db, &pid)
                .await
                .unwrap()
                .is_empty(),
            "dry_run não pode criar serviço"
        );

        // Valendo, informando o secret que faltava no B.
        r.dry_run = false;
        r.secrets.insert("SENTRY_DSN".into(), "https://dsn".into());
        let rep = report(handle_resp(&b, r).await);
        let sid = rep.service_id.clone().expect("criado");
        assert!(!rep.deployed);

        let svc = crate::db::services::get(&b.db, &sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(svc.status, ServiceStatus::Stopped, "nunca deploya sozinho");
        assert_eq!(svc.spec.domains.len(), 2, "as duas rotas de domínio vieram");
        let proj = crate::db::projects::get(&b.db, &svc.spec.project_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(proj.name, "Flow B");
        let keys: Vec<&str> = proj.env_vars.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["REDIS_URL", "SENTRY"]);
        assert_eq!(
            b.secrets.list_names(&proj.id).await.unwrap(),
            vec!["SENTRY_DSN"]
        );
    }

    #[tokio::test]
    async fn conflito_de_variavel_do_projeto_respeita_a_escolha() {
        let (a, id) = server_a().await;
        let yaml = export(&a, &id, true, &["REDIS_URL"]).await;

        let new_dest = || async {
            let b = daemon().await;
            let p = crate::db::projects::create(&b.db, "Dest".into(), None)
                .await
                .unwrap();
            crate::db::projects::update_env_vars(
                &b.db,
                &p.id,
                vec![plain("REDIS_URL", "redis://do-B:6379")],
                vec![],
            )
            .await
            .unwrap();
            (b, p.id)
        };
        let run = |b: AppState, pid: String, choice: Option<ProjectEnvChoice>| {
            let yaml = yaml.clone();
            async move {
                let mut r = ServiceImportReq {
                    yaml,
                    project_id: pid.clone(),
                    ..Default::default()
                };
                if let Some(c) = choice {
                    r.project_env.insert("REDIS_URL".into(), c);
                }
                let rep = report(handle_resp(&b, r).await);
                let p = crate::db::projects::get(&b.db, &pid)
                    .await
                    .unwrap()
                    .unwrap();
                let s = crate::db::services::get(&b.db, rep.service_id.as_ref().unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                (rep, p, s)
            }
        };
        let val = |vars: &[EnvVar], k: &str| {
            vars.iter().find(|e| e.key == k).map(|e| match &e.value {
                EnvVarValue::Plain(v) => v.clone(),
                EnvVarValue::Secret(n) => format!("secret:{n}"),
            })
        };

        // Padrão: mantém a do B.
        let (b, pid) = new_dest().await;
        let (rep, p, s) = run(b, pid, None).await;
        assert_eq!(rep.project_env[0].state, ProjectEnvState::Conflict);
        assert_eq!(val(&p.env_vars, "REDIS_URL").unwrap(), "redis://do-B:6379");
        assert!(val(&s.spec.env_vars, "REDIS_URL").is_none());

        // Sobrescrever: o projeto de B passa a ter a de A.
        let (b, pid) = new_dest().await;
        let (_, p, _) = run(b, pid, Some(ProjectEnvChoice::Overwrite)).await;
        assert_eq!(val(&p.env_vars, "REDIS_URL").unwrap(), "redis://cache:6379");

        // Só para o serviço: o projeto não muda, o serviço leva a dele.
        let (b, pid) = new_dest().await;
        let (_, p, s) = run(b, pid, Some(ProjectEnvChoice::ServiceOnly)).await;
        assert_eq!(val(&p.env_vars, "REDIS_URL").unwrap(), "redis://do-B:6379");
        assert_eq!(
            val(&s.spec.env_vars, "REDIS_URL").unwrap(),
            "redis://cache:6379"
        );

        // Ignorar: não traz.
        let (b, pid) = new_dest().await;
        let (_, p, s) = run(b, pid, Some(ProjectEnvChoice::Ignore)).await;
        assert_eq!(val(&p.env_vars, "REDIS_URL").unwrap(), "redis://do-B:6379");
        assert!(val(&s.spec.env_vars, "REDIS_URL").is_none());
    }

    #[tokio::test]
    async fn nome_repetido_bloqueia_e_renomear_resolve() {
        let (a, id) = server_a().await;
        let yaml = export(&a, &id, true, &[]).await;
        let b = daemon().await;
        let p = crate::db::projects::create(&b.db, "Dest".into(), None)
            .await
            .unwrap();
        crate::db::services::create(&b.db, spec(&p.id))
            .await
            .unwrap(); // já tem "api"

        let mut r = ServiceImportReq {
            yaml,
            project_id: p.id.clone(),
            ..Default::default()
        };
        r.dry_run = true;
        assert!(report(handle_resp(&b, r.clone()).await).name_conflict);

        r.dry_run = false;
        assert_eq!(err_code(handle_resp(&b, r.clone()).await), "NameConflict");
        assert_eq!(
            crate::db::services::list(&b.db, &p.id).await.unwrap().len(),
            1,
            "nada pode ter sido criado"
        );

        r.name = Some("api-copia".into());
        let rep = report(handle_resp(&b, r).await);
        assert_eq!(rep.service_name, "api-copia");
        assert!(rep.service_id.is_some());
    }

    #[tokio::test]
    async fn export_sem_valores_pede_os_valores_no_import() {
        let (a, id) = server_a().await;
        let yaml = export(&a, &id, false, &["REDIS_URL"]).await;
        assert!(!yaml.contains("redis://cache"), "valor vazou: {yaml}");
        assert!(!yaml.contains("mine"), "valor do serviço vazou: {yaml}");

        let b = daemon().await;
        let pid = project_b(&b).await;
        let mut r = req(&yaml, &pid);
        r.dry_run = true;
        let rep = report(handle_resp(&b, r.clone()).await);
        // Sem valores, TODO valor do serviço fica de fora (inclusive `CACHE`, cujo
        // valor original era o texto `${REDIS_URL}`) e o import os pede.
        assert_eq!(rep.missing_service_vars, vec!["CACHE", "OWN"]);
        assert_eq!(rep.missing_project_vars, vec!["REDIS_URL"]);

        r.dry_run = false;
        assert_eq!(err_code(handle_resp(&b, r.clone()).await), "MissingVars");
        assert!(
            crate::db::services::list(&b.db, &pid)
                .await
                .unwrap()
                .is_empty(),
            "faltando valor, nada é criado"
        );

        r.vars.insert("CACHE".into(), "${REDIS_URL}".into());
        r.vars.insert("OWN".into(), "mine".into());
        r.project_vars
            .insert("REDIS_URL".into(), "redis://novo:6379".into());
        let rep = report(handle_resp(&b, r).await);
        let svc = crate::db::services::get(&b.db, rep.service_id.as_ref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(
            svc.spec
                .env_vars
                .iter()
                .any(|e| e.key == "OWN" && e.value == EnvVarValue::Plain("mine".into()))
        );
    }

    #[tokio::test]
    async fn origem_por_zip_nao_exporta() {
        let a = daemon().await;
        let p = crate::db::projects::create(&a.db, "Z".into(), None)
            .await
            .unwrap();
        let mut s = spec(&p.id);
        s.source = ServiceSource::Archive(shared::ArchiveSource::default());
        let svc = crate::db::services::create(&a.db, s).await.unwrap();

        let Response::ServiceExportPlan(plan) =
            super::super::service_export::plan(a.clone(), svc.id.clone()).await
        else {
            panic!("plano")
        };
        assert!(plan.blocked.is_some());
        assert_eq!(
            err_code(service_export_export(&a, &svc.id, true, &[]).await),
            "NotExportable"
        );
    }

    #[tokio::test]
    async fn chave_de_projeto_desconhecida_e_erro() {
        let (a, id) = server_a().await;
        assert_eq!(
            err_code(service_export_export(&a, &id, true, &["NAO_EXISTE"]).await),
            "UnknownProjectVar"
        );
    }

    #[tokio::test]
    async fn pacote_invalido_e_sem_destino_sao_recusados() {
        let b = daemon().await;
        let pid = project_b(&b).await;
        assert_eq!(
            err_code(handle_resp(&b, req("isto: nao é pacote", &pid)).await),
            "InvalidBundle"
        );
        // Projeto de destino que não existe (tela desatualizada): erro claro.
        let (a, id) = server_a().await;
        let yaml = export(&a, &id, true, &[]).await;
        assert_eq!(
            err_code(handle_resp(&b, req(&yaml, "prj_nao_existe")).await),
            "NotFound"
        );
    }

    #[tokio::test]
    async fn dominios_podem_ficar_de_fora() {
        let (a, id) = server_a().await;
        let yaml = export(&a, &id, true, &[]).await;
        let b = daemon().await;
        let pid = project_b(&b).await;
        let mut r = req(&yaml, &pid);
        r.drop_domains = true;
        let rep = report(handle_resp(&b, r).await);
        let svc = crate::db::services::get(&b.db, rep.service_id.as_ref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(svc.spec.domains.is_empty() && svc.spec.domain.is_none());
        assert!(rep.warnings.iter().any(|w| w.code == "domains_dropped"));
    }

    async fn handle_resp(b: &AppState, r: ServiceImportReq) -> Response {
        super::handle(b.clone(), r).await
    }
}
