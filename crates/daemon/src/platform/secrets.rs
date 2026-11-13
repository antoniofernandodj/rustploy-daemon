//! `SecretsManager`: cifra e decifra os segredos do daemon.
//!
//! **Formato atual:** `ENC[age:<base64>]` — um arquivo `age` cifrado para a **identidade X25519**
//! do servidor (`[secrets] age_identity` no `config.toml` ou o arquivo `age_key_path`) e, se houver, também para a
//! `recovery_recipient` (chave pública guardada fora do servidor). Plano:
//! `docs/plano-backup-estado-em-git.md`, R5.

use age::{secrecy::ExposeSecret, x25519};
use anyhow::{Context, Result, anyhow, bail};
use std::{io::Write, path::Path, sync::Arc};
use tracing::{info, warn};
use ulid::Ulid;

use crate::platform::config::SecretsConfig;
use crate::store::Db;
use crate::store::infra::model::{InfraState, JobInfra, ProjectInfra, SecretInfra, ServiceInfra};
use shared::{EnvVar, EnvVarValue};

use crate::platform::age_format;
pub use crate::platform::age_format::is_enc;

pub struct SecretsManager {
    identity: x25519::Identity,
    /// A chave **pública** de recuperação. Muda em tempo de execução só pela recuperação de um servidor
    /// (`begin_recovery`), que também a grava no `config.toml`.
    recovery: std::sync::RwLock<Option<x25519::Recipient>>,
    /// A identidade **privada** de recuperação, **só na memória**, enquanto uma recuperação está em curso
    /// (`begin_recovery` … `end_recovery`): serve só para abrir o que foi cifrado para o servidor antigo.
    /// Nunca é gravada em disco nem entra em log.
    transient: std::sync::RwLock<Option<x25519::Identity>>,
    /// Onde gravar `recovery_recipient` (o `config.toml` em uso), se houver.
    config_path: Option<std::path::PathBuf>,
    db: Arc<Db>,
}

impl SecretsManager {
    /// Abre o gerenciador do daemon. A identidade vem, nesta ordem, de `[secrets] age_identity` e
    /// do arquivo `key_file` (`age_key_path`); se nenhuma existe, **gera uma** e a grava no
    /// `config.toml` (ou, se ele não for gravável, no `key_file`).
    ///
    /// Recusa subir se precisaria gerar uma identidade nova mas já existem segredos `ENC[age:…]`
    /// gravados: seria órfã-los em silêncio.
    pub fn open(
        key_file: &Path,
        db: Arc<Db>,
        cfg: &SecretsConfig,
        config_path: Option<&Path>,
    ) -> Result<Self> {
        let identity = match cfg.age_identity.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(text) => parse_identity(text).context("[secrets] age_identity inválida")?,
            None if key_file.exists() => parse_identity(&std::fs::read_to_string(key_file)?)
                .with_context(|| format!("{} inválido", key_file.display()))?,
            None => {
                if has_enc_values(&db) {
                    bail!(
                        "há segredos cifrados (ENC[age:…]) mas nenhuma identidade age foi encontrada \
                         (nem em [secrets] age_identity, nem em {}). Restaure a identidade; gerar uma \
                         nova tornaria esses segredos ilegíveis.",
                        key_file.display()
                    );
                }
                let fresh = x25519::Identity::generate();
                persist_identity(&fresh, config_path, key_file)?;
                fresh
            }
        };
        let recovery = cfg
            .recovery_recipient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                age_format::parse_recipient(s)
                    .map_err(|e| anyhow!("[secrets] recovery_recipient inválida: {e}"))
            })
            .transpose()?;
        Ok(Self {
            identity,
            recovery: std::sync::RwLock::new(recovery),
            transient: std::sync::RwLock::new(None),
            config_path: config_path.map(Path::to_path_buf),
            db,
        })
    }

    /// Para testes: identidade efêmera (some quando o objeto some), sem tocar na config.
    #[cfg(test)]
    pub fn new(db: Arc<Db>) -> Result<Self> {
        Ok(Self {
            identity: x25519::Identity::generate(),
            recovery: std::sync::RwLock::new(None),
            transient: std::sync::RwLock::new(None),
            config_path: None,
            db,
        })
    }

    /// Para testes: com uma identidade e uma recuperação escolhidas.
    #[cfg(test)]
    pub fn with_keys(
        db: Arc<Db>,
        identity: x25519::Identity,
        recovery: Option<x25519::Recipient>,
    ) -> Result<Self> {
        Ok(Self {
            identity,
            recovery: std::sync::RwLock::new(recovery),
            transient: std::sync::RwLock::new(None),
            config_path: None,
            db,
        })
    }

    /// Há uma chave de recuperação (`[secrets] recovery_recipient`) configurada?
    pub fn has_recovery(&self) -> bool {
        self.recovery().is_some()
    }

    fn recovery(&self) -> Option<x25519::Recipient> {
        self.recovery
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A chave pública do servidor (`age1…`).
    pub fn public_key(&self) -> String {
        self.identity.to_public().to_string()
    }

    /// Cifra para a identidade do servidor (e a de recuperação, se houver).
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut recipients = vec![self.identity.to_public()];
        recipients.extend(self.recovery());
        age_format::encrypt(plaintext, &recipients)
    }

    /// Cifra um arquivo para a identidade do servidor (e a de recuperação, se houver).
    pub fn encrypt_file(&self, src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
        let mut recipients = vec![self.identity.to_public()];
        recipients.extend(self.recovery());
        age_format::encrypt_file(src, dst, &recipients)
    }

    /// Decifra um arquivo age com a identidade deste servidor (ou, durante uma recuperação, com a
    /// identidade de recuperação em memória).
    pub fn decrypt_file(&self, src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
        match age_format::decrypt_file(src, dst, &self.identity) {
            Ok(()) => Ok(()),
            Err(own) => match self
                .transient
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                Some(t) => age_format::decrypt_file(src, dst, t).map_err(|_| own),
                None => Err(own),
            },
        }
    }

    /// Este servidor (ou, durante uma recuperação, a chave de recuperação em memória) abre o arquivo age
    /// cujo início é `head`? Só olha o cabeçalho (ver `age_format::head_opens`).
    pub fn can_open_file_head(&self, head: &[u8]) -> Result<bool> {
        if age_format::head_opens(head, &self.identity)? {
            return Ok(true);
        }
        match self
            .transient
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(t) => age_format::head_opens(head, t),
            None => Ok(false),
        }
    }

    /// Decifra um valor `ENC[age:…]` com a identidade deste servidor (ou, durante uma recuperação, com a
    /// de recuperação em memória).
    pub fn decrypt(&self, value: &str) -> Result<String> {
        if !is_enc(value) {
            bail!("valor não está no formato ENC[age:…]");
        }
        match age_format::decrypt(value, &self.identity) {
            Ok(v) => Ok(v),
            Err(own) => match self
                .transient
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                Some(t) => age_format::decrypt(value, t).map_err(|_| own),
                None => Err(own).context("com a identidade deste servidor"),
            },
        }
    }

    // ── recuperação de um servidor (docs/plano-backup-de-dados-para-s3.md, Fase 4) ──

    /// Começa uma recuperação: guarda a identidade **privada** de recuperação só na memória (para abrir
    /// o que foi cifrado para o servidor antigo) e passa a cifrar também para a chave **pública** dela,
    /// que é gravada no `config.toml` (só a pública). Devolve a chave pública (`age1…`).
    pub fn begin_recovery(&self, secret_key: &str) -> Result<String> {
        let identity = parse_identity(secret_key.trim()).context(
            "a chave de recuperação não é uma chave privada age válida (AGE-SECRET-KEY-1…)",
        )?;
        let public = identity.to_public();
        if let Some(path) = &self.config_path
            && let Err(e) =
                set_secrets_key_in_config(path, "recovery_recipient", &public.to_string())
        {
            // sem gravar, o servidor perderia a chave pública no próximo boot: não vale seguir em silêncio
            bail!("não consegui gravar recovery_recipient no config.toml: {e:#}");
        }
        *self.recovery.write().unwrap_or_else(|e| e.into_inner()) = Some(public.clone());
        *self.transient.write().unwrap_or_else(|e| e.into_inner()) = Some(identity);
        Ok(public.to_string())
    }

    /// **Gera** a chave de recuperação (`docs/plano-chave-de-recuperacao.md`): grava só a pública em
    /// `[secrets] recovery_recipient` e devolve `(pública, privada)`. A privada **não** é guardada
    /// (nem em disco, nem na memória): quem chama a entrega à pessoa na hora. Havendo chave, recusa,
    /// a não ser com `replace`. Sem `config.toml` não há onde gravar a pública — recusa também, senão
    /// o servidor passaria a cifrar para uma chave que esqueceria no próximo boot.
    pub fn generate_recovery(&self, replace: bool) -> Result<(String, String)> {
        if self.has_recovery() && !replace {
            bail!("já existe uma chave de recuperação; confirme a troca para gerar outra");
        }
        let Some(path) = &self.config_path else {
            bail!("este servidor não tem config.toml para gravar a chave pública de recuperação");
        };
        let identity = x25519::Identity::generate();
        let public = identity.to_public();
        set_secrets_key_in_config(path, "recovery_recipient", &public.to_string())
            .map_err(|e| anyhow!("não consegui gravar recovery_recipient no config.toml: {e:#}"))?;
        *self.recovery.write().unwrap_or_else(|e| e.into_inner()) = Some(public.clone());
        Ok((
            public.to_string(),
            identity.to_string().expose_secret().to_string(),
        ))
    }

    /// A chave **pública** de recuperação configurada (`age1…`), se houver.
    pub fn recovery_public_key(&self) -> Option<String> {
        self.recovery().map(|r| r.to_string())
    }

    /// Há uma recuperação em curso (a identidade de recuperação está na memória)?
    pub fn recovery_in_progress(&self) -> bool {
        self.transient
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Termina a recuperação: descarta a identidade privada de recuperação da memória.
    pub fn end_recovery(&self) {
        *self.transient.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Recifra um valor `ENC[age:…]` para a identidade **deste** servidor e a chave pública de
    /// recuperação (abre com a identidade do servidor ou com a de recuperação em memória).
    pub fn rekey(&self, value: &str) -> Result<String> {
        let plain = self.decrypt(value)?;
        self.encrypt(&plain)
    }

    // Os secrets (já cifrados) moram no YAML de infra: os de projeto em `ProjectInfra.secrets`, os de
    // serviço em `ServiceInfra.secrets`. A tabela antiga `secret` não é mais lida nem escrita.
    // Plano: `docs/plano-backup-estado-em-git.md`, Fases 4d e 5.

    /// Secret do **projeto**.
    pub async fn get_raw(&self, project_id: &str, name: &str) -> Result<String> {
        self.get_scoped(project_id, None, name).await
    }

    /// Secret de um escopo: serviço = só daquele serviço; job = só daquele job; projeto = só do
    /// projeto. (`Option<&str>` vira `Scope` — `Some` é serviço —, então quem só fala de serviço
    /// continua passando `service_id` direto.)
    pub async fn get_scoped<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        name: &str,
    ) -> Result<String> {
        let scope = scope.into();
        let st = self.db.infra().snapshot();
        let project = st.projects.iter().find(|p| p.id == project_id);
        let found = match scope {
            Scope::Project => project.and_then(|p| p.secrets.iter().find(|s| s.key == name)),
            Scope::Service(sid) => project
                .and_then(|p| p.services.iter().find(|s| s.id == sid))
                .and_then(|s| s.secrets.iter().find(|x| x.key == name)),
            Scope::Job(jid) => project
                .and_then(|p| p.jobs.iter().find(|j| j.id == jid))
                .and_then(|j| j.secrets.iter().find(|x| x.key == name)),
        };
        let encrypted = found.map(|s| s.value.clone()).ok_or_else(|| match scope {
            Scope::Project => anyhow!("secret '{name}' not found in project '{project_id}'"),
            Scope::Service(sid) => anyhow!("secret '{name}' not found in service '{sid}'"),
            Scope::Job(jid) => anyhow!("secret '{name}' not found in job '{jid}'"),
        })?;
        self.decrypt(&encrypted)
    }

    /// Resolução de uma env `Secret(name)` **de um job**: o secret do **job** primeiro, depois o do
    /// serviço gatilho (se houver), depois o do projeto.
    pub async fn resolve_for_job(
        &self,
        project_id: &str,
        job_id: &str,
        trigger_service_id: Option<&str>,
        name: &str,
    ) -> Result<String> {
        if let Ok(v) = self.get_scoped(project_id, Scope::Job(job_id), name).await {
            return Ok(v);
        }
        self.resolve(project_id, trigger_service_id, name).await
    }

    /// Resolução de uma env `Secret(name)`: o escopo do **serviço** primeiro, depois o do projeto.
    /// Sem serviço (env de projeto, job autônomo), só o do projeto.
    pub async fn resolve(
        &self,
        project_id: &str,
        service_id: Option<&str>,
        name: &str,
    ) -> Result<String> {
        if let Some(sid) = service_id
            && let Ok(v) = self.get_scoped(project_id, Some(sid), name).await
        {
            return Ok(v);
        }
        self.get_scoped(project_id, None, name).await
    }

    pub async fn set(&self, project_id: &str, name: &str, value: &str) -> Result<()> {
        self.set_scoped(project_id, None, name, value).await
    }

    pub async fn set_scoped<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        name: &str,
        value: &str,
    ) -> Result<()> {
        let scope = scope.into();
        let encrypted = self.encrypt(value)?;
        self.db.infra().update(|st| {
            let list = secret_list_mut(st, project_id, scope)?;
            match list.iter_mut().find(|s| s.key == name) {
                Some(s) => s.value = encrypted,
                None => list.push(new_secret(name, encrypted)),
            }
            Ok(())
        })
    }

    pub async fn delete_scoped<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        name: &str,
    ) -> Result<()> {
        let scope = scope.into();
        self.db.infra().update(|st| {
            // Apagar de um escopo que não existe é inofensivo (igual ao SQL de antes).
            if let Ok(list) = secret_list_mut(st, project_id, scope) {
                list.retain(|s| s.key != name);
            }
            Ok(())
        })
    }

    pub async fn list_names(&self, project_id: &str) -> Result<Vec<String>> {
        self.list_names_scoped(project_id, None).await
    }

    pub async fn list_names_scoped<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
    ) -> Result<Vec<String>> {
        let st = self.db.infra().snapshot();
        let project = st.projects.iter().find(|p| p.id == project_id);
        let mut names: Vec<String> = match scope.into() {
            Scope::Project => project
                .map(|p| p.secrets.iter().map(|s| s.key.clone()).collect())
                .unwrap_or_default(),
            Scope::Service(sid) => project
                .and_then(|p| p.services.iter().find(|s| s.id == sid))
                .map(|s| s.secrets.iter().map(|x| x.key.clone()).collect())
                .unwrap_or_default(),
            Scope::Job(jid) => project
                .and_then(|p| p.jobs.iter().find(|j| j.id == jid))
                .map(|j| j.secrets.iter().map(|x| x.key.clone()).collect())
                .unwrap_or_default(),
        };
        names.sort();
        Ok(names)
    }

    /// "Ver valor": decifra e **grava uma linha de auditoria** (quem/qual/quando — nunca o valor).
    pub async fn reveal<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        name: &str,
    ) -> Result<String> {
        let scope = scope.into();
        // A tabela de auditoria só tem a coluna `service_id`: o job entra como `job:<id>`.
        let service_id: Option<String> = match scope {
            Scope::Project => None,
            Scope::Service(sid) => Some(sid.to_string()),
            Scope::Job(jid) => Some(format!("job:{jid}")),
        };
        let service_id = service_id.as_deref();
        let value = self.get_scoped(project_id, scope, name).await?;
        // A auditoria é parte do contrato: se não der para gravar, o valor não sai.
        sqlx::query(
            "INSERT INTO secret_audit (at, action, project_id, service_id, name) VALUES (?, 'reveal', ?, ?, ?)",
        )
        .bind(chrono::Utc::now())
        .bind(project_id)
        .bind(service_id)
        .bind(name)
        .execute(self.db.pool())
        .await
        .context("gravando a auditoria da revelação")?;
        info!(target: "audit", project_id, service_id = ?service_id, secret = name, "secret revelado");
        Ok(value)
    }

    /// As quatro movimentações manuais da R5, cada uma **atômica** (uma transação do repositório):
    /// ou o secret e a env mudam juntos, ou nada muda. Falha se o destino já tem o nome.
    pub async fn move_secret<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        name: &str,
        op: shared::SecretMoveOp,
    ) -> Result<()> {
        use shared::SecretMoveOp::*;
        let scope = scope.into();
        // Os valores são decifrados/cifrados fora da closure só quando a operação precisa.
        self.db.infra().update(|st| {
            let p = project_mut(st, project_id)?;
            match (op, scope) {
                (EnvToSecret, Scope::Project) => {
                    let value = take_plain_env(&mut p.env_vars, name, "o projeto")?;
                    if p.secrets.iter().any(|s| s.key == name) {
                        bail!("o projeto já tem um secret chamado '{name}'");
                    }
                    p.secrets.push(new_secret(name, self.encrypt(&value)?));
                    set_env(&mut p.env_vars, name, EnvVarValue::Secret(name.to_string()));
                }
                (EnvToSecret, Scope::Service(sid)) => {
                    let svc = service_mut(p, sid)?;
                    let value = take_plain_env(&mut svc.spec.env_vars, name, "o serviço")?;
                    if svc.secrets.iter().any(|s| s.key == name) {
                        bail!("o serviço já tem um secret chamado '{name}'");
                    }
                    svc.secrets.push(new_secret(name, self.encrypt(&value)?));
                    set_env(
                        &mut svc.spec.env_vars,
                        name,
                        EnvVarValue::Secret(name.to_string()),
                    );
                }
                (SecretToEnv, Scope::Project) => {
                    let pos = p
                        .secrets
                        .iter()
                        .position(|s| s.key == name)
                        .ok_or_else(|| anyhow!("o projeto não tem um secret chamado '{name}'"))?;
                    let plain = self.decrypt(&p.secrets[pos].value)?;
                    secret_into_env(&mut p.env_vars, name, plain, "o projeto")?;
                    p.secrets.remove(pos);
                }
                (SecretToEnv, Scope::Service(sid)) => {
                    let svc = service_mut(p, sid)?;
                    let pos = svc
                        .secrets
                        .iter()
                        .position(|s| s.key == name)
                        .ok_or_else(|| anyhow!("o serviço não tem um secret chamado '{name}'"))?;
                    let plain = self.decrypt(&svc.secrets[pos].value)?;
                    secret_into_env(&mut svc.spec.env_vars, name, plain, "o serviço")?;
                    svc.secrets.remove(pos);
                }
                (ServiceSecretToProject, Scope::Service(sid)) => {
                    let ProjectInfra {
                        services, secrets, ..
                    } = p;
                    let svc = services
                        .iter_mut()
                        .find(|s| s.id == sid)
                        .ok_or_else(|| anyhow!("serviço {sid} não pertence ao projeto"))?;
                    let pos = svc
                        .secrets
                        .iter()
                        .position(|s| s.key == name)
                        .ok_or_else(|| anyhow!("o serviço não tem um secret chamado '{name}'"))?;
                    if secrets.iter().any(|s| s.key == name) {
                        bail!("o projeto já tem um secret chamado '{name}'");
                    }
                    secrets.push(svc.secrets.remove(pos));
                }
                (ProjectSecretToService, Scope::Service(sid)) => {
                    let ProjectInfra {
                        services, secrets, ..
                    } = p;
                    let svc = services
                        .iter_mut()
                        .find(|s| s.id == sid)
                        .ok_or_else(|| anyhow!("serviço {sid} não pertence ao projeto"))?;
                    let pos = secrets
                        .iter()
                        .position(|s| s.key == name)
                        .ok_or_else(|| anyhow!("o projeto não tem um secret chamado '{name}'"))?;
                    if svc.secrets.iter().any(|s| s.key == name) {
                        bail!("o serviço já tem um secret chamado '{name}'");
                    }
                    svc.secrets.push(secrets.remove(pos));
                }
                (EnvServiceToProject, Scope::Service(sid)) => {
                    let ProjectInfra {
                        services, env_vars, ..
                    } = p;
                    let svc = services
                        .iter_mut()
                        .find(|s| s.id == sid)
                        .ok_or_else(|| anyhow!("serviço {sid} não pertence ao projeto"))?;
                    let value = take_plain_env(&mut svc.spec.env_vars, name, "o serviço")?;
                    if env_vars.iter().any(|v| v.key == name) {
                        bail!("o projeto já tem uma env chamada '{name}'");
                    }
                    svc.spec.env_vars.retain(|v| v.key != name);
                    env_vars.push(EnvVar {
                        key: name.to_string(),
                        value: EnvVarValue::Plain(value),
                    });
                }
                (EnvProjectToService, Scope::Service(sid)) => {
                    let ProjectInfra {
                        services, env_vars, ..
                    } = p;
                    let svc = services
                        .iter_mut()
                        .find(|s| s.id == sid)
                        .ok_or_else(|| anyhow!("serviço {sid} não pertence ao projeto"))?;
                    let value = take_plain_env(env_vars, name, "o projeto")?;
                    if svc.spec.env_vars.iter().any(|v| v.key == name) {
                        bail!("o serviço já tem uma env chamada '{name}'");
                    }
                    env_vars.retain(|v| v.key != name);
                    svc.spec.env_vars.push(EnvVar {
                        key: name.to_string(),
                        value: EnvVarValue::Plain(value),
                    });
                }
                (EnvToSecret, Scope::Job(jid)) => {
                    let job = job_mut(p, jid)?;
                    let value = take_plain_env(&mut job.env_vars, name, "o job")?;
                    if job.secrets.iter().any(|s| s.key == name) {
                        bail!("o job já tem um secret chamado '{name}'");
                    }
                    job.secrets.push(new_secret(name, self.encrypt(&value)?));
                    set_env(
                        &mut job.env_vars,
                        name,
                        EnvVarValue::Secret(name.to_string()),
                    );
                }
                (SecretToEnv, Scope::Job(jid)) => {
                    let job = job_mut(p, jid)?;
                    let pos = job
                        .secrets
                        .iter()
                        .position(|s| s.key == name)
                        .ok_or_else(|| anyhow!("o job não tem um secret chamado '{name}'"))?;
                    let plain = self.decrypt(&job.secrets[pos].value)?;
                    secret_into_env(&mut job.env_vars, name, plain, "o job")?;
                    job.secrets.remove(pos);
                }
                (JobSecretToProject, Scope::Job(jid)) => {
                    let ProjectInfra { jobs, secrets, .. } = p;
                    let job = jobs
                        .iter_mut()
                        .find(|j| j.id == jid)
                        .ok_or_else(|| anyhow!("job {jid} não pertence ao projeto"))?;
                    let pos = job
                        .secrets
                        .iter()
                        .position(|s| s.key == name)
                        .ok_or_else(|| anyhow!("o job não tem um secret chamado '{name}'"))?;
                    if secrets.iter().any(|s| s.key == name) {
                        bail!("o projeto já tem um secret chamado '{name}'");
                    }
                    secrets.push(job.secrets.remove(pos));
                }
                (EnvJobToProject, Scope::Job(jid)) => {
                    let ProjectInfra { jobs, env_vars, .. } = p;
                    let job = jobs
                        .iter_mut()
                        .find(|j| j.id == jid)
                        .ok_or_else(|| anyhow!("job {jid} não pertence ao projeto"))?;
                    let value = take_plain_env(&mut job.env_vars, name, "o job")?;
                    if env_vars.iter().any(|v| v.key == name) {
                        bail!("o projeto já tem uma env chamada '{name}'");
                    }
                    job.env_vars.retain(|v| v.key != name);
                    env_vars.push(EnvVar {
                        key: name.to_string(),
                        value: EnvVarValue::Plain(value),
                    });
                }
                (
                    ServiceSecretToProject
                    | ProjectSecretToService
                    | EnvServiceToProject
                    | EnvProjectToService,
                    Scope::Project | Scope::Job(_),
                ) => {
                    bail!("esta operação precisa do id do serviço")
                }
                (JobSecretToProject | EnvJobToProject, Scope::Project | Scope::Service(_)) => {
                    bail!("esta operação precisa do id do job")
                }
            }
            Ok(())
        })
    }

    /// Renomeia a **chave** de uma env do projeto (`service_id: None`) ou de um serviço, sem tocar
    /// no valor (`Plain` ou `Secret`) — ele nunca passa pelo chamador. Os comentários do editor
    /// `.env` ancorados na chave antiga acompanham. Atômico; falha se `from` não existe ou `to` já
    /// existe no escopo.
    pub async fn rename_env_key<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        from: &str,
        to: &str,
    ) -> Result<()> {
        valid_env_key(to)?;
        let scope = scope.into();
        self.db.infra().update(|st| {
            let p = project_mut(st, project_id)?;
            let (vars, comments, onde) = match scope {
                Scope::Project => (&mut p.env_vars, &mut p.env_comments, "o projeto"),
                Scope::Service(sid) => {
                    let svc = service_mut(p, sid)?;
                    (
                        &mut svc.spec.env_vars,
                        &mut svc.spec.env_comments,
                        "o serviço",
                    )
                }
                Scope::Job(jid) => {
                    let job = job_mut(p, jid)?;
                    (&mut job.env_vars, &mut job.env_comments, "o job")
                }
            };
            if from == to {
                bail!("o nome novo é igual ao atual ('{from}')");
            }
            if vars.iter().any(|v| v.key == to) {
                bail!("{onde} já tem uma env chamada '{to}'");
            }
            let var = vars
                .iter_mut()
                .find(|v| v.key == from)
                .ok_or_else(|| anyhow!("{onde} não tem uma env chamada '{from}'"))?;
            var.key = to.to_string();
            for c in comments.iter_mut() {
                if c.before_key.as_deref() == Some(from) {
                    c.before_key = Some(to.to_string());
                }
            }
            Ok(())
        })
    }

    /// Troca só o **valor** de uma env do projeto (`service_id: None`) ou de um serviço, na mesma
    /// posição da lista — a edição "no lugar" da tela, sem reenviar a lista inteira.
    ///
    /// Um `Secret(nome)` precisa existir no escopo da env: num serviço vale o secret dele **ou** o
    /// do projeto (a resolução é "serviço primeiro, depois projeto", ver [`Self::resolve`]); numa
    /// env do projeto, só o do projeto. A máscara `***` (o que uma tela mostra no lugar de um
    /// segredo) é recusada: gravá-la apagaria o valor real sem aviso.
    pub async fn set_env_value<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        key: &str,
        value: EnvVarValue,
    ) -> Result<()> {
        let scope = scope.into();
        match &value {
            EnvVarValue::Plain(v) if v == "***" => {
                bail!("o valor `***` é a máscara de um segredo, não um valor: nada foi gravado")
            }
            EnvVarValue::Secret(name) => valid_secret_name(name)?,
            EnvVarValue::Plain(_) => {}
        }
        self.db.infra().update(|st| {
            let p = project_mut(st, project_id)?;
            if let EnvVarValue::Secret(name) = &value {
                let no_projeto = p.secrets.iter().any(|s| &s.key == name);
                let existe = match scope {
                    Scope::Project => no_projeto,
                    Scope::Service(sid) => {
                        no_projeto || service_mut(p, sid)?.secrets.iter().any(|s| &s.key == name)
                    }
                    // job: o dele, o do serviço gatilho ou o do projeto (a ordem da resolução)
                    Scope::Job(jid) => {
                        let job = job_mut(p, jid)?;
                        let no_job = job.secrets.iter().any(|s| &s.key == name);
                        let gatilho = job.trigger_service_id.clone();
                        let no_gatilho = gatilho.is_some_and(|tid| {
                            p.services
                                .iter()
                                .any(|s| s.id == tid && s.secrets.iter().any(|x| &x.key == name))
                        });
                        no_job || no_gatilho || no_projeto
                    }
                };
                if !existe {
                    bail!("não existe um secret chamado '{name}' neste escopo");
                }
            }
            let (vars, onde) = match scope {
                Scope::Project => (&mut p.env_vars, "o projeto"),
                Scope::Service(sid) => (&mut service_mut(p, sid)?.spec.env_vars, "o serviço"),
                Scope::Job(jid) => (&mut job_mut(p, jid)?.env_vars, "o job"),
            };
            let var = vars
                .iter_mut()
                .find(|v| v.key == key)
                .ok_or_else(|| anyhow!("{onde} não tem uma env chamada '{key}'"))?;
            var.value = value;
            Ok(())
        })
    }

    /// Renomeia um **secret** (do projeto, ou de um serviço) e, na mesma transação, reaponta as
    /// envs `Secret(from)` que resolviam para ele. O valor cifrado não é lido nem reescrito.
    ///
    /// A resolução é "serviço primeiro, depois projeto" ([`Self::resolve`]), então renomear um
    /// secret do **projeto** não mexe nos serviços (nem nos jobs deles) que têm um secret próprio
    /// com o mesmo nome: nesses, `Secret(from)` aponta para o do serviço e continua valendo.
    pub async fn rename_secret<'a>(
        &self,
        project_id: &str,
        scope: impl Into<Scope<'a>>,
        from: &str,
        to: &str,
    ) -> Result<()> {
        valid_secret_name(to)?;
        let scope = scope.into();
        self.db.infra().update(|st| {
            let p = project_mut(st, project_id)?;
            if from == to {
                bail!("o nome novo é igual ao atual ('{from}')");
            }
            match scope {
                Scope::Job(jid) => {
                    let job = job_mut(p, jid)?;
                    rename_in_list(&mut job.secrets, from, to, "o job")?;
                    repoint_secret_refs(&mut job.env_vars, from, to);
                }
                Scope::Service(sid) => {
                    let svc = service_mut(p, sid)?;
                    rename_in_list(&mut svc.secrets, from, to, "o serviço")?;
                    repoint_secret_refs(&mut svc.spec.env_vars, from, to);
                }
                Scope::Project => {
                    let ProjectInfra {
                        env_vars,
                        services,
                        jobs,
                        secrets,
                        ..
                    } = p;
                    rename_in_list(secrets, from, to, "o projeto")?;
                    repoint_secret_refs(env_vars, from, to);
                    for svc in services.iter_mut() {
                        if !svc.secrets.iter().any(|s| s.key == from) {
                            repoint_secret_refs(&mut svc.spec.env_vars, from, to);
                        }
                    }
                    for job in jobs.iter_mut() {
                        let sombreado = job.secrets.iter().any(|x| x.key == from)
                            || job.trigger_service_id.as_deref().is_some_and(|tid| {
                                services
                                    .iter()
                                    .any(|s| s.id == tid && s.secrets.iter().any(|x| x.key == from))
                            });
                        if !sombreado {
                            repoint_secret_refs(&mut job.env_vars, from, to);
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

/// Nome válido para a chave de uma env: `[A-Za-z_][A-Za-z0-9_]*` (o que o Docker e o `.env` aceitam).
fn valid_env_key(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!(
            "'{name}' não é um nome de variável válido (use letras, números e _, sem começar por número)"
        );
    }
    Ok(())
}

/// Nome válido para um secret: não vazio, só `[A-Za-z0-9_.-]` — fica de fora o que quebraria a
/// referência `<secret:NOME>` (espaço, `:`, `<`, `>`).
fn valid_secret_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        bail!("'{name}' não é um nome de secret válido (use letras, números, _, . e -)");
    }
    Ok(())
}

/// Renomeia o secret `from` para `to` na lista, recusando se `from` não existe ou `to` já existe.
fn rename_in_list(list: &mut [SecretInfra], from: &str, to: &str, onde: &str) -> Result<()> {
    if list.iter().any(|s| s.key == to) {
        bail!("{onde} já tem um secret chamado '{to}'");
    }
    let s = list
        .iter_mut()
        .find(|s| s.key == from)
        .ok_or_else(|| anyhow!("{onde} não tem um secret chamado '{from}'"))?;
    s.key = to.to_string();
    Ok(())
}

/// Reaponta toda env `Secret(from)` para `Secret(to)`.
fn repoint_secret_refs(vars: &mut [EnvVar], from: &str, to: &str) {
    for v in vars.iter_mut() {
        if v.value == EnvVarValue::Secret(from.to_string()) {
            v.value = EnvVarValue::Secret(to.to_string());
        }
    }
}

fn new_secret(name: &str, encrypted: String) -> SecretInfra {
    SecretInfra {
        id: format!("sec_{}", Ulid::new()),
        key: name.to_string(),
        value: encrypted,
    }
}

fn project_mut<'a>(st: &'a mut InfraState, project_id: &str) -> Result<&'a mut ProjectInfra> {
    st.projects
        .iter_mut()
        .find(|p| p.id == project_id)
        .ok_or_else(|| anyhow!("projeto {project_id} não encontrado"))
}

fn service_mut<'a>(p: &'a mut ProjectInfra, service_id: &str) -> Result<&'a mut ServiceInfra> {
    p.services
        .iter_mut()
        .find(|s| s.id == service_id)
        .ok_or_else(|| anyhow!("serviço {service_id} não pertence ao projeto"))
}

fn job_mut<'a>(p: &'a mut ProjectInfra, job_id: &str) -> Result<&'a mut JobInfra> {
    p.jobs
        .iter_mut()
        .find(|j| j.id == job_id)
        .ok_or_else(|| anyhow!("job {job_id} não pertence ao projeto"))
}

/// A lista de secrets do escopo pedido (projeto, um serviço ou um job dele).
fn secret_list_mut<'a>(
    st: &'a mut InfraState,
    project_id: &str,
    scope: Scope<'_>,
) -> Result<&'a mut Vec<SecretInfra>> {
    let p = project_mut(st, project_id)?;
    match scope {
        Scope::Project => Ok(&mut p.secrets),
        Scope::Service(sid) => Ok(&mut service_mut(p, sid)?.secrets),
        Scope::Job(jid) => Ok(&mut job_mut(p, jid)?.secrets),
    }
}

/// De quem é um secret/env: do projeto, de um serviço ou de um job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Project,
    Service(&'a str),
    Job(&'a str),
}

impl<'a> From<Option<&'a str>> for Scope<'a> {
    fn from(service_id: Option<&'a str>) -> Self {
        service_id.map_or(Scope::Project, Scope::Service)
    }
}

impl<'a> Scope<'a> {
    /// O escopo pedido por um `Command` (`service_id` e `job_id` são exclusivos).
    pub fn from_ids(service_id: Option<&'a str>, job_id: Option<&'a str>) -> Result<Self> {
        match (service_id, job_id) {
            (Some(_), Some(_)) => bail!("informe service_id ou job_id, não os dois"),
            (_, Some(jid)) => Ok(Scope::Job(jid)),
            (sid, None) => Ok(sid.into()),
        }
    }
}

/// Tira o valor `Plain` da env `key` (erro claro se não existe ou já é um secret).
fn take_plain_env(vars: &mut [EnvVar], key: &str, onde: &str) -> Result<String> {
    match vars.iter().find(|v| v.key == key) {
        None => bail!("{onde} não tem uma env chamada '{key}'"),
        Some(EnvVar {
            value: EnvVarValue::Secret(_),
            ..
        }) => bail!("a env '{key}' já aponta para um secret"),
        Some(EnvVar {
            value: EnvVarValue::Plain(v),
            ..
        }) => Ok(v.clone()),
    }
}

fn set_env(vars: &mut [EnvVar], key: &str, value: EnvVarValue) {
    if let Some(v) = vars.iter_mut().find(|v| v.key == key) {
        v.value = value;
    }
}

/// Secret → env: as envs que apontavam para o secret viram `Plain(valor)`; se nenhuma apontava,
/// cria-se uma env com chave = nome do secret (e falha se essa chave já existir com outro valor).
fn secret_into_env(vars: &mut Vec<EnvVar>, name: &str, plain: String, onde: &str) -> Result<()> {
    let mut pointed = 0;
    for v in vars.iter_mut() {
        if v.value == EnvVarValue::Secret(name.to_string()) {
            v.value = EnvVarValue::Plain(plain.clone());
            pointed += 1;
        }
    }
    if pointed == 0 {
        if vars.iter().any(|v| v.key == name) {
            bail!("{onde} já tem uma env chamada '{name}'");
        }
        vars.push(EnvVar {
            key: name.to_string(),
            value: EnvVarValue::Plain(plain),
        });
    }
    Ok(())
}

/// Grava `contents` num arquivo que já nasce 0600 (sem janela em que ele exista legível).
fn write_private(path: &Path, contents: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
        Ok(())
    }
}

fn parse_identity(text: &str) -> Result<x25519::Identity> {
    age_format::parse_identity(text)
}

/// Há algum segredo já no formato novo gravado no YAML?
fn has_enc_values(db: &Db) -> bool {
    let st = db.infra().snapshot();
    st.projects
        .iter()
        .flat_map(|p| p.secrets.iter())
        .any(|s| is_enc(&s.value))
        || st
            .providers
            .iter()
            .filter_map(|p| p.oauth_client_secret_enc.as_deref())
            .any(is_enc)
}

/// Grava a identidade recém-gerada no `config.toml` (seção `[secrets]`); se não der, no
/// `age.key` (0600). Nunca deixa a identidade só em memória.
fn persist_identity(
    identity: &x25519::Identity,
    config_path: Option<&Path>,
    key_file: &Path,
) -> Result<()> {
    let secret = identity.to_string();
    let line = format!("age_identity = \"{}\"", secret.expose_secret());
    if let Some(path) = config_path {
        match write_identity_into_config(path, &line) {
            Ok(()) => {
                info!(config = %path.display(), public_key = %identity.to_public(), "identidade age gerada e gravada no config.toml");
                return Ok(());
            }
            Err(e) => {
                warn!(config = %path.display(), error = %format!("{e:#}"), "não consegui gravar a identidade no config.toml; usando {}", key_file.display())
            }
        }
    }
    write_private(key_file, secret.expose_secret())?;
    info!(file = %key_file.display(), public_key = %identity.to_public(), "identidade age gerada e gravada em arquivo");
    Ok(())
}

/// Insere `line` logo abaixo de `[secrets]` (ou cria a seção no fim), preservando o resto do
/// arquivo e os comentários, e troca o arquivo por rename (0600).
fn write_identity_into_config(path: &Path, line: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut out = String::with_capacity(text.len() + line.len() + 64);
    let mut inserted = false;
    for l in text.lines() {
        out.push_str(l);
        out.push('\n');
        if !inserted && l.trim() == "[secrets]" {
            out.push_str(line);
            out.push('\n');
            inserted = true;
        }
    }
    if !inserted {
        out.push_str("\n[secrets]\n");
        out.push_str(line);
        out.push('\n');
    }
    // O resultado tem de continuar sendo uma config válida, senão o próximo boot cairia nos defaults.
    toml::from_str::<crate::platform::config::RustployConfig>(&out)
        .context("a config resultante não seria válida")?;
    let tmp = path.with_extension("toml.tmp");
    write_private(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Define `key = "value"` em `[secrets]` do `config.toml` (troca a linha se já existe, senão insere logo
/// abaixo do cabeçalho), preservando o resto e os comentários; troca o arquivo por rename (0600).
fn set_secrets_key_in_config(path: &Path, key: &str, value: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let new_line = format!("{key} = \"{value}\"");
    let mut out = String::with_capacity(text.len() + new_line.len() + 64);
    let (mut in_secrets, mut done) = (false, false);
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            in_secrets = t == "[secrets]";
        }
        let is_key = in_secrets && t.split('=').next().map(str::trim) == Some(key);
        if is_key && !done {
            out.push_str(&new_line);
            done = true;
        } else if !is_key {
            out.push_str(l);
        } else {
            continue; // linha repetida da mesma chave
        }
        out.push('\n');
        if !done && t == "[secrets]" {
            out.push_str(&new_line);
            out.push('\n');
            done = true;
        }
    }
    if !done {
        out.push_str("\n[secrets]\n");
        out.push_str(&new_line);
        out.push('\n');
    }
    toml::from_str::<crate::platform::config::RustployConfig>(&out)
        .context("a config resultante não seria válida")?;
    let tmp = path.with_extension("toml.tmp");
    write_private(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lazy_db() -> Arc<Db> {
        Arc::new(Db::lazy_for_test())
    }

    // ---- criptografia age (Fase 5) ------------------------------------------------------------

    fn mgr() -> SecretsManager {
        SecretsManager::new(lazy_db()).unwrap()
    }

    #[tokio::test]
    async fn cifra_no_formato_novo_e_volta() {
        let m = mgr();
        let c = m.encrypt("senha-super-secreta").unwrap();
        assert!(is_enc(&c) && c.starts_with("ENC[age:") && c.ends_with(']'));
        assert!(!c.contains("senha-super-secreta"));
        assert_eq!(m.decrypt(&c).unwrap(), "senha-super-secreta");
        // cada cifragem é diferente (nonce), mas as duas decifram
        assert_ne!(c, m.encrypt("senha-super-secreta").unwrap());
    }

    #[tokio::test]
    async fn identidade_errada_nao_decifra() {
        let a = mgr();
        let b = SecretsManager::new(lazy_db()).unwrap();
        let c = a.encrypt("x").unwrap();
        assert!(b.decrypt(&c).is_err());
    }

    #[tokio::test]
    async fn a_chave_de_recuperacao_tambem_decifra() {
        let recuperacao = x25519::Identity::generate();
        let servidor = x25519::Identity::generate();
        let m =
            SecretsManager::with_keys(lazy_db(), servidor, Some(recuperacao.to_public())).unwrap();
        let c = m.encrypt("segredo").unwrap();
        // um servidor NOVO, só com a chave de recuperação como identidade, lê o segredo
        let novo = SecretsManager::with_keys(lazy_db(), recuperacao, None).unwrap();
        assert_eq!(novo.decrypt(&c).unwrap(), "segredo");
    }

    #[tokio::test]
    async fn secret_gravado_nao_vaza_o_valor_para_o_yaml() {
        let db = Arc::new(Db::memory_for_test().await);
        db.seed_project_for_test("p1");
        let m = SecretsManager::new(db.clone()).unwrap();
        m.set("p1", "DB_PASSWORD", "hunter2-muito-secreto")
            .await
            .unwrap();
        let tree = db.infra().tree();
        let tudo: String = tree
            .list()
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&tree.read(r).unwrap().unwrap()).into_owned())
            .collect();
        assert!(!tudo.contains("hunter2-muito-secreto"));
        assert!(tudo.contains("ENC[age:"));
        assert_eq!(
            m.get_raw("p1", "DB_PASSWORD").await.unwrap(),
            "hunter2-muito-secreto"
        );
    }

    // ---- escopo de job -------------------------------------------------------------------------

    #[tokio::test]
    async fn secret_de_job_vale_antes_do_do_projeto_e_move_e_renomeia() {
        let db = Arc::new(Db::memory_for_test().await);
        db.seed_project_for_test("p1");
        let m = SecretsManager::new(db.clone()).unwrap();
        let job =
            crate::store::job::create(&db, "p1", None, "j", "x", None, "main", &[], &[], None)
                .await
                .unwrap();
        let j = job.id.as_str();

        m.set("p1", "TOKEN", "do-projeto").await.unwrap();
        m.set_scoped("p1", Scope::Job(j), "TOKEN", "do-job")
            .await
            .unwrap();
        assert_eq!(
            m.resolve_for_job("p1", j, None, "TOKEN").await.unwrap(),
            "do-job"
        );
        assert_eq!(m.get_raw("p1", "TOKEN").await.unwrap(), "do-projeto");
        assert_eq!(
            m.list_names_scoped("p1", Scope::Job(j)).await.unwrap(),
            ["TOKEN"]
        );
        // o valor nunca fica em claro no estado
        assert!(
            db.infra().snapshot().projects[0].jobs[0].secrets[0]
                .value
                .starts_with("ENC[age:")
        );

        // env do job que aponta para um secret que só existe no projeto também vale
        crate::store::job::update(
            &db,
            j,
            "j",
            "x",
            None,
            "main",
            &[EnvVar {
                key: "A".into(),
                value: EnvVarValue::Plain("1".into()),
            }],
            &[],
            true,
            None,
        )
        .await
        .unwrap();
        m.move_secret("p1", Scope::Job(j), "A", shared::SecretMoveOp::EnvToSecret)
            .await
            .unwrap();
        assert_eq!(m.get_scoped("p1", Scope::Job(j), "A").await.unwrap(), "1");
        m.rename_secret("p1", Scope::Job(j), "A", "B")
            .await
            .unwrap();
        let envs = db.infra().snapshot().projects[0].jobs[0].env_vars.clone();
        assert_eq!(envs[0].value, EnvVarValue::Secret("B".into()));
        m.move_secret("p1", Scope::Job(j), "B", shared::SecretMoveOp::SecretToEnv)
            .await
            .unwrap();
        let envs = db.infra().snapshot().projects[0].jobs[0].env_vars.clone();
        assert_eq!(envs[0].value, EnvVarValue::Plain("1".into()));
        m.rename_env_key("p1", Scope::Job(j), "A", "C")
            .await
            .unwrap();
        m.set_env_value(
            "p1",
            Scope::Job(j),
            "C",
            EnvVarValue::Secret("TOKEN".into()),
        )
        .await
        .unwrap();
        assert!(Scope::from_ids(Some("s"), Some(j)).is_err());
    }

    // ---- escopos de serviço e projeto, mover, ver valor (Fase 5) -----------------------------------

    async fn cenario() -> (Arc<Db>, SecretsManager, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::memory_for_test().await);
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s1");
        db.seed_service_for_test("p1", "s2");
        let m = SecretsManager::new(db.clone()).unwrap();
        (db, m, d)
    }

    fn env(db: &Db, service: Option<&str>) -> Vec<EnvVar> {
        let st = db.infra().snapshot();
        let p = &st.projects[0];
        match service {
            None => p.env_vars.clone(),
            Some(sid) => p
                .services
                .iter()
                .find(|s| s.id == sid)
                .unwrap()
                .spec
                .env_vars
                .clone(),
        }
    }

    fn set_env_plain(db: &Db, service: Option<&str>, key: &str, value: &str) {
        db.infra()
            .update(|st| {
                let p = &mut st.projects[0];
                let vars = match service {
                    None => &mut p.env_vars,
                    Some(sid) => {
                        &mut p
                            .services
                            .iter_mut()
                            .find(|s| s.id == sid)
                            .unwrap()
                            .spec
                            .env_vars
                    }
                };
                vars.push(EnvVar {
                    key: key.into(),
                    value: EnvVarValue::Plain(value.into()),
                });
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn resolucao_servico_primeiro_depois_projeto() {
        let (_db, m, _d) = cenario().await;
        m.set_scoped("p1", None, "DB_PASS", "do-projeto")
            .await
            .unwrap();
        // sem secret de serviço: cai no do projeto
        assert_eq!(
            m.resolve("p1", Some("s1"), "DB_PASS").await.unwrap(),
            "do-projeto"
        );
        m.set_scoped("p1", Some("s1"), "DB_PASS", "do-servico")
            .await
            .unwrap();
        assert_eq!(
            m.resolve("p1", Some("s1"), "DB_PASS").await.unwrap(),
            "do-servico"
        );
        // outro serviço e o escopo de projeto não enxergam o do serviço
        assert_eq!(
            m.resolve("p1", Some("s2"), "DB_PASS").await.unwrap(),
            "do-projeto"
        );
        assert_eq!(
            m.resolve("p1", None, "DB_PASS").await.unwrap(),
            "do-projeto"
        );
        // só no escopo de serviço: o projeto não vê
        m.set_scoped("p1", Some("s1"), "SO_AQUI", "x")
            .await
            .unwrap();
        assert!(m.resolve("p1", None, "SO_AQUI").await.is_err());
        assert!(m.resolve("p1", Some("s2"), "SO_AQUI").await.is_err());
        // listas separadas
        assert_eq!(m.list_names_scoped("p1", None).await.unwrap(), ["DB_PASS"]);
        assert_eq!(
            m.list_names_scoped("p1", Some("s1")).await.unwrap(),
            ["DB_PASS", "SO_AQUI"]
        );
        // serviço de outro projeto / inexistente: erro claro ao gravar
        assert!(
            m.set_scoped("p1", Some("fantasma"), "K", "v")
                .await
                .is_err()
        );
        m.delete_scoped("p1", Some("s1"), "DB_PASS").await.unwrap();
        assert_eq!(
            m.resolve("p1", Some("s1"), "DB_PASS").await.unwrap(),
            "do-projeto"
        );
    }

    #[tokio::test]
    async fn env_para_secret_e_de_volta_no_servico() {
        use shared::SecretMoveOp::*;
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, Some("s1"), "DB_PASSWORD", "hunter2");
        m.move_secret("p1", Some("s1"), "DB_PASSWORD", EnvToSecret)
            .await
            .unwrap();
        assert_eq!(
            env(&db, Some("s1"))[0].value,
            EnvVarValue::Secret("DB_PASSWORD".into())
        );
        assert_eq!(
            m.get_scoped("p1", Some("s1"), "DB_PASSWORD").await.unwrap(),
            "hunter2"
        );
        // o valor não está em claro no disco
        let tree = db.infra().tree();
        let tudo: String = tree
            .list()
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&tree.read(r).unwrap().unwrap()).into_owned())
            .collect();
        assert!(!tudo.contains("hunter2"));
        // de volta: a env volta a Plain e o secret some
        m.move_secret("p1", Some("s1"), "DB_PASSWORD", SecretToEnv)
            .await
            .unwrap();
        assert_eq!(
            env(&db, Some("s1"))[0].value,
            EnvVarValue::Plain("hunter2".into())
        );
        assert!(
            m.list_names_scoped("p1", Some("s1"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn env_para_secret_no_projeto_e_conflitos() {
        use shared::SecretMoveOp::*;
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, None, "TOKEN", "abc");
        m.set("p1", "TOKEN", "ja-existe").await.unwrap();
        // conflito: já há secret com esse nome → falha e NADA muda
        let e = m
            .move_secret("p1", None, "TOKEN", EnvToSecret)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("já tem um secret"), "{e}");
        assert_eq!(env(&db, None)[0].value, EnvVarValue::Plain("abc".into()));
        assert_eq!(m.get_raw("p1", "TOKEN").await.unwrap(), "ja-existe");
        // env que não existe / que já é secret
        assert!(
            m.move_secret("p1", None, "NADA", EnvToSecret)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn secret_para_env_aponta_todas_as_envs_ou_cria_uma() {
        use shared::SecretMoveOp::*;
        let (db, m, _d) = cenario().await;
        m.set_scoped("p1", Some("s1"), "SEGREDO", "v1")
            .await
            .unwrap();
        // duas envs apontam para o secret
        db.infra()
            .update(|st| {
                let vars = &mut st.projects[0].services[0].spec.env_vars;
                for k in ["A", "B"] {
                    vars.push(EnvVar {
                        key: k.into(),
                        value: EnvVarValue::Secret("SEGREDO".into()),
                    });
                }
                Ok(())
            })
            .unwrap();
        m.move_secret("p1", Some("s1"), "SEGREDO", SecretToEnv)
            .await
            .unwrap();
        let vars = env(&db, Some("s1"));
        assert!(
            vars.iter()
                .all(|v| v.value == EnvVarValue::Plain("v1".into()))
        );
        assert_eq!(vars.len(), 2);

        // nenhuma env apontava: cria uma com a chave = nome do secret
        m.set_scoped("p1", Some("s2"), "NOVO", "v2").await.unwrap();
        m.move_secret("p1", Some("s2"), "NOVO", SecretToEnv)
            .await
            .unwrap();
        let vars = env(&db, Some("s2"));
        assert_eq!(
            (vars[0].key.as_str(), &vars[0].value),
            ("NOVO", &EnvVarValue::Plain("v2".into()))
        );

        // ...e se já existe uma env com esse nome (sem apontar), é conflito
        m.set_scoped("p1", Some("s2"), "NOVO", "v3").await.unwrap();
        let e = m
            .move_secret("p1", Some("s2"), "NOVO", SecretToEnv)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("já tem uma env"), "{e}");
        assert!(
            m.get_scoped("p1", Some("s2"), "NOVO").await.is_ok(),
            "o secret não pode ter sumido"
        );
    }

    #[tokio::test]
    async fn mover_entre_servico_e_projeto() {
        use shared::SecretMoveOp::*;
        let (_db, m, _d) = cenario().await;
        // o caso de uso do usuário: o banco cria a senha no serviço; sobe para o projeto
        m.set_scoped("p1", Some("s1"), "DB_PASSWORD", "pw")
            .await
            .unwrap();
        m.move_secret("p1", Some("s1"), "DB_PASSWORD", ServiceSecretToProject)
            .await
            .unwrap();
        assert!(
            m.list_names_scoped("p1", Some("s1"))
                .await
                .unwrap()
                .is_empty()
        );
        // agora qualquer serviço do projeto resolve
        assert_eq!(
            m.resolve("p1", Some("s2"), "DB_PASSWORD").await.unwrap(),
            "pw"
        );
        // e dá para fechar o acesso: projeto → um serviço
        m.move_secret("p1", Some("s2"), "DB_PASSWORD", ProjectSecretToService)
            .await
            .unwrap();
        assert!(m.list_names("p1").await.unwrap().is_empty());
        assert_eq!(
            m.get_scoped("p1", Some("s2"), "DB_PASSWORD").await.unwrap(),
            "pw"
        );
        assert!(m.resolve("p1", Some("s1"), "DB_PASSWORD").await.is_err());

        // conflito de nome no destino: nada muda
        m.set("p1", "DB_PASSWORD", "outro").await.unwrap();
        let e = m
            .move_secret("p1", Some("s2"), "DB_PASSWORD", ServiceSecretToProject)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("já tem um secret"), "{e}");
        assert_eq!(
            m.get_scoped("p1", Some("s2"), "DB_PASSWORD").await.unwrap(),
            "pw"
        );
        assert_eq!(m.get_raw("p1", "DB_PASSWORD").await.unwrap(), "outro");
        // sem service_id onde é obrigatório
        assert!(
            m.move_secret("p1", None, "DB_PASSWORD", ProjectSecretToService)
                .await
                .is_err()
        );
        // serviço de fora do projeto
        assert!(
            m.move_secret(
                "p1",
                Some("fantasma"),
                "DB_PASSWORD",
                ProjectSecretToService
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn mover_env_entre_servico_e_projeto() {
        use shared::SecretMoveOp::*;
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, Some("s1"), "LOG_LEVEL", "debug");
        // serviço → projeto: sai do serviço, entra no projeto, valor intacto
        m.move_secret("p1", Some("s1"), "LOG_LEVEL", EnvServiceToProject)
            .await
            .unwrap();
        assert!(env(&db, Some("s1")).iter().all(|v| v.key != "LOG_LEVEL"));
        assert_eq!(
            env(&db, None)
                .iter()
                .find(|v| v.key == "LOG_LEVEL")
                .unwrap()
                .value,
            EnvVarValue::Plain("debug".into())
        );
        // projeto → outro serviço
        m.move_secret("p1", Some("s2"), "LOG_LEVEL", EnvProjectToService)
            .await
            .unwrap();
        assert!(env(&db, None).iter().all(|v| v.key != "LOG_LEVEL"));
        assert_eq!(
            env(&db, Some("s2"))
                .iter()
                .find(|v| v.key == "LOG_LEVEL")
                .unwrap()
                .value,
            EnvVarValue::Plain("debug".into())
        );
        // conflito no destino: nada muda
        set_env_plain(&db, None, "LOG_LEVEL", "info");
        let e = m
            .move_secret("p1", Some("s2"), "LOG_LEVEL", EnvServiceToProject)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("já tem uma env"), "{e}");
        assert_eq!(env(&db, Some("s2")).len(), 1);
        // env que aponta para um secret não se move aqui
        set_env_plain(&db, Some("s1"), "SENHA", "x");
        m.move_secret("p1", Some("s1"), "SENHA", EnvToSecret)
            .await
            .unwrap();
        let e = m
            .move_secret("p1", Some("s1"), "SENHA", EnvServiceToProject)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("já aponta para um secret"), "{e}");
        // sem service_id
        assert!(
            m.move_secret("p1", None, "LOG_LEVEL", EnvProjectToService)
                .await
                .is_err()
        );
    }

    // ---- renomear chave de env e secret (Fase C do plano de credenciais) --------------------------

    #[tokio::test]
    async fn renomear_chave_de_env_mantem_valor_e_acompanha_comentario() {
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, Some("s1"), "RABBITMQ_DEFAULT_PASS", "valor");
        db.infra()
            .update(|st| {
                st.projects[0].services[0]
                    .spec
                    .env_comments
                    .push(shared::EnvComment {
                        text: "# senha do broker".into(),
                        before_key: Some("RABBITMQ_DEFAULT_PASS".into()),
                    });
                Ok(())
            })
            .unwrap();
        m.rename_env_key("p1", Some("s1"), "RABBITMQ_DEFAULT_PASS", "RABBITMQ_PASS")
            .await
            .unwrap();
        let envs = env(&db, Some("s1"));
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].key, "RABBITMQ_PASS");
        assert_eq!(envs[0].value, EnvVarValue::Plain("valor".into()));
        let st = db.infra().snapshot();
        assert_eq!(
            st.projects[0].services[0].spec.env_comments[0]
                .before_key
                .as_deref(),
            Some("RABBITMQ_PASS")
        );
    }

    #[tokio::test]
    async fn renomear_chave_de_env_secret_nao_mexe_no_secret() {
        use shared::SecretMoveOp::EnvToSecret;
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, Some("s1"), "DB_PASSWORD", "hunter2");
        m.move_secret("p1", Some("s1"), "DB_PASSWORD", EnvToSecret)
            .await
            .unwrap();
        m.rename_env_key("p1", Some("s1"), "DB_PASSWORD", "POSTGRES_PASSWORD")
            .await
            .unwrap();
        // a chave mudou; a referência continua apontando para o secret de sempre
        assert_eq!(env(&db, Some("s1"))[0].key, "POSTGRES_PASSWORD");
        assert_eq!(
            env(&db, Some("s1"))[0].value,
            EnvVarValue::Secret("DB_PASSWORD".into())
        );
        assert_eq!(
            m.resolve("p1", Some("s1"), "DB_PASSWORD").await.unwrap(),
            "hunter2"
        );
    }

    #[tokio::test]
    async fn trocar_valor_de_env_mantem_a_posicao_e_valida() {
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, None, "A", "1");
        set_env_plain(&db, None, "B", "2");
        set_env_plain(&db, None, "C", "3");
        m.set_env_value("p1", None, "B", EnvVarValue::Plain("novo".into()))
            .await
            .unwrap();
        let envs = env(&db, None);
        let keys: Vec<_> = envs.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["A", "B", "C"], "a posição não muda");
        assert_eq!(envs[1].value, EnvVarValue::Plain("novo".into()));
        assert_eq!(envs[0].value, EnvVarValue::Plain("1".into()));
        // valor vazio é legítimo
        m.set_env_value("p1", None, "A", EnvVarValue::Plain(String::new()))
            .await
            .unwrap();
        assert_eq!(env(&db, None)[0].value, EnvVarValue::Plain(String::new()));
        // recusas: chave inexistente, máscara, secret que não existe
        assert!(
            m.set_env_value("p1", None, "NADA", EnvVarValue::Plain("x".into()))
                .await
                .is_err()
        );
        assert!(
            m.set_env_value("p1", None, "C", EnvVarValue::Plain("***".into()))
                .await
                .is_err()
        );
        assert!(
            m.set_env_value("p1", None, "C", EnvVarValue::Secret("FANTASMA".into()))
                .await
                .is_err()
        );
        assert_eq!(env(&db, None)[2].value, EnvVarValue::Plain("3".into()));
    }

    #[tokio::test]
    async fn trocar_valor_para_secret_existente_no_servico_e_no_projeto() {
        use shared::SecretMoveOp::EnvToSecret;
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, Some("s1"), "SENHA", "hunter2");
        set_env_plain(&db, Some("s1"), "OUTRA", "x");
        m.move_secret("p1", Some("s1"), "SENHA", EnvToSecret)
            .await
            .unwrap();
        // o secret `SENHA` é do serviço: OUTRA pode apontar para ele
        m.set_env_value(
            "p1",
            Some("s1"),
            "OUTRA",
            EnvVarValue::Secret("SENHA".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            env(&db, Some("s1"))[1].value,
            EnvVarValue::Secret("SENHA".into())
        );
        // numa env do PROJETO, o secret do serviço não vale
        set_env_plain(&db, None, "P", "1");
        assert!(
            m.set_env_value("p1", None, "P", EnvVarValue::Secret("SENHA".into()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn renomear_chave_de_env_recusa_conflito_inexistente_e_nome_ruim() {
        let (db, m, _d) = cenario().await;
        set_env_plain(&db, None, "A", "1");
        set_env_plain(&db, None, "B", "2");
        assert!(
            m.rename_env_key("p1", None, "A", "B").await.is_err(),
            "destino existe"
        );
        assert!(
            m.rename_env_key("p1", None, "NADA", "C").await.is_err(),
            "origem não existe"
        );
        assert!(
            m.rename_env_key("p1", None, "A", "1A").await.is_err(),
            "começa por número"
        );
        assert!(
            m.rename_env_key("p1", None, "A", "A B").await.is_err(),
            "espaço"
        );
        assert!(
            m.rename_env_key("p1", None, "A", "A").await.is_err(),
            "mesmo nome"
        );
        // nada mudou
        let keys: Vec<_> = env(&db, None).into_iter().map(|v| v.key).collect();
        assert_eq!(keys, ["A", "B"]);
    }

    #[tokio::test]
    async fn renomear_secret_de_servico_reaponta_so_as_envs_dele() {
        let (db, m, _d) = cenario().await;
        m.set_scoped("p1", Some("s1"), "RABBITMQ_DEFAULT_PASS", "segredo-1")
            .await
            .unwrap();
        for sid in ["s1", "s2"] {
            db.infra()
                .update(|st| {
                    let svc = st.projects[0]
                        .services
                        .iter_mut()
                        .find(|s| s.id == sid)
                        .unwrap();
                    svc.spec.env_vars.push(EnvVar {
                        key: "RABBITMQ_PASS".into(),
                        value: EnvVarValue::Secret("RABBITMQ_DEFAULT_PASS".into()),
                    });
                    Ok(())
                })
                .unwrap();
        }
        m.rename_secret("p1", Some("s1"), "RABBITMQ_DEFAULT_PASS", "RABBITMQ_PASS")
            .await
            .unwrap();
        assert_eq!(
            env(&db, Some("s1"))[0].value,
            EnvVarValue::Secret("RABBITMQ_PASS".into())
        );
        // o s2 não tinha esse secret: a env dele não é tocada
        assert_eq!(
            env(&db, Some("s2"))[0].value,
            EnvVarValue::Secret("RABBITMQ_DEFAULT_PASS".into())
        );
        // o valor nunca foi lido: continua o mesmo sob o nome novo
        assert_eq!(
            m.get_scoped("p1", Some("s1"), "RABBITMQ_PASS")
                .await
                .unwrap(),
            "segredo-1"
        );
        assert!(
            m.get_scoped("p1", Some("s1"), "RABBITMQ_DEFAULT_PASS")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn renomear_secret_de_projeto_respeita_o_sombreamento_do_servico() {
        let (db, m, _d) = cenario().await;
        m.set_scoped("p1", None, "DB_PASS", "do-projeto")
            .await
            .unwrap();
        // o s1 tem um secret próprio com o mesmo nome (sombreia o do projeto); o s2 usa o do projeto
        m.set_scoped("p1", Some("s1"), "DB_PASS", "do-s1")
            .await
            .unwrap();
        for sid in ["s1", "s2"] {
            db.infra()
                .update(|st| {
                    let svc = st.projects[0]
                        .services
                        .iter_mut()
                        .find(|s| s.id == sid)
                        .unwrap();
                    svc.spec.env_vars.push(EnvVar {
                        key: "PASS".into(),
                        value: EnvVarValue::Secret("DB_PASS".into()),
                    });
                    Ok(())
                })
                .unwrap();
        }
        db.infra()
            .update(|st| {
                st.projects[0].env_vars.push(EnvVar {
                    key: "PASS".into(),
                    value: EnvVarValue::Secret("DB_PASS".into()),
                });
                Ok(())
            })
            .unwrap();
        m.rename_secret("p1", None, "DB_PASS", "DATABASE_PASSWORD")
            .await
            .unwrap();
        // env do projeto e do s2 (que resolvia para o do projeto) foram reapontadas
        assert_eq!(
            env(&db, None)[0].value,
            EnvVarValue::Secret("DATABASE_PASSWORD".into())
        );
        assert_eq!(
            env(&db, Some("s2"))[0].value,
            EnvVarValue::Secret("DATABASE_PASSWORD".into())
        );
        // o s1 resolvia para o PRÓPRIO secret: a env dele não muda e continua valendo
        assert_eq!(
            env(&db, Some("s1"))[0].value,
            EnvVarValue::Secret("DB_PASS".into())
        );
        assert_eq!(
            m.resolve("p1", Some("s1"), "DB_PASS").await.unwrap(),
            "do-s1"
        );
        assert_eq!(
            m.resolve("p1", Some("s2"), "DATABASE_PASSWORD")
                .await
                .unwrap(),
            "do-projeto"
        );
    }

    #[tokio::test]
    async fn renomear_secret_recusa_conflito_inexistente_e_nome_ruim() {
        let (_db, m, _d) = cenario().await;
        m.set_scoped("p1", None, "A", "1").await.unwrap();
        m.set_scoped("p1", None, "B", "2").await.unwrap();
        assert!(
            m.rename_secret("p1", None, "A", "B").await.is_err(),
            "destino existe"
        );
        assert!(
            m.rename_secret("p1", None, "NADA", "C").await.is_err(),
            "origem não existe"
        );
        assert!(m.rename_secret("p1", None, "A", "").await.is_err(), "vazio");
        assert!(
            m.rename_secret("p1", None, "A", "a:b").await.is_err(),
            "dois-pontos"
        );
        assert!(
            m.rename_secret("p1", None, "A", "a b").await.is_err(),
            "espaço"
        );
        assert_eq!(m.get_scoped("p1", None, "A").await.unwrap(), "1");
    }

    #[tokio::test]
    async fn ver_valor_grava_auditoria_sem_o_valor() {
        let (db, m, _d) = cenario().await;
        m.set_scoped("p1", Some("s1"), "K", "valor-que-nao-pode-vazar")
            .await
            .unwrap();
        assert_eq!(
            m.reveal("p1", Some("s1"), "K").await.unwrap(),
            "valor-que-nao-pode-vazar"
        );
        let linhas: Vec<(String, String, Option<String>, String)> =
            sqlx::query_as("SELECT action, project_id, service_id, name FROM secret_audit")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            linhas,
            vec![("reveal".into(), "p1".into(), Some("s1".into()), "K".into())]
        );
        // secret que não existe: erro e nenhuma linha de auditoria
        assert!(m.reveal("p1", Some("s1"), "NADA").await.is_err());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secret_audit")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    // ---- origem da identidade -----------------------------------------------------------------

    fn config_completa(dir: &Path) -> PathBuf {
        let defaults =
            toml::to_string(&crate::platform::config::RustployConfig::default()).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!("# comentário que deve sobreviver\n{defaults}"),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn primeira_subida_gera_a_identidade_e_grava_no_config_toml() {
        let d = tempfile::tempdir().unwrap();
        let cfg_path = config_completa(d.path());
        let cfg = crate::platform::config::RustployConfig::default().secrets;
        let db = Arc::new(Db::memory_for_test().await);

        let m1 = SecretsManager::open(&d.path().join("age.key"), db.clone(), &cfg, Some(&cfg_path))
            .unwrap();
        let texto = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(
            texto.contains("age_identity = \"AGE-SECRET-KEY-1"),
            "{texto}"
        );
        assert!(texto.contains("# comentário que deve sobreviver"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // a config gravada ainda é válida e traz a identidade
        let relida: crate::platform::config::RustployConfig = toml::from_str(&texto).unwrap();
        assert!(relida.secrets.age_identity.is_some());
        assert!(
            !d.path().join("age.key").exists(),
            "não precisa do arquivo de reserva"
        );

        // segunda subida, lendo a identidade da config: decifra o que a primeira cifrou
        let c = m1.encrypt("x").unwrap();
        let m2 = SecretsManager::open(
            &d.path().join("age.key"),
            db,
            &relida.secrets,
            Some(&cfg_path),
        )
        .unwrap();
        assert_eq!(m2.decrypt(&c).unwrap(), "x");
        assert_eq!(m1.public_key(), m2.public_key());
    }

    /// O `config.toml` real de um servidor em produção (chiquitos, 2026-11-02), com os segredos
    /// trocados: tem `connection_token` (gravado pelo daemon), `[env_backup]` e `[ingress.acme]`.
    /// A gravação da identidade falhou ali com "a config resultante não seria válida".
    #[test]
    fn identidade_entra_numa_config_de_producao() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r####"[daemon]
# NOTA: em produção, db_path é GERIDO PELO systemd unit (rustployd.service),
# que o fixa via Environment=RUSTPLOY_DB_PATH apontando para /var/lib/rustploy
# (diretório criado por StateDirectory=). Como env vars têm prioridade sobre
# este arquivo, editar o campo abaixo NÃO terá efeito sob systemd — ajuste no
# unit ou em /etc/rustploy/environment (lido via EnvironmentFile=).
db_path      = "/var/lib/rustploy/db"
log_level    = "info"
# As demais seções abaixo VÊM deste arquivo normalmente — não são sobrescritas
# pelo unit.

# API HTTP/JSON + SSE — o canal do GUI. Serve TAMBÉM as rotas públicas sem token:
# os webhooks de CI/CD (POST /webhook/<service_id>/<token>) e o callback OAuth do
# Gitea (/oauth/gitea/callback), que antes tinham porta própria (8788). Ou seja:
# para o GitHub/Gitea alcançarem o webhook, esta porta precisa estar acessível.
# Defaults: bind 127.0.0.1, port 9797, sem token. Bind não-loopback EXIGE token.
[api]
bind_address = "0.0.0.0"
port         = 9797
token        = "tok"
connection_token = "abc="
domain       = "rustploy.chiquitos.tech"   # TLS automático (ACME) na própria porta

[ingress]
http_port    = 80
https_port   = 443
bind_address = "0.0.0.0"

[ingress.acme]
enabled   = true
email     = "a@b.c"               # set your email to enable ACME/TLS
directory = "https://acme-v02.api.letsencrypt.org/directory"

[docker]
socket_path = "/run/podman/podman.sock"

[deploy]
drain_secs  = 10
image_cache = 2

[metrics]
interval_secs  = 2
history_points = 60

[secrets]

# Faixa de portas de host que o rustploy aloca automaticamente para exposição
# externa de serviços TCP (bancos, brokers…) — o "host_port" automático.
# Também é a ÚNICA faixa que o helper privilegiado de firewall (rustployd-fw)
# aceita liberar/bloquear no ufw; portas fora dela são recusadas.
[external_ports]
range_start = 20000
range_end   = 20999

# Registry Docker OCI (Distribution API v2) embutido no daemon — Fase 1:
# escuta apenas em loopback (127.0.0.1), sem autenticação. Habilite e use
# docker push 127.0.0.1:5100/<repo>:<tag>.
[registry]
enabled = true
port    = 5100
# storage_dir = "/var/lib/rustploy/db/registry"   # default: <db_path>/registry/

# Env var backup — snapshots automáticos de todas as env vars de projetos e serviços.
# Restaurar: rustploy env-backup list | rustploy env-backup restore <snapshot>
# Docs: /usr/share/doc/rustploy/env-backup.md
[env_backup]
# dir = "/var/lib/rustploy/db/env_backups"  # default: <db_path>/env_backups/
interval_secs = 60000
"####,
        )
        .unwrap();
        let id = x25519::Identity::generate();
        let line = format!("age_identity = \"{}\"", id.to_string().expose_secret());
        write_identity_into_config(&path, &line).unwrap_or_else(|e| panic!("{e:#}"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("age_identity")
        );
    }

    #[tokio::test]
    async fn sem_config_gravavel_a_identidade_vai_para_o_age_key() {
        let d = tempfile::tempdir().unwrap();
        let cfg = crate::platform::config::RustployConfig::default().secrets;
        let db = Arc::new(Db::memory_for_test().await);
        let m1 = SecretsManager::open(&d.path().join("age.key"), db.clone(), &cfg, None).unwrap();
        assert!(d.path().join("age.key").exists());
        // e a próxima subida a encontra lá
        let m2 = SecretsManager::open(&d.path().join("age.key"), db, &cfg, None).unwrap();
        assert_eq!(m1.public_key(), m2.public_key());
    }

    #[tokio::test]
    async fn recusa_gerar_identidade_nova_se_ja_ha_segredos_age() {
        let d = tempfile::tempdir().unwrap();
        let cfg = crate::platform::config::RustployConfig::default().secrets;
        let db = Arc::new(Db::memory_for_test().await);
        db.seed_project_for_test("p1");
        let m = SecretsManager::new(db.clone()).unwrap();
        m.set("p1", "K", "v").await.unwrap();
        // identidade perdida: nem na config nem no age.key
        let e = SecretsManager::open(&d.path().join("age.key"), db, &cfg, None)
            .err()
            .expect("devia recusar");
        assert!(e.to_string().contains("ilegíveis"), "{e}");
    }

    #[tokio::test]
    async fn identidade_e_recuperacao_invalidas_dao_erro_claro() {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::memory_for_test().await);
        let mut cfg = crate::platform::config::RustployConfig::default().secrets;
        cfg.age_identity = Some("isto-nao-e-uma-chave".into());
        let e = SecretsManager::open(&d.path().join("age.key"), db.clone(), &cfg, None)
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("age_identity"), "{e:#}");
        cfg.age_identity = Some(
            x25519::Identity::generate()
                .to_string()
                .expose_secret()
                .clone(),
        );
        cfg.recovery_recipient = Some("age1lixo".into());
        let e = SecretsManager::open(&d.path().join("age.key"), db, &cfg, None)
            .err()
            .unwrap();
        assert!(e.to_string().contains("recovery_recipient"), "{e}");
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn lazy_db() -> Arc<Db> {
        Arc::new(Db::lazy_for_test())
    }

    /// Servidor antigo A (cifra para A + recuperação R) e servidor novo B (outra identidade, sem R).
    fn cenario() -> (SecretsManager, String, SecretsManager, String) {
        let r = x25519::Identity::generate();
        let r_secret = r.to_string().expose_secret().to_string();
        let a =
            SecretsManager::with_keys(lazy_db(), x25519::Identity::generate(), Some(r.to_public()))
                .unwrap();
        let cifrado_por_a = a.encrypt("senha-do-banco").unwrap();
        let b = SecretsManager::with_keys(lazy_db(), x25519::Identity::generate(), None).unwrap();
        (a, cifrado_por_a, b, r_secret)
    }

    #[tokio::test]
    async fn servidor_novo_so_abre_o_antigo_durante_a_recuperacao() {
        let (_a, cifrado, b, r_secret) = cenario();
        assert!(b.decrypt(&cifrado).is_err(), "B não é destinatário");
        assert!(!b.has_recovery() && !b.recovery_in_progress());

        let publica = b.begin_recovery(&r_secret).unwrap();
        assert!(publica.starts_with("age1"));
        assert!(b.has_recovery() && b.recovery_in_progress());
        assert_eq!(b.decrypt(&cifrado).unwrap(), "senha-do-banco");

        b.end_recovery();
        assert!(!b.recovery_in_progress());
        assert!(
            b.decrypt(&cifrado).is_err(),
            "a chave privada foi descartada"
        );
        // a pública continua: o que B cifrar daqui em diante também abre com a recuperação
        assert!(b.has_recovery());
    }

    #[tokio::test]
    async fn recifrar_passa_a_abrir_com_a_identidade_do_servidor_novo_sem_a_chave_privada() {
        let (_a, cifrado, b, r_secret) = cenario();
        b.begin_recovery(&r_secret).unwrap();
        let novo = b.rekey(&cifrado).unwrap();
        b.end_recovery();
        assert_eq!(b.decrypt(&novo).unwrap(), "senha-do-banco");
        // e ainda abre com a chave de recuperação (outro servidor, no futuro)
        let r = parse_identity(&r_secret).unwrap();
        assert_eq!(age_format::decrypt(&novo, &r).unwrap(), "senha-do-banco");
    }

    #[tokio::test]
    async fn arquivo_cifrado_para_o_servidor_antigo_abre_so_durante_a_recuperacao() {
        let (a, _c, b, r_secret) = cenario();
        let d = tempfile::tempdir().unwrap();
        let (src, enc, out) = (d.path().join("s"), d.path().join("e"), d.path().join("o"));
        std::fs::write(&src, b"dados do volume").unwrap();
        a.encrypt_file(&src, &enc).unwrap();
        assert!(b.decrypt_file(&enc, &out).is_err());
        b.begin_recovery(&r_secret).unwrap();
        b.decrypt_file(&enc, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"dados do volume");
    }

    #[tokio::test]
    async fn chave_invalida_e_recusada_sem_mudar_nada() {
        let (_a, _c, b, _r) = cenario();
        assert!(b.begin_recovery("isto-nao-e-uma-chave").is_err());
        assert!(!b.has_recovery() && !b.recovery_in_progress());
        // uma chave PÚBLICA não serve: é preciso a privada
        let publica = x25519::Identity::generate().to_public().to_string();
        assert!(b.begin_recovery(&publica).is_err());
    }

    #[tokio::test]
    async fn grava_so_a_chave_publica_no_config() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let base = toml::to_string(&crate::platform::config::RustployConfig::default()).unwrap();
        std::fs::write(&path, format!("# meu comentário\n{base}")).unwrap();
        let (_a, _c, mut b, r_secret) = cenario();
        b.config_path = Some(path.clone());
        let publica = b.begin_recovery(&r_secret).unwrap();
        let texto = std::fs::read_to_string(&path).unwrap();
        assert!(
            texto.contains(&format!("recovery_recipient = \"{publica}\"")),
            "{texto}"
        );
        assert!(texto.contains("# meu comentário"));
        assert!(
            !texto.contains("AGE-SECRET-KEY"),
            "a privada nunca vai para o disco"
        );
        // continua uma config válida e a chave volta a ser lida
        let cfg: crate::platform::config::RustployConfig = toml::from_str(&texto).unwrap();
        assert_eq!(
            cfg.secrets.recovery_recipient.as_deref(),
            Some(publica.as_str())
        );

        // trocar de novo substitui a linha em vez de duplicá-la
        let outra = x25519::Identity::generate();
        b.begin_recovery(outra.to_string().expose_secret()).unwrap();
        let texto = std::fs::read_to_string(&path).unwrap();
        assert_eq!(texto.matches("recovery_recipient").count(), 1, "{texto}");
    }

    #[tokio::test]
    async fn gerar_grava_so_a_publica_e_a_privada_abre_o_que_foi_cifrado() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let base = toml::to_string(&crate::platform::config::RustployConfig::default()).unwrap();
        std::fs::write(&path, &base).unwrap();
        let mut m = SecretsManager::new(lazy_db()).unwrap();
        m.config_path = Some(path.clone());
        assert!(!m.has_recovery() && m.recovery_public_key().is_none());

        let (publica, privada) = m.generate_recovery(false).unwrap();
        assert!(publica.starts_with("age1") && privada.starts_with("AGE-SECRET-KEY-1"));
        assert_eq!(m.recovery_public_key().as_deref(), Some(publica.as_str()));
        let texto = std::fs::read_to_string(&path).unwrap();
        assert!(texto.contains(&format!("recovery_recipient = \"{publica}\"")));
        assert!(
            !texto.contains("AGE-SECRET-KEY"),
            "a privada nunca vai para o disco"
        );

        // o que o servidor cifra daqui em diante abre com a privada devolvida
        let c = m.encrypt("segredo").unwrap();
        let novo =
            SecretsManager::with_keys(lazy_db(), parse_identity(&privada).unwrap(), None).unwrap();
        assert_eq!(novo.decrypt(&c).unwrap(), "segredo");
    }

    #[tokio::test]
    async fn gerar_de_novo_exige_confirmar_a_troca() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let base = toml::to_string(&crate::platform::config::RustployConfig::default()).unwrap();
        std::fs::write(&path, &base).unwrap();
        let mut m = SecretsManager::new(lazy_db()).unwrap();
        m.config_path = Some(path.clone());
        let (primeira, _) = m.generate_recovery(false).unwrap();
        assert!(m.generate_recovery(false).is_err());
        assert_eq!(m.recovery_public_key().as_deref(), Some(primeira.as_str()));
        let (segunda, _) = m.generate_recovery(true).unwrap();
        assert_ne!(primeira, segunda);
        let texto = std::fs::read_to_string(&path).unwrap();
        assert_eq!(texto.matches("recovery_recipient").count(), 1, "{texto}");
    }

    #[tokio::test]
    async fn gerar_sem_config_toml_e_recusado() {
        let m = SecretsManager::new(lazy_db()).unwrap();
        assert!(m.generate_recovery(false).is_err());
        assert!(!m.has_recovery());
    }
}
