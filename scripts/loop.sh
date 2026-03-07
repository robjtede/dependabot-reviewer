#!/usr/bin/env bash

set -eEuo pipefail
# set -x

prs="$(
  for o in robjtede x52dev actix; do
    gh search prs \
      --owner "$o" \
      --author 'dependabot[bot]' \
      --state open \
      --json 'repository,number,title,url'
  done | jq -s 'add'
)"

while true; do
  selection=$(echo "$prs" | jq -r '.[] | "\(.repository.nameWithOwner)#\(.number): \(.title)"' | fzf --prompt="Pick a Dependabot PR (ESC to quit): ")
  [[ -z "$selection" ]] && break

  repo=$(echo "$selection" | cut -d# -f1)
  number=$(echo "$selection" | cut -d# -f2 | cut -d: -f1)

  # Show diff
  gh pr diff --repo "$repo" "$number" | delta --paging=always

  echo "$selection"
  echo "https://github.com/${repo}/pull/${number}"

  ci_fails="$(
    gh pr checks --repo "$repo" "$number" --json name,bucket \
      | jq -r '.[] | select(.bucket != "pass") | "  - \(.name)"'
  )"

  if [ -n "$ci_fails" ]; then
    echo "CI Failures:"
    echo "$ci_fails"
  fi

  echo

  echo "Actions:"
  select action in Accept Rebase Recreate Skip; do
    case $action in
      Accept)
        gh pr review --repo "$repo" "$number" --approve
        gh pr merge --repo "$repo" "$number" --auto --squash
        echo
        echo
        break
        ;;
      Rebase)
        gh pr comment --repo "$repo" "$number" --body "@dependabot rebase"
        echo
        echo
        break
        ;;
      Recreate)
        gh pr comment --repo "$repo" "$number" --body "@dependabot recreate"
        echo
        echo
        break
        ;;
      Skip)
        echo
        echo
        break
        ;;
    esac
  done
done
