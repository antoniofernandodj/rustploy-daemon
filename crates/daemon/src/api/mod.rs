//! API do daemon: adaptadores (HTTP, web UI, agente MCP) que traduzem pedidos em chamadas às features.

pub mod agent;
pub mod dispatch;
pub mod handlers;
pub mod http;
pub mod web_ui;
