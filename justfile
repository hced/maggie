# =================================================================================================
# Global Configuration
# =================================================================================================

set shell := ["bash", "-c"]

name := `awk -F'"' '/^name[[:space:]]*=/ {print $2}' Cargo.toml`
set default-list := true

# =================================================================================================
# Core Workflow
# =================================================================================================

build:
  cargo build --release

run *args:
  cargo run --release -- $args

tests:
  cargo test --release

# =================================================================================================
# Code Quality
# =================================================================================================

fmt:
  cargo fmt

fmt-check:
  cargo fmt --check

lint:
  cargo clippy --release -- -D warnings

check:
  cargo check

# =================================================================================================
# Utility & Config
# =================================================================================================

install:
  cargo install --path . --force

clean:
  cargo clean

clean-all:
  cargo clean
  rm -rf target

config-delete:
  rm -rf ~/.config/{{ name }}/config.ron ~/.config/{{ name }}/recipes.ron
  echo "Cleanup complete."

config-edit:
  ${EDITOR:-nvim} ~/.config/{{ name }}/config.ron

version:
  grep '^version = ' Cargo.toml | cut -d' ' -f3 | tr -d '\"'

info:
  #!/usr/bin/env fish
  echo "Project: {{ name }} | Root: $PWD"
  echo "Binary: target/release/{{ name }}"
  if which {{ name }} > /dev/null
    echo "Installed in PATH: "(which {{ name }})
  else
    echo "Not installed in PATH."
  end


# =================================================================================================
# Git & Release Workflow
# =================================================================================================

# Internal helper to ensure cargo-bump is installed
_ensure-cargo-bump:
    @if ! command -v cargo-bump &> /dev/null; then \
        echo "📥 cargo-bump not found. Installing it now..."; \
        cargo install cargo-bump; \
    fi

# Bump patch version (1.0.0 -> 1.0.1)
bump-patch: _ensure-cargo-bump
    @cargo bump patch
    @echo "✅ Bumped patch version"

# Bump minor version (1.0.0 -> 1.1.0)
bump-minor: _ensure-cargo-bump
    @cargo bump minor
    @echo "✅ Bumped minor version"

# Bump major version (1.0.0 -> 2.0.0)
bump-major: _ensure-cargo-bump
    @cargo bump major
    @echo "✅ Bumped major version"

# Add all changes and open editor for commit message
git-commit:
    @git add .
    @echo "Opening editor for commit message..."
    @git commit && echo "✅ Commit successful" || (echo "❌ Commit aborted (no message or user cancelled)"; exit 1)

# Create and push a release tag
push-release-tag:
    @echo "Existing tags:"
    @git tag --sort=-v:refname | head -5 || echo "  (none)"
    @echo ""
    @echo "Current version in Cargo.toml: v$(grep -m1 '^version = ' Cargo.toml | cut -d' ' -f3 | tr -d '\"')"
    @echo ""
    @read -p "Tag name (e.g., v1.0.0): " tag; \
    if [ -z "$tag" ]; then echo "Cancelled."; exit 0; fi
    @echo ""
    @read -p "Create and push tag $tag? (y/N): " confirm; \
    if [ "$confirm" = "y" ] || [ "$confirm" = "Y" ]; then \
        git tag "$tag" && git push origin "$tag" && echo "✅ Tag $tag pushed!"; \
    else \
        echo "Cancelled."; \
    fi

# Master release recipe: verify git state, bump version, commit, tag, and push safely
release: _ensure-cargo-bump
    @echo "🔍 Running pre-flight git checks..."; \
    git fetch origin main -q; \
    LOCAL_STATUS=$(git status -uno); \
    if echo "$LOCAL_STATUS" | grep -q "Your branch is behind"; then \
        echo "❌ Aborting: Your local branch is behind 'origin/main'. Please run 'git pull' first."; \
        exit 1; \
    fi; \
    if ! git diff-index --quiet HEAD --; then \
        echo "❌ Aborting: You have unstaged changes in your working directory. Clean or commit them first."; \
        exit 1; \
    fi; \
    echo "✅ Pre-flight checks passed."; \
    echo ""; \
    echo "🚀 Starting release process..."; \
    echo ""; \
    CURRENT_VER=$(grep -m1 "^version = " Cargo.toml | cut -d"\"" -f2); \
    PATCH_PREVIEW=$(echo $CURRENT_VER | awk -F. '{print $1"."$2"."$3+1}'); \
    MINOR_PREVIEW=$(echo $CURRENT_VER | awk -F. '{print $1"."$2+1".0"}'); \
    MAJOR_PREVIEW=$(echo $CURRENT_VER | awk -F. '{print $1+1".0.0"}'); \
    echo "Current version: $CURRENT_VER"; \
    echo ""; \
    echo "Select bump type:"; \
    echo "  1) Patch ($CURRENT_VER -> $PATCH_PREVIEW)"; \
    echo "  2) Minor ($CURRENT_VER -> $MINOR_PREVIEW)"; \
    echo "  3) Major ($CURRENT_VER -> $MAJOR_PREVIEW)"; \
    echo "  4) Custom (enter manually)"; \
    echo "  5) No bump (use current version)"; \
    echo "  q) Cancel"; \
    echo ""; \
    read -p "Choice: " choice; \
    case $choice in \
        1) NEW_VERSION="$PATCH_PREVIEW" ;; \
        2) NEW_VERSION="$MINOR_PREVIEW" ;; \
        3) NEW_VERSION="$MAJOR_PREVIEW" ;; \
        4) read -p "Enter new version: " custom_v; \
           if [ -z "$custom_v" ]; then echo "Cancelled."; exit 0; fi; \
           NEW_VERSION="$custom_v" ;; \
        5) NEW_VERSION="$CURRENT_VER" ;; \
        q) echo "Cancelled."; exit 0 ;; \
        *) echo "Invalid choice. Cancelled."; exit 1 ;; \
    esac; \
    DEFAULT_TAG="v$NEW_VERSION"; \
    echo ""; \
    echo "Recent tags:"; \
    git tag --sort=-v:refname | head -5 || echo "  (none)"; \
    echo ""; \
    read -p "Use default tag name '$DEFAULT_TAG'? (y/N): " tag_choice; \
    if [ "$tag_choice" = "y" ] || [ "$tag_choice" = "Y" ] || [ -z "$tag_choice" ]; then \
        TAG="$DEFAULT_TAG"; \
    else \
        read -p "Enter custom tag name: " custom_tag; \
        if [ -z "$custom_tag" ]; then echo "Cancelled."; exit 0; fi; \
        TAG="$custom_tag"; \
    fi; \
    echo ""; \
    echo "Summary of planned actions:"; \
    echo "  - Update Cargo.toml version: $CURRENT_VER -> $NEW_VERSION"; \
    echo "  - Create Git tag:            $TAG"; \
    echo ""; \
    read -p "Apply changes and open commit editor? (y/N): " final_confirm; \
    if [ "$final_confirm" != "y" ] && [ "$final_confirm" != "Y" ]; then \
        echo "Cancelled. No files were altered."; \
        exit 0; \
    fi; \
    if [ "$NEW_VERSION" != "$CURRENT_VER" ]; then \
        sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml; \
        echo "✅ Cargo.toml updated to $NEW_VERSION"; \
    fi; \
    git add Cargo.toml Cargo.lock; \
    echo "Opening editor for commit message..."; \
    if ! git commit; then \
        echo "❌ Commit cancelled. Reverting Cargo.toml..."; \
        git checkout -- Cargo.toml; \
        exit 1; \
    fi; \
    echo ""; \
    read -p "Push commits and create tag $TAG? (y/N): " tag_confirm; \
    if [ "$tag_confirm" = "y" ] || [ "$tag_confirm" = "Y" ]; then \
        echo "Pushing changes to main..."; \
        if ! git push origin main; then \
            echo "❌ Error: Failed to push to origin/main. Aborting tag creation."; \
            echo "💡 Your commit is safe locally. Pull the remote changes, resolve conflicts, and push manually."; \
            exit 1; \
        fi; \
        echo "Creating tag $TAG..."; \
        git tag "$TAG" && git push origin "$TAG"; \
        echo ""; \
        echo "✅ Commits pushed and tag $TAG created!"; \
        echo "🚀 Release complete!"; \
    else \
        echo "Commit made locally, but tag creation and push skipped."; \
    fi


# =================================================================================================
# Project-Specific recipes
# =================================================================================================

# (add project-specific recipes here)
