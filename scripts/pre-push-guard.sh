#!/bin/sh
# Git passes the destination name/URL as arguments and proposed refs on stdin.
# Install without npm: git config core.hooksPath .husky
# Explicitly authorized destructive pushes to the primary only:
# PRIME_AGENT_ALLOW_MIRROR_PUSH=1 git push origin ...
# This never permits GitHub/upstream destinations or malformed input.
set -f
remote=${1-}
url=${2-}

refuse() {
    echo "pre-push guard: refusing push to $url: $1" >&2
    exit 1
}

# Match hosts, not substrings or repository paths. Git has already applied
# pushInsteadOf/insteadOf before supplying this URL. SSH aliases remain opaque.
case $url in
    *://*) authority=${url#*://}; authority=${authority%%/*}; host=${authority##*@}; host=${host%%:*} ;;
    *:*) authority=${url%%:*}; host=${authority##*@} ;;
    *) host= ;;
esac
host=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')
host=${host%.}
case $remote in
    github | upstream) refuse "push only to Mnemosyne; GitHub is an automatic mirror" ;;
esac
case $host in
    github.com | ssh.github.com | www.github.com)
        refuse "push only to Mnemosyne; GitHub is an automatic mirror" ;;
esac
case $remote:$host in
    origin:* | *:mnemosyne.sigilzero.dev) ;;
    *) exit 0 ;; # Local and other scratch remotes retain normal Git behavior.
esac

refs=0
deletions=0
outside=0
while IFS= read -r line || [ -n "$line" ]; do
    set -- $line
    [ $# -eq 0 ] && continue
    [ $# -eq 4 ] || refuse "malformed ref line"
    case $2:$4 in
        *[!0-9a-f:]* | :* | *:) refuse "malformed object ID" ;;
    esac
    case $3 in
        refs/heads/* | refs/tags/*) ;;
        refs/*) outside=$((outside + 1)) ;;
        *) refuse "malformed remote ref" ;;
    esac
    refs=$((refs + 1))
    case $1 in
        '(delete)') deletions=$((deletions + 1)) ;;
    esac
done
if [ "$refs" -gt 10 ] || [ "$deletions" -gt 0 ] || [ "$outside" -gt 0 ]; then
    if [ "${PRIME_AGENT_ALLOW_MIRROR_PUSH-}" = 1 ]; then
        exit 0
    fi
    refuse "$refs ref(s), $deletions deletion(s), $outside non-branch/tag ref(s). After explicit authorization only: PRIME_AGENT_ALLOW_MIRROR_PUSH=1 git push $remote ..."
fi
