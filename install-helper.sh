#!/bin/sh
# Installe l'assistant root d'ArchProjeteur (connexion Wi-Fi Direct « rejoindre
# le groupe », comme Windows) et autorise son lancement sans mot de passe.
# À relancer après chaque mise à jour du code :  sudo ./install-helper.sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "Lance ce script avec sudo : sudo ./install-helper.sh" >&2
    exit 1
fi
USER_NAME="${SUDO_USER:-}"
if [ -z "$USER_NAME" ] || [ "$USER_NAME" = root ]; then
    echo "Lance-le avec sudo depuis ton compte (pas en root direct)." >&2
    exit 1
fi

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/target/release/archprojeteur"
if [ ! -x "$BIN" ]; then
    echo "Binaire introuvable : compile d'abord avec « cargo build --release »." >&2
    exit 1
fi

HELPER=/usr/local/libexec/archprojeteur-p2p
install -D -m 755 -o root -g root "$BIN" "$HELPER"

RULE=/etc/sudoers.d/archprojeteur
TMP="$(mktemp)"
printf '# ArchProjeteur : Wi-Fi Direct pour le Caster (Miracast)\n%s ALL=(root) NOPASSWD: %s p2p-helper *\n' \
    "$USER_NAME" "$HELPER" > "$TMP"
if visudo -cf "$TMP" >/dev/null; then
    install -m 440 -o root -g root "$TMP" "$RULE"
    rm -f "$TMP"
else
    rm -f "$TMP"
    echo "Règle sudoers invalide, rien n'a été modifié." >&2
    exit 1
fi

echo "Assistant installé : $HELPER"
echo "Règle sudo : $RULE (uniquement « $HELPER p2p-helper … » pour $USER_NAME)"
echo "Pour désinstaller : sudo rm $HELPER $RULE"
