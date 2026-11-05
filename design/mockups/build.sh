#!/usr/bin/env bash
# Assembles the self-contained prototype (design/mockups/index.html) from src/.
# No dependencies beyond a POSIX shell + awk/sed; open index.html directly in a browser.
set -euo pipefail
cd "$(dirname "$0")"
{
  sed '/^<\/style>$/,$d' src/head.html
  cat src/styles-2.css
  printf '</style>\n</head>\n'
  awk 'FNR==NR { sprite = sprite $0 "\n"; next } /<!--ICON_SPRITE-->/ { printf "%s", sprite; next } { print }' src/icons.svg src/body.html
  cat src/script.html
} > index.html
echo "wrote $(pwd)/index.html ($(wc -c < index.html | tr -d ' ') bytes)"
