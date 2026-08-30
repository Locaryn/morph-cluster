//! Agent Cluster — le processus de fond qui fait exister le réseau du
//! cluster : balise UDP, écoute des pairs, poignée de main HMAC, démarrage à
//! la demande de `ggml-rpc-server`.
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
//!   se reconnaître.

use locaryn_plugin_cluster as cluster;
use locaryn_plugin_cluster::agent_protocol::{self, AgentHandle, AgentRequest, AgentResponse};
use locaryn_plugin_cluster::discovery::{
    Beacon, CapabilityAnnounce, ControlRequest, ControlResponse,
};
use locaryn_plugin_cluster::hmac_auth::{InitiatorHandshake, ResponderHandshake};
use locaryn_plugin_cluster::identity::ClusterIdentity;
use locaryn_plugin_cluster::{Capability, Peer};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Child;
use tokio::sync::Mutex;

struct AgentState {
    identity: Option<ClusterIdentity>,
    peers: HashMap<String, Peer>,
    rpc_child: Option<Child>,
    rpc_port: Option<u16>,
}

type Shared = Arc<Mutex<AgentState>>;

#[tokio::main]
async fn main() {
    let state_dir = cluster::state_dir();
    let _ = std::fs::create_dir_all(&state_dir);

    let identity = ClusterIdentity::load(&state_dir);
    let state: Shared = Arc::new(Mutex::new(AgentState {
        identity,
        peers: HashMap::new(),
        rpc_child: None,
        rpc_port: None,
    }));

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

    tokio::spawn(run_local_ipc(local_listener, state.clone()));
    tokio::spawn(run_peer_listener(peer_listener, state.clone()));
    tokio::spawn(run_beacon_sender(peer_port, state.clone()));
    tokio::spawn(run_beacon_listener(state.clone()));

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

async fn run_local_ipc(listener: TcpListener, state: Shared) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let (read_half, mut write_half) = socket.into_split();
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let response = match serde_json::from_str::<AgentRequest>(&line) {
                    Ok(req) => handle_local_request(req, &state).await,
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

async fn handle_local_request(req: AgentRequest, state: &Shared) -> AgentResponse {
    match req {
        AgentRequest::Status => {
            let s = state.lock().await;
            AgentResponse::Status {
                cluster_name: s.identity.as_ref().map(|i| i.name.clone()),
                cluster_id: s.identity.as_ref().map(|i| i.cluster_id.clone()),
                self_capability: local_capability(s.rpc_port.is_some()).await,
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
        AgentRequest::JoinCluster { pairing_code } => {
            match ClusterIdentity::from_pairing_code(&pairing_code) {
                Ok(identity) => {
                    if let Err(e) = identity.save(&cluster::state_dir()) {
                        return AgentResponse::Error {
                            message: format!("identité impossible à enregistrer : {e}"),
                        };
                    }
                    let name = identity.name.clone();
                    let mut s = state.lock().await;
                    s.identity = Some(identity);
                    s.peers.clear();
                    AgentResponse::Joined { cluster_name: name }
                }
                Err(message) => AgentResponse::Error { message },
            }
        }
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
            let s = state.lock().await;
            let local_vram = local_capability(s.rpc_port.is_some()).await.free_vram_gb;
            let peers: Vec<Peer> = s.peers.values().cloned().collect();
            drop(s);
            let plan = cluster::plan::compute(model_size_gb, local_vram, &peers, None);
            AgentResponse::Plan { plan }
        }
        AgentRequest::Bench { peer_id } => bench_peer(&peer_id, state).await,
        AgentRequest::Shutdown => AgentResponse::ShuttingDown,
    }
}

// ============================================================================
// Capacités locales : GPU, VRAM libre, présence des binaires llama.cpp.
// ============================================================================

async fn local_capability(worker_active: bool) -> Capability {
    let (gpu_name, free_vram_gb) = probe_gpu().await;
    let has_llama_server = cluster::launch::find_binary("llama-server").is_some();
    let has_rpc_server = worker_active || cluster::launch::find_binary("ggml-rpc-server").is_some();
    Capability {
        name: hostname(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        gpu_name,
        free_vram_gb,
        has_llama_server,
        has_rpc_server,
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

/// Interroge `nvidia-smi` pour la VRAM **libre** — pas la VRAM totale, qui ne
/// dit rien de ce qu'un autre modèle occupe déjà. À défaut de `nvidia-smi`
/// sur le chemin, retombe sur `LOCARYN_VRAM_GB` (la VRAM totale mesurée par
/// l'hôte à l'installation de l'extension) en le disant dans le nom : mieux
/// vaut un nombre pessimiste et signalé qu'un nombre inventé.
async fn probe_gpu() -> (Option<String>, f32) {
    let sortie = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await;
    if let Ok(o) = sortie {
        if o.status.success() {
            let texte = String::from_utf8_lossy(&o.stdout);
            if let Some(ligne) = texte.lines().next() {
                let mut parts = ligne.split(',');
                let nom = parts.next().map(|s| s.trim().to_string());
                let mio: Option<f32> = parts.next().and_then(|s| s.trim().parse().ok());
                if let (Some(nom), Some(mio)) = (nom, mio) {
                    return (Some(nom), mio / 1024.0);
                }
            }
        }
    }
    let total = std::env::var("LOCARYN_VRAM_GB")
        .ok()
        .and_then(|s| s.parse::<f32>().ok());
    (None, total.unwrap_or(0.0))
}

// ============================================================================
// Serveur RPC local : offrir son GPU au cluster.
// ============================================================================

async fn start_worker(state: &Shared) -> AgentResponse {
    {
        let s = state.lock().await;
        if let Some(port) = s.rpc_port {
            return AgentResponse::WorkerStarted { port };
        }
    }
    let Some(bin) = cluster::launch::find_binary("ggml-rpc-server") else {
        return AgentResponse::Error {
            message: "ggml-rpc-server introuvable — installez llama.cpp (cluster_status en dit \
                      plus) avant d'offrir cette machine au cluster"
                .to_string(),
        };
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
        .stderr(std::process::Stdio::null());
    cluster::launch::hide_console(&mut cmd);
    match cmd.spawn() {
        Ok(child) => {
            let mut s = state.lock().await;
            s.rpc_child = Some(child);
            s.rpc_port = Some(port);
            AgentResponse::WorkerStarted { port }
        }
        Err(e) => AgentResponse::Error {
            message: format!("{} : {e}", bin.display()),
        },
    }
}

async fn stop_worker(state: &Shared) -> AgentResponse {
    let mut s = state.lock().await;
    if let Some(mut child) = s.rpc_child.take() {
        let _ = child.kill().await;
    }
    s.rpc_port = None;
    AgentResponse::WorkerStopped
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
        std::time::Duration::from_secs(5),
        cluster::peer_channel::authenticated_request(&address, &secret, &ControlRequest::Ping),
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

// ============================================================================
// Réseau du cluster : balise, écoute, poignée de main.
// ============================================================================

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
    let mut intervalle = tokio::time::interval(std::time::Duration::from_secs(5));
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

async fn run_beacon_listener(state: Shared) {
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
        let fp_attendue = {
            let s = state.lock().await;
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
            let s = state.lock().await;
            s.peers.values().any(|p| p.address == adresse_controle)
        };
        if deja_connu {
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = pair_with(&adresse_controle, &state).await {
                eprintln!("[cluster-agent] pairage avec {adresse_controle} échoué : {e}");
            }
        });
    }
}

/// Initie une poignée de main vers un pair repéré par balise, ou ajouté
/// manuellement.
async fn pair_with(address: &str, state: &Shared) -> Result<(), String> {
    let secret = {
        let s = state.lock().await;
        s.identity
            .as_ref()
            .map(|i| i.secret.clone())
            .ok_or("aucun cluster actif")?
    };
    let mut stream = TcpStream::connect(address)
        .await
        .map_err(|e| format!("connexion impossible : {e}"))?;

    let (initiateur, hello) = InitiatorHandshake::start(&secret);
    write_json_line(&mut stream, &hello).await?;
    let ack: locaryn_plugin_cluster::hmac_auth::HelloAck = read_json_line(&mut stream).await?;
    let confirm = initiateur.receive_ack(&ack)?;
    write_json_line(&mut stream, &confirm).await?;

    // Authentifié des deux côtés : on échange les capacités.
    let self_cap = local_capability(state.lock().await.rpc_port.is_some()).await;
    let rpc_port = state.lock().await.rpc_port;
    let announce = CapabilityAnnounce {
        v: cluster::PROTOCOL_VERSION,
        capability: self_cap,
        rpc_port,
    };
    write_json_line(&mut stream, &announce).await?;
    let leur_annonce: CapabilityAnnounce = read_json_line(&mut stream).await?;

    let peer_id = peer_id_for(address);
    let peer = Peer {
        id: peer_id.clone(),
        address: address.to_string(),
        rpc_port: leur_annonce.rpc_port,
        capability: leur_annonce.capability,
        last_seen_unix: unix_now(),
        round_trip_ms: None,
    };
    let mut s = state.lock().await;
    s.peers.insert(peer_id, peer);
    Ok(())
}

/// Écoute entrante : un pair qui a reconnu notre balise se présente ici.
async fn run_peer_listener(listener: TcpListener, state: Shared) {
    loop {
        let Ok((socket, from)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_incoming_peer(socket, from.ip().to_string(), &state).await {
                eprintln!("[cluster-agent] pair entrant refusé : {e}");
            }
        });
    }
}

async fn handle_incoming_peer(
    mut stream: TcpStream,
    from_ip: String,
    state: &Shared,
) -> Result<(), String> {
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
    // main de découverte), soit une requête de contrôle sur une connexion
    // déjà établie. On distingue par la forme du prochain message.
    let raw = read_line(&mut stream).await?;
    if let Ok(leur_annonce) = serde_json::from_str::<CapabilityAnnounce>(&raw) {
        let self_cap = local_capability(state.lock().await.rpc_port.is_some()).await;
        let rpc_port = state.lock().await.rpc_port;
        let announce = CapabilityAnnounce {
            v: cluster::PROTOCOL_VERSION,
            capability: self_cap,
            rpc_port,
        };
        write_json_line(&mut stream, &announce).await?;

        let peer_id = peer_id_for(&from_ip);
        let peer = Peer {
            id: peer_id.clone(),
            address: format!("{from_ip}:{}", cluster::DEFAULT_CONTROL_PORT),
            rpc_port: leur_annonce.rpc_port,
            capability: leur_annonce.capability,
            last_seen_unix: unix_now(),
            round_trip_ms: None,
        };
        let mut s = state.lock().await;
        s.peers.insert(peer_id, peer);
        return Ok(());
    }

    if let Ok(req) = serde_json::from_str::<ControlRequest>(&raw) {
        let reponse = match req {
            ControlRequest::Ping => {
                let cap = local_capability(state.lock().await.rpc_port.is_some()).await;
                let rpc_port = state.lock().await.rpc_port;
                ControlResponse::Capability(CapabilityAnnounce {
                    v: cluster::PROTOCOL_VERSION,
                    capability: cap,
                    rpc_port,
                })
            }
            ControlRequest::StartRpc => match start_worker(state).await {
                AgentResponse::WorkerStarted { port } => ControlResponse::RpcStarted { port },
                AgentResponse::Error { message } => ControlResponse::Error { message },
                _ => ControlResponse::Error {
                    message: "réponse inattendue".into(),
                },
            },
            ControlRequest::StopRpc => {
                stop_worker(state).await;
                ControlResponse::RpcStopped
            }
        };
        write_json_line(&mut stream, &reponse).await?;
        return Ok(());
    }

    Err("message inattendu après authentification".to_string())
}

fn peer_id_for(address: &str) -> String {
    address.to_string()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn write_json_line<T: serde::Serialize>(
    stream: &mut TcpStream,
    value: &T,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(|e| e.to_string())
}

async fn read_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut buf),
    )
    .await
    .map_err(|_| "délai dépassé en attendant le pair".to_string())?
    .map_err(|e| e.to_string())?;
    if buf.trim().is_empty() {
        return Err("connexion fermée par le pair".to_string());
    }
    Ok(buf)
}

async fn read_json_line<T: serde::de::DeserializeOwned>(
    stream: &mut TcpStream,
) -> Result<T, String> {
    // `read_line` consomme le flux dans un `BufReader` neuf à chaque appel :
    // sur une même connexion on perdrait ce qui a été lu en trop. La poignée
    // de main n'envoie qu'une ligne à la fois de chaque côté, donc ce n'est
    // pas encore un problème ici — un futur multiplexage sur la même
    // connexion devra porter le `BufReader` d'un appel à l'autre.
    let raw = read_line_direct(stream).await?;
    serde_json::from_str(raw.trim()).map_err(|e| format!("message illisible : {e}"))
}

async fn read_line_direct(stream: &mut TcpStream) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut byte))
            .await
            .map_err(|_| "délai dépassé en attendant le pair".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connexion fermée par le pair".to_string());
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }
    String::from_utf8(buf).map_err(|e| e.to_string())
}
