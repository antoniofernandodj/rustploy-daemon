//! Backup dos dados dos serviços para um S3. Ver `docs/plano-backup-de-dados-para-s3.md`.

use crate::domain::Recurrence;
use serde::{Deserialize, Serialize};

/// O destino S3 dos backups (um por servidor). É o que a UI **envia**: `secret_key` é a chave secreta
/// em claro, só no sentido UI → daemon; `None` ao salvar significa "manter a que já está gravada".
/// O daemon a guarda cifrada (`ENC[age:…]`) e **nunca** a devolve (ver [`BackupDestinationView`]).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupDestination {
    /// `https://s3.us-east-005.backblazeb2.com`, `http://rp_s3:9000`… (sem o bucket).
    pub endpoint: String,
    /// Região de assinatura; `us-east-1` quando o provedor não pede uma.
    pub region: String,
    pub bucket: String,
    /// Pasta dentro do bucket (opcional): `rustploy/chiquitos`.
    #[serde(default)]
    pub prefix: String,
    /// `true`: `host/bucket/chave` (MinIO, RustFS, R2, Backblaze). `false`: `bucket.host/chave` (AWS).
    #[serde(default = "path_style_default")]
    pub path_style: bool,
    pub access_key: String,
    #[serde(default)]
    pub secret_key: Option<String>,
}

fn path_style_default() -> bool {
    true
}

impl std::fmt::Debug for BackupDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupDestination")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("access_key", &self.access_key)
            .field("secret_key", &self.secret_key.as_ref().map(|_| "***"))
            .finish_non_exhaustive()
    }
}

/// O destino como o daemon o **devolve**: sem a chave secreta, só se ela existe.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupDestinationView {
    /// Há um destino gravado.
    pub configured: bool,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub path_style: bool,
    #[serde(default)]
    pub access_key: String,
    /// A chave secreta está gravada (o valor nunca sai do daemon).
    #[serde(default)]
    pub secret_set: bool,
    /// Há chave de recuperação configurada. Sem ela o backup não pode ser ligado: abriria só neste servidor.
    #[serde(default)]
    pub recovery_configured: bool,
}

/// Um backup de um serviço, como está no S3.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupEntry {
    /// A chave do objeto no bucket (é o que se passa a `ServiceBackupRestore`/`ServiceBackupDelete`).
    pub key: String,
    /// Quando foi gravado (RFC 3339, como o S3 informa).
    pub last_modified: String,
    pub size_bytes: u64,
}

/// A última operação de backup ou restauração de um serviço (em andamento ou terminada).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupRun {
    /// `"backup"` ou `"restore"`.
    pub op: String,
    /// `"running"`, `"ok"` ou `"failed"`.
    pub state: String,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    /// Só `backup` terminado: o tamanho enviado.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// O objeto gravado (`backup`) ou usado (`restore`).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Só `restore` ok: conferências feitas depois de aplicar (contagens por tabela etc.).
    #[serde(default)]
    pub checks: Vec<String>,
}

/// A política de backup automático de **um serviço** (vive no YAML do serviço e vai para o git).
/// O destino S3 é um só por servidor (`BackupDestination`); aqui só o *quando* e o *quanto guardar*.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupPolicy {
    pub enabled: bool,
    /// Quando rodar (em UTC), no mesmo formato dos jobs: a cada N horas, todo dia às HH:MM ou toda semana.
    pub recurrence: Recurrence,
    /// Quantos backups manter (os mais novos); os antigos saem depois de um novo subir. De 1 a 365.
    #[serde(default = "backup_keep_default")]
    pub keep: u32,
    /// Parar o serviço durante a cópia de **volume** (banco não para, o dump é no ar).
    #[serde(default = "backup_stop_default")]
    pub stop: bool,
}

fn backup_keep_default() -> u32 {
    7
}

fn backup_stop_default() -> bool {
    true
}

impl BackupPolicy {
    /// Confere os limites; devolve a causa em linguagem de gente.
    pub fn validate(&self) -> Result<(), String> {
        match self.recurrence {
            Recurrence::IntervalHours(h) if !(1..=168).contains(&h) => {
                return Err("a frequência por horas vai de 1 a 168 (uma semana)".into());
            }
            Recurrence::Daily { hour, minute } | Recurrence::Weekly { hour, minute, .. }
                if hour > 23 || minute > 59 =>
            {
                return Err("horário inválido (hora 0–23, minuto 0–59)".into());
            }
            Recurrence::Weekly { weekday, .. } if weekday > 6 => {
                return Err("dia da semana inválido (0 = segunda … 6 = domingo)".into());
            }
            _ => {}
        }
        if !(1..=365).contains(&self.keep) {
            return Err("quantos manter: de 1 a 365".into());
        }
        Ok(())
    }
}

/// Resposta de `ServiceBackupList`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceBackups {
    /// Os backups do serviço no S3, do mais novo para o mais antigo.
    pub entries: Vec<BackupEntry>,
    #[serde(default)]
    pub last_run: Option<BackupRun>,
    /// Por que não dá para fazer backup agora (sem destino, sem chave de recuperação, sem dados…).
    #[serde(default)]
    pub blocked: Option<String>,
    /// Falha ao listar o S3 (credencial, rede…); a tela mostra e o resto continua válido.
    #[serde(default)]
    pub list_error: Option<String>,
    /// A política de backup automático deste serviço (`None` = nunca configurada).
    #[serde(default)]
    pub policy: Option<BackupPolicy>,
    /// Quando o próximo backup automático dispara (RFC 3339), se a política está ligada.
    #[serde(default)]
    pub next_run_at: Option<String>,
}

/// Um serviço na tela de recuperação.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecoverService {
    pub service_id: String,
    pub name: String,
    pub project: String,
    /// `"database"` (o dump volta com o banco no ar) ou `"volume"` (os volumes voltam antes de subir).
    pub kind: String,
    /// O backup mais recente no S3 (`last_modified`), se houver; sem ele o serviço sobe vazio.
    #[serde(default)]
    pub latest_backup: Option<String>,
    /// `pending`, `restoring`, `starting`, `ready`, `blocked` ou `failed`.
    pub step: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// Resposta dos comandos `ServerRecover*`: em que ponto a recuperação deste servidor está.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerRecoverInfo {
    /// `idle` (nada em curso), `ready` (a infra voltou; falta confirmar os dados), `running`, `done`,
    /// `failed`.
    pub phase: String,
    #[serde(default)]
    pub projects: u32,
    #[serde(default)]
    pub services: Vec<RecoverService>,
    /// O destino S3 veio na infra restaurada (ou já estava configurado).
    #[serde(default)]
    pub destination_configured: bool,
    /// Por que os backups não puderam ser listados (credencial, rede…); os serviços sobem vazios.
    #[serde(default)]
    pub destination_error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    /// A chave **pública** de recuperação que passou a valer neste servidor (`age1…`).
    #[serde(default)]
    pub recovery_public_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn politica_valida_os_limites() {
        let ok = BackupPolicy {
            enabled: true,
            recurrence: Recurrence::Daily { hour: 3, minute: 0 },
            keep: 7,
            stop: true,
        };
        assert!(ok.validate().is_ok());
        let mut p = ok.clone();
        p.keep = 0;
        assert!(p.validate().is_err());
        p.keep = 366;
        assert!(p.validate().is_err());
        p.keep = 7;
        p.recurrence = Recurrence::IntervalHours(0);
        assert!(p.validate().is_err());
        p.recurrence = Recurrence::IntervalHours(169);
        assert!(p.validate().is_err());
        p.recurrence = Recurrence::Daily {
            hour: 24,
            minute: 0,
        };
        assert!(p.validate().is_err());
        p.recurrence = Recurrence::Weekly {
            weekday: 7,
            hour: 1,
            minute: 0,
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn politica_sem_keep_e_stop_usa_os_padroes() {
        let p: BackupPolicy =
            serde_json::from_str(r#"{"enabled":true,"recurrence":{"IntervalHours":6}}"#).unwrap();
        assert_eq!((p.keep, p.stop), (7, true));
    }

    #[test]
    fn rascunho_sem_chave_e_sem_path_style_usa_os_padroes() {
        let d: BackupDestination = serde_json::from_str(
            r#"{"endpoint":"https://s3.x","region":"us-east-1","bucket":"b","access_key":"AK"}"#,
        )
        .unwrap();
        assert_eq!(d.secret_key, None);
        assert!(d.path_style);
        assert_eq!(d.prefix, "");
    }

    #[test]
    fn debug_do_destino_nao_mostra_a_chave() {
        let d = BackupDestination {
            endpoint: "https://s3.x".into(),
            region: "r".into(),
            bucket: "b".into(),
            prefix: String::new(),
            path_style: true,
            access_key: "AK".into(),
            secret_key: Some("muito-secreta".into()),
        };
        assert!(!format!("{d:?}").contains("muito-secreta"));
    }

    #[test]
    fn a_visao_nao_tem_campo_para_a_chave() {
        let v = BackupDestinationView {
            configured: true,
            secret_set: true,
            ..Default::default()
        };
        assert!(!serde_json::to_string(&v).unwrap().contains("secret_key"));
    }
}
