# morph-cluster

Extension Locaryn qui met en commun la **VRAM de plusieurs machines** sur le
réseau pour faire tourner un modèle GGUF trop gros pour une seule carte —
par exemple un modèle de 16 Go sur deux GPU de 8 Go.

Le calcul est fait par **llama.cpp** lui-même : `ggml-rpc-server` sur les
machines qui prêtent leur GPU, `llama-server --rpc host:port,…` sur celle qui
pilote la conversation. Ce mécanisme existe dans llama.cpp depuis longtemps ;
ce que cette extension ajoute, c'est tout ce qui manquait autour — trouver les
autres machines, s'assurer qu'elles font partie du même cluster avant de leur
envoyer des couches de poids, calculer combien chacune peut en porter,
démarrer les bons processus au bon moment.

---

## Comment ça marche

1. **Une machine crée un cluster** (`cluster_create`) : un nom, un secret
   généré au hasard. L'outil renvoie un **code de pairage** à copier sur
   chaque autre machine.
2. **Chaque autre machine rejoint** (`cluster_join`) avec ce code, puis offre
   son GPU (`cluster_worker_start`).
3. **Découverte automatique** : chaque machine diffuse une balise sur le
   réseau local toutes les cinq secondes. La balise ne révèle qu'une
   empreinte du cluster — jamais le secret ni son identifiant en clair. Deux
   machines qui reconnaissent la même empreinte s'authentifient mutuellement
   par une poignée de main HMAC-SHA256, sans jamais faire transiter le
   secret lui-même sur le réseau.
4. **Un pair est une adresse `hôte:port`.** Comment cette adresse est devenue
   joignable — réseau local, VPN, tunnel personnel — ne regarde pas cette
   extension. Elle ne connaît, ne nomme et ne dépend d'aucune extension
   d'accès distant en particulier : si l'adresse est routable, ça marche.
5. **Choisir le moteur** dans Réglages → Moteur → « Cluster GPU (llama.cpp
   RPC) » → un modèle GGUF. Le lanceur calcule alors la répartition à partir
   des pairs joignables, s'assure qu'ils ont démarré leur serveur RPC, et
   lance `llama-server --rpc …`.

---

## Sécurité — sans détour

- Le **pairage** (qui peut rejoindre le cluster) est protégé par un secret de
  32 octets et une poignée de main HMAC-SHA256 mutuelle : ni la balise de
  découverte ni la poignée de main ne font transiter le secret en clair.
- **`ggml-rpc-server` lui-même n'a pas d'authentification.** C'est une
  limite de llama.cpp, pas de cette extension : une fois qu'un serveur RPC
  tourne, n'importe qui capable d'atteindre son port peut lui parler. Cette
  extension réduit la fenêtre — le serveur ne tourne que le temps d'une
  session, sur un port choisi au hasard, communiqué seulement après
  authentification — mais **n'invente pas une sécurité qui n'existe pas en
  amont**. Réservez ceci à un réseau de confiance (réseau domestique, votre
  propre VPN) — jamais à un réseau partagé avec des tiers non fiables.

---

## Protocoles de calcul distribué

| Protocole | État |
|---|---|
| **llama.cpp RPC** (`ggml-rpc-server`) | ✅ implémenté — c'est ce que cette extension utilise |
| Exo (exo-explore) | 📋 à l'étude, aucun code |
| vLLM multi-nœud (Ray + NCCL) | 📋 à l'étude, aucun code — la piste la plus indiquée pour du matériel identique relié par un lien rapide (plusieurs machines NVIDIA GB10 reliées en ConnectX, par exemple) |
| Petals / hivemind (DHT) | 📋 à l'étude, aucun code |

L'outil `cluster_protocols` renvoie cet état à jour. Les trois protocoles non
implémentés n'ont **aucun outil qui prétend les faire fonctionner** — mieux
vaut un tableau honnête qu'une fonctionnalité qui échoue en silence.

---

## Ce que la machine doit offrir

- **llama.cpp** avec le backend RPC (`llama-server` + `ggml-rpc-server`).
  Cherché dans cet ordre : le dossier privé de l'extension, le dossier
  `bin/llama/` que l'application gère pour son propre runtime (réutilisé
  s'il est déjà là), puis le chemin du système. À défaut, l'archive Vulkan
  officielle est téléchargée automatiquement pour Windows, Linux x86_64,
  Linux ARM64 (dont les machines NVIDIA GB10) et macOS.
- Un **GPU** pour prêter de la VRAM — sans, une machine reste utilisable
  comme pilote (elle calcule sur CPU) mais n'apporte rien au cluster.
- **Réseau** : diffusion UDP autorisée sur le réseau local pour la
  découverte automatique (port `41337`), et une connexion TCP directe entre
  les machines pour le canal de contrôle (port `41338` par défaut) et le
  serveur RPC (port choisi au hasard, communiqué après authentification).

---

## Installation

Réglages → Extensions → Ajouter :

```
github:Locaryn/morph-cluster@v0.1.0
```

Accordez ses permissions, activez-la. Voir `SKILL.md` pour l'ordre des
étapes (créer, rejoindre, planifier, choisir le moteur).

---

## Limites connues, dites clairement

- **Pas de test sur un vrai cluster de plusieurs machines physiques** dans
  l'environnement où cette extension a été écrite. La logique de pairage
  (HMAC), le format des messages et le calcul de répartition sont
  intégralement testés unitairement ; l'échange réseau réel entre deux
  machines distinctes ne l'a pas été au même degré. Vérifiez sur votre
  matériel avant d'en dépendre pour un usage important.
- **La balise de découverte utilise la diffusion limitée** (`255.255.255.255`),
  qui ne franchit pas les routeurs ni, souvent, les réseaux Wi-Fi « isolation
  client » de certains routeurs grand public. Sur un réseau où la diffusion
  ne passe pas, deux machines pourtant joignables ne se découvriront pas
  automatiquement — il n'y a pas aujourd'hui de moyen d'ajouter un pair par
  adresse manuelle.
- **Seul le format GGUF** est pris en charge — c'est ce que `llama-server`
  sait charger.

---

## Un bogue trouvé en chemin, signalé et non corrigé ici

L'installateur natif de llama.cpp de l'application principale demande, pour
Linux, une archive `.zip` qui n'existe plus sous ce nom dans les publications
récentes de llama.cpp (seule une `.tar.gz` est publiée) — l'installation
automatique du runtime intégré échoue donc aujourd'hui sur Linux. Cette
extension n'en dépend pas (elle gère son propre téléchargement, avec la bonne
extension par plateforme), mais le bogue touche le runtime natif de
l'application, hors du périmètre de cette extension.

---

## Construire depuis les sources

```bash
cargo build --release
cargo test
cargo clippy --all-targets --locked -- -D warnings
```

Trois exécutables dans `bin/` :

- `locaryn-cluster-agent` — le processus de fond : découverte, pairage,
  démarrage à la demande de `ggml-rpc-server`.
- `locaryn-cluster-mcp` — les outils.
- `locaryn-cluster-launch` — nommé par `engine.lifecycle.start`.

`bin/` est ignoré par Git — l'archive des sources d'un dépôt GitHub ne le
contient donc pas. La CI compile par plateforme et publie une archive nommée
avec l'OS et l'architecture ; l'application cherche ce paquet en premier.

---

## Licence

Apache-2.0.
