#!/usr/bin/env bash
# Expose Sunbox's MCP server to a hosted agent (Instinct) through a Cloudflare Tunnel.
#   - sunbox-mcp.service    : `sunbox mcp --http` on 127.0.0.1:8787 (token-protected)
#   - sunbox-tunnel.service : cloudflared named tunnel → https://$HOST
# Undo: scripts/expose-for-instinct.sh --remove
set -euo pipefail

HOST="${SUNBOX_MCP_HOST:-sunbox-mcp.bramwellfreeman.com}"
TUNNEL=sunbox
UNITS="$HOME/.config/systemd/user"
CF="$(mise which cloudflared 2>/dev/null || command -v cloudflared)"
SUNBOX="$HOME/.local/bin/sunbox"

if [[ "${1:-}" == "--remove" ]]; then
  systemctl --user disable --now sunbox-tunnel.service sunbox-mcp.service 2>/dev/null || true
  rm -f "$UNITS/sunbox-tunnel.service" "$UNITS/sunbox-mcp.service"
  systemctl --user daemon-reload
  echo "Stopped and removed. (Tunnel '$TUNNEL' + DNS record remain in Cloudflare: '$CF tunnel delete $TUNNEL')"
  exit 0
fi

mkdir -p "$UNITS"

echo "→ 1/4  MCP server service"
cat > "$UNITS/sunbox-mcp.service" <<EOF
[Unit]
Description=Sunbox MCP over HTTP (for Instinct)
After=network-online.target

[Service]
ExecStart=$SUNBOX mcp --http --bind 127.0.0.1:8787 --name instinct
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF

echo "→ 2/4  Cloudflare login (browser opens once; pick bramwellfreeman.com)"
[[ -f "$HOME/.cloudflared/cert.pem" ]] || "$CF" tunnel login

echo "→ 3/4  Tunnel '$TUNNEL' → $HOST"
"$CF" tunnel info "$TUNNEL" >/dev/null 2>&1 || "$CF" tunnel create "$TUNNEL"
"$CF" tunnel route dns --overwrite-dns "$TUNNEL" "$HOST"
TID="$("$CF" tunnel list --output json | python3 -c "import json,sys; print(next(t['id'] for t in json.load(sys.stdin) if t['name']=='$TUNNEL'))")"
cat > "$HOME/.cloudflared/sunbox.yml" <<EOF
tunnel: $TID
credentials-file: $HOME/.cloudflared/$TID.json
ingress:
  - hostname: $HOST
    service: http://127.0.0.1:8787
  - service: http_status:404
EOF
cat > "$UNITS/sunbox-tunnel.service" <<EOF
[Unit]
Description=Cloudflare Tunnel for Sunbox MCP ($HOST)
After=network-online.target sunbox-mcp.service

[Service]
ExecStart=$CF tunnel --no-autoupdate --config $HOME/.cloudflared/sunbox.yml run $TUNNEL
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF

echo "→ 4/4  Starting services"
systemctl --user daemon-reload
systemctl --user enable --now sunbox-mcp.service sunbox-tunnel.service
sleep 4
curl -fsS "https://$HOST/health" >/dev/null && echo "✓ https://$HOST is live" || echo "… tunnel still warming up; retry: curl https://$HOST/health"

TOKEN="$("$SUNBOX" mcp --show-token)"
cat <<EOF

Give Instinct:
  MCP URL : https://$HOST/mcp
  Header  : Authorization: Bearer <token>
  (or, if it only takes a URL:  https://$HOST/mcp/<token>)
Token: run  sunbox mcp --show-token   (rotate any time: sunbox mcp --rotate-token, then restart sunbox-mcp)
Permissions: sunbox agent permissions   ·   Activity: sunbox agent audit
EOF
