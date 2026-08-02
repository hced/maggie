# =================================================================================================
# Global Configuration
# =================================================================================================

set shell := ["fish", "-c"]

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
  grep '^version = ' Cargo.toml | cut -d' ' -f2 | tr -d '"'

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

# (no recipes added here yet)


# =================================================================================================
# Project-Specific recipes
# =================================================================================================

# (add project-specific recipes here)
