# ArchProjeteur

Équivalent de **« Projeter »** (Win+P) et **« Caster »** (Win+K) de Windows, pour **Hyprland** (config Lua, 0.57+), écrit en Rust (egui).

| Mode | Effet |
|---|---|
| Écran du PC uniquement | éteint les écrans externes |
| Dupliquer | les écrans externes reflètent l'écran du portable |
| Étendre | les écrans externes sont placés à droite |
| Deuxième écran uniquement | allume l'externe, puis éteint l'écran du portable |

## Utilisation

- **Super+K** : onglet **Caster** (écrans sans fil Miracast / Chromecast).
- **Super+Shift+K** : onglet **Projeter** (modes d'affichage ci-dessus).
- Rappuyer sur le raccourci passe à l'option suivante. Flèches / Tab / chiffres pour choisir, Gauche/Droite pour changer d'onglet, **Entrée** pour valider, **Échap** pour fermer. Un clic marche aussi.

### Caster (sans fil)

1. **Une seule fois** (et après chaque mise à jour du code) : `cargo build --release && sudo ./install-helper.sh`.
   Ça installe l'assistant root `/usr/local/libexec/archprojeteur-p2p` et une règle sudo qui n'autorise que lui.
2. Super+K : les écrans Miracast à portée apparaissent (« Occupé » = quelqu'un diffuse déjà dessus).
3. Choisis l'écran puis Entrée. Le sélecteur de partage d'Hyprland s'ouvre : choisis l'écran ou la fenêtre à envoyer.
4. Accepte sur l'écran de la salle s'il le demande. Une notification confirme « Diffusion vers … ».
5. Pour arrêter : Super+K puis Entrée sur l'écran « Connecté », ou `archprojeteur cast-stop`.

En cas de souci, le journal détaillé est dans `$XDG_RUNTIME_DIR/archprojeteur-cast.log`.

En ligne de commande :

```bash
archprojeteur pc | duplicate | extend | second | next | status
archprojeteur cast        # panneau Caster
archprojeteur cast-list   # écrans sans fil visibles
archprojeteur cast-stop   # arrête la diffusion
```

## Installation

```bash
cargo install --path . --root ~/.local
```

Raccourci dans `~/.config/hypr/config/binds.lua` :

```lua
hl.bind(mainMod .. " + K",         hl.dsp.exec_cmd(os.getenv("HOME") .. "/.local/bin/archprojeteur cast"))
hl.bind(mainMod .. " + SHIFT + K", hl.dsp.exec_cmd(os.getenv("HOME") .. "/.local/bin/archprojeteur"))
```

## Fonctionnement

**Caster** : les écrans de salle sont déjà « propriétaires de groupe » Wi-Fi Direct. NetworkManager ne sait que *négocier* un groupe, pas en *rejoindre* un. ArchProjeteur fait donc comme Windows :

- découverte via NetworkManager (`StartFind`, pairs avec IE Wi-Fi Display « sink ») ;
- assistant root : `wpa_cli p2p_connect <mac> pbc join` (ou négociation si l'écran n'a pas de groupe), en profitant d'une activation NetworkManager (sinon NetworkManager détruit le groupe) ; NetworkManager adopte le groupe et fait le DHCP ;
- source Miracast intégrée : l'écran se connecte en RTSP sur le port 7236 (échanges M1…M7, keep-alive, TEARDOWN) ;
- flux GStreamer : capture via le portail (PipeWire) → H.264 constrained baseline (x264) + AAC → MPEG-TS (PID 0x1011 / 0x1100) → RTP/UDP.

**Projeter** : 
Chaque changement fait `hyprctl reload` (pour effacer les règles posées à chaud et revenir à ta config), puis applique les règles du mode choisi avec `hyprctl eval 'hl.monitor{...}'`. Les modes ne persistent donc pas après un reload de la config : c'est voulu, ta config reste la référence.

Si tu te retrouves sans image, `hyprctl reload` (ou `archprojeteur pc`) remet tout comme dans ta config.
