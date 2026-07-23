#!/usr/bin/env bash
# uid-guard — pre-commit hook that refuses to sign when user.email has no
# matching UID on user.signingkey. Vendored from the home-manager template
# so the repo carries its own copy independent of any contributor's host.

set -e

if ! git config --get commit.gpgsign | grep -qE '^(true|1)$'; then
  exit 0
fi

email=$(git config --get user.email || true)
signingkey=$(git config --get user.signingkey || true)

if [ -z "$email" ] || [ -z "$signingkey" ]; then
  exit 0
fi

key_ref=${signingkey%!}
matched_uid=$(gpg --list-keys --with-colons "$key_ref" 2>/dev/null \
  | awk -F: -v e="$email" 'tolower($1)=="uid" && index(tolower($10), tolower(e)) { print $10; exit }')

if [ -z "$matched_uid" ]; then
  cat >&2 <<EOF
✗ pre-commit blocked: signing-key UID mismatch.

  user.email      : $email
  user.signingkey : $signingkey
  primary key UIDs:
EOF
  gpg --list-keys --with-colons "$key_ref" 2>/dev/null \
    | awk -F: '$1=="uid" { print "    - " $10 }' >&2
  cat >&2 <<EOF

  No UID on this key contains "$email".
  Either fix user.email / user.signingkey, or attach a matching UID to the key.
EOF
  exit 1
fi

exit 0
