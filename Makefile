# Run `make` targets from the workspace root.
export PATH := $(HOME)/.cargo/bin:$(PATH)

# Application to package: make img APP=irq_timer
APP        ?= hello_world
OUT        := target/armv7a-none-eabihf/release
ELF        := $(OUT)/$(APP)
BIN        := $(OUT)/$(APP).bin
IMG        := $(OUT)/boot.img
TOOLS       = $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/host: //p')/bin
MKBOOTIMG  ?= $(HOME)/mkbootimg/mkbootimg
# bootcmd reads 0x4FC0 sectors of 512 bytes, one 4096-byte boot.img page may precede the image.
MAX_SIZE   := $$(( 0x4FC0 * 512 - 4096 ))

.PHONY: build bin img disasm clean

build:
	cargo build --release

bin: build
	$(TOOLS)/rust-objcopy -O binary $(ELF) $(BIN)
	@size=$$(stat -c %s $(BIN)); echo "$(BIN): $$size bytes"; \
	if [ $$size -gt $(MAX_SIZE) ]; then echo "error: larger than bootcmd reads"; exit 1; fi

# Same invocation as ~/prepare.sh, with the same pagesize and no ramdisk.
img: bin
	$(MKBOOTIMG) --kernel $(BIN) --ramdisk NONE --cmdline "console=ttySAC2,115200" --pagesize 4096 -o $(IMG)
	@echo "$(IMG): $$(stat -c %s $(IMG)) bytes"

disasm: build
	@$(TOOLS)/llvm-nm $(ELF) | grep -E ' (_start|__exynos_vectors|__image_end|__bss_start|__stacks_end)$$'
	@echo; echo "--- _start: zImage header, the data words at 0x24-0x33 are not code"
	@$(TOOLS)/llvm-objdump -d --no-show-raw-insn --disassemble-symbols=_start $(ELF) | sed -n '/<_start>:/,$$p'
	@echo; echo "--- entry code up to main"
	@$(TOOLS)/llvm-objdump -d --no-show-raw-insn --disassemble-symbols=__exynos_boot,__exynos_relocated $(ELF) | grep -E '^[0-9a-f]+( <|:)'
	@echo; echo "--- vector table"
	@$(TOOLS)/llvm-objdump -d --no-show-raw-insn --disassemble-symbols=__exynos_vectors $(ELF) | sed -n '/<__exynos_vectors>:/,$$p'

clean:
	cargo clean
