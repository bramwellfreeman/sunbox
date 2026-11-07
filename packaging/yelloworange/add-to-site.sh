#!/bin/sh
# Adds Sunbox to yelloworange.fun: logo, the install & use guide (/sunbox.html) and the apps.js
# listing, then commits and deploys. Run on the Mac:  sh add-to-site.sh   (safe to run again).
set -e
SITE="${SITE:-$HOME/Projects/yelloworange-hub}"
RAW=https://raw.githubusercontent.com/bramwellfreeman/sunbox/master/packaging/yelloworange
[ -f "$SITE/public/assets/apps.js" ] || { echo "Can't find $SITE/public/assets/apps.js — set SITE=/path/to/yelloworange-hub"; exit 1; }
cd "$SITE"

curl -fsSL "$RAW/sunbox.svg" -o public/assets/sunbox.svg
curl -fsSL "$RAW/sunbox.html" -o public/sunbox.html
echo "✓ logo + guide page"

if grep -q 'id: "sunbox"' public/assets/apps.js; then
  echo "✓ apps.js already lists Sunbox"
else
  entry="$(mktemp)"
  curl -fsSL "$RAW/apps-entry.js" -o "$entry"
  # Insert as the first app, so it shows in "Latest apps" on the home page.
  E="$entry" perl -0pi -e 'BEGIN{ open my $f, "<", $ENV{E} or die "entry: $!"; local $/; $e = <$f>; } s/(window\.APPS = \[\n)/$1$e/ or die "window.APPS = [ not found\n"' public/assets/apps.js
  rm -f "$entry"
  node -e 'global.window={};eval(require("fs").readFileSync("public/assets/apps.js","utf8"));if(!window.APPS.some(a=>a.id==="sunbox"))process.exit(1)' \
    || { echo "apps.js check failed — restoring"; git checkout -- public/assets/apps.js; exit 1; }
  echo "✓ added Sunbox to apps.js"
fi

git add public/assets/sunbox.svg public/sunbox.html public/assets/apps.js
git commit -q -m "Add Sunbox: listing + install & use guide" && echo "✓ committed" || echo "· nothing new to commit"
# Plain deploy (no dir arg) keeps the wrangler.toml bindings (VIEWS KV for the counter).
npx wrangler pages deploy
echo "✓ Live: https://yelloworange.fun/sunbox.html  ·  https://yelloworange.fun/app.html?id=sunbox"
