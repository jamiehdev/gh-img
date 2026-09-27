#!/usr/bin/env bash
# runs on the server from the directory scripts/deploy.sh uploads:
#   sudo bash deploy/install.sh                 install or upgrade
#   sudo bash deploy/install.sh --rotate-token  write a new upload token and restart
# safe to re-run. The token is created only when /etc/gh-img/token is missing.
set -euo pipefail
cd "$(dirname "$0")/.."

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
# shellcheck source=/dev/null
. deploy/host.env

new_token() {
  (umask 077; openssl rand -base64 48 | tr -d '\n' > /etc/gh-img/token.new)
  mv /etc/gh-img/token.new /etc/gh-img/token
}

if [ "${1:-}" = --rotate-token ]; then
  new_token
  systemctl restart gh-img
  echo "token rotated; copy /etc/gh-img/token to each client"
  exit 0
fi

render() {
  sed -e "s|@BIND_HOST@|$BIND_HOST|g" -e "s|@PORT@|$PORT|g" -e "s|@ALLOWED_CLIENTS@|$ALLOWED_CLIENTS|g" \
      -e "s|@PUBLIC_HOST@|$PUBLIC_HOST|g" -e "s|@SSL_CERT@|$SSL_CERT|g" -e "s|@SSL_KEY@|$SSL_KEY|g" "$1"
}

id gh-img >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin gh-img

# setgid www-data, so new images inherit the group nginx reads with
install -d -o gh-img -g www-data -m 2750 /srv/gh-img
find /srv/gh-img -maxdepth 1 -type f -exec chown gh-img:www-data {} + -exec chmod 0640 {} +
install -d -o root -g root -m 700 /etc/gh-img
[ -s /etc/gh-img/token ] || new_token
chmod 600 /etc/gh-img/token

# keep the Bun unit for rollback until the Bun files are removed
if [ -e /opt/gh-img/bun ] && [ ! -e /opt/gh-img/gh-img.service.bun ]; then
  cp /etc/systemd/system/gh-img.service /opt/gh-img/gh-img.service.bun
fi

install -o root -g root -m 755 gh-img-server /usr/local/bin/gh-img-server
render deploy/gh-img.service.in > /etc/systemd/system/gh-img.service
install -o root -g root -m 644 deploy/gh-img-sweep.service deploy/gh-img-sweep.timer /etc/systemd/system/
chmod 644 /etc/systemd/system/gh-img.service
render deploy/nginx.conf.in > "/etc/nginx/sites-available/$PUBLIC_HOST"
ln -sf "/etc/nginx/sites-available/$PUBLIC_HOST" "/etc/nginx/sites-enabled/$PUBLIC_HOST"

systemctl daemon-reload
# index images that predate the index, as the service user so file ownership stays right
systemd-run --wait --quiet --pipe --uid=gh-img --gid=gh-img -p StateDirectory=gh-img \
  -E IMG_DIR=/srv/gh-img -E INDEX_PATH=/var/lib/gh-img/index.json /usr/local/bin/gh-img-server adopt

nginx -t
systemctl enable --now gh-img-sweep.timer
systemctl enable gh-img
systemctl restart gh-img
systemctl reload nginx
sleep 1
systemctl --no-pager --lines=5 status gh-img
