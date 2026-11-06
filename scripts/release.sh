#!/usr/bin/env bash
set -euo pipefail

# ANSI color codes (auto-disabled if output is not a terminal)
if [ -t 1 ]; then
  BOLD='\033[1m'
  GREEN='\033[0;32m'
  YELLOW='\033[0;33m'
  BLUE='\033[0;34m'
  RED='\033[0;31m'
  NC='\033[0m'
else
  BOLD=''
  GREEN=''
  YELLOW=''
  BLUE=''
  RED=''
  NC=''
fi

info() {
  printf "${BLUE}${BOLD}[INFO]${NC} %s\n" "$1"
}

success() {
  printf "${GREEN}${BOLD}[SUCCESS]${NC} %s\n" "$1"
}

warn() {
  printf "${YELLOW}${BOLD}[WARN]${NC} %s\n" "$1"
}

error() {
  printf "${RED}${BOLD}[ERROR]${NC} %s\n" "$1" >&2
}

print_help() {
  cat <<EOF
Usage: $(basename "$0") [OPTIONS] [VERSION | patch | minor | major]

Automates version bumping, tagging, testing, local packaging, and pushing
to trigger GitHub Actions automatic release.

Arguments:
  VERSION                Explicit version (e.g. 1.0.1 or v1.0.1)
  patch | minor | major  Increment semantic version component
                         (If omitted, uses the current version in Cargo.toml)

Options:
  -p, --push             Push commit and tag to git remote ('origin')
  -b, --build            Build binary and create release package in dist/
      --no-tag           Skip creating git tag and commit (useful for packaging only)
  -n, --dry-run          Simulate steps without making changes or pushing
      --no-test          Skip running 'cargo test'
  -h, --help             Show this help message

Examples:
  $(basename "$0") patch --push
  $(basename "$0") 1.1.0 --build --push
  $(basename "$0") --build --no-tag
EOF
}

# Resolve script directory and project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

# Default options
DO_PUSH=false
DO_BUILD=false
CREATE_TAG=true
DRY_RUN=false
RUN_TESTS=true
VERSION_ARG=""

# Parse command line options
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      print_help
      exit 0
      ;;
    -p|--push)
      DO_PUSH=true
      shift
      ;;
    -b|--build)
      DO_BUILD=true
      shift
      ;;
    --no-tag)
      CREATE_TAG=false
      shift
      ;;
    -n|--dry-run)
      DRY_RUN=true
      shift
      ;;
    --no-test)
      RUN_TESTS=false
      shift
      ;;
    -*)
      error "Unknown option: $1"
      print_help
      exit 1
      ;;
    *)
      if [ -z "${VERSION_ARG}" ]; then
        VERSION_ARG="$1"
      else
        error "Unexpected argument: $1"
        print_help
        exit 1
      fi
      shift
      ;;
  esac
done

# Read current version from Cargo.toml
if [ ! -f "Cargo.toml" ]; then
  error "Cargo.toml not found in ${PROJECT_ROOT}"
  exit 1
fi

CURRENT_VERSION=$(awk -F ' = ' '/^version/ {gsub(/"/, "", $2); print $2; exit}' Cargo.toml)
if [ -z "${CURRENT_VERSION}" ]; then
  error "Failed to read current version from Cargo.toml"
  exit 1
fi

info "Current version in Cargo.toml: ${CURRENT_VERSION}"

# Calculate target version
bump_semver() {
  local ver="$1"
  local type="$2"
  local major minor patch
  IFS='.' read -r major minor patch <<< "$ver"
  patch="${patch%%-*}"
  case "$type" in
    major)
      major=$((major + 1))
      minor=0
      patch=0
      ;;
    minor)
      minor=$((minor + 1))
      patch=0
      ;;
    patch)
      patch=$((patch + 1))
      ;;
    *)
      error "Invalid bump type: $type"
      exit 1
      ;;
  esac
  echo "${major}.${minor}.${patch}"
}

if [ -z "${VERSION_ARG}" ]; then
  TARGET_VERSION="${CURRENT_VERSION}"
elif [[ "${VERSION_ARG}" =~ ^(patch|minor|major)$ ]]; then
  TARGET_VERSION=$(bump_semver "${CURRENT_VERSION}" "${VERSION_ARG}")
else
  # Strip leading 'v' if present
  TARGET_VERSION="${VERSION_ARG#v}"
fi

# Validate semantic version format (X.Y.Z or X.Y.Z-*)
if ! [[ "${TARGET_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  error "Invalid version format: '${TARGET_VERSION}'. Expected format like 1.0.0 or 1.0.0-rc.1"
  exit 1
fi

TAG="v${TARGET_VERSION}"
info "Target release: ${TARGET_VERSION} (Tag: ${TAG})"

# Validate git status for tagging
if [ "${CREATE_TAG}" = true ] && [ "${DRY_RUN}" = false ]; then
  if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
    error "Repository does not have any commits yet. Please commit your changes first before creating a release tag."
    exit 1
  fi

  if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1; then
    error "Git tag '${TAG}' already exists locally."
    exit 1
  fi
fi

# Check git status if bumping version
VERSION_CHANGED=false
if [ "${TARGET_VERSION}" != "${CURRENT_VERSION}" ]; then
  VERSION_CHANGED=true
fi

if [ "${VERSION_CHANGED}" = true ] && [ "${DRY_RUN}" = false ]; then
  # Check for uncommitted tracked changes before editing files
  if git rev-parse --verify HEAD >/dev/null 2>&1 && ! git diff-index --quiet HEAD -- 2>/dev/null; then
    warn "Uncommitted changes detected in git working tree."
    read -rp "Continue anyway? (y/N): " CONFIRM
    if [[ ! "${CONFIRM}" =~ ^[Yy]$ ]]; then
      info "Aborted by user."
      exit 0
    fi
  fi

  info "Updating version in Cargo.toml to ${TARGET_VERSION}..."
  sed -i '0,/^version = .*/s//version = "'"${TARGET_VERSION}"'"/' Cargo.toml

  info "Updating Cargo.lock..."
  cargo check --quiet >/dev/null 2>&1 || true
fi

# Run tests
if [ "${RUN_TESTS}" = true ]; then
  info "Running tests ('cargo test')..."
  if [ "${DRY_RUN}" = false ]; then
    cargo test --locked
  else
    info "[dry-run] cargo test --locked"
  fi
fi

# Local build and packaging if requested
if [ "${DO_BUILD}" = true ]; then
  info "Building release binary..."
  if [ "${DRY_RUN}" = false ]; then
    cargo build --release --locked
    ARCH="x86_64-unknown-linux-gnu"
    PKG_NAME="clipvault-sequential-${TAG}-${ARCH}"
    DIST_DIR="dist"

    info "Packaging artifacts into ${DIST_DIR}/${PKG_NAME}.tar.gz..."
    rm -rf "${DIST_DIR}/${PKG_NAME}"
    mkdir -p "${DIST_DIR}/${PKG_NAME}"
    cp target/release/clipvault-sequential "${DIST_DIR}/${PKG_NAME}/"
    [ -f README.md ] && cp README.md "${DIST_DIR}/${PKG_NAME}/"
    [ -f LICENSE ] && cp LICENSE "${DIST_DIR}/${PKG_NAME}/"
    [ -f LICENSE.md ] && cp LICENSE.md "${DIST_DIR}/${PKG_NAME}/"

    (
      cd "${DIST_DIR}"
      tar -czvf "${PKG_NAME}.tar.gz" "${PKG_NAME}"
      sha256sum "${PKG_NAME}.tar.gz" > "${PKG_NAME}.tar.gz.sha256"
      rm -rf "${PKG_NAME}"
    )
    success "Artifacts created in ${DIST_DIR}/"
  else
    info "[dry-run] cargo build --release --locked"
    info "[dry-run] package tarball and sha256 checksum into dist/"
  fi
fi

if [ "${CREATE_TAG}" = false ]; then
  success "Operation finished (tagging skipped as requested)."
  exit 0
fi

# Git commit and tagging
if [ "${DRY_RUN}" = true ]; then
  info "[dry-run] Would commit version bump (if any) and create tag ${TAG}"
  if [ "${DO_PUSH}" = true ]; then
    info "[dry-run] Would push commit and tag ${TAG} to remote origin"
  fi
  success "Dry run completed successfully."
  exit 0
fi

if [ "${VERSION_CHANGED}" = true ]; then
  info "Committing version bump..."
  git add Cargo.toml Cargo.lock
  git commit -m "chore(release): ${TAG}"
fi

info "Creating git tag ${TAG}..."
git tag -a "${TAG}" -m "Release ${TAG}"
success "Created git tag ${TAG}"

# Push to remote if requested
if [ "${DO_PUSH}" = true ]; then
  info "Pushing commit and tag to remote..."
  CURRENT_BRANCH=$(git branch --show-current 2>/dev/null || echo "main")
  git push origin "${CURRENT_BRANCH}"
  git push origin "${TAG}"
  success "Pushed tag ${TAG} to origin. GitHub Actions workflow has been triggered!"
else
  echo ""
  info "Tag ${TAG} is ready locally."
  info "To trigger GitHub Actions automatic release, push the tag to origin:"
  printf "  ${BOLD}git push origin ${TAG}${NC}\n\n"
fi
