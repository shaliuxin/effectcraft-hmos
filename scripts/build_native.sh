#!/usr/bin/env bash
# 交叉编译 EffectCraft 鸿蒙原生库（staticlib → libeffectcraft_hmos.a），双 ABI。
# 用法：
#   ./build_native.sh            # release，arm64-v8a + x86_64
#   ./build_native.sh check      # 只跑 cargo check（快速类型校验，不链接）
#   FC_NATIVE_TARGETS=aarch64-unknown-linux-ohos ./build_native.sh   # 单 ABI
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDK="${FC_NATIVE_SDK:-C:/ohosnative}"
if [[ ! -d "$SDK/llvm" && ! -d "$SDK/sysroot" ]]; then
  echo "HarmonyOS native SDK not found: $SDK (set FC_NATIVE_SDK)" >&2; exit 1
fi
CLANG_WIN="$SDK/llvm/bin/clang.exe"
AR_WIN="$SDK/llvm/bin/llvm-ar.exe"
[[ -x "$CLANG_WIN" || -f "$CLANG_WIN" ]] || { echo "clang.exe not found: $CLANG_WIN" >&2; exit 1; }

MODE="${1:-build}"
TARGETS="${FC_NATIVE_TARGETS:-aarch64-unknown-linux-ohos x86_64-unknown-linux-ohos}"
ARGS=(--manifest-path "$ROOT/native/rust/Cargo.toml")
if [[ "$MODE" == "check" ]]; then
  ARGS+=(--release)
else
  ARGS+=(--release --locked)
fi

for target in $TARGETS; do
  case "$target" in
    aarch64-unknown-linux-ohos|x86_64-unknown-linux-ohos|armv7-unknown-linux-ohos) ;;
    *) echo "Unsupported target: $target" >&2; exit 2;;
  esac
  suffix="${target//-/_}"
  upper="${suffix^^}"
  case "$target" in
    aarch64-unknown-linux-ohos) clang_target="aarch64-linux-ohos";;
    x86_64-unknown-linux-ohos)  clang_target="x86_64-linux-ohos";;
    armv7-unknown-linux-ohos)   clang_target="armv7-linux-ohos";;
  esac
  cflags="--target=$clang_target --sysroot=$SDK/sysroot -D__MUSL__"
  # boa_engine 的 cdylib 已在 third_party/boa_engine-0.22.0 收敛为 rlib-only（见适配层 [patch]），
  # 目标侧不再有 dylib 链接步骤；最终 staticlib 本身也不链接，故无需任何 lld/GNU-ld 特调。
  echo "=== building $target ($MODE) ==="
  if [[ "$MODE" == "check" ]]; then
    env "CC_${suffix}=$CLANG_WIN" "CFLAGS_${suffix}=$cflags" "CFLAGS=$cflags" \
        "AR_${suffix}=$AR_WIN" \
        "CARGO_TARGET_${upper}_LINKER=$CLANG_WIN" \
        cargo check --target "$target" "${ARGS[@]}"
  else
    env "CC_${suffix}=$CLANG_WIN" "CFLAGS_${suffix}=$cflags" "CFLAGS=$cflags" \
        "AR_${suffix}=$AR_WIN" \
        "CARGO_TARGET_${upper}_LINKER=$CLANG_WIN" \
        cargo build --target "$target" "${ARGS[@]}"
  fi
done
echo "=== all targets done ($MODE) ==="
