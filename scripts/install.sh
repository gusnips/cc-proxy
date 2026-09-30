#!/usr/bin/env bash
#
# cc-proxy installation script
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/gusnips/cc-proxy/main/scripts/install.sh | bash
#   ./scripts/install.sh --local [--release|--debug]   # build this checkout and install it
#
# Environment variables:
#   CC_PROXY_VERSION      - Pin a specific version (e.g., v0.1.0)
#   CC_PROXY_INSTALL_DIR  - Override install directory (default: /usr/local/bin or ~/.local/bin)
#
# Examples:
#   CC_PROXY_VERSION=v0.1.0 bash install.sh
#   CC_PROXY_INSTALL_DIR=/opt/bin bash install.sh
#   ./scripts/install.sh --local
#

set -e

BIN_NAME="cc-proxy"
REPO="gusnips/cc-proxy"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info()    { echo -e "${BLUE}==>${NC} $1"; }
log_success() { echo -e "${GREEN}==>${NC} $1"; }
log_warning() { echo -e "${YELLOW}==>${NC} $1"; }
log_error()   { echo -e "${RED}Error:${NC} $1" >&2; }

detect_platform() {
	local os arch

	case "$(uname -s)" in
	Darwin) os="darwin" ;;
	Linux)  os="linux" ;;
	*)
		log_error "Unsupported operating system: $(uname -s)"
		echo ""
		echo "${BIN_NAME} supports macOS and Linux."
		echo "For other platforms, build from source with Cargo:"
		echo "  git clone https://github.com/${REPO}"
		echo "  ./scripts/install.sh --local"
		echo ""
		exit 1
		;;
	esac

	case "$(uname -m)" in
	x86_64 | amd64)   arch="amd64" ;;
	aarch64 | arm64)  arch="arm64" ;;
	*)
		log_error "Unsupported architecture: $(uname -m)"
		echo ""
		echo "${BIN_NAME} prebuilt binaries are available for amd64 and arm64."
		echo "For other architectures, build from source with Cargo:"
		echo "  git clone https://github.com/${REPO}"
		echo "  ./scripts/install.sh --local"
		echo ""
		exit 1
		;;
	esac

	echo "${os}-${arch}"
}

install_from_release() {
	log_info "Installing ${BIN_NAME} from GitHub releases..."

	local platform=$1
	local tmp_dir
	tmp_dir=$(mktemp -d)
	trap 'rm -rf "$tmp_dir"' EXIT

	local version="${CC_PROXY_VERSION:-${CLAUDE_CODE_PROXY_VERSION:-}}"

	if [ -z "$version" ]; then
		log_info "Fetching latest release..."
		local latest_url="https://api.github.com/repos/${REPO}/releases/latest"
		local release_json

		if command -v curl &>/dev/null; then
			release_json=$(curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 30 "$latest_url")
		elif command -v wget &>/dev/null; then
			release_json=$(wget --tries=3 --timeout=30 -qO- "$latest_url")
		else
			log_error "Neither curl nor wget found. Please install one of them."
			exit 1
		fi

		version=$(echo "$release_json" | grep '"tag_name"' | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')

		if [ -z "$version" ]; then
			log_error "Failed to fetch latest version"
			echo ""
			echo "This might be due to network issues or GitHub API rate limits."
			echo "You can specify a version manually:"
			echo "  CC_PROXY_VERSION=v0.1.0 bash install.sh"
			echo ""
			exit 1
		fi
	fi

	log_info "Installing version: $version"

	local archive_name="${BIN_NAME}-${platform}.tar.gz"
	local download_url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"

	log_info "Downloading $archive_name..."

	cd "$tmp_dir"
	if command -v curl &>/dev/null; then
		if ! curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 120 -o "$archive_name" "$download_url"; then
			log_error "Download failed"
			echo ""
			echo "The release may not have a prebuilt binary for your platform."
			echo ""
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	elif command -v wget &>/dev/null; then
		if ! wget --tries=3 --timeout=120 -q -O "$archive_name" "$download_url"; then
			log_error "Download failed"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	fi

	log_info "Verifying checksum..."
	local checksum_file="${BIN_NAME}-${platform}.sha256"
	local checksum_url="https://github.com/${REPO}/releases/download/${version}/${checksum_file}"

	if command -v curl &>/dev/null; then
		if ! curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 30 -o "$checksum_file" "$checksum_url"; then
			log_error "Failed to download checksum file"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	elif command -v wget &>/dev/null; then
		if ! wget --tries=3 --timeout=30 -q -O "$checksum_file" "$checksum_url"; then
			log_error "Failed to download checksum file"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	fi

	if command -v sha256sum &>/dev/null; then
		if ! sha256sum -c "$checksum_file" &>/dev/null; then
			log_error "Checksum verification failed"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	elif command -v shasum &>/dev/null; then
		if ! shasum -a 256 -c "$checksum_file" &>/dev/null; then
			log_error "Checksum verification failed"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	else
		log_warning "Neither sha256sum nor shasum found, skipping checksum verification"
	fi

	log_success "Checksum verified"

	log_info "Extracting archive..."
	if ! tar -xzf "$archive_name"; then
		log_error "Failed to extract archive"
		exit 1
	fi

	cd - >/dev/null || cd "$HOME"

	install_built_binary "$tmp_dir/${BIN_NAME}" "$version"
}

install_from_source() {
	local profile="$1"
	local repo_root
	repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

	if ! command -v cargo &>/dev/null; then
		log_error "cargo not found. Install Rust first: https://rustup.rs/"
		exit 1
	fi

	log_info "Building ${BIN_NAME} from $repo_root ($profile)..."
	if [ "$profile" = "release" ]; then
		(cd "$repo_root" && cargo build --release --locked)
		install_built_binary "$repo_root/target/release/${BIN_NAME}" "local"
	else
		(cd "$repo_root" && cargo build --locked)
		install_built_binary "$repo_root/target/debug/${BIN_NAME}" "local-debug"
	fi
}

install_built_binary() {
	local binary="$1"
	local version="$2"

	if [ ! -x "$binary" ]; then
		log_error "Built binary not found or not executable: $binary"
		exit 1
	fi

	local install_dir="${CC_PROXY_INSTALL_DIR:-${CLAUDE_CODE_PROXY_INSTALL_DIR:-}}"
	if [ -z "$install_dir" ]; then
		if [[ -w /usr/local/bin ]]; then
			install_dir="/usr/local/bin"
		else
			install_dir="$HOME/.local/bin"
			mkdir -p "$install_dir"
		fi
	fi

	if [ -f "$install_dir/${BIN_NAME}" ]; then
		local existing_version
		existing_version=$("$install_dir/${BIN_NAME}" --version 2>/dev/null | grep -oE 'v?[0-9]+\.[0-9]+\.[0-9]+' || echo "unknown")
		log_info "Existing installation found: $existing_version"
		log_info "Upgrading to: $version"
	fi

	log_info "Installing to $install_dir..."
	local tmp_binary="$install_dir/${BIN_NAME}.tmp.$$"

	if [[ -w "$install_dir" ]]; then
		cp "$binary" "$tmp_binary"
		chmod +x "$tmp_binary"
		mv -f "$tmp_binary" "$install_dir/${BIN_NAME}"
	else
		if ! sudo cp "$binary" "$tmp_binary"; then
			log_error "Failed to install to $install_dir (sudo required)"
			exit 1
		fi
		sudo chmod +x "$tmp_binary"
		sudo mv -f "$tmp_binary" "$install_dir/${BIN_NAME}"
	fi

	if [[ "$(uname -s)" == "Darwin" ]]; then
		if command -v xattr &>/dev/null; then
			xattr -d com.apple.quarantine "$install_dir/${BIN_NAME}" 2>/dev/null || true
		fi
		if command -v codesign &>/dev/null; then
			codesign --remove-signature "$install_dir/${BIN_NAME}" 2>/dev/null || true
			codesign --sign - --force "$install_dir/${BIN_NAME}" 2>/dev/null || true
		fi
	fi

	log_success "${BIN_NAME} installed to $install_dir/${BIN_NAME}"

	if [[ ":$PATH:" != *":$install_dir:"* ]]; then
		log_warning "$install_dir is not in your PATH"
		echo ""
		echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
		echo "  export PATH=\"\$PATH:$install_dir\""
		echo ""
	fi

	INSTALL_DIR="$install_dir"
}

verify_installation() {
	local install_dir="$1"

	if [ ! -x "$install_dir/${BIN_NAME}" ]; then
		log_error "${BIN_NAME} binary not found or not executable at $install_dir/${BIN_NAME}"
		exit 1
	fi

	if ! "$install_dir/${BIN_NAME}" --version &>/dev/null; then
		log_error "${BIN_NAME} binary exists but failed to run"
		exit 1
	fi

	log_success "${BIN_NAME} is installed and ready!"
	echo ""
	"$install_dir/${BIN_NAME}" --version
	echo ""

	echo "Get started:"
	echo "  ${BIN_NAME} codex auth login    # authenticate with your ChatGPT account, or"
	echo "  ${BIN_NAME} kimi auth login     # authenticate with your kimi.com account"
	echo "  ${BIN_NAME} serve               # start the proxy"
	echo ""
	echo "Documentation: https://github.com/${REPO}"
	echo ""
}

main() {
	echo ""
	echo "${BIN_NAME} installer"
	echo ""

	local profile="release"
	local from_source=0
	while [[ $# -gt 0 ]]; do
		case "$1" in
		--local) from_source=1 ;;
		--debug) profile="debug"; from_source=1 ;;
		--release) profile="release" ;;
		-h | --help)
			echo "Usage: install.sh [--local [--debug|--release]]"
			echo ""
			echo "  (no flags)  download the latest release from github.com/${REPO}"
			echo "  --local     build this checkout with cargo and install it"
			echo ""
			return 0
			;;
		*)
			log_error "Unknown argument: $1 (try --help)"
			exit 1
			;;
		esac
		shift
	done

	if [[ "$from_source" -eq 1 ]]; then
		install_from_source "$profile"
	else
		log_info "Detecting platform..."
		local platform
		platform=$(detect_platform)
		log_info "Platform: $platform"

		install_from_release "$platform"
	fi

	verify_installation "$INSTALL_DIR"
}

main "$@"
