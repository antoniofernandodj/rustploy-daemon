//! Conversão dos dados de origem para os modelos do rustploy
//! (`TransformedData`).

pub mod dokploy;

use crate::domain::{Project, Service};

pub struct TransformedData {
    pub projects: Vec<Project>,
    pub services: Vec<Service>,
}
