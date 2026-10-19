//! API do daemon: adaptadores (HTTP, web UI, agente MCP) que traduzem pedidos em chamadas às features.

pub mod agent;
pub mod handlers;
pub mod http_api;
pub mod public_routes;
pub mod routes;
pub mod web_ui;
