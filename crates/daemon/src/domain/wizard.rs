use serde::{Deserialize, Serialize};

/// Campos coletados pelo wizard no cliente; o daemon (`WizardCreate`) resolve o
/// `kind`/`id` e monta o `ServiceSpec`.
///
/// Só `kind`, `project_id` e `name` são obrigatórios. O resto tem `#[serde(default)]`
/// porque o `build_spec` já preenche o que vier vazio (usuário padrão do motor, banco
/// `app`, imagem do catálogo, senha gerada): a GUI e a webui mandam tudo, e o agente
/// pode mandar só o mínimo, ex. `{"kind":"database","id":"postgres","project_id":"prj_…","name":"db"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardCreateReq {
    /// "application" | "compose" | "database" | "broker" | "storage" | "template".
    pub kind: String,
    /// db kind_id / broker kind_id / template id (vazio p/ application/compose).
    #[serde(default)]
    pub id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub db_name: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub root_password: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub use_replica: bool,
    #[serde(default)]
    pub template_values: Vec<String>,
    /// Quando true, o serviço nasce com `host_port = Some(0)` — a sentinela que
    /// faz o daemon alocar uma porta externa automaticamente (faixa
    /// `[external_ports]` da config) e liberá-la no firewall via `rustployd-fw`.
    #[serde(default)]
    pub expose_external: bool,
}

#[cfg(test)]
mod tests {
    use super::WizardCreateReq;

    #[test]
    fn so_kind_projeto_e_nome_sao_obrigatorios() {
        let req: WizardCreateReq = serde_json::from_str(
            r#"{"kind":"database","id":"postgres","project_id":"prj_1","name":"db"}"#,
        )
        .expect("o mínimo tem que ser aceito");
        assert_eq!(req.id, "postgres");
        assert!(req.password.is_empty() && req.image.is_empty() && !req.use_replica);
        assert!(req.template_values.is_empty() && !req.expose_external);

        let sem_nome =
            serde_json::from_str::<WizardCreateReq>(r#"{"kind":"database","project_id":"prj_1"}"#);
        assert!(sem_nome.is_err(), "nome continua obrigatório");
    }
}
