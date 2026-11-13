#!/bin/sh
#
# Builds the BIOS the PlayStation core starts: OpenBIOS (pcsx-redux's
# nugget, MIT licence) at a fixed commit, with clang and ld.lld (no MIPS
# gcc needed), without its shell, so it boots the disc. Sony's BIOS is not
# used.
#
#   build-openbios.sh OUT_DIR    -> OUT_DIR/openbios.bin (512 KB)
#
# Needs git, clang with the mipsel target, ld.lld and llvm-objcopy.

set -e
COMMIT=175f329db302892e4aac146a22778d1876cae48f
O=$(realpath -m "$1")
mkdir -p "$O"
if [ ! -d "$O/nugget" ]; then
	git clone -q https://github.com/pcsx-redux/nugget.git "$O/nugget"
fi
git -C "$O/nugget" -c advice.detachedHead=false checkout -q "$COMMIT"
set -- "$O/nugget" "$O"
# A copy of the sources, patched for LLVM's assembler: no rfe (an R3000
# instruction) and no two-operand sltiu in it, and an absolute address
# above 2 GB must be written as a negative 32-bit number.
rm -rf "$O/src"
mkdir -p "$O/src"
cp -r "$1/openbios" "$1/common" "$1/monitor" "$1/nooverlay.ld" "$O/src/"
N=$O/src
B=$N/openbios
find "$N" -name '*.s' -exec sed -i -E 's/^([[:space:]]*)rfe\b/\1.word 0x42000010/; s/sltiu (\$[a-z0-9]+), (0x[0-9a-f]+)/sltiu \1, \1, \2/' {} +
# LLVM assembles li with %lo() as 0, without a relocation: addiu it is.
find "$N" -name '*.s' -exec sed -i -E 's/^([[:space:]]*)li([[:space:]]+)(\$[a-z0-9]+),[[:space:]]*%lo\(/\1addiu\2\3, $zero, %lo(/' {} +
sed -i 's/^\.set BIU_CONFIG, *0xfffe0130/.set BIU_CONFIG, -130768/' "$N/common/hardware/hwregs.inc"
# lld wants power-of-two alignments: the kernel's code starts at 500h.
sed -i 's/\. = ALIGN(0x500);/. = 0x500;/' "$B/psx-bios.ld"
# lld puts .data's load address at its alignment (16), where the boot code
# copies it from __rom_data_start: align that the same.
sed -i '/\.rom_data_start : {/,/}/ s/ALIGN(4)/ALIGN(16)/' "$B/psx-bios.ld"
# The fonts are found by their address, not a symbol: keep them.
sed -i -E 's/^([[:space:]]*)\*\(\.(font[12])\)/\1KEEP(*(.\2))/' "$B/psx-bios.ld"
SRCS="../common/psxlibc/fastmemset.s boot/psx.s card/backupunit.c card/device.c cdrom/cdrom.c cdrom/events.c
cdrom/filesystem.c cdrom/helpers.c cdrom/statemachine.c charset/sjis.c fileio/filesystem.c fileio/misc.c fileio/stdio.c
gpu/gpu.c handlers/irq.c handlers/setup.c handlers/syscall.c kernel/alloc.c kernel/events.c kernel/flushcache.s
kernel/handlers.c kernel/libcmisc.c kernel/memory-c.c kernel/memory-s.s kernel/misc.c kernel/qsort.c kernel/psxexe.c
kernel/psxexec.s kernel/setjmp.s kernel/threads.c kernel/util.c kernel/vectors.s kernel/xprintf.c main/main.c
main/splash.c pio/pio.c shell/shell.c sio0/busyloop.s sio0/card.c sio0/cardfasttrack.s sio0/driver.c sio0/pad.c tty/tty.c
patches/hash.c patches/patches.c patches/clear_card_1.c patches/custom_handler_1.c patches/initgun_1.c
patches/patch_card_info_1.c patches/patch_card_1.c patches/patch_card_2.c patches/patch_card2_1.c patches/patch_card2_2.c
patches/patch_gte_1.c patches/patch_gte_2.c patches/patch_gte_3.c patches/patch_pad_1.c patches/patch_pad_2.c
patches/patch_pad_3.c patches/remove_ChgclrPAD_1.c patches/remove_ChgclrPAD_2.c patches/send_pad_1.c patches/send_pad_2.c"
FLAGS="--target=mipsel-none-elf -march=mips1 -mabi=32 -EL -fno-pic -mno-abicalls -G0 -msoft-float -mno-check-zero-division
-fno-stack-protector -nostdlib -ffreestanding -ffunction-sections -fdata-sections -fomit-frame-pointer -fno-builtin
-fno-strict-aliasing -Wno-attributes -Os -I$N -DNUGGET_BIOS_LONG_CALLS -DXPRINTFNOALLOC -DOPENBIOS_BOOT_MODE_NO_SHELL"
OBJS=""
fail=0
for s in $SRCS; do
	o=$O/$(echo "$s" | tr '/.' '__').o
	if ! clang $FLAGS -c "$B/$s" -o "$o" 2>"$o.log"; then
		echo "FAILED: $s"; head -5 "$o.log"; fail=1
	fi
	OBJS="$OBJS $o"
done
# No shell: an empty one for shell.c to point at; the fonts in their
# sections.
AS="clang --target=mipsel-none-elf -march=mips1 -mabi=32 -msoft-float -c"
printf '.section .rodata\n.global _binary_shell_bin_start\n.global _binary_shell_bin_end\n_binary_shell_bin_start:\n_binary_shell_bin_end:\n' > "$O/shell_data.s"
$AS "$O/shell_data.s" -o "$O/shell_data.o"
for f in font1 font2; do
	printf '.section .%s,"a"\n.balign 4\n.incbin "%s"\n' "$f" "$B/charset/$f.raw" > "$O/$f.s"
	$AS "$O/$f.s" -o "$O/$f.o"
done
[ $fail = 0 ] || exit 1
ld.lld -m elf32ltsmip -nostdlib -static --gc-sections -T "$N/nooverlay.ld" -T "$B/psx-bios.ld" -Map "$O/openbios.map" \
	-o "$O/openbios.elf" $OBJS "$O/shell_data.o" "$O/font1.o" "$O/font2.o"
llvm-objcopy -O binary "$O/openbios.elf" "$O/openbios.bin"
ls -la "$O/openbios.bin"
