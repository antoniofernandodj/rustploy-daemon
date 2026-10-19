//! Handlers de docker: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod docker_cleanup;
pub mod docker_inventory;
pub mod docker_prune;
pub mod docker_remove;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::PruneContainers => docker_prune::prune_containers(state).await,
        Command::PruneVolumes { all } => docker_prune::prune_volumes(state, all).await,
        Command::PruneImages { all } => docker_prune::prune_images(state, all).await,
        Command::PruneBuildCache => docker_prune::prune_build_cache(state).await,
        Command::PruneNetworks => docker_prune::prune_networks(state).await,
        Command::DockerCleanupConfigGet => docker_cleanup::get_config(state).await,
        Command::DockerCleanupConfigSet { config } => {
            docker_cleanup::set_config(state, config).await
        }
        Command::DockerCleanupRunNow => docker_cleanup::run_now(state).await,
        Command::DockerImages => docker_inventory::list_images(state).await,
        Command::DockerVolumes => docker_inventory::list_volumes(state).await,
        Command::DockerNetworks => docker_inventory::list_networks(state).await,
        Command::DockerContainers => docker_inventory::list_containers(state).await,
        Command::RemoveContainer { id } => docker_remove::remove_container(state, id).await,
        Command::RemoveImage { id } => docker_remove::remove_image(state, id).await,
        Command::RemoveVolume { name } => docker_remove::remove_volume(state, name).await,
        Command::RemoveNetwork { id } => docker_remove::remove_network(state, id).await,
        Command::StopAllManaged => docker_inventory::stop_all_managed(state).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
