# Opal — build automation for the arm64 Image format.
#
# `cargo build` produces an ELF (`target/.../opal`). m1n1 (and any arm64
# bootloader speaking the Linux Image protocol) needs a *flat binary* —
# the ELF with the headers stripped, the 64-byte Image header at offset 0
# (already in .text.boot, see boot.rs §0), and nothing else.
#
# `make image` (or `make`) does both steps: builds the ELF, then objcopies
# it to `target/.../opal.img`. `make run` boots the ELF in QEMU (the
# existing `cargo run` path). `make run-img` boots the flat image via
# QEMU's `-kernel` with the Image format (QEMU understands both).
#
# The objcopy path is found via rustup's llvm-tools component, so it works
# on any machine with `rustup` and the toolchain pinned by
# rust-toolchain.toml — no Homebrew LLVM or system objcopy needed.

CARGO    ?= cargo
TARGET   := aarch64-unknown-none-softfloat
PROFILE  := debug
ELF      := target/$(TARGET)/$(PROFILE)/opal
IMG      := target/$(TARGET)/$(PROFILE)/opal.img

# Find llvm-objcopy from the active rustup toolchain's llvm-tools.
# `rustup which` doesn't proxy llvm-objcopy (not a first-class binary),
# so we search the toolchain's lib/rustlib/<host-triple>/bin directory.
# `rustup show active-toolchain -v` prints the toolchain path; we extract
# it and look for llvm-objcopy inside lib/rustlib/*/bin/.
TOOLCHAIN_DIR := $(shell rustup show active-toolchain -v 2>/dev/null | grep '^path:' | sed 's/^path: *//')
OBJCOPY  := $(shell find "$(TOOLCHAIN_DIR)/lib/rustlib" -name llvm-objcopy -type f 2>/dev/null | head -1)

# QEMU flags — mirror .cargo/config.toml's runner.
QEMU     := qemu-system-aarch64
QEMUFLAGS := -machine virt -cpu max -smp 1 -m 512M -nographic

.PHONY: all image build clean run run-img inspect smoke

# Default: build the flat Image.
all: image

# Build the ELF only (what `cargo build` does).
build:
	$(CARGO) build

# Build the ELF, then objcopy to a flat arm64 Image binary.
image: $(IMG)

$(IMG): $(ELF)
	@if [ -z "$(OBJCOPY)" ]; then \
		echo "error: llvm-objcopy not found — run 'rustup component add llvm-tools'"; \
		exit 1; \
	fi
	$(OBJCOPY) -O binary $(ELF) $(IMG)
	@echo "built: $(IMG) ($$(wc -c < $(IMG)) bytes)"

# Ensure the ELF is fresh before objcopy (cargo handles incrementalism).
$(ELF):
	$(CARGO) build

# Boot the ELF in QEMU (the standard dev path).
run:
	$(CARGO) run

# Boot the flat Image in QEMU — exercises the Image header path.
# QEMU's -kernel loader recognizes the arm64 Image magic and loads
# accordingly. This is the closest QEMU gets to the m1n1 load path.
run-img: $(IMG)
	$(QEMU) $(QEMUFLAGS) -kernel $(IMG)

# Automated smoke test: build Image, boot in QEMU, grep for banner proof.
# Runs non-interactively with stdin from /dev/null; kills QEMU after timeout.
# This is the heartbeat's real check that the M7 Image path still works.
SMOKE_TIMEOUT := 8
SMOKE_MATCHES := "opal — milestone" "current EL" "mmu        : on" "monitor ready"
smoke: $(IMG)
	@echo "=== smoke: booting Image in QEMU (timeout $(SMOKE_TIMEOUT)s) ==="
	@output=$$(timeout $(SMOKE_TIMEOUT) $(QEMU) $(QEMUFLAGS) -kernel $(IMG) < /dev/null 2>&1); \
	status=$$?; \
	if [ $$status -eq 124 ]; then \
		echo "smoke: QEMU survived $(SMOKE_TIMEOUT)s (expected; no shutdown yet)"; \
	elif [ $$status -ne 0 ]; then \
		echo "smoke: QEMU exited unexpectedly (code $$status)"; \
		printf '%s\n' "$$output" | tail -40; \
		exit 1; \
	fi; \
	failed=; \
	for m in $(SMOKE_MATCHES); do \
		if ! printf '%s\n' "$$output" | grep -q "$$m"; then \
			echo "smoke: missing expected output: $$m"; \
			failed=1; \
		fi; \
	done; \
	if [ -n "$$failed" ]; then \
		echo "smoke: output dump (last 40 lines):"; \
		printf '%s\n' "$$output" | tail -40; \
		exit 1; \
	fi; \
	echo "smoke: all checks passed"

# Inspect the Image header (first 64 bytes) — verifies magic, flags,
# text_offset, image_size, and the branch target.
inspect: $(IMG)
	@echo "=== $(IMG) ==="
	@file $(IMG)
	@echo "--- header (first 64 bytes) ---"
	@xxd -l 64 $(IMG)
	@echo "--- size ---"
	@wc -c < $(IMG)

# Clean cargo build artifacts AND the flat image.
clean:
	$(CARGO) clean