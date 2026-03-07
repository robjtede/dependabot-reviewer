#!/usr/bin/env bash

set -eEuo pipefail

# Default values
ORGS=("actix" "robjtede" "x52dev")
TARGET_REPO=""
CONFIRM=false
DRY_RUN=false
VERBOSE=false

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Debug logging function
debug() {
  if [[ "$VERBOSE" = true ]]; then
    echo "DEBUG: $*" >&2
  fi
}

# Usage function
usage() {
  echo "Usage: $0 [-o ORG]... [--repo REPO] [--confirm] [--dry-run] [--verbose]"
  echo "  -o, --org ORG   GitHub organization to search (can be used multiple times)"
  echo "  --repo REPO     Repository name (owner/repo) to process directly"
  echo "  --confirm       Require confirmation before commenting on each PR"
  echo "  --dry-run       Show what would be done without actually commenting"
  echo "  --verbose       Enable debug logging"
  echo "  -h, --help      Show this help message"
  exit 1
}

# Parse arguments
while [[ "$#" -gt 0 ]]; do
  case $1 in
    -o|--org)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --org requires an argument" >&2
        usage
      fi
      ORGS+=("$2")
      shift 2
      ;;
    --repo)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --repo requires an argument" >&2
        usage
      fi
      TARGET_REPO="$2"
      shift 2
      ;;
    --confirm) CONFIRM=true; shift ;;
    --dry-run) DRY_RUN=true; shift ;;
    --verbose) VERBOSE=true; shift ;;
    -h|--help) usage ;;
    *) echo "Unknown parameter: $1" >&2; usage ;;
  esac
done

# Deduplicate organizations
ORGS=($(printf "%s\n" "${ORGS[@]}" | sort -u))

debug "Script started with arguments: $*"
debug "Configuration: ORGS=${ORGS[*]}, TARGET_REPO=$TARGET_REPO, CONFIRM=$CONFIRM, DRY_RUN=$DRY_RUN, VERBOSE=$VERBOSE"

echo -e "${GREEN}Mass Rebase Dependabot PRs${NC}"
echo "Organizations: ${ORGS[*]}"
echo "Target repo: ${TARGET_REPO:-<will select>}"
echo "Dry run: $DRY_RUN"
echo "Require confirmation: $CONFIRM"
echo

# Function to fetch all open Dependabot PRs for a specific repo
fetch_repo_prs() {
  local repo="$1"
  gh pr list \
    --repo "$repo" \
    --author 'dependabot[bot]' \
    --state open \
    --json number,title,url \
    --jq '.[] | {number, title, url}' 2>/dev/null || echo "[]"
}

# Function to aggregate PRs by repository across organizations
aggregate_repos() {
  debug "Aggregating repos from organizations: ${ORGS[*]}"

  # Collect all repo names, sort and count
  local all_repos=""

  for org in "${ORGS[@]}"; do
    debug "Searching PRs in org: $org"
    PRS="$(
      gh search prs \
        --owner "$org" \
        --author 'dependabot[bot]' \
        --state open \
        --json 'repository,number,title,url' \
        --jq '.[] | .repository.nameWithOwner' 2>/dev/null || echo ""
    )"

    if [[ -n "$PRS" ]]; then
      all_repos="${all_repos}${PRS}"$'\n'
    fi
  done

  # Count occurrences and sort by count (descending), output: "repo (N PRs)"
  echo "$all_repos" | grep -v '^$' | sort | uniq -c | sort -rn | awk '{print $2, " ("$1" PRs)"}'
}

# If target repo is specified, use it directly
if [[ -n "$TARGET_REPO" ]]; then
  SELECTED_REPO="$TARGET_REPO"
  debug "Using specified repository: $SELECTED_REPO"
else
  # Get list of repos with PR counts
  REPO_LIST=$(aggregate_repos)

  if [[ -z "$REPO_LIST" ]]; then
    echo -e "${YELLOW}No open Dependabot PRs found in any repository.${NC}"
    exit 0
  fi

  # Check if we're in an interactive terminal
  if [[ -t 1 ]] && command -v fzf &> /dev/null; then
    # Interactive mode with fzf
    echo "Repositories with open Dependabot PRs:"
    echo
    SELECTED_REPO=$(echo "$REPO_LIST" | fzf --prompt="Select a repository: " --header="repository (N PRs)" --nth='1')

    if [[ -z "$SELECTED_REPO" ]]; then
      echo "No repository selected. Exiting."
      exit 0
    fi

    # Extract just the repo name from the selected line (format: "count repo")
    SELECTED_REPO=$(echo "$SELECTED_REPO" | awk '{print $1}')
    debug "Selected repository: $SELECTED_REPO"
  else
    # Non-interactive mode: just print the list
    echo "Repositories with open Dependabot PRs:"
    echo "$REPO_LIST" | while read -r line; do
      echo "  $line"
    done
    echo
    echo "To process a specific repository, run with: --repo owner/repo"
    echo "Or run in an interactive terminal to select from a list."
    exit 0
  fi
fi

# Fetch all PRs for the selected repository
debug "Fetching open Dependabot PRs for repository: $SELECTED_REPO"
PRS=$(fetch_repo_prs "$SELECTED_REPO")

if [[ -z "$PRS" ]] || [[ "$PRS" == "[]" ]]; then
  echo -e "${YELLOW}No open Dependabot PRs found in $SELECTED_REPO.${NC}"
  exit 0
fi

PR_COUNT=$(echo "$PRS" | grep -c '^' 2>/dev/null || echo "$PRS" | wc -l | tr -d ' ')
debug "PR count for $SELECTED_REPO: $PR_COUNT"

echo
echo -e "Processing ${GREEN}$SELECTED_REPO${NC}:"
echo -e "Found ${GREEN}($PR_COUNT PRs)${NC} Dependabot PR(s):"
echo

# Display PRs
echo "$PRS" | jq -r '.number, .title' 2>/dev/null | paste - - | while IFS=$'\t' read -r number title; do
  echo "  #$number: $title"
done

echo

# Ask for confirmation
if [[ "$CONFIRM" = false ]]; then
  read -p "Comment '@dependabot rebase' on all $PR_COUNT PR(s) in $SELECTED_REPO? (y/N): " -n 1 -r
  echo
  if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Aborted."
    exit 0
  fi
fi

# Process each PR
echo "$PRS" | jq -c '.' 2>/dev/null | while read -r pr; do
  number=$(echo "$pr" | jq -r '.number')
  title=$(echo "$pr" | jq -r '.title')

  debug "Processing PR: repo=$SELECTED_REPO, number=$number, title=$title"

  echo
  echo -e "${YELLOW}Processing PR #$number:${NC} $title"

  if [[ "$CONFIRM" = true ]]; then
    read -p "  Comment '@dependabot rebase' on PR #$number? (y/N): " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
      debug "User chose to skip PR #$number"
      echo "  Skipping PR #$number"
      continue
    fi
    debug "User confirmed PR #$number"
  fi

  if [[ "$DRY_RUN" = true ]]; then
    debug "Dry run mode - not actually commenting"
    echo "  [DRY RUN] Would comment: @dependabot rebase"
    echo "  [DRY RUN] URL: https://github.com/$SELECTED_REPO/pull/$number"
  else
    debug "Executing: gh pr comment --repo $SELECTED_REPO $number --body '@dependabot rebase'"
    echo "  Commenting '@dependabot rebase'..."
    gh pr comment \
      --repo "$SELECTED_REPO" \
      "$number" \
      --body "@dependabot rebase"
    debug "Successfully commented on PR #$number in $SELECTED_REPO"
    echo "  ${GREEN}✓${NC} Commented on PR #$number in $SELECTED_REPO"
  fi
done

debug "Script completed successfully"
echo
echo -e "${GREEN}Done!${NC}"
if [[ "$DRY_RUN" = false ]]; then
  echo "Dependabot will rebase each PR automatically."
  echo "You can monitor the progress in the PRs."
else
  echo "Run without --dry-run to actually comment on PRs."
fi
