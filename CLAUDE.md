# picme : consignes pour Claude Code

Build et tests passent par le conteneur du projet : `docker compose exec -T dev sh -c 'cargo ...'`.

Une seule commande cargo à la fois : deux cargo en parallèle se bloquent sur le verrou du dossier `target`. Les tests durent moins d'une seconde, c'est la compilation qui coûte (39 s en test incrémental, 2 min 20 en release).

Pendant l'itération : `cargo test -p picme-core <filtre>` ou `cargo test -p picme <filtre>` sur le seul crate touché. `cargo test --workspace` une seule fois, à la fin.

Le build release (`cargo build --release`) et l'installation dans `/home/sam/.local/bin/picme` reviennent à la session principale après le commit, jamais à un sous-agent pendant ses tests.

Un correctif mineur à cause connue se fait en session principale, sans agent. Deux correctifs indépendants : deux agents en parallèle, jamais le second ajouté à un agent qui tourne. Origine : 24/11/2026, deux correctifs mineurs livrés en 22 min (insight localrag #5599).
