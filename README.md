# morph-cluster

Extension Locaryn qui fait **travailler ensemble les machines d'un même
réseau**. Chaque poste connecté au serveur Locaryn peut prêter sa carte
graphique, sa mémoire vive et de la place disque. Deux usages :

- **faire tourner un modèle trop gros pour une seule carte** — par exemple un
  modèle de 16 Go sur deux GPU de 8 Go — en répartissant ses couches entre les
  machines ;
- **garder une copie des modèles partagés** sur chaque poste qui l'accepte, pour
  qu'un modèle activé sur le serveur arrive aussi sur les postes, sans rien
  télécharger à la main.

Le calcul est fait par **llama.cpp** lui-même : `ggml-rpc-server` sur les
machines qui prêtent leurs ressources, `llama-server --rpc host:port,…` sur
celle qui pilote la conversation. Cette extension ajoute tout ce qui manquait
autour : l'inscription des postes par le compte Locaryn, le choix de ce que
chaque machine prête, le catalogue des modèles partagés, leur copie vérifiée,
et la répartition au démarrage du moteur.

---

## Avec un serveur Locaryn et des postes clients

C'est le parcours prévu. **Tout se fait depuis Réglages → Compte → Partage de
ressources** ; aucun code n'est à recopier.

1. **Sur la machine serveur** : Réglages → Serveur & fonctions → Serveur actif,
   comptes créés. Le panneau « Partage de ressources » de cette machine liste
   sa bibliothèque de modèles, avec un interrupteur par modèle.
2. **Sur chaque poste client** : se connecter au serveur avec son compte.
   L'extension est un *compagnon d'appareil* — elle doit tourner sur le poste
   pour agir sur lui. Le panneau propose de l'installer ici, avec ses
   autorisations.
3. **Cocher « Allouer cette machine au partage de ressources »**. Le poste
   s'inscrit auprès du serveur **par le compte de la personne** : le secret du
   cluster passe par la connexion chiffrée et authentifiée du compte, jamais
   par un copier-coller. Puis choisir ce qui est prêté :
   - **carte graphique** (VRAM) ;
   - **mémoire vive** — plus lente, mais permet des modèles plus gros ;
   - **stockage**, avec un quota en Go, pour héberger les modèles partagés.
4. **Partager un modèle** : un interrupteur, sur le serveur ou depuis n'importe
   quel poste connecté. Le serveur calcule son empreinte SHA-256 ; chaque poste
   qui héberge des modèles en fait une copie dans sa bibliothèque, vérifiée
   avant d'être rendue visible, reprise là où elle s'était arrêtée après une
   coupure. **Couper le partage retire ces copies.**
5. **Faire tourner un modèle réparti** : sur le serveur, Réglages → Moteur →
   « Cluster GPU (llama.cpp RPC) » → un modèle GGUF. Le lanceur retient les
   postes qui prêtent, dans la limite de ce qu'ils offrent.

Le panneau montre, pour chaque modèle partagé : sa taille, les machines qui en
ont déjà une copie, et sur un poste client son état ici — installé, copie en
cours avec sa progression, en attente, place insuffisante (avec ce qui manque).

### Ce qui se passe quand on décoche

- **La case principale** : plus rien n'est prêté ; le serveur RPC s'arrête. Un
  coordinateur authentifié ne peut plus le redémarrer : appartenir au cluster ne
  vaut pas consentement à prêter sa machine.
- **Le stockage** : les copies faites par le cluster sont supprimées. Les
  modèles que la personne avait posés elle-même ne sont jamais touchés, même
  sous le même nom — seules les copies inscrites au registre du poste le sont.
- **« Quitter le cluster »** : oublie le secret, arrête de prêter, supprime les
  copies.

---

## Sans serveur : le cluster monté à la main

Les outils de la version 0.1 restent là, pour un réseau sans serveur Locaryn :
`cluster_create` sur une machine (renvoie un code de pairage), `cluster_join`
avec ce code sur les autres, `cluster_worker_start` sur celles qui prêtent leur
GPU. Une machine qui n'a jamais enregistré de préférences de partage garde ce
comportement.

---

## Comment ça marche

- **Découverte** : chaque machine diffuse une balise sur le réseau local
  toutes les cinq secondes. La balise ne révèle qu'une empreinte du cluster —
  jamais le secret ni son identifiant en clair. Un poste inscrit par le serveur
  n'en dépend pas : il joint directement l'adresse du serveur, ce qui marche
  aussi quand la diffusion ne passe pas (Wi-Fi à isolation client).
- **Authentification** : deux machines s'authentifient mutuellement par une
  poignée de main HMAC-SHA256, sans faire transiter le secret.
- **Rafraîchissement** : toutes les quinze secondes, chaque machine relit ses
  pairs (mémoire libre, ce qu'ils prêtent, modèles hébergés) et oublie ceux qui
  ne répondent plus depuis 90 s.
- **Appareils** : la liste vient de `ggml-rpc-server` lui-même, qui connaît les
  cartes NVIDIA, AMD, Intel et Apple sous le nom exact que `-d` attend. « Carte
  seule » sur une machine sans carte est refusé plutôt que de laisser llama.cpp
  se rabattre en silence sur le processeur.
- **Cache de tenseurs** : sur un poste qui héberge des modèles, `ggml-rpc-server`
  tourne avec `-c`, son cache rangé dans le dossier de l'extension et non sur
  le disque système.

---

## Sécurité — sans détour

- Le **pairage** est protégé par un secret de 32 octets et une poignée de main
  HMAC-SHA256 mutuelle. Avec un serveur, le secret n'est remis qu'à un compte
  authentifié du serveur, par sa connexion TLS.
- Le **catalogue et les copies** ne sont servis qu'aux pairs authentifiés, et
  uniquement pour les fichiers explicitement partagés : un nom de fichier venu
  du réseau ne peut ni sortir de la bibliothèque (`../`), ni désigner un modèle
  non partagé.
- Le canal de copie est **authentifié mais pas chiffré** : sur un réseau local,
  un modèle partagé n'est pas un secret. Chaque copie est vérifiée contre
  l'empreinte SHA-256 du serveur avant d'être utilisable.
- **`ggml-rpc-server` lui-même n'a pas d'authentification.** C'est une limite
  de llama.cpp : tant qu'il tourne, n'importe qui capable d'atteindre son port
  peut lui parler. Il écoute sur un port tiré au hasard, communiqué seulement
  après authentification, et seulement sur une machine dont la personne a
  coché le partage — mais **cette extension n'invente pas une sécurité qui
  n'existe pas en amont**. À réserver à un réseau de confiance.

---

## Protocoles de calcul distribué

| Protocole | État |
|---|---|
| **llama.cpp RPC** (`ggml-rpc-server`) | ✅ implémenté — c'est ce que cette extension utilise |
| Exo (exo-explore) | 📋 à l'étude, aucun code |
| vLLM multi-nœud (Ray + NCCL) | 📋 à l'étude, aucun code |
| Petals / hivemind (DHT) | 📋 à l'étude, aucun code |

---

## Ce que la machine doit offrir

- **llama.cpp** avec le backend RPC (`llama-server` + `ggml-rpc-server`),
  cherché dans le dossier privé de l'extension, puis dans `bin/llama/` de
  l'application, puis sur le chemin du système ; à défaut, l'archive Vulkan
  officielle est téléchargée pour Windows, Linux x86_64 et ARM64, et macOS.
- **Réseau** : TCP `41338` entre les machines (contrôle et copie des modèles),
  UDP `41337` pour la découverte, et le port RPC tiré au hasard.
- **Locaryn 0.3.78 ou plus récent** pour le panneau de compte, l'inscription
  par le serveur et l'installation comme compagnon d'appareil.

---

## Limites connues, dites clairement

- **Pas encore éprouvé sur un vrai parc de plusieurs machines physiques.** La
  poignée de main, le format des messages, la répartition, le plan de copie et
  la copie elle-même (reprise, vérification, refus hors catalogue) sont testés
  automatiquement, dont une copie réelle de bout en bout sur la boucle locale.
  L'échange entre deux machines distinctes ne l'a pas été au même degré.
- **Une seule copie à la fois** par poste, dans l'ordre du catalogue.
- **La mémoire vive prêtée n'a pas de plafond** : llama.cpp n'offre pas d'option
  pour la limiter, et annonce la mémoire libre au moment du chargement.
- **Seul le format GGUF** est pris en charge.

---

## Construire depuis les sources

```bash
cargo build --release
cargo test
cargo clippy --all-targets --locked -- -D warnings
```

Trois exécutables dans `bin/` :

- `locaryn-cluster-agent` — le processus de fond : découverte, pairage,
  serveur RPC prêté, catalogue, copies.
- `locaryn-cluster-mcp` — les outils, et ce que le panneau appelle.
- `locaryn-cluster-launch` — nommé par `engine.lifecycle.start`.

Le panneau est `dist/ui.js`, un élément personnalisé sans dépendance.

---

## Licence

Apache-2.0.
