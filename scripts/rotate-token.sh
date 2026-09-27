#!/usr/bin/env bash
# rotate the upload token on the server and copy it to this machine
set -euo pipefail
host="${GH_IMG_SSH:-personal-server}"
dest="$HOME/.config/gh-img/token"

ssh "$host" 'sudo bash gh-img-src/deploy/install.sh --rotate-token'
(umask 077; ssh "$host" 'sudo cat /etc/gh-img/token' > "$dest.new")
mv "$dest.new" "$dest"
echo "token updated in $dest"
