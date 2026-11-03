//! Redação de segredos nas respostas que vão para o **agente** (MCP/`rpc`).
//!
//! Regra do dono (docs/plano-credenciais-cifradas-e-storage.md, Fase A): o agente nunca recebe o
//! valor de um secret nem a senha de um serviço. A GUI/webui não passam por aqui — só o que sai de
//! `api::agent::tools`.
//!
//! Isto é uma **rede de segurança**, não um cofre: a solução de verdade é a senha deixar de ser
//! `Plain` (Fase B). Mascara-se por heurística, de propósito generosa — melhor esconder um valor
//! inocente (`VAPID_PUBLIC_KEY`) do que mostrar uma senha.
//!
//! Há uma armadilha que o módulo também resolve: `ServiceUpdate` e `ProjectEnvSet` substituem tudo.
//! Se o agente fizer `ServiceGet` → muda um campo → `ServiceUpdate`, o `***` voltaria e
//! sobrescreveria o valor real. Por isso [`restaurar_envs`] devolve o valor original quando o que
//! chega é exatamente a versão mascarada dele.

use serde_json::Value;
use shared::{EnvVar, EnvVarValue};

/// O que substitui um valor escondido.
pub const MASCARA: &str = "***";

/// A chave de uma env parece guardar um segredo? Generosa de propósito.
pub fn chave_sensivel(chave: &str) -> bool {
    let up = chave.to_ascii_uppercase();
    if ["PASSWORD", "PASSWD", "SECRET", "TOKEN", "CREDENTIAL"]
        .iter()
        .any(|p| up.contains(p))
    {
        return true;
    }
    up.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|seg| matches!(seg, "PASS" | "PWD" | "KEY" | "APIKEY"))
}

/// `scheme://user:senha@host/...` → `scheme://user:***@host/...`. Vale para toda ocorrência no
/// texto. Sem `user:senha@` o texto sai igual.
pub fn mascarar_url(texto: &str) -> String {
    let mut saida = String::with_capacity(texto.len());
    let mut resto = texto;
    while let Some(i) = resto.find("://") {
        let (antes, depois) = resto.split_at(i + 3);
        saida.push_str(antes);
        let fim = depois
            .find(|c: char| {
                c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(depois.len());
        let (autoridade, cauda) = depois.split_at(fim);
        match autoridade.rfind('@') {
            Some(arroba) => {
                let userinfo = &autoridade[..arroba];
                match userinfo.find(':') {
                    Some(dois_pontos) => {
                        saida.push_str(&userinfo[..=dois_pontos]);
                        saida.push_str(MASCARA);
                        saida.push_str(&autoridade[arroba..]);
                    }
                    None => saida.push_str(autoridade),
                }
            }
            None => saida.push_str(autoridade),
        }
        resto = cauda;
    }
    saida.push_str(resto);
    saida
}

/// O valor `Plain` de uma env como o agente deve vê-lo.
pub fn valor_env_para_agente(chave: &str, valor: &str) -> String {
    if chave_sensivel(chave) && !valor.is_empty() {
        MASCARA.to_string()
    } else {
        mascarar_url(valor)
    }
}

/// É uma referência (não um valor)? Essas ficam como estão.
fn eh_referencia(valor: &str) -> bool {
    let v = valor.trim_matches(|c| c == '"' || c == '\'');
    v.starts_with("${") || v.starts_with("secret:") || v.starts_with("<secret:")
}

/// Para texto de várias linhas (YAML de manifesto, `.env`): `CHAVE=valor` / `CHAVE: valor` com
/// chave sensível vira `CHAVE=***`. Referências (`${X}`, `secret:X`) ficam.
pub fn mascarar_linhas(texto: &str) -> String {
    let mut linhas: Vec<String> = Vec::new();
    for linha in texto.split('\n') {
        let sem_indent = linha.trim_start();
        let indent = &linha[..linha.len() - sem_indent.len()];
        let (traco, corpo) = match sem_indent.strip_prefix("- ") {
            Some(r) => ("- ", r),
            None => ("", sem_indent),
        };
        let separador = corpo.find(['=', ':']);
        let mascarada = separador.and_then(|i| {
            let chave = corpo[..i].trim();
            let valor = corpo[i + 1..].trim();
            let chave_ok = !chave.is_empty()
                && chave
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if chave_ok && chave_sensivel(chave) && !valor.is_empty() && !eh_referencia(valor) {
                let sep = &corpo[i..=i];
                let espaco = if sep == ":" { " " } else { "" };
                Some(format!("{indent}{traco}{chave}{sep}{espaco}{MASCARA}"))
            } else {
                None
            }
        });
        linhas.push(mascarada.unwrap_or_else(|| mascarar_url(linha)));
    }
    linhas.join("\n")
}

/// Percorre o JSON de uma resposta escondendo o que não pode chegar ao agente:
/// - `{"key": K, "value": {"Plain": V}}` (uma env) → `V` mascarado;
/// - `internal_url` (resposta de `ServiceConnectionInfo`) → senha mascarada.
pub fn redigir(valor: &mut Value) {
    match valor {
        Value::Object(mapa) => {
            let chave = mapa.get("key").and_then(Value::as_str).map(str::to_owned);
            if let Some(chave) = chave
                && let Some(Value::Object(v)) = mapa.get_mut("value")
                && let Some(Value::String(plain)) = v.get_mut("Plain")
            {
                *plain = valor_env_para_agente(&chave, plain);
            }
            if let Some(Value::String(url)) = mapa.get_mut("internal_url") {
                *url = mascarar_url(url);
            }
            for filho in mapa.values_mut() {
                redigir(filho);
            }
        }
        Value::Array(itens) => itens.iter_mut().for_each(redigir),
        _ => {}
    }
}

/// [`redigir`] mais os pacotes de exportação, que carregam texto (YAML/`.env`) em vez de envs
/// estruturadas.
pub fn redigir_resposta(resposta: &mut Value) {
    if let Some(Value::String(yaml)) = resposta.get_mut("Manifest") {
        *yaml = mascarar_linhas(yaml);
    }
    for variante in ["ManifestBundle", "ServiceBundleYaml"] {
        if let Some(Value::Object(campos)) = resposta.get_mut(variante) {
            for campo in ["yaml", "dotenv"] {
                if let Some(Value::String(texto)) = campos.get_mut(campo) {
                    *texto = mascarar_linhas(texto);
                }
            }
        }
    }
    redigir(resposta);
}

/// Devolve o valor real das envs que o agente reenviou mascaradas (ver o doc do módulo).
pub fn restaurar_envs(novas: &mut [EnvVar], atuais: &[EnvVar]) {
    for nova in novas {
        let EnvVarValue::Plain(v) = &mut nova.value else {
            continue;
        };
        let Some(EnvVarValue::Plain(real)) =
            atuais.iter().find(|e| e.key == nova.key).map(|e| &e.value)
        else {
            continue;
        };
        if v != real && *v == valor_env_para_agente(&nova.key, real) {
            *v = real.clone();
        }
    }
}

/// Sobrou valor mascarado depois de [`restaurar_envs`]? Então o agente está tentando gravar o
/// literal `***` (ou `:***@`) por cima de algo que ele nunca viu — recusa em vez de corromper.
pub fn sobrou_mascara(envs: &[EnvVar]) -> Option<&str> {
    envs.iter().find_map(|e| match &e.value {
        EnvVarValue::Plain(v) if v == MASCARA || v.contains(":***@") => Some(e.key.as_str()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain(k: &str, v: &str) -> EnvVar {
        EnvVar {
            key: k.into(),
            value: EnvVarValue::Plain(v.into()),
        }
    }

    #[test]
    fn chaves_sensiveis() {
        for k in [
            "POSTGRES_PASSWORD",
            "RABBITMQ_PASS",
            "JWT_SECRET",
            "S3_ACCESS_KEY_ID",
            "S3_SECRET_ACCESS_KEY",
            "TOKEN_DE_AUTENTICACAO_ASAAS",
            "MYSQLPASSWORD",
            "api_key",
        ] {
            assert!(chave_sensivel(k), "{k} deveria ser sensível");
        }
        for k in [
            "MODE",
            "RUST_LOG",
            "APP_PORT",
            "S3_BUCKET_NAME_1",
            "KEYCLOAK_URL",
            "RABBITMQ_USER",
        ] {
            assert!(!chave_sensivel(k), "{k} não deveria ser sensível");
        }
    }

    #[test]
    fn url_mascara_so_a_senha() {
        assert_eq!(
            mascarar_url("postgresql://postgres:abc123@rp_db:5432/db"),
            "postgresql://postgres:***@rp_db:5432/db"
        );
        assert_eq!(
            mascarar_url("redis://:segredo@rp_cache:6379"),
            "redis://:***@rp_cache:6379"
        );
        // Sem senha, ou sem credencial: igual.
        assert_eq!(
            mascarar_url("https://github.com/a/b.git"),
            "https://github.com/a/b.git"
        );
        assert_eq!(
            mascarar_url("https://usuario@host/x"),
            "https://usuario@host/x"
        );
        // Mais de uma URL no mesmo texto.
        assert_eq!(
            mascarar_url("a=amqp://u:p1@h1/ b=amqp://u:p2@h2/"),
            "a=amqp://u:***@h1/ b=amqp://u:***@h2/"
        );
    }

    #[test]
    fn connection_info_nao_leva_senha() {
        let mut v = json!({"ConnectionInfo": {"host": "rp_db", "internal_url": "postgresql://u:SENHA@rp_db:5432/db"}});
        redigir_resposta(&mut v);
        let texto = v.to_string();
        assert!(!texto.contains("SENHA"), "{texto}");
        assert!(texto.contains("rp_db"));
    }

    #[test]
    fn env_plain_sensivel_e_mascarada_e_o_resto_fica() {
        let mut v = json!({"Service": {"spec": {"env_vars": [
            {"key": "POSTGRES_PASSWORD", "value": {"Plain": "SENHA"}},
            {"key": "MODE", "value": {"Plain": "production"}},
            {"key": "DATABASE_URL", "value": {"Plain": "postgres://u:SENHA@h/db"}},
            {"key": "JWT_SECRET", "value": {"Secret": "JWT_SECRET"}},
        ]}}});
        redigir_resposta(&mut v);
        let envs = &v["Service"]["spec"]["env_vars"];
        assert_eq!(envs[0]["value"]["Plain"], "***");
        assert_eq!(envs[1]["value"]["Plain"], "production");
        assert_eq!(envs[2]["value"]["Plain"], "postgres://u:***@h/db");
        assert_eq!(
            envs[3]["value"]["Secret"], "JWT_SECRET",
            "referência a secret não muda"
        );
        assert!(!v.to_string().contains("SENHA"));
    }

    #[test]
    fn dotenv_e_yaml_mascaram_so_chaves_sensiveis() {
        let mut v = json!({"ManifestBundle": {
            "yaml": "env:\n  POSTGRES_PASSWORD: secret:POSTGRES_PASSWORD\n  JWT_SECRET: abc\n  MODE: production\n",
            "dotenv": "POSTGRES_PASSWORD=SENHA\nMODE=production\nREF=${X}\nTOKEN=${TOKEN}\n",
        }});
        redigir_resposta(&mut v);
        let yaml = v["ManifestBundle"]["yaml"].as_str().unwrap();
        let dotenv = v["ManifestBundle"]["dotenv"].as_str().unwrap();
        assert!(
            yaml.contains("POSTGRES_PASSWORD: secret:POSTGRES_PASSWORD"),
            "{yaml}"
        );
        assert!(
            yaml.contains("JWT_SECRET: ***") && yaml.contains("MODE: production"),
            "{yaml}"
        );
        assert!(
            dotenv.contains("POSTGRES_PASSWORD=***") && !dotenv.contains("SENHA"),
            "{dotenv}"
        );
        assert!(
            dotenv.contains("MODE=production") && dotenv.contains("TOKEN=${TOKEN}"),
            "{dotenv}"
        );
    }

    #[test]
    fn reenviar_mascarado_restaura_o_valor_real() {
        let atuais = vec![
            plain("POSTGRES_PASSWORD", "SENHA"),
            plain("DATABASE_URL", "postgres://u:SENHA@h/db"),
            plain("MODE", "production"),
        ];
        // O que o agente viu e devolveu:
        let mut novas = vec![
            plain("POSTGRES_PASSWORD", "***"),
            plain("DATABASE_URL", "postgres://u:***@h/db"),
            plain("MODE", "development"), // alterou de propósito
        ];
        restaurar_envs(&mut novas, &atuais);
        assert_eq!(novas[0].value, EnvVarValue::Plain("SENHA".into()));
        assert_eq!(
            novas[1].value,
            EnvVarValue::Plain("postgres://u:SENHA@h/db".into())
        );
        assert_eq!(novas[2].value, EnvVarValue::Plain("development".into()));
        assert!(sobrou_mascara(&novas).is_none());
    }

    #[test]
    fn mascara_sem_original_e_recusada() {
        // Env nova com o literal `***`: não há o que restaurar → o chamador recusa.
        let mut novas = vec![plain("NOVA_SENHA", "***")];
        restaurar_envs(&mut novas, &[]);
        assert_eq!(sobrou_mascara(&novas), Some("NOVA_SENHA"));
        // O agente trocou a senha de propósito por outra coisa: respeitado.
        let mut trocada = vec![plain("POSTGRES_PASSWORD", "outra")];
        restaurar_envs(&mut trocada, &[plain("POSTGRES_PASSWORD", "SENHA")]);
        assert_eq!(trocada[0].value, EnvVarValue::Plain("outra".into()));
    }
}
