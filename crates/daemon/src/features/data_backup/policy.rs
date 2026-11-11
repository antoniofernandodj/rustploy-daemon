//! A política de backup automático de cada serviço e a conta de "está na hora?"
//! (`docs/plano-backup-de-dados-para-s3.md`, Fase 3). Mora no YAML do serviço (`ServiceInfra.backup`) e,
//! portanto, vai para o git: depois de restaurar um servidor, os agendamentos continuam.

use crate::store::Db;
use crate::store::infra::model::BackupInfra;
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use shared::BackupPolicy;

/// A política gravada do serviço e desde quando vale.
pub fn get(db: &Db, service_id: &str) -> Option<BackupInfra> {
    db.infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .find(|s| s.id == service_id)
        .and_then(|s| s.backup.clone())
}

/// Todos os serviços com política **ligada**.
pub fn enabled(db: &Db) -> Vec<(String, BackupInfra)> {
    db.infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .filter_map(|s| {
            s.backup
                .clone()
                .filter(|b| b.policy.enabled)
                .map(|b| (s.id.clone(), b))
        })
        .collect()
}

/// Grava (ou, com `None`, apaga) a política. Ligar exige o destino S3 e a chave de recuperação: sem
/// eles o agendador falharia toda vez, em silêncio. `since` só é refeito quando a recorrência muda ou a
/// política acaba de ser ligada, para editar só o "manter N" não adiar o próximo disparo.
pub fn set(
    db: &Db,
    service_id: &str,
    policy: Option<BackupPolicy>,
    destination_ok: bool,
    recovery_ok: bool,
) -> Result<()> {
    if let Some(p) = &policy {
        p.validate().map_err(|e| anyhow::anyhow!(e))?;
        if p.enabled {
            if !destination_ok {
                bail!("configure antes o destino S3 em Settings → Backups");
            }
            if !recovery_ok {
                bail!(
                    "configure a chave de recuperação ([secrets] recovery_recipient no config.toml): \
                     o backup é sempre cifrado, e sem ela só este servidor o abriria"
                );
            }
        }
    }
    db.infra().update(|st| {
        let Some(svc) = st
            .projects
            .iter_mut()
            .flat_map(|p| p.services.iter_mut())
            .find(|s| s.id == service_id)
        else {
            bail!("serviço não encontrado");
        };
        svc.backup = policy.map(|new| {
            let keep_since = svc
                .backup
                .as_ref()
                .filter(|old| old.policy.enabled && old.policy.recurrence == new.recurrence)
                .map(|old| old.since);
            BackupInfra {
                since: keep_since.unwrap_or_else(Utc::now),
                policy: new,
            }
        });
        Ok(())
    })
}

/// O próximo disparo: um período depois da última vez que rodou (ou, se nunca rodou, depois de `since`).
pub fn next_run(b: &BackupInfra, last_start: Option<DateTime<Utc>>) -> DateTime<Utc> {
    b.policy
        .recurrence
        .next_after(last_start.unwrap_or(b.since))
}

/// Está na hora? Perder vários horários (daemon desligado) dispara **uma** vez só.
pub fn is_due(b: &BackupInfra, last_start: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    b.policy.enabled && next_run(b, last_start) <= now
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use shared::Recurrence;

    fn b(rec: Recurrence, since: DateTime<Utc>) -> BackupInfra {
        BackupInfra {
            policy: BackupPolicy {
                enabled: true,
                recurrence: rec,
                keep: 7,
                stop: true,
            },
            since,
        }
    }

    fn t(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap()
    }

    #[test]
    fn diario_ligado_as_10h_roda_as_3h_do_dia_seguinte() {
        let x = b(Recurrence::Daily { hour: 3, minute: 0 }, t(11, 10, 0));
        assert!(!is_due(&x, None, t(11, 23, 0)));
        assert!(!is_due(&x, None, t(12, 2, 59)));
        assert!(is_due(&x, None, t(12, 3, 0)));
    }

    #[test]
    fn depois_de_rodar_conta_a_partir_da_ultima_vez() {
        let x = b(Recurrence::IntervalHours(6), t(11, 0, 0));
        // rodou às 12:00: o próximo é 18:00, não importa o `since`
        assert!(!is_due(&x, Some(t(11, 12, 0)), t(11, 17, 59)));
        assert!(is_due(&x, Some(t(11, 12, 0)), t(11, 18, 0)));
    }

    #[test]
    fn horarios_perdidos_disparam_uma_vez_so() {
        let x = b(Recurrence::IntervalHours(1), t(11, 0, 0));
        // 5 horas depois: está vencido (um disparo); depois de rodar, o próximo é daqui a 1 h
        assert!(is_due(&x, None, t(11, 5, 0)));
        assert!(!is_due(&x, Some(t(11, 5, 0)), t(11, 5, 30)));
    }

    #[test]
    fn desligada_nunca_vence() {
        let mut x = b(Recurrence::IntervalHours(1), t(11, 0, 0));
        x.policy.enabled = false;
        assert!(!is_due(&x, None, t(12, 0, 0)));
    }
}
