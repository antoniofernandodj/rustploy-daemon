//! Mensagens de erro amigáveis para o usuário.

/// Converte um erro de banco (sqlx) numa mensagem amigável para o usuário.
/// Reconhece as violações de constraint mais comuns (UNIQUE) e devolve algo
/// legível em vez do texto cru do SQLite (`(code: 2067) UNIQUE constraint
/// failed: project.name`). `subject` é o rótulo do recurso (ex.: "projeto").
pub fn humanize_db_error(err: &(impl std::fmt::Display + ?Sized), subject: &str) -> String {
    let s = err.to_string();
    if s.contains("UNIQUE constraint failed") {
        return format!("Já existe um {subject} com esse nome.");
    }
    if s.contains("FOREIGN KEY constraint failed") {
        return format!("Operação inválida no {subject}: referência inexistente.");
    }
    // Regras de validação do próprio rustploy (já em português, sem SQL).
    if s.contains("reservado") || s.contains("servidor compartilhado") {
        return s;
    }
    // Fallback: mensagem genérica, sem vazar o SQL cru.
    format!("Falha ao salvar o {subject}. Tente novamente.")
}
