//! Estágio `Draining`: espera as conexões do container antigo terminarem.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_draining(
        &self,
        dep: &Deployment,
        _svc: &Service,
    ) -> Result<DeployState> {
        info!(
            deployment_id = %dep.id,
            drain_secs = self.drain_secs,
            "step[Draining]: aguardando drain de conexões"
        );
        sleep(Duration::from_secs(self.drain_secs)).await;
        info!(deployment_id = %dep.id, "step[Draining]: drain concluído");
        Ok(DeployState::Promoting)
    }
}
