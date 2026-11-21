#!/bin/sh
# A stand-in for a server's model swap commands (tblink models / tblink use),
# for the swap tests. Every branch exits; the slowest sleeps 5 s.
#
#   fake-swap.sh list        the models, as `tblink models --json` prints them
#   fake-swap.sh use NAME    progress lines, then a `tblink use --json` result
set -eu
case "${1:-}" in
list)
  echo "Warning: Permanently added 'stable' (ED25519) to the list of known hosts." >&2
  echo '{"models_dir":"/m","models":[{"name":"small","size_bytes":1,"configured":true,"live":true},{"name":"big","size_bytes":2,"configured":false,"live":false},{"name":"huge","size_bytes":3,"configured":false,"live":false}],"configured":"small","live":["small"],"only_here":[],"only_peer":[]}'
  ;;
use)
  case "${2:-}" in
  big)
    printf '\033[1mholding the lock\033[0m\n'
    echo "progress on stderr" >&2
    printf 'loading big\r\n'
    echo '{"ok":true,"model":"big","model_id":"big","changed":true,"deployment_id":"big-1","backend":"jaccl"}'
    ;;
  huge)
    echo "planning huge"
    echo '{"ok":false,"model":"huge","error":"huge does not fit in the two Macs'"'"' memory budgets","shortfall_bytes":3460000000,"rolled_back":true,"current_model":"small"}'
    exit 1
    ;;
  slow)
    echo "starting"
    sleep 5
    ;;
  *)
    echo "unknown model ${2:-}" >&2
    exit 2
    ;;
  esac
  ;;
*)
  echo "usage: fake-swap.sh list | use NAME" >&2
  exit 2
  ;;
esac
