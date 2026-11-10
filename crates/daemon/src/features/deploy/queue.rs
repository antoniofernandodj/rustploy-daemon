//! Fila **global** de deploys: no máximo um deploy rodando por vez no daemon.
//!
//! Antes, cada `deploy_start` spawnava um `DeployExecutor` na hora e N pedidos
//! rodavam concorrentes. Agora `deploy_start` só **enfileira** o `deployment_id`
//! (o deployment nasce em [`DeployState::Pending`] e o serviço em
//! [`ServiceStatus::Queued`]) e um **único worker** ([`run_worker`]) puxa um por
//! vez, roda até terminar e só então pega o próximo.
//!
//! **Jobs avulsos** (Schedules: agendados ou "Executar agora") dividem a mesma
//! **vaga única** ([`DeployQueue::acquire_job_slot`]): enquanto um deploy roda, o
//! job espera a vez, e vice-versa — só uma coisa executa por vez no daemon. O
//! gate de pré-deploy **não** pega a vaga: roda por dentro do deploy, que já a
//! tem (pegá-la de novo travaria o deploy esperando a si mesmo). A pausa da fila
//! segura os dois. Ver `docs/plano-jobs-na-fila-de-deploy.md`.
//!
//! A ordem "verdadeira" da fila vive na `VecDeque` em memória (não no banco);
//! num restart ela é reconstruída da ordem de criação dos `Pending`
//! (`recovery::recover`). O worker roda o executor como *task* (não `await`
//! inline) para preservar o mecanismo de abort via `active_deploys`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use shared::{Event, QueueItemKind, ServiceStatus};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tracing::{info, warn};

use crate::features::deploy::DeployContext;
use crate::features::deploy::executor::DeployExecutor;

/// O que roda quando a vez de um item que **não é deploy** chega (a execução de um job ou o
/// aquecimento de um). Fica na fila até o worker o puxar; o worker o executa com a vaga em mãos.
pub type QueueTaskFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
pub type QueueTaskFn = Box<dyn FnOnce() -> QueueTaskFuture + Send>;

/// Para a tela (Deploy Engine) saber o que é cada item que não é deploy.
#[derive(Debug, Clone)]
pub struct QueueTaskInfo {
    pub kind: QueueItemKind,
    pub job_id: String,
}

/// Um item da fila que não é deploy: o que rodar na vez dele e o que fazer se for tirado da fila
/// antes (`on_cancel`: fecha a execução como cancelada, solta o "aquecendo", etc.).
pub struct QueueTask {
    pub run: QueueTaskFn,
    pub on_cancel: QueueTaskFn,
}

#[derive(Default)]
struct QueueInner {
    /// Itens que não são deploy (jobs, aquecimentos), por id. O id também está em `queued` (ou em
    /// `running`); aqui mora o que fazer com ele.
    tasks: HashMap<String, QueueTask>,
    /// O que cada item que não é deploy é, **até terminar** (inclusive enquanto roda: o `tasks`
    /// já foi esvaziado pelo worker, mas a tela ainda precisa saber o que está rodando).
    infos: HashMap<String, QueueTaskInfo>,
    /// deployment_ids esperando, em ordem de execução (frente = próximo).
    queued: VecDeque<String>,
    /// deployment_id rodando agora (ou `None`).
    running: Option<String>,
    /// Fila pausada — o worker não puxa o próximo até retomar.
    paused: bool,
    /// Deploys de um lote (`DeployWithDependencies`/`ProjectDeploy`): `deployment_id` → os deployments
    /// de que ele depende. Se algum deles não terminou bem, este **não roda** (ver `run_one`). Só em
    /// memória: depois de um reinício do daemon o deploy pendente roda sem essa trava.
    waits: std::collections::HashMap<String, Vec<String>>,
}

/// Handle compartilhado da fila (fica no `DeployContext`). Sem `DeployContext` dentro,
/// para não criar ciclo de tipos: o worker recebe o `DeployContext` por fora.
pub struct DeployQueue {
    inner: Mutex<QueueInner>,
    notify: Notify,
    /// A vaga única: quem a segura é a "coisa" que está executando (um deploy ou
    /// um job avulso). Semáforo do tokio é justo: quem pede primeiro entra primeiro.
    slot: Arc<Semaphore>,
    /// Espelho de `QueueInner::paused` para os jobs esperarem a retomada sem
    /// polling (`wait_for`).
    paused_tx: watch::Sender<bool>,
}

impl DeployQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(QueueInner::default()),
            notify: Notify::new(),
            slot: Arc::new(Semaphore::new(1)),
            paused_tx: watch::channel(false).0,
        })
    }

    /// Trava o estado da fila. Um `Mutex` envenenado (outra thread entrou em pânico segurando a
    /// trava) não derruba a fila inteira: o estado é só listas de ids, que seguem válidas, então
    /// segue-se com o que havia em vez de propagar o pânico para todo deploy seguinte.
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registra de quais deployments `dep_id` depende (um lote). Chamar **antes** de `enqueue`: o worker
    /// pode puxá-lo assim que entrar na fila.
    pub fn set_waits(&self, dep_id: &str, waits: Vec<String>) {
        if !waits.is_empty() {
            self.lock().waits.insert(dep_id.to_string(), waits);
        }
    }

    /// Tira (e devolve) os deployments de que `dep_id` depende.
    pub fn take_waits(&self, dep_id: &str) -> Vec<String> {
        self.lock().waits.remove(dep_id).unwrap_or_default()
    }

    /// Enfileira um deployment e acorda o worker. Idempotente: ignora se o id já
    /// está na fila ou rodando.
    pub fn enqueue(&self, dep_id: String) {
        {
            let mut g = self.lock();
            if g.running.as_deref() == Some(dep_id.as_str()) || g.queued.contains(&dep_id) {
                return;
            }
            g.queued.push_back(dep_id);
        }
        self.notify.notify_one();
    }

    /// Enfileira um item que **não é deploy** (a execução de um job, o aquecimento de um): entra na
    /// mesma lista dos deploys, na mesma ordem de execução, e pode ser promovido, reordenado ou
    /// tirado da fila como eles. `run` roda com a vaga única em mãos; `on_cancel` roda se o item
    /// for tirado da fila antes de a vez chegar ([`DeployQueue::cancel_queued`]).
    pub fn enqueue_task(
        &self,
        id: String,
        info: QueueTaskInfo,
        run: QueueTaskFn,
        on_cancel: QueueTaskFn,
    ) {
        {
            let mut g = self.lock();
            g.tasks.insert(id.clone(), QueueTask { run, on_cancel });
            g.infos.insert(id.clone(), info);
        }
        self.enqueue(id);
    }

    /// Tira (e devolve) o que rodar do item `id`, se ele é um que não é deploy. O worker chama
    /// depois de pegar a vaga: `None` quer dizer que é um deploy.
    pub fn take_task(&self, id: &str) -> Option<QueueTask> {
        self.lock().tasks.remove(id)
    }

    /// Os itens que não são deploy e ainda não terminaram (esperando ou rodando), por id.
    pub fn task_infos(&self) -> HashMap<String, QueueTaskInfo> {
        self.lock().infos.clone()
    }

    /// Tira da fila um item que não é deploy **e ainda não começou**. Devolve o `on_cancel` dele
    /// (quem chamou o roda e fecha a execução como cancelada); `None` se o id não está esperando
    /// (já rodando, já terminado ou é um deploy).
    pub fn cancel_queued(&self, id: &str) -> Option<QueueTaskFuture> {
        let task = {
            let mut g = self.lock();
            // Só item que não é deploy: um deploy não pode sair da fila por aqui (tem o caminho dele).
            if !g.tasks.contains_key(id) {
                return None;
            }
            let pos = g.queued.iter().position(|d| d == id)?;
            g.queued.remove(pos);
            g.infos.remove(id);
            g.tasks.remove(id)?
        };
        Some((task.on_cancel)())
    }

    /// Remove um deployment **enfileirado** (não afeta o que está rodando —
    /// abortar o running é papel do `active_deploys`). Retorna `true` se removeu.
    pub fn remove_queued(&self, dep_id: &str) -> bool {
        let mut g = self.lock();
        if let Some(pos) = g.queued.iter().position(|d| d == dep_id) {
            g.queued.remove(pos);
            g.tasks.remove(dep_id);
            g.infos.remove(dep_id);
            true
        } else {
            false
        }
    }

    /// Move um enfileirado para o início da fila ("furar fila").
    pub fn promote(&self, dep_id: &str) {
        let mut g = self.lock();
        if let Some(pos) = g.queued.iter().position(|d| d == dep_id)
            && let Some(d) = g.queued.remove(pos)
        {
            g.queued.push_front(d);
        }
    }

    /// Reordena a fila para a ordem dada. Ids desconhecidos são ignorados;
    /// enfileirados não citados vão ao fim, preservando a ordem relativa.
    pub fn reorder(&self, order: &[String]) {
        let mut g = self.lock();
        let mut remaining: VecDeque<String> = std::mem::take(&mut g.queued);
        let mut next = VecDeque::with_capacity(remaining.len());
        for id in order {
            if let Some(pos) = remaining.iter().position(|d| d == id)
                && let Some(d) = remaining.remove(pos)
            {
                next.push_back(d);
            }
        }
        next.extend(remaining);
        g.queued = next;
    }

    /// Pausa/retoma a fila. Ao retomar, acorda o worker e os jobs que esperavam.
    pub fn set_paused(&self, paused: bool) {
        {
            let mut g = self.lock();
            g.paused = paused;
        }
        self.paused_tx.send_replace(paused);
        if !paused {
            self.notify.notify_one();
        }
    }

    /// Snapshot para o handler de status: `(running, queued_em_ordem, paused)`.
    pub fn snapshot(&self) -> (Option<String>, Vec<String>, bool) {
        let g = self.lock();
        (
            g.running.clone(),
            g.queued.iter().cloned().collect(),
            g.paused,
        )
    }

    /// Nada rodando, nada esperando e a fila não está pausada: um item novo começa na hora.
    pub fn is_idle(&self) -> bool {
        let g = self.lock();
        g.running.is_none() && g.queued.is_empty() && !g.paused
    }

    /// Há algo na fila e ela não está pausada?
    fn has_ready(&self) -> bool {
        let g = self.lock();
        !g.paused && !g.queued.is_empty()
    }

    /// Espera a vaga e o próximo deploy da fila, nesta ordem: só depois de ter a
    /// vaga o deployment sai da fila e vira `running` — com um job em execução,
    /// ele segue aparecendo como "na fila" (e ainda pode ser removido, promovido
    /// ou reordenado), em vez de aparecer como rodando sem estar.
    pub async fn acquire_next(&self) -> (OwnedSemaphorePermit, String) {
        loop {
            // Espera haver algo pronto SEM segurar a vaga (senão os jobs travam).
            while !self.has_ready() {
                self.wait().await;
            }
            let permit = self.slot.clone().acquire_owned().await.expect(
                "o semáforo da fila vive na própria fila e nunca é fechado (`close` não é chamado)",
            );
            // Enquanto esperava a vaga, o item pode ter sido removido ou a fila
            // pausada: `take_next` reconfere e devolve `None` nesses casos.
            if let Some(id) = self.take_next() {
                return (permit, id);
            }
            drop(permit);
        }
    }

    /// A vaga para um **job avulso**: espera a fila estar despausada e a vaga
    /// livre, em ordem de chegada. Quem a solta (ao terminar o job) é o `Drop`
    /// do permit devolvido.
    pub async fn acquire_job_slot(&self) -> OwnedSemaphorePermit {
        let mut paused = self.paused_tx.subscribe();
        loop {
            // O `Err` só ocorreria com o `Sender` destruído — ele vive na fila.
            let _ = paused.wait_for(|p| !*p).await;
            let permit = self.slot.clone().acquire_owned().await.expect(
                "o semáforo da fila vive na própria fila e nunca é fechado (`close` não é chamado)",
            );
            // A fila pode ter sido pausada enquanto esperávamos a vaga.
            if !*paused.borrow() {
                return permit;
            }
            drop(permit);
        }
    }

    /// Versão sem espera de [`acquire_job_slot`]: `None` se a vaga está ocupada
    /// ou a fila pausada (o chamador então avisa "aguardando a vez" e espera).
    pub fn try_job_slot_now(&self) -> Option<OwnedSemaphorePermit> {
        if *self.paused_tx.borrow() {
            return None;
        }
        self.slot.clone().try_acquire_owned().ok()
    }

    /// Tira o próximo da fila e marca como running. `None` se vazia OU pausada.
    fn take_next(&self) -> Option<String> {
        let mut g = self.lock();
        if g.paused {
            return None;
        }
        let next = g.queued.pop_front();
        if let Some(id) = &next {
            g.running = Some(id.clone());
        }
        next
    }

    fn clear_running(&self) {
        let mut g = self.lock();
        if let Some(id) = g.running.take() {
            g.infos.remove(&id);
        }
    }

    async fn wait(&self) {
        self.notify.notified().await;
    }
}

/// Worker único da fila global. Spawnado uma vez no startup. Puxa um deploy por
/// vez, roda até terminar (ou ser abortado) e só então pega o próximo.
pub async fn run_worker(state: DeployContext) {
    let queue = state.deploy_queue.clone();
    info!("deploy queue worker iniciado");
    loop {
        // Espera algo para rodar, a fila não estar pausada e a vaga estar livre
        // (um job avulso pode estar executando). `Notify` guarda um permit se
        // `notify_one` chegar antes do `wait` — sem wakeup perdido.
        let (slot, dep_id) = queue.acquire_next().await;

        // running mudou → avisa a GUI.
        state.bus.publish(Event::DeployQueueChanged);
        match queue.take_task(&dep_id) {
            // Job ou aquecimento: roda com a vaga que o worker já tem.
            Some(task) => (task.run)().await,
            None => run_one(&state, &dep_id).await,
        }
        queue.clear_running();
        // Solta a vaga só depois de limpar `running`: quem entra em seguida (um
        // job esperando) não enxerga um deploy "rodando" que já terminou.
        drop(slot);
        state.bus.publish(Event::DeployQueueChanged);
    }
}

/// Roda um deployment: marca o serviço como `Deploying`, spawna o executor como
/// task (guardando o `AbortHandle` em `active_deploys` para o `deploy_abort`) e
/// aguarda terminar.
async fn run_one(state: &DeployContext, dep_id: &str) {
    // Deploy de um lote: se uma dependência dele não terminou bem, ele não roda (nem faz sentido: a
    // aplicação subiria sem o banco que ela espera). Ver `features::deploy::tree`.
    let waits = state.deploy_queue.take_waits(dep_id);
    if !waits.is_empty()
        && let Some(motivo) =
            crate::features::deploy::tree::failed_dependency(&state.db, &waits).await
    {
        crate::features::deploy::tree::skip_for_dependency(state, dep_id, &motivo).await;
        return;
    }

    // Marca o serviço como Deploying (deploy_start deixou em Queued).
    if let Ok(Some(dep)) = crate::store::deployments::get(&state.db, dep_id).await {
        let _ = crate::store::services::update_status(
            &state.db,
            &dep.service_id,
            &ServiceStatus::Deploying,
            None,
        )
        .await;
        state.bus.publish(Event::ServiceStatusChanged {
            service_id: dep.service_id.clone(),
            status: ServiceStatus::Deploying,
        });
    } else {
        warn!(deployment_id = %dep_id, "deploy queue: deployment sumiu antes de rodar");
        return;
    }

    let executor = Arc::new(DeployExecutor {
        db: state.db.clone(),
        docker: state.docker.clone(),
        ingress: state.ingress.clone(),
        bus: state.bus.clone(),
        secrets: state.secrets.clone(),
        tls: state.tls.clone(),
        db_path: state.db_path.clone(),
        drain_secs: state.drain_secs,
        registry_internal_token: state.registry_internal_token.clone(),
    });

    let dep_owned = dep_id.to_string();
    let handle = tokio::spawn(async move { executor.run(dep_owned).await });
    if let Ok(mut map) = state.active_deploys.lock() {
        map.insert(dep_id.to_string(), handle.abort_handle());
    }
    // Aguarda concluir (ou ser abortado via active_deploys → JoinError).
    let _ = handle.await;
    if let Ok(mut map) = state.active_deploys.lock() {
        map.remove(dep_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(q: &DeployQueue) -> Vec<String> {
        q.snapshot().1
    }

    #[test]
    fn mutex_envenenado_nao_derruba_a_fila() {
        let q = DeployQueue::new();
        q.enqueue("a".into());
        // uma thread entra em pânico segurando a trava
        let q2 = q.clone();
        let _ = std::thread::spawn(move || {
            let _g = q2.inner.lock().unwrap();
            panic!("pânico de propósito com a trava na mão");
        })
        .join();
        assert!(q.inner.is_poisoned());
        // a fila segue funcionando, com o que já havia
        q.enqueue("b".into());
        assert_eq!(queued(&q), ["a", "b"]);
    }

    #[test]
    fn enqueue_orders_fifo_and_dedups() {
        let q = DeployQueue::new();
        q.enqueue("a".into());
        q.enqueue("b".into());
        q.enqueue("a".into()); // duplicata: ignorada
        assert_eq!(queued(&q), vec!["a", "b"]);
    }

    #[test]
    fn promote_moves_to_front() {
        let q = DeployQueue::new();
        for id in ["a", "b", "c"] {
            q.enqueue(id.into());
        }
        q.promote("c");
        assert_eq!(queued(&q), vec!["c", "a", "b"]);
        // promover um id inexistente é no-op.
        q.promote("zzz");
        assert_eq!(queued(&q), vec!["c", "a", "b"]);
    }

    #[test]
    fn reorder_applies_and_appends_omitted() {
        let q = DeployQueue::new();
        for id in ["a", "b", "c"] {
            q.enqueue(id.into());
        }
        // "b" omitido e "zzz" desconhecido: b vai ao fim, zzz ignorado.
        q.reorder(&["c".into(), "zzz".into(), "a".into()]);
        assert_eq!(queued(&q), vec!["c", "a", "b"]);
    }

    #[test]
    fn remove_queued_reports_and_removes() {
        let q = DeployQueue::new();
        q.enqueue("a".into());
        q.enqueue("b".into());
        assert!(q.remove_queued("a"));
        assert!(!q.remove_queued("nope"));
        assert_eq!(queued(&q), vec!["b"]);
    }

    #[test]
    fn take_next_respects_pause_and_marks_running() {
        let q = DeployQueue::new();
        q.enqueue("a".into());
        q.set_paused(true);
        assert!(q.take_next().is_none(), "pausada não entrega");
        q.set_paused(false);
        assert_eq!(q.take_next().as_deref(), Some("a"));
        let (running, queued_now, paused) = q.snapshot();
        assert_eq!(running.as_deref(), Some("a"));
        assert!(queued_now.is_empty());
        assert!(!paused);
        q.clear_running();
        assert!(q.snapshot().0.is_none());
    }

    // ── vaga única compartilhada entre deploys e jobs avulsos ────────────────

    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    const ESPERA: Duration = Duration::from_millis(60);
    const LIMITE: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn job_e_deploy_nao_executam_juntos() {
        let q = DeployQueue::new();
        q.enqueue("a".into());

        // Um job em execução segura a vaga: o deploy NÃO vira "rodando" e continua
        // na fila (visível e removível), em vez de aparecer rodando sem estar.
        let job = q.acquire_job_slot().await;
        let q2 = q.clone();
        let worker = tokio::spawn(async move { q2.acquire_next().await });
        sleep(ESPERA).await;
        assert!(
            !worker.is_finished(),
            "o deploy não pode começar com um job rodando"
        );
        let (running, queued, _) = q.snapshot();
        assert!(running.is_none(), "ainda não está rodando");
        assert_eq!(queued, vec!["a"], "segue na fila");

        // O job termina: o deploy assume a vaga.
        drop(job);
        let (permit, id) = timeout(LIMITE, worker).await.unwrap().unwrap();
        assert_eq!(id, "a");
        assert_eq!(q.snapshot().0.as_deref(), Some("a"));

        // E agora é o job que espera o deploy.
        assert!(q.try_job_slot_now().is_none(), "vaga ocupada pelo deploy");
        let q3 = q.clone();
        let job2 = tokio::spawn(async move { q3.acquire_job_slot().await });
        sleep(ESPERA).await;
        assert!(
            !job2.is_finished(),
            "o job não pode começar com um deploy rodando"
        );
        q.clear_running();
        drop(permit);
        let _vaga = timeout(LIMITE, job2)
            .await
            .expect("o job entra quando o deploy termina")
            .unwrap();
    }

    #[tokio::test]
    async fn dois_jobs_nao_executam_juntos_e_entram_em_ordem_de_chegada() {
        let q = DeployQueue::new();
        let primeiro = q.acquire_job_slot().await;
        let ordem = Arc::new(Mutex::new(Vec::new()));
        let mut tarefas = Vec::new();
        for n in 1..=3 {
            let (q, ordem) = (q.clone(), ordem.clone());
            tarefas.push(tokio::spawn(async move {
                let _vaga = q.acquire_job_slot().await;
                ordem.lock().unwrap().push(n);
                sleep(Duration::from_millis(20)).await;
            }));
            sleep(Duration::from_millis(20)).await; // garante a ordem de chegada
        }
        assert!(
            ordem.lock().unwrap().is_empty(),
            "nenhum entra com a vaga ocupada"
        );
        drop(primeiro);
        for t in tarefas {
            timeout(LIMITE, t).await.unwrap().unwrap();
        }
        assert_eq!(*ordem.lock().unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn pausa_segura_os_jobs_ate_retomar() {
        let q = DeployQueue::new();
        q.set_paused(true);
        assert!(
            q.try_job_slot_now().is_none(),
            "pausada: não entrega, mesmo com a vaga livre"
        );
        let q2 = q.clone();
        let job = tokio::spawn(async move { q2.acquire_job_slot().await });
        sleep(ESPERA).await;
        assert!(!job.is_finished(), "o job espera a fila ser retomada");
        q.set_paused(false);
        let _vaga = timeout(LIMITE, job)
            .await
            .expect("retomada libera o job")
            .unwrap();
    }

    #[tokio::test]
    async fn pausa_durante_a_espera_da_vaga_devolve_a_vaga() {
        // O job já está esperando a vaga (ocupada por um deploy) quando a fila é
        // pausada: ao receber a vaga ele a devolve e espera a retomada.
        let q = DeployQueue::new();
        q.enqueue("a".into());
        let (permit, _) = q.acquire_next().await;
        let q2 = q.clone();
        let job = tokio::spawn(async move { q2.acquire_job_slot().await });
        sleep(ESPERA).await;
        q.set_paused(true);
        q.clear_running();
        drop(permit);
        sleep(ESPERA).await;
        assert!(!job.is_finished(), "pausada: não pode começar");
        assert!(q.try_job_slot_now().is_none());
        q.set_paused(false);
        let _vaga = timeout(LIMITE, job).await.expect("retomou").unwrap();
    }

    #[tokio::test]
    async fn item_removido_enquanto_espera_a_vaga_nao_e_entregue() {
        let q = DeployQueue::new();
        q.enqueue("a".into());
        let job = q.acquire_job_slot().await;
        let q2 = q.clone();
        let worker = tokio::spawn(async move { q2.acquire_next().await });
        sleep(ESPERA).await;
        assert!(q.remove_queued("a"), "ainda está na fila, dá para remover");
        drop(job);
        sleep(ESPERA).await;
        assert!(!worker.is_finished(), "sem item, o worker segue esperando");
        assert!(
            q.try_job_slot_now().is_some(),
            "e não fica com a vaga presa"
        );
        q.enqueue("b".into());
        let (_, id) = timeout(LIMITE, worker).await.unwrap().unwrap();
        assert_eq!(id, "b");
    }

    #[tokio::test]
    async fn worker_espera_pausa_e_fila_vazia_sem_segurar_a_vaga() {
        let q = DeployQueue::new();
        let q2 = q.clone();
        let worker = tokio::spawn(async move { q2.acquire_next().await });
        sleep(ESPERA).await;
        // Sem nada na fila o worker espera, mas a vaga tem de continuar livre.
        assert!(!worker.is_finished());
        assert!(
            q.try_job_slot_now().is_some(),
            "vaga livre: jobs não podem ficar presos"
        );
        worker.abort();
    }

    // ── itens que não são deploy (jobs, aquecimentos) ────────────────────────────────────────

    fn tarefa(
        q: &DeployQueue,
        id: &str,
        kind: QueueItemKind,
        rodou: Arc<std::sync::atomic::AtomicU32>,
    ) {
        let (r, c) = (rodou.clone(), rodou);
        q.enqueue_task(
            id.into(),
            QueueTaskInfo {
                kind,
                job_id: "job_1".into(),
            },
            Box::new(move || {
                Box::pin(async move {
                    r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            }),
            Box::new(move || {
                Box::pin(async move {
                    c.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
                })
            }),
        );
    }

    #[test]
    fn job_entra_na_mesma_ordem_do_deploy_e_pode_ser_promovido() {
        let q = DeployQueue::new();
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        q.enqueue("dep_1".into());
        tarefa(&q, "jrun_1", QueueItemKind::Job, n.clone());
        q.enqueue("dep_2".into());
        assert_eq!(queued(&q), ["dep_1", "jrun_1", "dep_2"]);
        q.promote("jrun_1");
        assert_eq!(queued(&q), ["jrun_1", "dep_1", "dep_2"]);
        q.reorder(&["dep_2".into(), "jrun_1".into()]);
        assert_eq!(queued(&q), ["dep_2", "jrun_1", "dep_1"]);
        let infos = q.task_infos();
        assert_eq!(infos.len(), 1, "só o job é item que não é deploy");
        assert_eq!(infos["jrun_1"].kind, QueueItemKind::Job);
    }

    #[tokio::test]
    async fn worker_acha_a_tarefa_do_job_e_ela_roda_uma_vez() {
        let q = DeployQueue::new();
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        tarefa(&q, "jrun_1", QueueItemKind::Job, n.clone());
        let (_vaga, id) = timeout(LIMITE, q.acquire_next()).await.unwrap();
        assert_eq!(id, "jrun_1");
        assert_eq!(
            q.snapshot().0.as_deref(),
            Some("jrun_1"),
            "aparece como rodando"
        );
        let task = q.take_task(&id).expect("é um job, não um deploy");
        (task.run)().await;
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(q.take_task(&id).is_none(), "não roda duas vezes");
        assert!(
            q.task_infos().contains_key("jrun_1"),
            "enquanto roda, a tela ainda sabe o que é"
        );
        q.clear_running();
        assert!(q.task_infos().is_empty(), "terminou: some");
    }

    #[tokio::test]
    async fn tirar_o_job_da_fila_roda_o_on_cancel_e_nao_o_run() {
        let q = DeployQueue::new();
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        tarefa(&q, "jrun_1", QueueItemKind::Job, n.clone());
        let cancelar = q.cancel_queued("jrun_1").expect("estava esperando");
        cancelar.await;
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 100);
        assert!(queued(&q).is_empty());
        assert!(q.task_infos().is_empty());
        assert!(q.cancel_queued("jrun_1").is_none(), "já saiu");
    }

    #[test]
    fn cancel_queued_ignora_deploy_e_o_que_ja_esta_rodando() {
        let q = DeployQueue::new();
        q.enqueue("dep_1".into());
        assert!(
            q.cancel_queued("dep_1").is_none(),
            "deploy tem o caminho dele"
        );
        assert_eq!(queued(&q), ["dep_1"], "e continua na fila");
    }
}
