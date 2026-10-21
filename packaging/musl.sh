#!/bin/sh
# Runs cargo for a static musl target. fdk-aac (C++) is compiled by zig, which brings musl headers
# for every architecture; plain clang would use the host's glibc ones.
#
#   packaging/musl.sh x86_64 build --locked --release
#   packaging/musl.sh aarch64 test --locked
#
# Needs zig and, for aarch64, rust-lld (shipped with rustup's toolchains).
set -eu

arch=${1:?usage: $0 x86_64|aarch64 <cargo command and arguments>}
shift
case $arch in
x86_64 | aarch64) ;;
*) echo "unsupported architecture: $arch" >&2 && exit 1 ;;
esac
target=$arch-unknown-linux-musl
var=$(echo "$target" | tr a-z- A-Z_)
tools=$(cd "$(dirname "$0")/.." && pwd)/target/musl-tools
mkdir -p "$tools"

# cc-rs passes its own --target; zig takes the target from -target instead.
cat >"$tools/cc-$arch" <<EOF
#!/bin/sh
for arg; do shift; case "\$arg" in --target=*) ;; *) set -- "\$@" "\$arg" ;; esac; done
exec zig cc -target $arch-linux-musl "\$@"
EOF
printf '#!/bin/sh\nexec zig ar "$@"\n' >"$tools/ar"
chmod +x "$tools/cc-$arch" "$tools/ar"
# fdk-aac needs only libc, but fdk-aac-sys always links libstdc++: an empty archive satisfies it.
[ -f "$tools/libstdc++.a" ] || zig ar rc "$tools/libstdc++.a"

export "CC_$(echo "$target" | tr - _)=$tools/cc-$arch"
export "AR_$(echo "$target" | tr - _)=$tools/ar"
# Without -O flags (debug builds) zig turns UBSan on, whose runtime is not linked.
export "CFLAGS_$(echo "$target" | tr - _)=-fno-sanitize=all"
export "CXXFLAGS_$(echo "$target" | tr - _)=-fno-sanitize=all"
export "CARGO_TARGET_${var}_RUSTFLAGS=-L $tools"
if [ "$arch" = aarch64 ]; then
    export "CARGO_TARGET_${var}_LINKER=rust-lld"
fi
exec cargo "$@" --target "$target"
