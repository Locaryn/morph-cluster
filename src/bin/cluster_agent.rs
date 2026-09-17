//! Agent Cluster — le processus de fond qui fait exister le réseau du
//! cluster : balise UDP, écoute des pairs, poignée de main HMAC, serveur RPC
//! prêté selon les préférences de la machine, catalogue des modèles partagés
//! et copie de ces modèles sur les postes clients.
//!
//! Démarré une fois (par le serveur MCP, au premier outil appelé), il tourne
//! ensuite tant que Locaryn tourne. Le serveur MCP et le lanceur du moteur
//! lui parlent en boucle locale — voir [`locaryn_plugin_cluster::agent_protocol`].
//!
//! Deux canaux réseau bien séparés :
//! - **local** (boucle locale, éphémère) : le serveur MCP et le lanceur s'y
//!   adressent, jamais un pair.
//! - **cluster** (port fixe [`DEFAULT_CONTROL_PORT`], visible du réseau) :
//!   les autres machines s'y présentent, après la balise UDP qui les a fait
//!   se reconnaître — ou directement, pour un poste inscrit par le serveur.
//!
//! Deux rôles possibles, jamais les deux :
//! - **coordinateur** : la machine du serveur Locaryn. Elle tient le
//!   catalogue des modèles partagés et les envoie à qui les demande.
//! - **poste client** : une machine connectée à ce serveur. Si la personne l'a
//!   permis, elle prête sa carte, sa mémoire, et héberge une copie des
//!   modèles partagés dans la limite de son quota.

use locaryn_plugin_cluster as cluster;
use locaryn_plugin_cluster::agent_protocol::{
    self, AgentHandle, AgentRequest, AgentResponse, MemberView, SharedModelView, SyncEntry,
};
use locaryn_plugin_cluster::catalog::{self, Catalog, LocalState, SharedModel, SyncLedger};
use locaryn_plugin_cluster::discovery::{
    Beacon, CapabilityAnnounce, ControlRequest, ControlResponse,
};
use locaryn_plugin_cluster::hmac_auth::ResponderHandshake;
use locaryn_plugin_cluster::identity::ClusterIdentity;
use locaryn_plugin_cluster::peer_channel::{self, read_json_line, write_json_line};
use locaryn_plugin_cluster::sharing::{self, RpcDevice, SharePrefs, SharingAnnounce};
use locaryn_plugin_cluster::{transfer, Capability, Peer};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Child;
use tokio::sync::{Mutex, Notify};

/// Un pair qui n'a pas répondu depuis ce délai est oublié : il réapparaîtra
/// à sa prochaine balise ou à sa prochaine connexion.
const OUBLI_PAIR_SECS: u64 = 90;

struct AgentState {
    identity: Option<ClusterIdentity>,
    peers: HashMap<String, Peer>,
    rpc_child: Option<Child>,
    rpc_port: Option<u16>,
    /// `None` : aucun choix enregistré (cluster monté à la main).
    prefs: Option<SharePrefs>,
    /// Les appareils que llama.cpp voit, lus une fois puis à la demande.
    devices: Vec<RpcDevice>,
    /// Les appareils passés à `-d` au dernier démarrage du serveur RPC.
    lent: Vec<String>,
    /// Pourquoi rien n'est prêté alors que la case est cochée.
    worker_problem: Option<String>,
    /// Coordinateur : le sien. Poste client : la dernière copie lue.
    catalog: Catalog,
    catalog_reachable: bool,
    sync_error: Option<String>,
    downloading: Option<(String, Arc<AtomicU64>)>,
    /// Les empreintes en cours de calcul, pour ne pas lancer deux lectures
    /// de 16 Go du même fichier.
    hashing: HashSet<String>,
}

type Shared = Arc<Mutex<AgentState>>;

#[derive(Clone)]
struct Agent {
    state: Shared,
    /// Réveille la synchronisation dès qu'un choix change, sans attendre le
    /// prochain passage.
    wake: Arc<Notify>,
}

#[tokio::main]
async fn main() {
    let state_dir = cluster::state_dir();
    let _ = std::fs::create_dir_all(&state_dir);

    let prefs = SharePrefs::load(&state_dir);
    let catalog = if prefs.as_ref().is_some_and(|p| p.coordinator) {
        Catalog::load(&state_dir)
    } else {
        Catalog::default()
    };
    let agent = Agent {
        state: Arc::new(Mutex::new(AgentState {
            identity: ClusterIdentity::load(&state_dir),
            peers: HashMap::new(),
            rpc_child: None,
            rpc_port: None,
            prefs,
            devices: Vec::new(),
            lent: Vec::new(),
            worker_problem: None,
            catalog,
            catalog_reachable: false,
            sync_error: None,
            downloading: None,
            hashing: HashSet::new(),
        })),
        wake: Arc::new(Notify::new()),
    };

    // Canal local : boucle locale, port choisi par le système. Le serveur MCP
    // et le lanceur le lisent depuis `agent.json`, jamais deviné.
    let local_listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[cluster-agent] canal local impossible à ouvrir : {e}");
            std::process::exit(1);
        }
    };
    let local_port = local_listener.local_addr().map(|a| a.port()).unwrap_or(0);

    let handle = AgentHandle {
        pid: std::process::id(),
        control_port: local_port,
    };
    if let Err(e) = std::fs::write(
        agent_protocol::handle_file(&state_dir),
        serde_json::to_vec_pretty(&handle).unwrap_or_default(),
    ) {
        eprintln!("[cluster-agent] fiche d'agent impossible à écrire : {e}");
        std::process::exit(1);
    }

    // Canal cluster : port fixe par défaut, mais jamais bloquant — un port
    // déjà pris (deux agents sur la même machine, improbable mais possible en
    // développement) fait basculer sur un port libre, annoncé par la balise.
    let peer_listener = match TcpListener::bind(("0.0.0.0", cluster::DEFAULT_CONTROL_PORT)).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind(("0.0.0.0", 0))
            .await
            .expect("aucun port TCP disponible pour le canal du cluster"),
    };
    let peer_port = peer_listener.local_addr().map(|a| a.port()).unwrap_or(0);
    eprintln!(
        "[cluster-agent] prêt — local:{local_port} cluster:{peer_port} pid:{}",
        handle.pid
    );

    tokio::spawn(run_local_ipc(local_listener, agent.clone()));
    tokio::spawn(run_peer_listener(peer_listener, agent.clone()));
    tokio::spawn(run_beacon_sender(peer_port, agent.state.clone()));
    tokio::spawn(run_beacon_listener(agent.clone(), peer_port));
    tokio::spawn(run_peer_refresh(agent.clone()));
    tokio::spawn(run_sync(agent.clone()));

    // Ce que les préférences enregistrées demandent : prêter dès le démarrage,
    // recalculer les empreintes que la dernière session n'a pas finies.
    let demarrage = agent.clone();
    tokio::spawn(async move {
        refresh_devices(&demarrage.state).await;
        apply_prefs(&demarrage.state).await;
        rehash_catalog(&demarrage.state).await;
    });

    // Le processus vit tant qu'on ne lui demande pas explicitement de
    // s'arrêter (requête `Shutdown` sur le canal local) ou qu'un signal
    // d'interruption arrive — pas de minuterie d'inactivité : un cluster sans
    // trafic pendant un moment reste un cluster.
    tokio::signal::ctrl_c().await.ok();
    let _ = std::fs::remove_file(agent_protocol::handle_file(&state_dir));
}

// ============================================================================
// Canal local : réponses aux commandes du serveur MCP et du lanceur.
// ============================================================================

async fn run_local_ipc(listener: TcpListener, agent: Agent) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let agent = agent.clone();
        tokio::spawn(async move {
            let (read_half, mut write_half) = socket.into_split();
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let response = match serde_json::from_str::<AgentRequest>(&line) {
                    Ok(req) => handle_local_request(req, &agent).await,
                    Err(e) => AgentResponse::Error {
                        message: format!("requête illisible : {e}"),
                    },
                };
                let bytes = agent_protocol::encode_response(&response);
                if write_half.write_all(&bytes).await.is_err() {
                    break;
                }
                if matches!(response, AgentResponse::ShuttingDown) {
                    std::process::exit(0);
                }
            }
        });
    }
}

async fn handle_local_request(req: AgentRequest, agent: &Agent) -> AgentResponse {
    let state = &agent.state;
    match req {
        AgentRequest::Status => {
            let capability = local_capability(state).await;
            let s = state.lock().await;
            AgentResponse::Status {
                cluster_name: s.identity.as_ref().map(|i| i.name.clone()),
                cluster_id: s.identity.as_ref().map(|i| i.cluster_id.clone()),
                self_capability: capability,
                worker_active: s.rpc_child.is_some(),
                rpc_port: s.rpc_port,
                peer_count: s.peers.len(),
            }
        }
        AgentRequest::CreateCluster { name } => {
            let identity = ClusterIdentity::generate(&name);
            if let Err(e) = identity.save(&cluster::state_dir()) {
                return AgentResponse::Error {
                    message: format!("identité impossible à enregistrer : {e}"),
                };
            }
            let code = identity.pairing_code();
            let mut s = state.lock().await;
            s.identity = Some(identity);
            s.peers.clear();
            AgentResponse::Created { pairing_code: code }
        }
        AgentRequest::JoinCluster {
            pairing_code,
            coordinator,
        } => join_cluster(agent, &pairing_code, coordinator).await,
        AgentRequest::PairingCode => {
            let s = state.lock().await;
            match &s.identity {
                Some(identity) => AgentResponse::PairingCode {
                    pairing_code: identity.pairing_code(),
                },
                None => AgentResponse::Error {
                    message: "aucun cluster : appelez d'abord cluster_create ou cluster_join"
                        .to_string(),
                },
            }
        }
        AgentRequest::Peers => {
            let s = state.lock().await;
            AgentResponse::Peers {
                peers: s.peers.values().cloned().collect(),
            }
        }
        AgentRequest::WorkerStart => start_worker(state).await,
        AgentRequest::WorkerStop => stop_worker(state).await,
        AgentRequest::Plan { model_size_gb } => {
            let local_vram = local_capability(state).await.free_vram_gb;
            let peers: Vec<Peer> = state.lock().await.peers.values().cloned().collect();
            let plan = cluster::plan::compute(model_size_gb, local_vram, &peers, None);
            AgentResponse::Plan { plan }
        }
        AgentRequest::Bench { peer_id } => bench_peer(&peer_id, state).await,
        AgentRequest::Enroll => enroll(state).await,
        AgentRequest::Leave => leave(agent).await,
        AgentRequest::ShareGet => {
            if state.lock().await.devices.is_empty() {
                refresh_devices(state).await;
            }
            share_view(state).await
        }
        AgentRequest::ShareSet { prefs } => set_prefs(agent, prefs).await,
        AgentRequest::SharedModels => shared_models_view(state).await,
        AgentRequest::ShareModel { file, enabled } => share_model(state, &file, enabled).await,
        AgentRequest::SyncStatus => sync_view(state).await,
        AgentRequest::Shutdown => {
            stop_worker(state).await;
            AgentResponse::ShuttingDown
        }
    }
}

// ============================================================================
// Inscription : rejoindre, coordonner, quitter.
// ============================================================================

async fn join_cluster(agent: &Agent, code: &str, coordinator: Option<String>) -> AgentResponse {
    let identity = match ClusterIdentity::from_pairing_code(code) {
        Ok(i) => i,
        Err(message) => return AgentResponse::Error { message },
    };
    let dir = cluster::state_dir();
    if let Err(e) = identity.save(&dir) {
        return AgentResponse::Error {
            message: format!("identité impossible à enregistrer : {e}"),
        };
    }
    let name = identity.name.clone();
    let hote = coordinator
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    {
        let mut s = agent.state.lock().await;
        let meme_cluster = s
            .identity
            .as_ref()
            .is_some_and(|i| i.cluster_id == identity.cluster_id);
        s.identity = Some(identity);
        if !meme_cluster {
            s.peers.clear();
        }
        if let Some(hote) = &hote {
            let mut prefs = s.prefs.clone().unwrap_or_default();
            prefs.coordinator_host = Some(hote.clone());
            prefs.coordinator = false;
            if let Err(e) = prefs.save(&dir) {
                return AgentResponse::Error {
                    message: format!("préférences impossibles à enregistrer : {e}"),
                };
            }
            s.prefs = Some(prefs);
        }
    }
    if let Some(hote) = hote {
        let adresse = control_address(&hote);
        let state = agent.state.clone();
        tokio::spawn(async move {
            if let Err(e) = pair_with(&adresse, &state).await {
                eprintln!("[cluster-agent] coordinateur {adresse} injoignable : {e}");
            }
        });
    }
    agent.wake.notify_one();
    AgentResponse::Joined { cluster_name: name }
}

async fn enroll(state: &Shared) -> AgentResponse {
    let dir = cluster::state_dir();
    let mut s = state.lock().await;
    if s.identity.is_none() {
        let identity = ClusterIdentity::generate(&hostname());
        if let Err(e) = identity.save(&dir) {
            return AgentResponse::Error {
                message: format!("identité impossible à enregistrer : {e}"),
            };
        }
        s.identity = Some(identity);
    }
    let mut prefs = s.prefs.clone().unwrap_or_default();
    if !prefs.coordinator {
        prefs.coordinator = true;
        prefs.coordinator_host = None;
        if let Err(e) = prefs.save(&dir) {
            return AgentResponse::Error {
                message: format!("préférences impossibles à enregistrer : {e}"),
            };
        }
        s.catalog = Catalog::load(&dir);
    }
    s.prefs = Some(prefs);
    let identity = s.identity.as_ref().expect("identité posée juste au-dessus");
    AgentResponse::Enrolled {
        cluster_name: identity.name.clone(),
        pairing_code: identity.pairing_code(),
    }
}

async fn leave(agent: &Agent) -> AgentResponse {
    stop_worker(&agent.state).await;
    let dir = cluster::state_dir();
    let _ = std::fs::remove_file(dir.join("identity.json"));
    {
        let mut s = agent.state.lock().await;
        s.identity = None;
        s.peers.clear();
        s.catalog = Catalog::default();
        s.catalog_reachable = false;
        s.sync_error = None;
        let mut prefs = s.prefs.clone().unwrap_or_default();
        prefs.enabled = false;
        prefs.coordinator = false;
        prefs.coordinator_host = None;
        let _ = prefs.save(&dir);
        s.prefs = Some(prefs);
    }
    remove_synced_copies(None).await;
    agent.wake.notify_one();
    AgentResponse::Left
}

fn control_address(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') && host.matches(':').count() == 1 {
        host.to_string()
    } else {
        format!("{host}:{}", cluster::DEFAULT_CONTROL_PORT)
    }
}

// ============================================================================
// Partage de ressources : préférences, appareils, serveur RPC.
// ============================================================================

async fn set_prefs(agent: &Agent, mut prefs: SharePrefs) -> AgentResponse {
    let dir = cluster::state_dir();
    {
        let mut s = agent.state.lock().await;
        // Le rôle ne se choisit pas depuis le panneau : il vient de
        // l'inscription. Un panneau ancien qui renverrait ces champs ne peut
        // pas faire d'un poste client un coordinateur.
        if let Some(anciennes) = &s.prefs {
            prefs.coordinator = anciennes.coordinator;
            prefs.coordinator_host = anciennes.coordinator_host.clone();
        } else {
            prefs.coordinator = false;
            prefs.coordinator_host = None;
        }
        prefs.storage_gb = prefs.storage_gb.clamp(0.0, 100_000.0);
        if let Err(e) = prefs.save(&dir) {
            return AgentResponse::Error {
                message: format!("préférences impossibles à enregistrer : {e}"),
            };
        }
        s.prefs = Some(prefs);
    }
    if agent.state.lock().await.devices.is_empty() {
        refresh_devices(&agent.state).await;
    }
    apply_prefs(&agent.state).await;
    agent.wake.notify_one();
    share_view(&agent.state).await
}

/// Met le serveur RPC en accord avec les préférences : le démarre, l'arrête,
/// ou le relance si le choix des appareils a changé.
async fn apply_prefs(state: &Shared) {
    let (prefs, actif, lent_actuel, devices) = {
        let s = state.lock().await;
        (
            s.prefs.clone(),
            s.rpc_child.is_some(),
            s.lent.clone(),
            s.devices.clone(),
        )
    };
    let Some(prefs) = prefs else {
        return;
    };
    if !prefs.lends_compute() {
        if actif {
            stop_worker(state).await;
        }
        state.lock().await.worker_problem = None;
        return;
    }
    let voulus = sharing::devices_to_lend(&prefs, &devices).unwrap_or_default();
    if actif && voulus == lent_actuel {
        return;
    }
    if actif {
        stop_worker(state).await;
    }
    if let AgentResponse::Error { message } = start_worker(state).await {
        state.lock().await.worker_problem = Some(message);
    }
}

/// Demande à `ggml-rpc-server` la liste de ses appareils.
///
/// Il n'a pas d'option pour la lister : nommer un appareil qui n'existe pas
/// lui fait imprimer la liste complète avant de refuser — sans rien ouvrir
/// sur le réseau.
async fn refresh_devices(state: &Shared) {
    let Some(bin) = cluster::launch::find_binary("ggml-rpc-server") else {
        return;
    };
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["-d", "locaryn-liste-des-appareils"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    cluster::launch::hide_console(&mut cmd);
    let Ok(Ok(sortie)) = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await else {
        return;
    };
    let texte = format!(
        "{}\n{}",
        String::from_utf8_lossy(&sortie.stdout),
        String::from_utf8_lossy(&sortie.stderr)
    );
    let devices = sharing::parse_device_list(&texte);
    if !devices.is_empty() {
        state.lock().await.devices = devices;
    }
}

async fn share_view(state: &Shared) -> AgentResponse {
    let mut s = state.lock().await;
    reap_worker(&mut s);
    AgentResponse::Share {
        prefs: s.prefs.clone().unwrap_or_default(),
        devices: s.devices.clone(),
        worker_active: s.rpc_child.is_some(),
        rpc_port: s.rpc_port,
        problem: s.worker_problem.clone(),
    }
}

/// Oublie un serveur RPC mort de lui-même, pour ne pas annoncer un port qui
/// ne répond plus.
fn reap_worker(s: &mut AgentState) {
    if let Some(child) = s.rpc_child.as_mut() {
        if let Ok(Some(statut)) = child.try_wait() {
            s.rpc_child = None;
            s.rpc_port = None;
            s.lent.clear();
            s.worker_problem = Some(format!("ggml-rpc-server s'est arrêté ({statut})"));
        }
    }
}

async fn start_worker(state: &Shared) -> AgentResponse {
    let (prefs, devices) = {
        let mut s = state.lock().await;
        reap_worker(&mut s);
        if let Some(port) = s.rpc_port {
            return AgentResponse::WorkerStarted { port };
        }
        (s.prefs.clone(), s.devices.clone())
    };
    let Some(bin) = cluster::launch::find_binary("ggml-rpc-server") else {
        return AgentResponse::Error {
            message: "ggml-rpc-server introuvable — installez llama.cpp (cluster_status en dit \
                      plus) avant d'offrir cette machine au cluster"
                .to_string(),
        };
    };
    // Avec des préférences enregistrées, on prête exactement ce qui est
    // coché. Sans, c'est le cluster monté à la main : llama.cpp choisit.
    let lent = match &prefs {
        Some(p) if p.enabled => {
            if devices.is_empty() {
                return AgentResponse::Error {
                    message: "appareils de calcul non détectés — llama.cpp n'a rien répondu"
                        .to_string(),
                };
            }
            match sharing::devices_to_lend(p, &devices) {
                Ok(noms) => noms,
                Err(message) => return AgentResponse::Error { message },
            }
        }
        _ => Vec::new(),
    };
    // Port choisi par le système : deviné, il serait joignable par n'importe
    // qui sur le réseau avant même qu'un coordinateur authentifié ne le
    // demande.
    let listener = match std::net::TcpListener::bind(("0.0.0.0", 0)) {
        Ok(l) => l,
        Err(e) => {
            return AgentResponse::Error {
                message: format!("aucun port disponible pour ggml-rpc-server : {e}"),
            }
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    drop(listener);

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["--host", "0.0.0.0", "--port", &port.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if !lent.is_empty() {
        cmd.args(["-d", &lent.join(",")]);
    }
    // Le cache de tenseurs de llama.cpp rend un rechargement du même modèle
    // presque instantané. Il n'a sa place que sur une machine qui accepte
    // d'héberger des modèles, et dans le dossier de l'extension — sinon il
    // remplit `%LOCALAPPDATA%` ou `~/.cache` sur le disque système.
    if prefs.as_ref().is_some_and(SharePrefs::hosts_models) {
        let cache = cluster::extension_data_dir().join("rpc-cache");
        let _ = std::fs::create_dir_all(&cache);
        cmd.arg("-c").env("LLAMA_CACHE", &cache);
    }
    cluster::launch::hide_console(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return AgentResponse::Error {
                message: format!("{} : {e}", bin.display()),
            }
        }
    };
    // Un appareil refusé ou un port pris font sortir le programme aussitôt :
    // mieux vaut le voir ici qu'annoncer un port mort aux autres machines.
    tokio::time::sleep(Duration::from_millis(800)).await;
    if let Ok(Some(statut)) = child.try_wait() {
        return AgentResponse::Error {
            message: format!("ggml-rpc-server s'est arrêté au démarrage ({statut})"),
        };
    }
    let mut s = state.lock().await;
    s.rpc_child = Some(child);
    s.rpc_port = Some(port);
    s.lent = lent;
    s.worker_problem = None;
    AgentResponse::WorkerStarted { port }
}

async fn stop_worker(state: &Shared) -> AgentResponse {
    let mut s = state.lock().await;
    if let Some(mut child) = s.rpc_child.take() {
        let _ = child.kill().await;
    }
    s.rpc_port = None;
    s.lent.clear();
    AgentResponse::WorkerStopped
}

// ============================================================================
// Capacités locales : GPU, VRAM libre, présence des binaires llama.cpp.
// ============================================================================

async fn local_capability(state: &Shared) -> Capability {
    let (worker_active, devices, lent) = {
        let mut s = state.lock().await;
        reap_worker(&mut s);
        (s.rpc_child.is_some(), s.devices.clone(), s.lent.clone())
    };
    let ((gpu_name, free_vram_gb, total_vram_gb), (cpu_usage_percent, total_ram_gb, free_ram_gb)) =
        tokio::join!(probe_gpu(&devices), sample_cpu_ram());
    Capability {
        name: hostname(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        gpu_name,
        free_vram_gb,
        has_llama_server: cluster::launch::find_binary("llama-server").is_some(),
        has_rpc_server: worker_active || cluster::launch::find_binary("ggml-rpc-server").is_some(),
        offered_memory_gb: if worker_active {
            sharing::offered_memory_gb(&devices, &lent)
        } else {
            0.0
        },
        total_vram_gb,
        total_ram_gb,
        free_ram_gb,
        cpu_usage_percent,
    }
}

fn hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "machine".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOSTNAME")
            .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
            .unwrap_or_else(|_| "machine".to_string())
    }
}

/// Le nom de la carte, sa VRAM **libre** — pas la totale, qui ne dit rien de
/// ce qu'un autre modèle occupe déjà — et sa VRAM totale, pour que le
/// tableau de bord puisse dessiner une barre plutôt qu'un nombre seul.
///
/// Dans l'ordre : `nvidia-smi`, puis la liste des appareils de llama.cpp (qui
/// connaît aussi les cartes AMD, Intel et Apple), puis `LOCARYN_VRAM_GB` (la
/// VRAM totale mesurée par l'hôte, sans le libre — mieux vaut un total seul
/// qu'un nombre inventé pour le reste).
async fn probe_gpu(devices: &[RpcDevice]) -> (Option<String>, f32, f32) {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,memory.free,memory.total",
        "--format=csv,noheader,nounits",
    ]);
    cluster::launch::hide_console(&mut cmd);
    if let Ok(o) = cmd.output().await {
        if o.status.success() {
            let texte = String::from_utf8_lossy(&o.stdout);
            if let Some(ligne) = texte.lines().next() {
                let mut parts = ligne.split(',');
                let nom = parts.next().map(|s| s.trim().to_string());
                let libre_mio: Option<f32> = parts.next().and_then(|s| s.trim().parse().ok());
                let total_mio: Option<f32> = parts.next().and_then(|s| s.trim().parse().ok());
                if let (Some(nom), Some(libre_mio)) = (nom, libre_mio) {
                    return (
                        Some(nom),
                        libre_mio / 1024.0,
                        total_mio.unwrap_or(0.0) / 1024.0,
                    );
                }
            }
        }
    }
    if let Some(carte) = devices
        .iter()
        .filter(|d| !d.is_cpu())
        .max_by_key(|d| d.free_mib)
    {
        return (
            Some(carte.description.clone()),
            carte.free_mib as f32 / 1024.0,
            carte.total_mib as f32 / 1024.0,
        );
    }
    let total = std::env::var("LOCARYN_VRAM_GB")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0);
    (None, total, total)
}

/// CPU (moyenne tous cœurs, 0-100) et mémoire vive, mesurés en direct.
///
/// `sysinfo` exige deux lectures espacées pour une charge CPU qui veuille
/// dire quelque chose : la première pose la référence, l'attente minimale
/// de la bibliothèque sépare les deux prises.
async fn sample_cpu_ram() -> (f32, f32, f32) {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let cpu = sys.global_cpu_usage();
    let total_ram_gb = sys.total_memory() as f32 / 1_073_741_824.0;
    let free_ram_gb = sys.available_memory() as f32 / 1_073_741_824.0;
    (cpu, total_ram_gb, free_ram_gb)
}

// ============================================================================
// Catalogue des modèles partagés (coordinateur).
// ============================================================================

fn not_coordinator() -> AgentResponse {
    AgentResponse::Error {
        message: "cette machine ne coordonne pas de cluster : les modèles partagés se gèrent \
                  sur la machine du serveur Locaryn (cluster_enroll)"
            .to_string(),
    }
}

async fn shared_models_view(state: &Shared) -> AgentResponse {
    let models_dir = match cluster::require_models_dir() {
        Ok(d) => d,
        Err(message) => return AgentResponse::Error { message },
    };
    // Prise avant le verrou : `local_capability` verrouille `state` elle
    // aussi (pour relever les processus morts), la tenir ici bloquerait.
    let local_cap = local_capability(state).await;
    let s = state.lock().await;
    if !s.prefs.as_ref().is_some_and(|p| p.coordinator) {
        return not_coordinator();
    }
    let mut vues: Vec<SharedModelView> = catalog::scan_library(&models_dir)
        .into_iter()
        .map(|m| {
            let partage = s.catalog.get(&m.file);
            SharedModelView {
                hosted_by: hosts_of(&s, &m.file),
                shared: partage.is_some(),
                ready_to_copy: partage.is_some_and(|p| p.sha256.is_some()),
                file: m.file,
                size_bytes: m.size_bytes,
            }
        })
        .collect();
    // Un modèle partagé puis retiré de la bibliothèque reste visible : sinon
    // on ne pourrait plus cesser de le partager.
    for m in &s.catalog.models {
        if !vues.iter().any(|v| v.file == m.file) {
            vues.push(SharedModelView {
                file: m.file.clone(),
                size_bytes: m.size_bytes,
                shared: true,
                ready_to_copy: false,
                hosted_by: hosts_of(&s, &m.file),
            });
        }
    }
    let mut members: Vec<MemberView> = s
        .peers
        .values()
        .map(|p| {
            let partage = p.sharing.clone().unwrap_or_default();
            MemberView {
                name: p.capability.name.clone(),
                address: p.address.clone(),
                sharing: partage.enabled,
                gpu: partage.enabled && partage.gpu,
                ram: partage.enabled && partage.ram,
                storage: partage.enabled && partage.storage,
                lendable_gb: if p.rpc_port.is_some() {
                    p.capability.lendable_gb()
                } else {
                    0.0
                },
                last_seen_unix: p.last_seen_unix,
                is_self: false,
                os: p.capability.os.clone(),
                gpu_name: p.capability.gpu_name.clone(),
                free_vram_gb: p.capability.free_vram_gb,
                total_vram_gb: p.capability.total_vram_gb,
                free_ram_gb: p.capability.free_ram_gb,
                total_ram_gb: p.capability.total_ram_gb,
                cpu_usage_percent: p.capability.cpu_usage_percent,
            }
        })
        .collect();
    let local_partage = s.prefs.clone().unwrap_or_default();
    members.push(MemberView {
        name: local_cap.name.clone(),
        address: "local".into(),
        sharing: local_partage.enabled,
        gpu: local_partage.enabled && local_partage.gpu,
        ram: local_partage.enabled && local_partage.ram,
        storage: local_partage.enabled && local_partage.storage,
        lendable_gb: if s.rpc_child.is_some() {
            local_cap.lendable_gb()
        } else {
            0.0
        },
        last_seen_unix: unix_now(),
        is_self: true,
        os: local_cap.os.clone(),
        gpu_name: local_cap.gpu_name.clone(),
        free_vram_gb: local_cap.free_vram_gb,
        total_vram_gb: local_cap.total_vram_gb,
        free_ram_gb: local_cap.free_ram_gb,
        total_ram_gb: local_cap.total_ram_gb,
        cpu_usage_percent: local_cap.cpu_usage_percent,
    });
    members.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    AgentResponse::SharedModels {
        models: vues,
        members,
    }
}

fn hosts_of(s: &AgentState, file: &str) -> Vec<String> {
    let mut noms: Vec<String> = s
        .peers
        .values()
        .filter(|p| {
            p.sharing
                .as_ref()
                .is_some_and(|sh| sh.models_ready.iter().any(|f| f == file))
        })
        .map(|p| p.capability.name.clone())
        .collect();
    // Une même machine peut être connue sous deux adresses — celle de sa
    // balise et celle de sa connexion entrante : elle ne compte qu'une fois.
    noms.sort();
    noms.dedup();
    noms
}

async fn share_model(state: &Shared, file: &str, enabled: bool) -> AgentResponse {
    let models_dir = match cluster::require_models_dir() {
        Ok(d) => d,
        Err(message) => return AgentResponse::Error { message },
    };
    {
        let mut s = state.lock().await;
        if !s.prefs.as_ref().is_some_and(|p| p.coordinator) {
            return not_coordinator();
        }
        if enabled {
            if !s.catalog.contains(file) {
                let Some(meta) = catalog::local_path(&models_dir, file)
                    .and_then(|p| std::fs::metadata(p).ok())
                    .filter(|m| m.is_file())
                else {
                    return AgentResponse::Error {
                        message: format!("« {file} » n'est pas dans la bibliothèque de poids"),
                    };
                };
                s.catalog.models.push(SharedModel {
                    file: file.to_string(),
                    size_bytes: meta.len(),
                    sha256: None,
                    modified_unix: catalog::modified_unix(&meta),
                });
            }
        } else {
            s.catalog.models.retain(|m| m.file != file);
        }
        if let Err(e) = s.catalog.save(&cluster::state_dir()) {
            return AgentResponse::Error {
                message: format!("catalogue impossible à enregistrer : {e}"),
            };
        }
    }
    rehash_catalog(state).await;
    shared_models_view(state).await
}

/// Calcule, en arrière-plan, l'empreinte des modèles partagés qui n'en ont
/// pas — ou dont le fichier a changé depuis.
async fn rehash_catalog(state: &Shared) {
    let Ok(models_dir) = cluster::require_models_dir() else {
        return;
    };
    let a_calculer: Vec<SharedModel> = {
        let mut s = state.lock().await;
        if !s.prefs.as_ref().is_some_and(|p| p.coordinator) {
            return;
        }
        let mut modifie = false;
        for m in s.catalog.models.iter_mut() {
            let meta =
                catalog::local_path(&models_dir, &m.file).and_then(|p| std::fs::metadata(p).ok());
            if let Some(meta) = meta {
                let mtime = catalog::modified_unix(&meta);
                if meta.len() != m.size_bytes || mtime != m.modified_unix {
                    m.size_bytes = meta.len();
                    m.modified_unix = mtime;
                    m.sha256 = None;
                    modifie = true;
                }
            }
        }
        if modifie {
            let _ = s.catalog.save(&cluster::state_dir());
        }
        let liste: Vec<SharedModel> = s
            .catalog
            .models
            .iter()
            .filter(|m| m.sha256.is_none() && !s.hashing.contains(&m.file))
            .cloned()
            .collect();
        for m in &liste {
            s.hashing.insert(m.file.clone());
        }
        liste
    };
    for modele in a_calculer {
        let state = state.clone();
        let dir = models_dir.clone();
        tokio::spawn(async move {
            let chemin = catalog::local_path(&dir, &modele.file);
            let resultat = match chemin {
                Some(p) => tokio::task::spawn_blocking(move || catalog::sha256_file(&p))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string())),
                None => Err("nom refusé".to_string()),
            };
            let mut s = state.lock().await;
            s.hashing.remove(&modele.file);
            match resultat {
                Ok(hash) => {
                    // Toujours partagé, et toujours le même fichier : sinon
                    // cette empreinte ne décrit plus rien.
                    if let Some(m) = s.catalog.models.iter_mut().find(|m| {
                        m.file == modele.file
                            && m.size_bytes == modele.size_bytes
                            && m.modified_unix == modele.modified_unix
                    }) {
                        m.sha256 = Some(hash);
                        let _ = s.catalog.save(&cluster::state_dir());
                    }
                }
                Err(e) => eprintln!("[cluster-agent] empreinte de {} : {e}", modele.file),
            }
        });
    }
}

// ============================================================================
// Copie des modèles partagés (poste client).
// ============================================================================

/// Taille, sur ce poste, de chaque fichier du catalogue.
fn local_sizes(catalog: &Catalog, models_dir: &std::path::Path) -> BTreeMap<String, u64> {
    catalog
        .models
        .iter()
        .filter_map(|m| {
            let taille = catalog::local_path(models_dir, &m.file)
                .and_then(|p| std::fs::metadata(p).ok())
                .filter(|meta| meta.is_file())?
                .len();
            Some((m.file.clone(), taille))
        })
        .collect()
}

async fn run_sync(agent: Agent) {
    loop {
        let _ = tokio::time::timeout(Duration::from_secs(20), agent.wake.notified()).await;
        if let Err(e) = sync_once(&agent).await {
            agent.state.lock().await.sync_error = Some(e);
        }
    }
}

async fn sync_once(agent: &Agent) -> Result<(), String> {
    let (prefs, secret) = {
        let s = agent.state.lock().await;
        (
            s.prefs.clone(),
            s.identity.as_ref().map(|i| i.secret.clone()),
        )
    };
    let Some(prefs) = prefs else {
        return Ok(());
    };
    let (Some(hote), Some(secret)) = (prefs.coordinator_host.clone(), secret) else {
        // Ni poste client ni membre : s'il reste des copies, elles n'ont plus
        // de raison d'être.
        if !prefs.coordinator {
            remove_synced_copies(None).await;
        }
        return Ok(());
    };
    let models_dir = cluster::require_models_dir()?;
    let adresse = control_address(&hote);

    let catalogue = match peer_channel::authenticated_request(
        &adresse,
        &secret,
        &ControlRequest::Catalog,
    )
    .await
    {
        Ok(ControlResponse::Catalog { models }) => Catalog { models },
        Ok(ControlResponse::Error { message }) => {
            agent.state.lock().await.catalog_reachable = false;
            return Err(message);
        }
        Ok(_) => return Err("réponse inattendue du coordinateur".to_string()),
        Err(e) => {
            agent.state.lock().await.catalog_reachable = false;
            return Err(format!("coordinateur injoignable : {e}"));
        }
    };
    {
        let mut s = agent.state.lock().await;
        s.catalog = catalogue.clone();
        s.catalog_reachable = true;
        s.sync_error = None;
    }

    let dir = cluster::state_dir();
    let mut ledger = SyncLedger::load(&dir);
    let plan = catalog::plan_sync(
        &catalogue,
        &local_sizes(&catalogue, &models_dir),
        &ledger,
        prefs.hosts_models(),
        prefs.quota_bytes(),
    );
    for file in &plan.remove {
        remove_copy(&models_dir, file);
        ledger.files.remove(file);
    }
    if !plan.remove.is_empty() {
        ledger.save(&dir).map_err(|e| e.to_string())?;
    }

    // Un seul modèle par passage : le suivant partira au passage d'après,
    // avec un catalogue et des préférences relus entre-temps.
    let Some(modele) = plan.download.first().cloned() else {
        return Ok(());
    };
    let progres = Arc::new(AtomicU64::new(0));
    agent.state.lock().await.downloading = Some((modele.file.clone(), progres.clone()));
    let copie = transfer::fetch_model(&adresse, &secret, &models_dir, &modele, progres);
    tokio::pin!(copie);
    let resultat = loop {
        tokio::select! {
            r = &mut copie => break Some(r),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                // La personne a décoché le stockage, ou le modèle n'est plus
                // partagé : on arrête la copie au lieu de finir 15 Go pour
                // rien. Le fragment est supprimé au passage suivant.
                let s = agent.state.lock().await;
                let toujours = s.prefs.as_ref().is_some_and(SharePrefs::hosts_models);
                if !toujours {
                    break None;
                }
            }
        }
    };
    agent.state.lock().await.downloading = None;
    match resultat {
        Some(Ok(_)) => {
            ledger.files.insert(modele.file.clone());
            ledger.save(&dir).map_err(|e| e.to_string())?;
            agent.wake.notify_one();
            Ok(())
        }
        Some(Err(e)) => Err(e),
        None => {
            if let Some(p) = catalog::local_path(&models_dir, &modele.file) {
                let _ = std::fs::remove_file(transfer::part_path(&p));
            }
            Ok(())
        }
    }
}

fn remove_copy(models_dir: &std::path::Path, file: &str) {
    if let Some(p) = catalog::local_path(models_dir, file) {
        let _ = std::fs::remove_file(transfer::part_path(&p));
        if let Err(e) = std::fs::remove_file(&p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[cluster-agent] copie {file} non supprimée : {e}");
            }
        }
    }
}

/// Supprime les copies faites par ce poste — toutes, ou celles hors du
/// catalogue donné.
async fn remove_synced_copies(keep: Option<&Catalog>) {
    let Ok(models_dir) = cluster::require_models_dir() else {
        return;
    };
    let dir = cluster::state_dir();
    let mut ledger = SyncLedger::load(&dir);
    if ledger.files.is_empty() {
        return;
    }
    let a_retirer: Vec<String> = ledger
        .files
        .iter()
        .filter(|f| keep.is_none_or(|c| !c.contains(f)))
        .cloned()
        .collect();
    for file in &a_retirer {
        remove_copy(&models_dir, file);
        ledger.files.remove(file);
    }
    let _ = ledger.save(&dir);
}

async fn sync_view(state: &Shared) -> AgentResponse {
    let models_dir = cluster::models_dir();
    let s = state.lock().await;
    let prefs = s.prefs.clone().unwrap_or_default();
    let sizes = models_dir
        .as_deref()
        .map(|d| local_sizes(&s.catalog, d))
        .unwrap_or_default();
    let plan = catalog::plan_sync(
        &s.catalog,
        &sizes,
        &SyncLedger::load(&cluster::state_dir()),
        prefs.hosts_models(),
        prefs.quota_bytes(),
    );
    let en_cours = s.downloading.clone();
    let models = plan
        .states
        .into_iter()
        .map(|(file, state)| {
            let taille = s.catalog.get(&file).map(|m| m.size_bytes).unwrap_or(0);
            let copie = en_cours
                .as_ref()
                .filter(|(f, _)| f == &file)
                .map(|(_, p)| p.load(Ordering::Relaxed));
            SyncEntry {
                size_bytes: taille,
                copied_bytes: copie.unwrap_or(if state == LocalState::Ready {
                    taille
                } else {
                    0
                }),
                downloading: copie.is_some(),
                state,
                file,
            }
        })
        .collect();
    AgentResponse::Sync {
        coordinator: prefs.coordinator_host.clone(),
        reachable: s.catalog_reachable,
        models,
        used_bytes: plan.used_bytes,
        quota_bytes: prefs.quota_bytes(),
        last_error: s.sync_error.clone(),
    }
}

// ============================================================================
// Réseau du cluster : balise, écoute, poignée de main, rafraîchissement.
// ============================================================================

/// Ce que cette machine annonce aux autres.
async fn own_announce(state: &Shared) -> CapabilityAnnounce {
    let capability = local_capability(state).await;
    let s = state.lock().await;
    let sharing = s.prefs.as_ref().map(|p| {
        let models_ready = match cluster::models_dir() {
            Some(_) if p.coordinator => s
                .catalog
                .models
                .iter()
                .filter(|m| m.sha256.is_some())
                .map(|m| m.file.clone())
                .collect(),
            Some(dir) => {
                let tailles = local_sizes(&s.catalog, &dir);
                s.catalog
                    .models
                    .iter()
                    .filter(|m| tailles.get(&m.file) == Some(&m.size_bytes))
                    .map(|m| m.file.clone())
                    .collect()
            }
            None => Vec::new(),
        };
        SharingAnnounce {
            enabled: p.enabled,
            gpu: p.gpu,
            ram: p.ram,
            storage: p.storage,
            coordinator: p.coordinator,
            models_ready,
        }
    });
    CapabilityAnnounce {
        v: cluster::PROTOCOL_VERSION,
        capability,
        rpc_port: s.rpc_port,
        sharing,
    }
}

async fn bench_peer(peer_id: &str, state: &Shared) -> AgentResponse {
    let (address, secret) = {
        let s = state.lock().await;
        let Some(peer) = s.peers.get(peer_id) else {
            return AgentResponse::Error {
                message: format!("pair inconnu : {peer_id}"),
            };
        };
        let Some(identity) = &s.identity else {
            return AgentResponse::Error {
                message: "aucun cluster actif".to_string(),
            };
        };
        (peer.address.clone(), identity.secret.clone())
    };
    let debut = SystemTime::now();
    let resultat = tokio::time::timeout(
        Duration::from_secs(5),
        peer_channel::authenticated_request(&address, &secret, &ControlRequest::Ping),
    )
    .await;
    let rtt_ms = SystemTime::now()
        .duration_since(debut)
        .unwrap_or_default()
        .as_millis() as u32;
    match resultat {
        Ok(Ok(_)) => {
            let mut s = state.lock().await;
            if let Some(p) = s.peers.get_mut(peer_id) {
                p.round_trip_ms = Some(rtt_ms);
            }
            AgentResponse::Benched {
                peer_id: peer_id.to_string(),
                round_trip_ms: rtt_ms,
            }
        }
        Ok(Err(e)) => AgentResponse::Error { message: e },
        Err(_) => AgentResponse::Error {
            message: format!("{peer_id} ne répond pas (délai dépassé)"),
        },
    }
}

/// Relit régulièrement chaque pair : sa VRAM libre et ce qu'il prête
/// changent, et un pair éteint doit sortir des plans. Un poste client
/// rejoint aussi son coordinateur s'il ne le connaît pas encore.
async fn run_peer_refresh(agent: Agent) {
    let mut intervalle = tokio::time::interval(Duration::from_secs(15));
    loop {
        intervalle.tick().await;
        let (secret, peers, coordinateur) = {
            let s = agent.state.lock().await;
            let Some(identity) = &s.identity else {
                continue;
            };
            (
                identity.secret.clone(),
                s.peers
                    .iter()
                    .map(|(id, p)| (id.clone(), p.address.clone()))
                    .collect::<Vec<_>>(),
                s.prefs.as_ref().and_then(|p| p.coordinator_host.clone()),
            )
        };
        if let Some(hote) = coordinateur {
            let adresse = control_address(&hote);
            if !peers.iter().any(|(_, a)| a == &adresse) {
                if let Err(e) = pair_with(&adresse, &agent.state).await {
                    eprintln!("[cluster-agent] coordinateur {adresse} injoignable : {e}");
                }
            }
        }
        for (id, adresse) in peers {
            let debut = SystemTime::now();
            let reponse = tokio::time::timeout(
                Duration::from_secs(5),
                peer_channel::authenticated_request(&adresse, &secret, &ControlRequest::Ping),
            )
            .await;
            let rtt = SystemTime::now()
                .duration_since(debut)
                .unwrap_or_default()
                .as_millis() as u32;
            let mut s = agent.state.lock().await;
            if let Ok(Ok(ControlResponse::Capability(annonce))) = reponse {
                if let Some(p) = s.peers.get_mut(&id) {
                    p.capability = annonce.capability;
                    p.rpc_port = annonce.rpc_port;
                    p.sharing = annonce.sharing;
                    p.last_seen_unix = unix_now();
                    p.round_trip_ms = Some(rtt);
                }
            }
            let maintenant = unix_now();
            s.peers
                .retain(|_, p| maintenant.saturating_sub(p.last_seen_unix) < OUBLI_PAIR_SECS);
        }
    }
}

async fn run_beacon_sender(ctrl_port: u16, state: Shared) {
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        eprintln!("[cluster-agent] impossible d'ouvrir le socket de balise");
        return;
    };
    if socket.set_broadcast(true).is_err() {
        eprintln!("[cluster-agent] la diffusion réseau n'est pas autorisée sur cette machine");
        return;
    }
    let cible = format!("255.255.255.255:{}", cluster::DISCOVERY_UDP_PORT);
    let mut intervalle = tokio::time::interval(Duration::from_secs(5));
    loop {
        intervalle.tick().await;
        let fp = {
            let s = state.lock().await;
            s.identity
                .as_ref()
                .map(|i| cluster::beacon_fingerprint(&i.cluster_id, &i.secret))
        };
        let Some(fp) = fp else {
            // Pas de cluster créé ou rejoint : rien à annoncer. La balise
            // reprendra dès qu'une identité existe.
            continue;
        };
        let beacon = Beacon {
            v: cluster::PROTOCOL_VERSION,
            fp,
            ctrl_port,
        };
        let _ = socket.send_to(&beacon.encode(), &cible).await;
    }
}

async fn run_beacon_listener(agent: Agent, mon_ctrl_port: u16) {
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", cluster::DISCOVERY_UDP_PORT)).await else {
        eprintln!(
            "[cluster-agent] port de découverte {} indisponible — une autre instance tourne \
             peut-être déjà",
            cluster::DISCOVERY_UDP_PORT
        );
        return;
    };
    let mut buf = [0u8; 1024];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let Some(beacon) = Beacon::decode(&buf[..n]) else {
            continue;
        };
        // Une diffusion revient souvent à son émetteur (bouclage réseau
        // fréquent sous WSL et certaines configurations Wi-Fi) : sans ce
        // garde-fou, la machine s'appairait avec elle-même et le tableau de
        // bord comptait ses propres ressources deux fois. Le port de
        // contrôle annoncé, tiré au hasard par machine, suffit à s'en
        // distinguer sans avoir à comparer des adresses IP.
        if beacon.ctrl_port == mon_ctrl_port {
            continue;
        }
        let fp_attendue = {
            let s = agent.state.lock().await;
            s.identity
                .as_ref()
                .map(|i| cluster::beacon_fingerprint(&i.cluster_id, &i.secret))
        };
        if fp_attendue != Some(beacon.fp) {
            continue;
        }
        let adresse_controle = format!("{}:{}", from.ip(), beacon.ctrl_port);
        // Une balise arrive toutes les cinq secondes de chaque pair déjà
        // connu : ne relancer une poignée de main que si ce pair est
        // nouveau, pour ne pas saturer le canal de contrôle.
        let deja_connu = {
            let s = agent.state.lock().await;
            s.peers.values().any(|p| p.address == adresse_controle)
        };
        if deja_connu {
            continue;
        }
        let state = agent.state.clone();
        tokio::spawn(async move {
            if let Err(e) = pair_with(&adresse_controle, &state).await {
                eprintln!("[cluster-agent] pairage avec {adresse_controle} échoué : {e}");
            }
        });
    }
}

/// Initie une poignée de main vers un pair repéré par balise, ou vers le
/// coordinateur d'un poste inscrit.
async fn pair_with(address: &str, state: &Shared) -> Result<(), String> {
    let secret = {
        let s = state.lock().await;
        s.identity
            .as_ref()
            .map(|i| i.secret.clone())
            .ok_or("aucun cluster actif")?
    };
    let mut stream = peer_channel::authenticated_stream(address, &secret).await?;

    // Authentifié des deux côtés : on échange les capacités.
    let annonce = own_announce(state).await;
    write_json_line(&mut stream, &annonce).await?;
    let leur_annonce: CapabilityAnnounce = read_json_line(&mut stream).await?;

    let peer = Peer {
        id: address.to_string(),
        address: address.to_string(),
        rpc_port: leur_annonce.rpc_port,
        capability: leur_annonce.capability,
        last_seen_unix: unix_now(),
        round_trip_ms: None,
        sharing: leur_annonce.sharing,
    };
    state.lock().await.peers.insert(peer.id.clone(), peer);
    Ok(())
}

/// Écoute entrante : un pair qui a reconnu notre balise se présente ici.
async fn run_peer_listener(listener: TcpListener, agent: Agent) {
    loop {
        let Ok((socket, from)) = listener.accept().await else {
            continue;
        };
        let agent = agent.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_incoming_peer(socket, from.ip().to_string(), &agent).await {
                eprintln!("[cluster-agent] pair entrant refusé : {e}");
            }
        });
    }
}

async fn handle_incoming_peer(
    mut stream: TcpStream,
    from_ip: String,
    agent: &Agent,
) -> Result<(), String> {
    let state = &agent.state;
    let secret = {
        let s = state.lock().await;
        s.identity
            .as_ref()
            .map(|i| i.secret.clone())
            .ok_or("aucun cluster actif — connexion refusée")?
    };
    let hello: locaryn_plugin_cluster::hmac_auth::Hello = read_json_line(&mut stream).await?;
    let (repondant, ack) = ResponderHandshake::respond(&secret, &hello);
    write_json_line(&mut stream, &ack).await?;
    let confirm: locaryn_plugin_cluster::hmac_auth::Confirm = read_json_line(&mut stream).await?;
    repondant.receive_confirm(&confirm)?;

    // Authentifié : soit c'est l'échange initial de capacités (poignée de
    // main de découverte), soit une requête de contrôle. On distingue par la
    // forme du prochain message.
    let raw: serde_json::Value = read_json_line(&mut stream).await?;
    if let Ok(requete) = serde_json::from_value::<ControlRequest>(raw.clone()) {
        return answer_control(requete, &mut stream, agent).await;
    }
    let leur_annonce: CapabilityAnnounce = serde_json::from_value(raw)
        .map_err(|_| "message inattendu après authentification".to_string())?;
    let annonce = own_announce(state).await;
    write_json_line(&mut stream, &annonce).await?;

    let address = format!("{from_ip}:{}", cluster::DEFAULT_CONTROL_PORT);
    let peer = Peer {
        id: address.clone(),
        address,
        rpc_port: leur_annonce.rpc_port,
        capability: leur_annonce.capability,
        last_seen_unix: unix_now(),
        round_trip_ms: None,
        sharing: leur_annonce.sharing,
    };
    state.lock().await.peers.insert(peer.id.clone(), peer);
    Ok(())
}

async fn answer_control(
    requete: ControlRequest,
    stream: &mut TcpStream,
    agent: &Agent,
) -> Result<(), String> {
    let state = &agent.state;
    let reponse = match requete {
        ControlRequest::Ping => ControlResponse::Capability(own_announce(state).await),
        ControlRequest::StartRpc => {
            let permis = SharePrefs::allows_remote_start(state.lock().await.prefs.as_ref());
            if !permis {
                ControlResponse::Error {
                    message: format!(
                        "{} n'a pas accepté de prêter ses ressources (réglages du compte)",
                        hostname()
                    ),
                }
            } else {
                match start_worker(state).await {
                    AgentResponse::WorkerStarted { port } => ControlResponse::RpcStarted { port },
                    AgentResponse::Error { message } => ControlResponse::Error { message },
                    _ => ControlResponse::Error {
                        message: "réponse inattendue".into(),
                    },
                }
            }
        }
        ControlRequest::StopRpc => {
            // Une machine qui prête par choix garde son serveur : c'est la
            // personne qui l'arrête, depuis ses réglages.
            let garde = state
                .lock()
                .await
                .prefs
                .as_ref()
                .is_some_and(SharePrefs::lends_compute);
            if !garde {
                stop_worker(state).await;
            }
            ControlResponse::RpcStopped
        }
        ControlRequest::Catalog => {
            let s = state.lock().await;
            if s.prefs.as_ref().is_some_and(|p| p.coordinator) {
                ControlResponse::Catalog {
                    models: s.catalog.models.clone(),
                }
            } else {
                ControlResponse::Error {
                    message: "cette machine ne coordonne pas de cluster".to_string(),
                }
            }
        }
        ControlRequest::FetchModel { file, offset } => {
            let (coordinateur, catalogue) = {
                let s = state.lock().await;
                (
                    s.prefs.as_ref().is_some_and(|p| p.coordinator),
                    s.catalog.clone(),
                )
            };
            if !coordinateur {
                ControlResponse::Error {
                    message: "cette machine ne coordonne pas de cluster".to_string(),
                }
            } else {
                let models_dir = cluster::require_models_dir()?;
                return transfer::serve_model(stream, &catalogue, &models_dir, &file, offset).await;
            }
        }
    };
    write_json_line(stream, &reponse).await
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
