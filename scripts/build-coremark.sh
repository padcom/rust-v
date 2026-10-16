#!/usr/bin/env bash
# Builds a bare-metal CoreMark image for the rust-v board into third_party/bench/coremark.bin.
# Run it as firmware: rust-v --bios third_party/bench/coremark.bin
# Usage: scripts/build-coremark.sh [ITERATIONS]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${RISCV_PREFIX:-riscv64-elf-}"
ITERATIONS="${1:-2000}"
SRC="$ROOT/third_party/coremark"
PORT="$ROOT/scripts/bench/coremark"
OUT="$ROOT/third_party/bench"
BUILD="$OUT/coremark-build"

[ -d "$SRC" ] || git clone https://github.com/eembc/coremark.git "$SRC"
mkdir -p "$BUILD"

# Reuse the barebones port's code, plugging in the time source (rdtime at 10 MHz) and dropping
# the placeholder UART routine (ours is in support.c).
perl -0pe 's/CORETIMETYPE\nbarebones_clock\(\)\n\{.*?\n\}/CORETIMETYPE\nbarebones_clock()\n{\n    unsigned long t;\n    __asm__ volatile("rdtime %0" : "=r"(t));\n    return (CORETIMETYPE)t;\n}/s' \
  "$SRC/barebones/core_portme.c" | perl -0pe 's/#error \\\n\s*"Call board initialization[^\n]*\n//s' > "$BUILD/core_portme.c"
perl -0pe 's/void\nuart_send_char\(char c\)\n\{.*?\n\}\n//s' "$SRC/barebones/ee_printf.c" > "$BUILD/ee_printf.c"

FLAGS="-O2 -march=rv64imac_zicsr -mabi=lp64 -mcmodel=medany"
"${PREFIX}gcc" $FLAGS -static -nostdlib -ffreestanding -mno-relax -Wl,--no-relax \
  -DCLOCKS_PER_SEC=10000000 -DPERFORMANCE_RUN=1 -DITERATIONS="$ITERATIONS" -DFLAGS_STR="\"$FLAGS\"" \
  -I"$PORT" -I"$SRC" -T "$PORT/link.ld" \
  "$PORT/crt0.S" "$PORT/support.c" "$BUILD/core_portme.c" "$BUILD/ee_printf.c" \
  "$SRC"/core_list_join.c "$SRC"/core_main.c "$SRC"/core_matrix.c "$SRC"/core_state.c "$SRC"/core_util.c \
  -lgcc -o "$OUT/coremark.elf"
"${PREFIX}objcopy" -O binary "$OUT/coremark.elf" "$OUT/coremark.bin"
echo "built $OUT/coremark.bin ($ITERATIONS iterations)"
