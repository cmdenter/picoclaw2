#!/bin/bash
# Inline dfinity.bundle.js into index.html as a single self-contained file
set -e

BUNDLE="frontend/dfinity.bundle.js"
HTML="frontend/index.html"
OUT="src/frontend.html"

# Read the bundle content
BUNDLE_CONTENT=$(cat "$BUNDLE")

# Replace the import line with inline script, output to src/frontend.html
sed "s|import { HttpAgent, Actor, Principal, AuthClient } from './dfinity.bundle.js';|// -- dfinity bundle inlined --|" "$HTML" > "$OUT.tmp"

# Now insert the bundle content after the marker
python3 -c "
import sys
marker = '// -- dfinity bundle inlined --'
with open('$OUT.tmp', 'r') as f:
    html = f.read()
with open('$BUNDLE', 'r') as f:
    bundle = f.read()
# Wrap bundle exports into inline-friendly form
html = html.replace(marker, bundle + '\n// -- end dfinity bundle --')
with open('$OUT', 'w') as f:
    f.write(html)
"
rm -f "$OUT.tmp"
echo "Built $OUT ($(wc -c < "$OUT") bytes)"
