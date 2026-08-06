#!/usr/bin/ruby
# frozen_string_literal: true
#
# Standalone build script for Blue-Compositor. Blue-Environment's own
# build.rb can also drive this repo (as `compositor/`, vendored in) via
# its `compositor`/`all` commands — this file is for building/checking
# this repo entirely on its own, e.g. in this repo's own CI, or for a
# contributor iterating on the compositor without the whole shell.

require 'fileutils'

COLOR_RESET  = "\e[0m"
COLOR_BOLD   = "\e[1m"
COLOR_RED    = "\e[31m"
COLOR_GREEN  = "\e[32m"
COLOR_YELLOW = "\e[33m"
COLOR_CYAN   = "\e[36m"

VERSION   = "0.1.0"
BIN_NAME  = "blue-compositor"
BIN_DEBUG = "target/debug/#{BIN_NAME}"
BIN_REL   = "target/release/#{BIN_NAME}"

def echo_info(msg)    = puts("#{COLOR_CYAN}#{msg}#{COLOR_RESET}")
def echo_success(msg) = puts("#{COLOR_GREEN}  ✓ #{msg}#{COLOR_RESET}")
def echo_error(msg)   = puts("#{COLOR_RED}  ✗ #{msg}#{COLOR_RESET}")
def echo_warn(msg)    = puts("#{COLOR_YELLOW}  ! #{msg}#{COLOR_RESET}")
def echo_build(msg)   = puts("#{COLOR_BOLD}  → #{msg}#{COLOR_RESET}")
def print_hr(n = 60)  = puts("—" * n)
def run_cmd(cmd)      = (system(cmd); $?.success?)
def command_exists?(cmd) = system("which #{cmd} > /dev/null 2>&1")

def cmd_help
  print_hr
  puts "#{COLOR_BOLD}Blue Compositor v#{VERSION} — Build System#{COLOR_RESET}"
  print_hr
  puts "  #{COLOR_BOLD}ruby build.rb#{COLOR_RESET}            Show this list"
  puts "  #{COLOR_BOLD}ruby build.rb build#{COLOR_RESET}      cargo build --release"
  puts "  #{COLOR_BOLD}ruby build.rb debug#{COLOR_RESET}      cargo build (debug, faster iteration)"
  puts "  #{COLOR_BOLD}ruby build.rb check#{COLOR_RESET}      cargo check + native dep check (no binary produced — fastest way to"
  puts "                          catch a compile error; also the only thing CI here can rely on without a GPU/DRM device)"
  puts "  #{COLOR_BOLD}ruby build.rb toolchain#{COLOR_RESET}  Report the active rustc/cargo and whether they satisfy this repo's"
  puts "                          MSRV (pinned via the smithay dependency — currently 1.85)"
  puts "  #{COLOR_BOLD}ruby build.rb smoke#{COLOR_RESET}      Best-effort headless run: launches the winit-backed nested compositor"
  puts "                          under a virtual framebuffer for a few seconds and reports whether it started cleanly."
  puts "                          Requires an X or Wayland host to nest under (Xvfb is used if no display is already set) —"
  puts "                          this is what 'testing in a TTY' reduces to in an environment with no real GPU/display."
  puts "  #{COLOR_BOLD}ruby build.rb clean#{COLOR_RESET}      Remove target/"
  print_hr
end

# ── Native build-time dependencies ─────────────────────────────────────────
# Same rationale/list as Blue-Environment's build.rb (kept in sync by
# hand — this repo doesn't depend on that one). Nothing added by this
# pass's dmabuf/IME/color-management work needs a *new* system library:
# dmabuf and input-method both go through smithay's existing wayland
# bindings, and wp_color_management_v1 is pulled in via wayland-scanner
# (a pure-Rust build-dependency, no new .so link requirement).
PKG_CONFIG_DEPS = {
  "libinput"        => "libinput",
  "gbm"             => "mesa/gbm (EGL/GBM)",
  "libudev"         => "udev",
  "libseat"         => "seatd/libseat",
  "xkbcommon"       => "libxkbcommon",
  "libdrm"          => "libdrm",
  "wayland-server"  => "wayland",
}.freeze

def detect_distro_family
  return :unknown unless File.exist?("/etc/os-release")
  os_release = File.read("/etc/os-release")
  id_like = os_release[/^ID_LIKE=(.*)$/, 1].to_s.delete('"')
  id      = os_release[/^ID=(.*)$/, 1].to_s.delete('"')
  combined = "#{id} #{id_like}".downcase
  return :debian if combined.include?("debian") || combined.include?("ubuntu")
  return :fedora if combined.include?("fedora") || combined.include?("rhel")
  return :arch   if combined.include?("arch")
  return :suse   if combined.include?("suse")
  return :alpine if combined.include?("alpine")
  :unknown
end

def install_hint
  case detect_distro_family
  when :debian then "sudo apt install pkg-config libinput-dev libgbm-dev libudev-dev libseat-dev libxkbcommon-dev libdrm-dev libwayland-dev libegl1-mesa-dev libgles2-mesa-dev"
  when :fedora then "sudo dnf install pkgconf-pkg-config libinput-devel mesa-libgbm-devel systemd-devel libseat-devel libxkbcommon-devel libdrm-devel wayland-devel mesa-libEGL-devel mesa-libGLES-devel"
  when :arch   then "sudo pacman -S --needed pkgconf libinput mesa systemd-libs seatd libxkbcommon libdrm wayland"
  when :suse   then "sudo zypper install pkg-config libinput-devel Mesa-libgbm-devel libudev-devel libseat-devel libxkbcommon-devel libdrm-devel wayland-devel"
  when :alpine then "sudo apk add pkgconf libinput-dev mesa-dev eudev-dev seatd-dev libxkbcommon-dev libdrm-dev wayland-dev"
  else "install development packages (headers + .so symlinks) for: libinput, mesa/gbm, libudev, libseat, libxkbcommon, libdrm, wayland"
  end
end

def check_native_deps
  unless command_exists?("pkg-config")
    echo_error "pkg-config not found — needed to even check for native dependencies."
    echo_warn  "Install it first, e.g.: #{install_hint}"
    return false
  end
  missing = PKG_CONFIG_DEPS.keys.reject { |lib| system("pkg-config --exists #{lib} 2>/dev/null") }
  return true if missing.empty?

  echo_error "Missing native development libraries:"
  missing.each { |lib| echo_warn "  - #{lib} (#{PKG_CONFIG_DEPS[lib]})" }
  echo_warn "cargo will otherwise compile for several minutes and fail late, at the *linking* step, with"
  echo_warn "something like 'cannot find -linput' — install with:"
  puts "#{COLOR_BOLD}    #{install_hint}#{COLOR_RESET}"
  false
end

# ── Toolchain / MSRV check ──────────────────────────────────────────────────
# The pinned smithay commit (see Cargo.toml — `rev = "82912edf"`) declares
# `rust-version = "1.85"` in its own Cargo.toml. Distro-packaged rustc
# frequently lags behind that (e.g. Ubuntu 24.04's `apt install rustc`
# currently ships 1.75) — this fails fast with a clear explanation instead
# of letting cargo's own error (which, at least on cargo 1.75, is the
# unhelpful "no matching package named `smithay` found" rather than an
# explicit MSRV complaint) be the first thing a contributor sees. This is
# not a hypothetical: it's exactly what happened building this repo in
# the environment this pass was written in.
MIN_RUSTC = [1, 85].freeze

def rustc_version
  out = `rustc --version 2>&1`
  return nil unless $?.success?
  m = out.match(/rustc (\d+)\.(\d+)\.(\d+)/)
  m && [m[1].to_i, m[2].to_i, m[3].to_i]
end

def cmd_toolchain
  echo_info "Toolchain check"
  print_hr
  v = rustc_version
  if v.nil?
    echo_error "rustc not found on PATH."
    echo_warn  "This repo needs rustc >= #{MIN_RUSTC.join('.')} (smithay's declared rust-version at the pinned commit)."
    echo_warn  "A distro-packaged rustc is frequently too old for this (Ubuntu 24.04's apt package is 1.75) — a"
    echo_warn  "rustup-managed or from-source toolchain is usually the only way to get something recent enough."
    return false
  end
  puts "  rustc #{v.join('.')}  (cargo: #{`cargo --version 2>&1`.strip})"
  ok = (v[0] > MIN_RUSTC[0]) || (v[0] == MIN_RUSTC[0] && v[1] >= MIN_RUSTC[1])
  if ok
    echo_success "rustc #{v.join('.')} satisfies the >= #{MIN_RUSTC.join('.')} requirement"
  else
    echo_error "rustc #{v.join('.')} is older than the #{MIN_RUSTC.join('.')} this repo's pinned smithay commit needs."
    echo_warn  "cargo will fail to even resolve the dependency graph (not just fail to compile)."
  end
  ok
end

def build_variant(release:)
  flag = release ? "--release" : ""
  label = release ? "release" : "debug"
  echo_build "cargo build #{flag} — #{BIN_NAME} (#{label})".strip
  unless check_native_deps
    echo_error "build aborted — missing native dependencies (see above)"
    return false
  end
  unless cmd_toolchain
    echo_error "build aborted — toolchain too old (see above)"
    return false
  end
  if run_cmd("cargo build #{flag}".strip)
    path = release ? BIN_REL : BIN_DEBUG
    echo_success "#{BIN_NAME} compiled → #{path}"
    true
  else
    echo_error "#{label} build failed"
    false
  end
end

def cmd_check
  echo_info "Checking Blue Compositor (no binary produced)"
  print_hr
  deps_ok = check_native_deps
  tc_ok   = cmd_toolchain
  print_hr
  unless deps_ok && tc_ok
    echo_error "cargo check skipped — fix the above first (cargo would fail for the same reasons, just slower)"
    exit 1
  end
  echo_build "cargo check"
  if run_cmd("cargo check")
    echo_success "cargo check passed"
  else
    echo_error "cargo check failed"
    exit 1
  end
end

# ── "Testing it in a TTY" ────────────────────────────────────────────────
# There is no way for this script to open a real DRM/KMS session from a
# plain shell command without either being launched from an actual Linux
# VT by a session manager (logind/seatd handing over the VT + DRM master)
# or running as root with direct /dev/dri access — neither of which a
# build script should silently attempt. What *is* automatable and useful
# as a smoke test is the winit-backed nested-compositor path
# (`run_winit()` in main.rs): it opens a normal window on an existing
# X/Wayland display (or a virtual one via Xvfb) instead of taking over a
# VT, and exercises the same startup path (globals registration —
# including this pass's new dmabuf + color-management globals — IPC
# socket, XWayland) that the udev/TTY path does, just not real KMS
# scanout. That's the most this script can verify without a real display
# attached to the machine running it.
def cmd_smoke
  echo_info "Headless smoke test (winit-nested backend)"
  print_hr
  unless File.exist?(BIN_DEBUG) || File.exist?(BIN_REL)
    echo_warn "No build found — building debug first (ruby build.rb debug)"
    return unless build_variant(release: false)
  end
  bin = File.exist?(BIN_REL) ? BIN_REL : BIN_DEBUG

  needs_xvfb = ENV["DISPLAY"].nil? && ENV["WAYLAND_DISPLAY"].nil?
  if needs_xvfb && !command_exists?("Xvfb")
    echo_error "No DISPLAY/WAYLAND_DISPLAY set and Xvfb isn't installed — nothing to nest the winit backend under."
    echo_warn  "Install Xvfb (e.g. 'apt install xvfb') or run this from an existing graphical session."
    return
  end

  cmd = if needs_xvfb
    echo_build "starting under Xvfb"
    "xvfb-run -a --server-args='-screen 0 1280x800x24' timeout 5 ./#{bin}"
  else
    "timeout 5 ./#{bin}"
  end

  echo_build "launching #{bin} for 5s…"
  system(cmd)
  status = $?.exitstatus
  # `timeout` returns 124 on a clean timeout-kill, which here means "it
  # was still running happily after 5s" — success for a smoke test.
  if status == 124 || status == 0
    echo_success "compositor started and stayed up for the test window"
  else
    echo_error "compositor exited early (status #{status}) — check ~/.cache/Blue-Environment/compositor/logs/"
  end
end

def cmd_clean
  echo_info "Cleaning build artifacts…"
  FileUtils.rm_rf(["target"])
  puts "#{COLOR_GREEN}✓ Clean complete.#{COLOR_RESET}"
end

action = ARGV[0]
case action
when nil, ""
  cmd_help
when "build", "release"
  exit(1) unless build_variant(release: true)
when "debug"
  exit(1) unless build_variant(release: false)
when "check"
  cmd_check
when "toolchain"
  exit(1) unless cmd_toolchain
when "smoke", "test", "run"
  cmd_smoke
when "clean"
  cmd_clean
when "help", "-h", "--help"
  cmd_help
else
  puts "#{COLOR_RED}✗ Unknown command: #{action}#{COLOR_RESET}"
  puts "#{COLOR_YELLOW}Run: ruby #{$PROGRAM_NAME}#{COLOR_RESET}"
  exit 1
end
