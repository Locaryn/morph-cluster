---
name: gpu-cluster
description: Mettre en commun la VRAM de plusieurs machines pour faire tourner un modèle GGUF trop gros pour une seule carte — créer ou rejoindre un cluster, voir qui y participe, calculer si la capacité suffit, faire offrir son GPU par une machine.
---

# Le cluster GPU

Cette extension répartit un modèle **GGUF** entre plusieurs machines, par le
mécanisme RPC natif de llama.cpp (`ggml-rpc-server`). Une machine « pilote »
la conversation (le moteur « Cluster GPU », choisi dans Réglages → Moteur) ;
les autres prêtent leur GPU. C'est ce qui permet à un modèle de 16 Go de
tourner sur deux cartes de 8 Go — à condition que les deux machines soient sur
le même cluster.

## Ce qui appartient à cette extension, et ce qui n'y appartient pas

Le **choix du modèle et le démarrage du moteur** ne sont pas des outils : ils
appartiennent à Réglages → Moteur, comme pour tout moteur d'extension. Ces
outils servent à préparer et surveiller le cluster **avant** de choisir ce
moteur — pas à le piloter à sa place.

## Ordre à suivre pour mettre en place un cluster

1. Sur la première machine : `cluster_status` pour vérifier qu'un GPU et
   llama.cpp sont détectés, puis `cluster_create` avec un nom. L'outil renvoie
   un **code de pairage** — un secret. Ne le montrez que si l'utilisateur veut
   explicitement ajouter une machine, et rappelez que c'est un secret à ne
   transmettre que sur un canal de confiance (pas un forum, pas un ticket
   public).
2. Sur chaque autre machine qui doit prêter son GPU : `cluster_join` avec ce
   code, puis `cluster_worker_start`.
3. Depuis la première machine : `cluster_peers` (la découverte prend jusqu'à
   une dizaine de secondes après le démarrage des deux agents) pour vérifier
   que les autres machines apparaissent, avec leur VRAM libre.
4. `cluster_plan` avec la taille du fichier GGUF visé (en Go) : dit si la
   capacité cumulée suffit, et lesquels des pairs seraient retenus. **Faites
   toujours cet appel avant de proposer le moteur Cluster GPU** — le
   démarrage refait le même calcul et échoue pour la même raison si ça ne
   suffit pas, mais après avoir commencé à charger.
5. Si un pair semble lent, `cluster_bench` avant de s'inquiéter d'un mauvais
   plan : au-delà de 150 ms d'aller-retour de contrôle, le pair est écarté
   automatiquement, et un lien lent (Wi-Fi faible) ralentit souvent plus
   qu'il n'aide, RPC oblige.
6. Une fois satisfait du plan : Réglages → Moteur → « Cluster GPU (llama.cpp
   RPC) » → choisir le modèle → Utiliser.

## Ce qu'il faut dire honnêtement

- **La capacité cumulée est un plafond réel**, pas un objectif. Un modèle
  dont la taille dépasse la somme des VRAM utilisables (après la marge de
  sécurité de 10 %) ne tournera pas — `cluster_plan` le dit avant, avec le
  manque exact en gigaoctets. Ne pas suggérer de contourner ce chiffre.
- **Ce n'est que du GGUF.** Un checkpoint safetensors ne passe pas par ce
  moteur.
- **La sécurité du pairage n'est pas celle du calcul lui-même.** Le pairage
  (secret partagé, poignée de main HMAC) garantit que seule une machine qui
  connaît le secret devient un pair reconnu. Mais une fois qu'un pair a
  démarré `ggml-rpc-server`, ce programme de llama.cpp — pas cette
  extension — n'a pas d'authentification à son bord : n'importe qui capable
  d'atteindre son port pendant qu'il tourne peut lui parler. À réserver à un
  réseau de confiance (réseau domestique, propre VPN de l'utilisateur) — ne
  jamais présenter ceci comme sûr sur un réseau partagé ou public.
- **Un pair est une adresse `hôte:port`.** Comment cette adresse est devenue
  joignable — réseau local, VPN, tunnel — ne regarde pas cette extension, et
  elle ne sait rien d'une éventuelle extension d'accès distant installée à
  côté. Si la découverte automatique ne trouve rien (réseaux séparés), il n'y
  a pour l'instant pas d'entrée manuelle d'adresse — dites-le plutôt que de
  laisser croire qu'un contournement existe.
- **Les autres protocoles annoncés par `cluster_protocols` ne sont pas
  implémentés.** Exo, vLLM/Ray, Petals/hivemind sont un état des lieux, pas
  une option disponible. N'annoncez jamais qu'ils fonctionnent ; appelez
  `cluster_protocols` si l'utilisateur en demande un et répondez avec ce que
  cet outil renvoie, pas de mémoire.
- **Cette extension n'a pas été testée sur un vrai cluster de plusieurs
  machines physiques dans son environnement de développement.** La logique de
  pairage, de calcul de répartition et de format des messages est
  intégralement testée ; l'échange réseau réel entre deux machines ne l'a pas
  été au même degré. Si un comportement semble incohérent sur le terrain, dites-le
  clairement plutôt que de chercher une explication qui sauve la façade.
